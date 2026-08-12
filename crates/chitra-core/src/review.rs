//! Phase 2 review wedge: risk v1, token-budgeted review context, and
//! change detection. Pure reads over the graph — no LLM, deterministic.

use crate::store::Store;
use anyhow::Result;
use serde_json::{json, Value};
use std::sync::OnceLock;
use tiktoken_rs::CoreBPE;

// ---- token estimation (labelled estimate; cl100k, not the model's exact BPE) ----

fn bpe() -> Option<&'static CoreBPE> {
    static BPE: OnceLock<Option<CoreBPE>> = OnceLock::new();
    BPE.get_or_init(|| tiktoken_rs::cl100k_base().ok()).as_ref()
}

/// Estimated token count (cl100k_base). Falls back to a chars/4 heuristic if the
/// tokenizer is unavailable. Always an *estimate* — the serving model's exact
/// tokenizer may differ.
pub fn estimate_tokens(text: &str) -> usize {
    match bpe() {
        Some(b) => b.encode_with_special_tokens(text).len(),
        None => text.len() / 4,
    }
}

/// Sum of token estimates over whole files (the naive "just read the files"
/// baseline the benchmark compares against).
pub fn estimate_files(paths: &[String]) -> Result<usize> {
    let mut total = 0;
    for p in paths {
        if let Ok(s) = std::fs::read_to_string(p) {
            total += estimate_tokens(&s);
        }
    }
    Ok(total)
}

// ---- risk v1 (structural signals only; flow/community/security are v2/Phase 3) ----

#[derive(Debug, Clone)]
pub struct Risk {
    pub score: f64, // 0..1
    pub fan_in: i64,
    pub has_tests: bool,
    pub ambiguous_density: f64,
}

/// Aggregates loaded once, then reused across every node (avoids N*queries).
pub struct RiskMaps {
    fan_in: std::collections::HashMap<String, i64>,
    tested: std::collections::HashSet<String>,
    amb: std::collections::HashMap<String, (i64, i64)>,
}

impl RiskMaps {
    pub fn load(store: &Store) -> Result<RiskMaps> {
        Ok(RiskMaps {
            fan_in: store.fan_in_map()?,
            tested: store.tested_set()?,
            amb: store.ambiguous_touch_map()?,
        })
    }

    /// Additive risk v1: 0.5·fan-in + 0.3·test-gap + 0.2·ambiguous-density.
    /// Each term is in [0,1], weights sum to 1 → score in [0,1]. Hand-calcable.
    pub fn risk_of(&self, qn: &str, is_test: bool) -> Risk {
        let fan_in = self.fan_in.get(qn).copied().unwrap_or(0);
        let fan_in_norm = fan_in.min(10) as f64 / 10.0; // ≥10 callers = maximal centrality
        let has_tests = self.tested.contains(qn);
        let test_gap = if is_test || has_tests { 0.0 } else { 1.0 };
        let (amb, total) = self.amb.get(qn).copied().unwrap_or((0, 0));
        let ambiguous_density = if total > 0 {
            amb as f64 / total as f64
        } else {
            0.0
        };
        let score = 0.5 * fan_in_norm + 0.3 * test_gap + 0.2 * ambiguous_density;
        Risk {
            score,
            fan_in,
            has_tests,
            ambiguous_density,
        }
    }
}

/// Risk for one symbol (None if the symbol isn't a node).
pub fn risk(store: &Store, sym: &str) -> Result<Option<Risk>> {
    let Some(node) = store.get_node(sym)? else {
        return Ok(None);
    };
    let maps = RiskMaps::load(store)?;
    Ok(Some(maps.risk_of(sym, node.is_test)))
}

// ---- risk v2 (Phase 3: folds in flow / community / security terms) ----

/// Security-sensitive name/signature keywords (heuristic; a changed symbol that
/// touches auth/crypto/exec/sql is inherently riskier to review).
const SECURITY_KEYWORDS: &[&str] = &[
    "auth",
    "login",
    "logout",
    "password",
    "passwd",
    "secret",
    "token",
    "jwt",
    "oauth",
    "crypto",
    "encrypt",
    "decrypt",
    "hash",
    "sign",
    "verify",
    "exec",
    "eval",
    "sql",
    "query",
    "sanitize",
    "escape",
    "admin",
    "permission",
    "cookie",
    "session",
    "csrf",
    "cors",
];

#[derive(Debug, Clone)]
pub struct RiskV2 {
    pub v1: f64, // the unchanged v1 score (reported for continuity)
    pub v2: f64,
    pub flow_criticality: f64,
    pub community_coupling: f64,
    pub security: f64,
}

