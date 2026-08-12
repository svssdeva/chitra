//! chitra-mcp — a minimal MCP server over chitra-core.
//!
//! Hand-rolled JSON-RPC 2.0 over stdio (newline-delimited), no framework: the
//! protocol surface an assistant needs is `initialize` / `tools/list` /
//! `tools/call`, and stdio is what Claude Code speaks by default. Every tool
//! returns compact JSON with a `_meta.token_estimate` (cl100k, labelled an
//! estimate). Transport is stdio only; streamable-http is deferred (see serve()).

use anyhow::Result;
use chitra_core::{
    architecture, detect_changes, estimate_tokens, minimal_context, review_context, Detail, Store,
};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::Path;

const PROTOCOL_VERSION: &str = "2024-11-05";

/// Run the stdio server loop until EOF. `allow` (if set) restricts which tools
/// are listed and callable. `root` is the repo root for git-based tools.
pub fn serve(db: &str, root: &Path, allow: Option<Vec<String>>) -> Result<()> {
    let store = Store::open(db)?;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                write_msg(&mut stdout, &parse_error(e.to_string()))?;
                continue;
            }
        };
        if let Some(resp) = handle(&req, &store, root, allow.as_deref()) {
            write_msg(&mut stdout, &resp)?;
        }
    }
    Ok(())
}

fn write_msg(out: &mut impl Write, v: &Value) -> Result<()> {
    writeln!(out, "{v}")?;
    out.flush()?;
    Ok(())
}

/// Dispatch one JSON-RPC message. Returns `None` for notifications (no `id`).
pub fn handle(req: &Value, store: &Store, root: &Path, allow: Option<&[String]>) -> Option<Value> {
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = req.get("id").cloned();

    // Notifications carry no id and get no response.
    let id = id?;

    let result: std::result::Result<Value, (i64, String)> = match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "chitra", "version": env!("CARGO_PKG_VERSION") },
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_specs(allow) })),
        "tools/call" => call_tool(req, store, root, allow),
        other => Err((-32601, format!("method not found: {other}"))),
    };

    Some(match result {
        Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
        Err((code, message)) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
        }
    })
}

fn call_tool(
    req: &Value,
    store: &Store,
    root: &Path,
    allow: Option<&[String]>,
) -> std::result::Result<Value, (i64, String)> {
    let params = req.get("params").cloned().unwrap_or(json!({}));
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    if let Some(list) = allow {
        if !list.iter().any(|t| t == name) {
            return Err((-32601, format!("tool not in allowlist: {name}")));
        }
    }

    // Tool errors are returned as isError content, not protocol errors, so the
    // assistant can read the message (MCP convention).
    match dispatch(name, &args, store, root) {
        Ok(v) => Ok(mcp_text(v, false)),
        Err(e) => Ok(mcp_text(json!({ "error": e.to_string() }), true)),
    }
}

