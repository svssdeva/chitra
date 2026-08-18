//! chitra — the binary. Thin shell over chitra-core.
//! Subcommands: build, update, impact, query, search, export.

mod install;

use anyhow::{bail, Result};
use std::path::Path;

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

/// First positional arg after the subcommand (not a `--flag` or its value).
fn positional(args: &[String]) -> Option<&str> {
    args.get(2)
        .map(|s| s.as_str())
        .filter(|a| !a.starts_with("--"))
}

/// Accept a bare symbol name wherever a qualified one is expected.
///
/// Nobody types `apps/main/src/data/github.ts::enrichProjects` from memory, and
/// the graph already knows whether the short form is unambiguous. Ambiguity
/// lists the candidates instead of guessing one.
fn resolve_symbol(store: &chitra_core::Store, sym: &str) -> Result<String> {
    match store.resolve_symbol(sym)? {
        chitra_core::SymbolMatch::Exact(qn) => Ok(qn),
        chitra_core::SymbolMatch::None => bail!("no such symbol: {sym}"),
        chitra_core::SymbolMatch::Ambiguous(hits) => bail!(
            "`{sym}` matches {} symbols; qualify it:\n  {}",
            hits.len(),
            hits.join("\n  ")
        ),
    }
}

fn ensure_parent(db: &str) -> Result<()> {
    if let Some(parent) = Path::new(db).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let db = flag(&args, "--db")
        .unwrap_or(".chitra/graph.db")
        .to_string();

    match args.get(1).map(|s| s.as_str()) {
        Some("build") | Some("update") => {
            let full = args[1] == "build";
            let dir = positional(&args).unwrap_or(".");
            // Without this, a typo or an unrecognised single-dash flag is read
            // as the directory and the build "succeeds" over zero files —
            // reporting `0 nodes, 0 edges` as if the repository were empty.
            if !Path::new(dir).is_dir() {
                bail!("not a directory: {dir}");
            }
            ensure_parent(&db)?;
            let mut store = chitra_core::Store::open(&db)?;
            let s = if full {
                chitra_core::build(&mut store, Path::new(dir))?
            } else {
                chitra_core::update(&mut store, Path::new(dir))?
            };
            println!(
                "{} {} files ({} re-parsed) -> {} nodes, {} edges  (db: {db})",
                if full { "built" } else { "updated" },
                s.files,
                s.reparsed,
                s.nodes,
                s.edges
            );
        }
        Some("impact") => {
            let Some(sym) = positional(&args) else {
                bail!("usage: chitra impact <symbol> [--db <path>] [--depth <n>]");
            };
            let depth: i64 = flag(&args, "--depth")
                .and_then(|s| s.parse().ok())
                .unwrap_or(3);
            let store = chitra_core::Store::open(&db)?;
            let sym = &resolve_symbol(&store, sym)?;
            let deps = store.impact(sym, depth)?;
            if deps.is_empty() {
                println!("no dependents of {sym} within depth {depth}");
            } else {
                println!("impact of {sym} (depth {depth}): {} dependents", deps.len());
                for d in deps {
                    println!("  {d}");
                }
            }
        }
        // query <pattern> <symbol>: callers_of | callees_of | tests_for
        Some("query") => {
            let pattern = positional(&args);
            let sym = args.get(3).filter(|a| !a.starts_with("--"));
            let (Some(pattern), Some(sym)) = (pattern, sym) else {
                bail!(
                    "usage: chitra query <callers_of|callees_of|tests_for> <symbol> [--db <path>]"
                );
            };
            let store = chitra_core::Store::open(&db)?;
            // Same bare-name resolution as risk/impact/review: accepting a name
            // in one command and reporting "no results" for it in the next is a
            // false negative at exit 0.
            let sym = &resolve_symbol(&store, sym)?;
            let out = match pattern {
                "callers_of" => store.callers_of(sym)?,
                "callees_of" => store.callees_of(sym)?,
                "tests_for" => store.tests_for(sym)?,
                other => bail!("unknown pattern {other}; use callers_of|callees_of|tests_for"),
            };
            if out.is_empty() {
                println!("no {pattern} results for {sym}");
            } else {
                for r in out {
                    println!("  {r}");
                }
            }
        }
        Some("search") => {
            let Some(q) = positional(&args) else {
                bail!("usage: chitra search <fts-query> [--hybrid] [--db <path>] [--limit <n>]");
            };
            let limit: i64 = flag(&args, "--limit")
                .and_then(|s| s.parse().ok())
                .unwrap_or(20);
            let store = chitra_core::Store::open(&db)?;
            let hybrid = args.iter().any(|a| a == "--hybrid");
            let results = if hybrid {
                #[cfg(feature = "embeddings")]
                {
                    chitra_core::hybrid_search(&store, q, limit as usize)?
                }
                #[cfg(not(feature = "embeddings"))]
                {
                    bail!("--hybrid needs the `embeddings` feature: cargo build --features embeddings")
                }
            } else {
                store.search(q, limit)?
            };
            for r in results {
                println!("  {r}");
            }
        }
        Some("export") => {
            let store = chitra_core::Store::open(&db)?;
            let json = chitra_core::export_json(&store)?;
            match flag(&args, "--out") {
                Some(path) => {
                    std::fs::write(path, json)?;
                    println!("wrote {path}");
                }
                None => println!("{json}"),
            }
        }
        // ---- Phase 2 review wedge ----
        Some("risk") => {
            let Some(sym) = positional(&args) else {
                bail!("usage: chitra risk <symbol> [--db <path>]");
            };
            let store = chitra_core::Store::open(&db)?;
            let sym = &resolve_symbol(&store, sym)?;
            match chitra_core::risk(&store, sym)? {
                None => bail!("no such symbol: {sym}"),
                Some(r) => println!(
                    "risk v1 {:.2}  (fan_in={}, has_tests={}, ambiguous_density={:.2})  {sym}",
                    r.score,
                    r.fan_in,
                    // Say so when the term was not applied, or the printed
                    // numbers do not reconstruct the printed score.
                    if r.test_gap_applies {
                        r.has_tests.to_string()
                    } else {
                        "n/a (kind cannot be tested)".to_string()
                    },
                    r.ambiguous_density
                ),
            }
            if let Some(v) = chitra_core::risk_v2(&store, sym)? {
                println!(
                    "risk v2 {:.2}  (flow_criticality={:.2}, community_coupling={:.2}, security={:.0})",
                    v.v2, v.flow_criticality, v.community_coupling, v.security
                );
            }
        }
        // ---- Phase 3 structure ----
        Some("communities") => {
            let store = chitra_core::Store::open(&db)?;
            let comms = store.communities()?;
            if comms.is_empty() {
                println!("no communities (build first, or graph too sparse)");
            }
            for (id, size, label) in comms {
                println!("  #{id}  size={size}  {label}");
            }
        }
        Some("community") => {
            let Some(id) = positional(&args).and_then(|s| s.parse::<i64>().ok()) else {
                bail!("usage: chitra community <id> [--db <path>]");
            };
            let store = chitra_core::Store::open(&db)?;
            for m in store.community_members(id)? {
                println!("  {m}");
            }
        }
        Some("architecture") => {
            let store = chitra_core::Store::open(&db)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&chitra_core::architecture(&store)?)?
            );
        }
        Some("flows") => {
            let store = chitra_core::Store::open(&db)?;
            let flows = store.flows()?;
            if flows.is_empty() {
                println!("no flows detected");
            }
            for f in flows {
                println!(
                    "  #{}  criticality={:.2}  size={}  entry={}",
                    f.id, f.criticality, f.size, f.entry
                );
            }
        }
        Some("flow") => {
            let Some(id) = positional(&args).and_then(|s| s.parse::<i64>().ok()) else {
                bail!("usage: chitra flow <id> [--db <path>]");
            };
            let store = chitra_core::Store::open(&db)?;
            for m in store.flow_members(id)? {
                println!("  {m}");
            }
        }
        Some("review") => {
            let Some(sym) = positional(&args) else {
                bail!("usage: chitra review <symbol> [--detail minimal|standard] [--db <path>]");
            };
            let detail = chitra_core::Detail::parse(flag(&args, "--detail").unwrap_or("minimal"));
            let store = chitra_core::Store::open(&db)?;
            let sym = &resolve_symbol(&store, sym)?;
            let ctx = chitra_core::review_context(&store, sym, detail)?;
            println!("{}", serde_json::to_string_pretty(&ctx)?);
        }
        Some("detect-changes") => {
            let base = flag(&args, "--base").unwrap_or("HEAD");
            let root = flag(&args, "--root").unwrap_or(".");
            let limit: usize = flag(&args, "--limit")
                .and_then(|s| s.parse().ok())
                .unwrap_or(20);
            let store = chitra_core::Store::open(&db)?;
            let report = chitra_core::detect_changes(&store, Path::new(root), base, limit)?;
            match flag(&args, "--format") {
                Some("md") => println!("{}", chitra_core::changes_markdown(&report)),
                _ => println!("{}", serde_json::to_string_pretty(&report)?),
            }
            // Optional CI risk gate: nonzero exit when a high-risk symbol changed.
            if args.iter().any(|a| a == "--fail-on-risk")
                && report["summary"]["high_risk"].as_u64().unwrap_or(0) > 0
            {
                eprintln!("risk gate: high-risk changes present");
                std::process::exit(1);
            }
        }
        Some("serve") => {
            if args.iter().any(|a| a == "--http") {
                bail!("streamable-http transport is not implemented; stdio is the supported transport");
            }
            let root = flag(&args, "--root").unwrap_or(".");
            let allow = flag(&args, "--tools").map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_string())
                    .collect::<Vec<_>>()
            });
            chitra_mcp::serve(&db, Path::new(root), allow)?;
        }
        // ---- Phase 4 watch/daemon ----
        Some("watch") | Some("daemon") => {
            let interval = std::time::Duration::from_millis(
                flag(&args, "--interval")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(chitra_core::DEFAULT_INTERVAL_MS),
            );
            let roots: Vec<chitra_core::WatchRoot> = match flag(&args, "--roots") {
                // Multi-repo: each root keeps its own in-repo database.
                Some(list) => list
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(chitra_core::WatchRoot::new)
                    .collect(),
                // Single repo honours --db like every other subcommand.
                None => vec![chitra_core::WatchRoot::with_db(
                    positional(&args).unwrap_or("."),
                    db.clone(),
                )],
            };
            chitra_core::watch_loop(&roots, interval)?;
        }
        Some("install") => {
            let root = flag(&args, "--root").unwrap_or(".");
            let root =
                std::fs::canonicalize(root).unwrap_or_else(|_| Path::new(root).to_path_buf());
            // An agent starts the server from an arbitrary directory, so the
            // database it is told to open must be absolute too.
            let abs_db = if Path::new(&db).is_absolute() {
                db.clone()
            } else {
                install::plain(&root.join(&db))
            };
            install::run(&install::Options {
                root,
                db: abs_db,
                only: flag(&args, "--platform").map(str::to_string),
                dry_run: args.iter().any(|a| a == "--dry-run"),
            })?;
        }
        Some("visualize") => {
            let out = flag(&args, "--out").unwrap_or("graph.html").to_string();
            let mode = chitra_core::VizMode::parse(flag(&args, "--mode").unwrap_or("community"));
            let max_nodes: usize = flag(&args, "--max-nodes")
                .and_then(|s| s.parse().ok())
                .unwrap_or(20_000);
            let store = chitra_core::Store::open(&db)?;
            ensure_parent(&out)?;
            let html = chitra_core::visualize_html(&store, mode, max_nodes)?;
            std::fs::write(&out, &html)?;
            println!(
                "wrote {out} ({} KB) — open it in a browser; nothing is fetched at view time",
                html.len() / 1024
            );
        }
        // ---- Phase 4 cross-repo ----
        Some("federate") => {
            let Some(list) = flag(&args, "--repos") else {
                bail!(
                    "usage: chitra federate --repos <name=path|path>,... [--out combined.db]\n\
                     each path is a repo directory or a graph.db built with `chitra build`"
                );
            };
            let repos: Vec<chitra_core::RepoRef> = list
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(chitra_core::RepoRef::parse)
                .collect::<Result<_>>()?;
            let out_db = flag(&args, "--out").unwrap_or("combined.db").to_string();
            ensure_parent(&out_db)?;
            let mut out = chitra_core::Store::open(&out_db)?;
            let s = chitra_core::federate(&repos, &mut out)?;
            println!(
                "federated {} repos -> {} nodes, {} edges ({} crossing a repo boundary)  (db: {out_db})",
                s.repos, s.nodes, s.edges, s.cross_edges
            );
        }
        // Benchmark helper: sum cl100k token estimates over whole files.
        Some("estimate") => {
            let paths: Vec<String> = args[2..]
                .iter()
                .filter(|a| !a.starts_with("--"))
                .cloned()
                .collect();
            if paths.is_empty() {
                bail!("usage: chitra estimate <file>...");
            }
            println!("{}", chitra_core::estimate_files(&paths)?);
        }
        _ => {
            eprintln!(
                "chitra <build|update [dir] | impact <sym> | query <pattern> <sym> \
                 | search <q> [--hybrid] \
                 | risk <sym> | review <sym> | detect-changes --base <ref> \
                 | communities | community <id> | architecture | flows | flow <id> \
                 | watch [dir] | daemon --roots a,b [--interval <ms>] \
                 | visualize [--out graph.html] [--mode community|full]                  | install [--platform claude|codex|cursor|antigravity|...] [--dry-run] \
                 | federate --repos a=x.db,b=y.db [--out combined.db] \
                 | serve [--tools a,b] | estimate <file>... | export> \
                 [--db <path>] [--depth <n>] [--out <file>]"
            );
            std::process::exit(2);
        }
    }
    Ok(())
}