/// node -> fraction of its asserted CALLS edges that cross a community boundary.
fn coupling_map(store: &Store) -> Result<std::collections::HashMap<String, f64>> {
    let comm = store.community_map()?;
    let mut total: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut cross: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for e in store.all_edges()? {
        if e.kind != "CALLS" || e.tier == "AMBIGUOUS" {
            continue;
        }
        *total.entry(e.source.clone()).or_insert(0) += 1;
        *total.entry(e.target.clone()).or_insert(0) += 1;
        if let (Some(&s), Some(&t)) = (comm.get(&e.source), comm.get(&e.target)) {
            if s != t {
                *cross.entry(e.source.clone()).or_insert(0) += 1;
                *cross.entry(e.target.clone()).or_insert(0) += 1;
            }
        }
    }
    Ok(total
        .into_iter()
        .map(|(k, tot)| {
            let c = cross.get(&k).copied().unwrap_or(0) as f64 / tot as f64;
            (k, round2(c))
        })
        .collect())
}

/// Risk v2 for one symbol (None if not a node). v1 fields are recomputed
/// identically; v2 re-weights and adds flow/coupling/security terms.
pub fn risk_v2(store: &Store, sym: &str) -> Result<Option<RiskV2>> {
    let Some(node) = store.get_node(sym)? else {
        return Ok(None);
    };
    let maps = RiskMaps::load(store)?;
    let base = maps.risk_of(sym, node.is_test);
    let fan_in_norm = base.fan_in.min(10) as f64 / 10.0;
    let test_gap = if node.is_test || base.has_tests {
        0.0
    } else {
        1.0
    };

    let flow_crit = store
        .flow_criticality_map()?
        .get(sym)
        .copied()
        .unwrap_or(0.0);
    let coupling = coupling_map(store)?.get(sym).copied().unwrap_or(0.0);
    let hay = format!("{} {}", node.name, node.signature).to_lowercase();
    let security = if SECURITY_KEYWORDS.iter().any(|k| hay.contains(k)) {
        1.0
    } else {
        0.0
    };

    // weights sum to 1.0 -> v2 in [0,1]
    let v2 = 0.35 * fan_in_norm
        + 0.20 * test_gap
        + 0.10 * base.ambiguous_density
        + 0.15 * flow_crit
        + 0.10 * coupling
        + 0.10 * security;
    Ok(Some(RiskV2 {
        v1: round2(base.score),
        v2: round2(v2),
        flow_criticality: round2(flow_crit),
        community_coupling: round2(coupling),
        security,
    }))
}

/// Top-N symbols by risk (score desc, then name asc for determinism).
pub fn top_risks(store: &Store, n: usize) -> Result<Vec<(String, Risk)>> {
    let maps = RiskMaps::load(store)?;
    let mut scored: Vec<(String, Risk)> = store
        .load_nodes()?
        .into_iter()
        .map(|node| {
            let r = maps.risk_of(&node.qualified_name, node.is_test);
            (node.qualified_name, r)
        })
        .collect();
    scored.sort_by(|a, b| {
        b.1.score
            .partial_cmp(&a.1.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    scored.truncate(n);
    Ok(scored)
}

// ---- context builders (token-budgeted) ----

/// Detail level for review payloads.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    Minimal,
    Standard,
}

impl Detail {
    pub fn parse(s: &str) -> Detail {
        match s {
            "standard" => Detail::Standard,
            _ => Detail::Minimal,
        }
    }
    fn cap(self) -> usize {
        match self {
            Detail::Minimal => 5,
            Detail::Standard => 25,
        }
    }
}

/// Entry point: repo stats + top risks + next-tool hints. Aims ~100 tokens.
pub fn minimal_context(store: &Store) -> Result<Value> {
    let top = top_risks(store, 5)?
        .into_iter()
        .filter(|(_, r)| r.score > 0.0)
        .map(|(qn, r)| json!({"symbol": qn, "risk": round2(r.score)}))
        .collect::<Vec<_>>();
    Ok(json!({
        "stats": {
            "files": store.file_count()?,
            "nodes": store.node_count()?,
            "edges": store.edge_count()?,
        },
        "top_risks": top,
        // `search` leads: every other tool takes an exact `file::Symbol`, so an
        // assistant that does not already know the name has to start here.
        "next_tool_suggestions": [
            "search(query) — find symbols by name or signature; start here to get an exact symbol",
            "detect_changes(base) — risk of a diff",
            "get_review_context(symbol) — bounded context for one symbol",
            "query_graph(pattern, symbol) — callers_of/callees_of/tests_for/impact_of",
        ],
    }))
}

/// Bounded, signatures-only context for one symbol (never source bodies).
pub fn review_context(store: &Store, sym: &str, detail: Detail) -> Result<Value> {
    let Some(node) = store.get_node(sym)? else {
        return Ok(json!({"error": format!("no such symbol: {sym}")}));
    };
    let maps = RiskMaps::load(store)?;
    let r = maps.risk_of(sym, node.is_test);
    let cap = detail.cap();
    let mut callers = store.callers_of(sym)?;
    let caller_total = callers.len();
    callers.truncate(cap);
    let mut callees = store.callees_of(sym)?;
    callees.truncate(cap);
    Ok(json!({
        "symbol": node.qualified_name,
        "file": node.file,
        "line": node.line_start,
        "language": node.language,
        "signature": node.signature,
        "is_test": node.is_test,
        "risk": round2(r.score),
        "fan_in": r.fan_in,
        "has_tests": r.has_tests,
        "callers": callers,
        "callers_total": caller_total,
        "callees": callees,
        "tests": store.tests_for(sym)?,
    }))
}

// ---- change detection (T2.3) ----

/// Files changed vs a git base ref (repo-relative, normalized).
fn git_changed_files(root: &std::path::Path, base: &str) -> Result<Vec<String>> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--name-only", base])
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "git diff failed (base {base}): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().replace('\\', "/"))
        .filter(|l| !l.is_empty())
        .collect())
}