/// Wrap a payload as MCP tool content, stamping a token estimate into it.
fn mcp_text(mut payload: Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(&payload).unwrap_or_default();
    let tokens = estimate_tokens(&text);
    if let Some(obj) = payload.as_object_mut() {
        obj.insert(
            "_meta".into(),
            json!({ "token_estimate": tokens, "tokenizer": "cl100k_base (estimate)" }),
        );
    }
    let text = serde_json::to_string_pretty(&payload).unwrap_or(text);
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

fn dispatch(name: &str, args: &Value, store: &Store, root: &Path) -> Result<Value> {
    let sym = || args.get("symbol").and_then(|s| s.as_str()).unwrap_or("");
    match name {
        "get_minimal_context" => minimal_context(store),
        "architecture" => architecture(store),
        "get_review_context" => {
            let detail = Detail::parse(
                args.get("detail_level")
                    .and_then(|d| d.as_str())
                    .unwrap_or("minimal"),
            );
            review_context(store, sym(), detail)
        }
        "detect_changes" => {
            let base = args.get("base").and_then(|b| b.as_str()).unwrap_or("HEAD");
            let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(20) as usize;
            detect_changes(store, root, base, limit)
        }
        "impact" => {
            let depth = args.get("depth").and_then(|d| d.as_i64()).unwrap_or(3);
            let forward = args.get("direction").and_then(|d| d.as_str()) == Some("forward");
            let nodes = if forward {
                store.impact_forward(sym(), depth)?
            } else {
                store.impact(sym(), depth)?
            };
            Ok(json!({
                "symbol": sym(),
                "direction": if forward { "forward" } else { "reverse" },
                "depth": depth,
                "nodes": nodes,
            }))
        }
        // Every other tool needs an exact `file::Symbol`. This is the one that
        // produces one, so it is the entry point for an assistant that only has
        // words to go on.
        "search" => {
            let q = args.get("query").and_then(|s| s.as_str()).unwrap_or("");
            let limit = args.get("limit").and_then(|l| l.as_i64()).unwrap_or(20);
            let (results, mode) = search_symbols(store, q, limit)?;
            Ok(json!({
                "query": q,
                "mode": mode,
                "count": results.len(),
                "symbols": results,
            }))
        }
        "query_graph" => {
            let pattern = args.get("pattern").and_then(|p| p.as_str()).unwrap_or("");
            let out = match pattern {
                "callers_of" => json!({ "callers": store.callers_of(sym())? }),
                "callees_of" => json!({ "callees": store.callees_of(sym())? }),
                "tests_for" => json!({ "tests": store.tests_for(sym())? }),
                "impact_of" => json!({ "impact": store.impact(sym(), 3)? }),
                "community_of" => {
                    let members = match store.community_map()?.get(sym()) {
                        Some(&id) => store.community_members(id)?,
                        None => vec![],
                    };
                    json!({ "symbol": sym(), "community_members": members })
                }
                "file_summary" => {
                    let names: Vec<String> = store
                        .nodes_in_file(sym())?
                        .into_iter()
                        .map(|n| n.qualified_name)
                        .collect();
                    json!({ "file": sym(), "symbols": names })
                }
                other => anyhow::bail!(
                    "unknown pattern {other}; use callers_of|callees_of|tests_for|impact_of|community_of|file_summary"
                ),
            };
            Ok(out)
        }
        other => anyhow::bail!("unknown tool: {other}"),
    }
}

/// FTS by default. An `embeddings` build additionally fuses lexical trigrams
/// and graph context, exactly as `chitra search --hybrid` does — otherwise the
/// CLI would have a retrieval channel the assistant could not reach. The mode
/// is reported back so a caller knows which one answered.
fn search_symbols(store: &Store, q: &str, limit: i64) -> Result<(Vec<String>, &'static str)> {
    #[cfg(feature = "embeddings")]
    {
        Ok((
            chitra_core::hybrid_search(store, q, limit as usize)?,
            "hybrid",
        ))
    }
    #[cfg(not(feature = "embeddings"))]
    {
        Ok((store.search(q, limit)?, "fts"))
    }
}

/// Tool catalog (filtered by allowlist), each with a JSON-Schema input.
fn tool_specs(allow: Option<&[String]>) -> Vec<Value> {
    let all = vec![
        json!({
            "name": "get_minimal_context",
            "description": "Repo stats, top risks, and next-tool suggestions (~100 tokens). Start here.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "get_review_context",
            "description": "Bounded, signatures-only context for one symbol: risk, callers, callees, tests.",
            "inputSchema": { "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "qualified name file::Symbol" },
                    "detail_level": { "type": "string", "enum": ["minimal", "standard"] }
                }, "required": ["symbol"] }
        }),
        json!({
            "name": "detect_changes",
            "description": "Risk report over files changed vs a git base ref (symbols ranked by change risk).",
            "inputSchema": { "type": "object",
                "properties": { "base": { "type": "string", "description": "git ref, e.g. origin/main" } },
                "required": ["base"] }
        }),
        json!({
            "name": "impact",
            "description": "Bounded blast radius: who transitively reaches a symbol (or, forward, what it reaches).",
            "inputSchema": { "type": "object",
                "properties": {
                    "symbol": { "type": "string" },
                    "depth": { "type": "integer" },
                    "direction": { "type": "string", "enum": ["reverse", "forward"] }
                }, "required": ["symbol"] }
        }),
        json!({
            "name": "search",
            "description": "Find symbols by name or signature. Use this first when you have words rather than an exact symbol — every other tool needs a `file::Symbol` id, and this is what produces one.",
            "inputSchema": { "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "words to match against symbol names and signatures" },
                    "limit": { "type": "integer", "description": "max results, default 20" }
                }, "required": ["query"] }
        }),
        json!({
            "name": "query_graph",
            "description": "Pattern query: callers_of | callees_of | tests_for | impact_of | community_of | file_summary.",
            "inputSchema": { "type": "object",
                "properties": {
                    "pattern": { "type": "string",
                        "enum": ["callers_of", "callees_of", "tests_for", "impact_of", "community_of", "file_summary"] },
                    "symbol": { "type": "string" }
                }, "required": ["pattern", "symbol"] }
        }),
        json!({
            "name": "architecture",
            "description": "Community structure overview: modules, sizes, top members, cross-community coupling.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
    ];
    match allow {
        Some(list) => all
            .into_iter()
            .filter(|t| list.iter().any(|n| n == t["name"].as_str().unwrap_or("")))
            .collect(),
        None => all,
    }
}

fn parse_error(msg: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": msg } })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Per-test directory. These tests run in parallel inside one binary, and a
    /// shared fixture let one test's `remove_dir_all` land between another's
    /// `create_dir_all` and `write` — a ~17% flake rate, and the failure pointed
    /// at whichever test lost the race rather than at the cause.
    fn store_with_fixture(tag: &str) -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("chitra_mcp_{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("m.rs"),
            "fn helper() -> i32 { 1 }\nfn run() { helper(); }\n",
        )
        .unwrap();
        let mut store = Store::open_in_memory().unwrap();
        chitra_core::build(&mut store, &dir).unwrap();
        (store, dir)
    }

    fn req(id: i64, method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    #[test]
    fn initialize_returns_protocol_and_server_info() {
        let (store, dir) = store_with_fixture("initialize_returns_p");
        let r = handle(&req(1, "initialize", json!({})), &store, &dir, None).unwrap();
        assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(r["result"]["serverInfo"]["name"], "chitra");
    }

    /// The gap this tool closes: every other tool needs an exact
    /// `file::Symbol`, so without search an assistant holding only words has no
    /// way in.
    #[test]
    fn search_finds_a_symbol_from_words_alone() {
        let (store, dir) = store_with_fixture("search_finds_a_sym");
        let r = handle(
            &req(
                1,
                "tools/call",
                json!({ "name": "search", "arguments": { "query": "helper" } }),
            ),
            &store,
            &dir,
            None,
        )
        .unwrap();
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        let out: Value = serde_json::from_str(text).unwrap();
        assert_eq!(out["symbols"][0], "m.rs::helper");
        assert!(out["_meta"]["token_estimate"].as_u64().unwrap() > 0);
    }

    /// FTS5 reads `(` and `)` as operators, so an unguarded MATCH answers a
    /// query like this with a syntax error instead of results.
    #[test]
    fn search_survives_fts_operator_characters() {
        let (store, _dir) = store_with_fixture("search_survives_fts");
        assert!(
            store.search("helper()", 10).is_ok(),
            "a query containing FTS5 operators must not error"
        );
    }

    #[test]
    fn minimal_context_counts_files_not_nodes() {
        let (store, _dir) = store_with_fixture("minimal_ctx_files");
        let ctx = chitra_core::minimal_context(&store).unwrap();
        // One fixture file holding two functions.
        assert_eq!(ctx["stats"]["files"], 1);
        assert_eq!(ctx["stats"]["nodes"], 2);
    }

    #[test]
    fn notifications_get_no_response() {
        let (store, dir) = store_with_fixture("notifications_get_no");
        let n = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle(&n, &store, &dir, None).is_none());
    }

    #[test]
    fn tools_list_and_allowlist() {
        let (store, dir) = store_with_fixture("tools_list_and_allow");
        let all = handle(&req(2, "tools/list", json!({})), &store, &dir, None).unwrap();
        let names: Vec<&str> = all["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        // Named rather than counted: a bare count says nothing about which tool
        // went missing, and the surface is what an assistant actually sees.
        assert_eq!(
            names,
            vec![
                "get_minimal_context",
                "get_review_context",
                "detect_changes",
                "impact",
                "search",
                "query_graph",
                "architecture",
            ]
        );

        let allow = vec!["impact".to_string()];
        let one = handle(&req(3, "tools/list", json!({})), &store, &dir, Some(&allow)).unwrap();
        assert_eq!(one["result"]["tools"].as_array().unwrap().len(), 1);
        // a call outside the allowlist is rejected
        let call = req(
            4,
            "tools/call",
            json!({ "name": "get_minimal_context", "arguments": {} }),
        );
        let denied = handle(&call, &store, &dir, Some(&allow)).unwrap();
        assert!(denied["error"].is_object());
    }

    #[test]
    fn tools_call_impact_returns_token_stamped_json() {
        let (store, dir) = store_with_fixture("tools_call_impact_re");
        let call = req(
            5,
            "tools/call",
            json!({ "name": "impact", "arguments": { "symbol": "m.rs::helper", "depth": 3 } }),
        );
        let r = handle(&call, &store, &dir, None).unwrap();
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        let payload: Value = serde_json::from_str(text).unwrap();
        assert!(payload["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n == "m.rs::run"));
        assert!(payload["_meta"]["token_estimate"].as_u64().unwrap() > 0);
    }

    #[test]
    fn unknown_method_is_jsonrpc_error() {
        let (store, dir) = store_with_fixture("unknown_method_is_js");
        let r = handle(&req(6, "nope", json!({})), &store, &dir, None).unwrap();
        assert_eq!(r["error"]["code"], -32601);
    }
}