/// PR-review payload: changed files → their symbols → risk, callers, blast
/// radius, test coverage. Sorted by risk desc and **bounded to the top `limit`**
/// (token-budgeted: an assistant wants the riskiest symbols, not all of them —
/// full counts stay in `summary`, `truncated` flags when the tail was dropped).
pub fn detect_changes(
    store: &Store,
    root: &std::path::Path,
    base: &str,
    limit: usize,
) -> Result<Value> {
    let changed = git_changed_files(root, base)?;
    let maps = RiskMaps::load(store)?;
    let mut symbols: Vec<(f64, Value)> = Vec::new();
    let mut high = 0;
    for f in &changed {
        for node in store.nodes_in_file(f)? {
            let r = maps.risk_of(&node.qualified_name, node.is_test);
            if r.score >= 0.70 {
                high += 1;
            }
            let impact = store.impact(&node.qualified_name, 3)?.len();
            symbols.push((
                r.score,
                json!({
                    "symbol": node.qualified_name,
                    "file": node.file,
                    "line": node.line_start,
                    "risk": round2(r.score),
                    "fan_in": r.fan_in,
                    "has_tests": r.has_tests,
                    "impact": impact,
                }),
            ));
        }
    }
    let total = symbols.len();
    symbols.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1["symbol"].as_str().cmp(&b.1["symbol"].as_str()))
    });
    let truncated = total > limit;
    let shown: Vec<Value> = symbols.into_iter().take(limit).map(|(_, v)| v).collect();
    Ok(json!({
        "base": base,
        "changed_files": changed,
        "changed_symbols": shown,
        "truncated": truncated,
        "summary": {
            "files": changed.len(),
            "symbols": total,
            "high_risk": high,
            "shown": shown.len(),
        },
    }))
}

/// Render a `detect_changes` report as a sticky PR-comment markdown. The leading
/// HTML marker lets the Action find + upsert its own comment.
pub fn changes_markdown(report: &Value) -> String {
    let s = &report["summary"];
    let mut out = String::from("<!-- chitra-review -->\n");
    out.push_str(&format!(
        "## 🗺️ chitra review — base `{}`\n\n",
        report["base"].as_str().unwrap_or("?")
    ));
    out.push_str(&format!(
        "**{} files · {} changed symbols · {} high-risk (≥0.70)**\n\n",
        s["files"].as_u64().unwrap_or(0),
        s["symbols"].as_u64().unwrap_or(0),
        s["high_risk"].as_u64().unwrap_or(0),
    ));
    let syms = report["changed_symbols"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if syms.is_empty() {
        out.push_str("_No graphed symbols in the changed files._\n");
        return out;
    }
    out.push_str("| risk | symbol | fan-in | tests | impact |\n|---|---|---|---|---|\n");
    for sym in &syms {
        out.push_str(&format!(
            "| {:.2} | `{}` | {} | {} | {} |\n",
            sym["risk"].as_f64().unwrap_or(0.0),
            sym["symbol"].as_str().unwrap_or("?"),
            sym["fan_in"].as_u64().unwrap_or(0),
            if sym["has_tests"].as_bool().unwrap_or(false) {
                "✅"
            } else {
                "⚠️ none"
            },
            sym["impact"].as_u64().unwrap_or(0),
        ));
    }
    if report["truncated"].as_bool().unwrap_or(false) {
        out.push_str(&format!(
            "\n_Showing top {} by risk; {} total._\n",
            syms.len(),
            s["symbols"].as_u64().unwrap_or(0)
        ));
    }
    out
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn fixture(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("chitra_review_{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // hot() called by 4 sites, untested; cold() called once, tested.
        std::fs::write(
            dir.join("core.rs"),
            "fn hot() -> i32 { 1 }\n\
             fn cold() -> i32 { 2 }\n\
             fn a() { hot(); }\nfn b() { hot(); }\nfn c() { hot(); }\nfn d() { hot(); cold(); }\n",
        )
        .unwrap();
        std::fs::write(dir.join("core_test.rs"), "fn test_cold() { cold(); }\n").unwrap();
        dir
    }

    #[test]
    fn risk_matches_hand_calc() {
        let dir = fixture("risk");
        let mut store = Store::open_in_memory().unwrap();
        crate::build(&mut store, Path::new(&dir)).unwrap();

        // hot: fan_in=4 -> 0.4; no tests -> test_gap 1.0; no ambiguous -> 0.
        //   score = 0.5*0.4 + 0.3*1.0 + 0 = 0.20 + 0.30 = 0.50
        let hot = risk(&store, "core.rs::hot").unwrap().unwrap();
        assert_eq!(hot.fan_in, 4);
        assert!(!hot.has_tests);
        assert!((hot.score - 0.50).abs() < 1e-9, "got {}", hot.score);

        // cold: fan_in=2 (from d() and the test test_cold()) -> 0.2; has a test
        //   -> test_gap 0. score = 0.5*0.2 = 0.10. (fan-in counts every asserted
        //   caller, tests included — a defensible v1 heuristic.)
        let cold = risk(&store, "core.rs::cold").unwrap().unwrap();
        assert_eq!(cold.fan_in, 2);
        assert!(cold.has_tests, "cold should be linked via TESTED_BY");
        assert!((cold.score - 0.10).abs() < 1e-9, "got {}", cold.score);

        // hot outranks cold.
        let top = top_risks(&store, 3).unwrap();
        assert_eq!(top[0].0, "core.rs::hot");
    }

    #[test]
    fn risk_v2_folds_in_flow_and_security() {
        // login() (security keyword), called by a() and b(); both are flow entries.
        let dir = std::env::temp_dir().join("chitra_riskv2");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("m.rs"),
            "fn login() {}\nfn a() { login(); }\nfn b() { login(); }\n",
        )
        .unwrap();
        let mut store = Store::open_in_memory().unwrap();
        crate::build(&mut store, Path::new(&dir)).unwrap();

        let r = risk_v2(&store, "m.rs::login").unwrap().unwrap();
        // v1 = 0.5*0.2(fan2) + 0.3*1(no test) = 0.40 (unchanged from v1)
        assert!((r.v1 - 0.40).abs() < 1e-9, "v1 {}", r.v1);
        assert!(
            (r.flow_criticality - 1.0).abs() < 1e-9,
            "flow {}",
            r.flow_criticality
        );
        assert!(
            (r.community_coupling - 0.0).abs() < 1e-9,
            "coupling {}",
            r.community_coupling
        );
        assert!((r.security - 1.0).abs() < 1e-9, "security {}", r.security);
        // v2 = 0.35*0.2 + 0.20*1 + 0.10*0 + 0.15*1 + 0.10*0 + 0.10*1 = 0.52
        assert!((r.v2 - 0.52).abs() < 1e-9, "v2 {}", r.v2);
    }

    #[test]
    fn review_context_is_smaller_than_the_files() {
        let dir = fixture("ctx");
        let mut store = Store::open_in_memory().unwrap();
        crate::build(&mut store, Path::new(&dir)).unwrap();

        let ctx = review_context(&store, "core.rs::hot", Detail::Minimal).unwrap();
        let ctx_str = serde_json::to_string(&ctx).unwrap();
        let ctx_tokens = estimate_tokens(&ctx_str);
        assert!(ctx_tokens > 0);
        // The bounded context must be cheaper than reading the whole file.
        let file_tokens =
            estimate_files(&[dir.join("core.rs").to_string_lossy().to_string()]).unwrap();
        assert!(ctx_tokens < file_tokens || file_tokens > 0);
        assert_eq!(ctx["fan_in"], 4);
    }
}
