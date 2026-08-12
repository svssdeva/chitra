//! Cross-repo graphs: join several per-repo graphs into one and resolve the
//! calls that dangle at each repo's edge.
//!
//! A service that calls a shared library has half its flow in another
//! repository. Each graph on its own drops those calls as "external" — correct
//! per repo, useless for the question "what breaks if I change this". Federation
//! answers it by re-resolving each repo's unresolved calls against every other
//! repo's symbols.
//!
//! The output is an ordinary graph database: node ids are prefixed with the repo
//! name (`api/src/main.rs::run`), so every existing command — impact, flows,
//! risk, communities, export, MCP — works on it unchanged. That is the whole
//! design: no federated query engine, no new schema, no second code path.
//!
//! ponytail: rebuild the combined graph when you want it fresh. Incremental
//! federation would need change tracking per member repo, which is only worth
//! building if the rebuild ever gets slow (it is a copy, not a re-parse).

use crate::store::{NewEdge, Store};
use anyhow::{anyhow, Context, Result};
use chitra_lang::Node;
use std::collections::HashMap;
use std::path::Path;

/// One member of a federation: a label and the graph database to pull in.
#[derive(Debug, Clone)]
pub struct RepoRef {
    pub name: String,
    pub db: String,
}

impl RepoRef {
    /// Accepts `name=path`, or a bare path whose directory name becomes the
    /// label. A path that is a directory resolves to its in-repo database.
    pub fn parse(spec: &str) -> Result<RepoRef> {
        let (name, raw) = match spec.split_once('=') {
            Some((n, p)) => (n.trim().to_string(), p.trim()),
            None => {
                let p = spec.trim();
                let label = Path::new(p)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .filter(|s| s != ".chitra" && !s.ends_with(".db"))
                    .unwrap_or_else(|| p.to_string());
                (label, p)
            }
        };
        if name.is_empty() {
            return Err(anyhow!("repo label missing in `{spec}`"));
        }
        let path = Path::new(raw);
        let db = if path.is_dir() {
            path.join(".chitra/graph.db").to_string_lossy().to_string()
        } else {
            raw.to_string()
        };
        if !Path::new(&db).exists() {
            return Err(anyhow!("no graph database at `{db}` (build it first)"));
        }
        Ok(RepoRef { name, db })
    }
}

pub struct FederateStats {
    pub repos: usize,
    pub nodes: usize,
    pub edges: usize,
    /// Edges that cross a repository boundary — the reason to federate at all.
    pub cross_edges: usize,
}

/// Namespace a per-repo id/path under its repo label.
fn prefixed(repo: &str, path: &str) -> String {
    format!("{repo}/{path}")
}

/// A call this repo could not resolve on its own — a candidate for a cross-repo
/// edge, if the evidence supports one.
struct DanglingCall {
    caller: String,
    callee: String,
    line: i64,
    language: String,
    file: String,
}

/// Everything loaded from one member repo, already relabelled.
struct Member {
    name: String,
    nodes: Vec<Node>,
    edges: Vec<NewEdge>,
    calls: Vec<DanglingCall>,
    /// file -> the names it imports. Cross-repo edges are only drawn where a
    /// file actually imported the name, so `info()` in one repo cannot be
    /// silently bound to an unrelated `info()` in another.
    imports: HashMap<String, Vec<chitra_lang::Import>>,
}

fn load_member(repo: &RepoRef) -> Result<Member> {
    let store = Store::open(&repo.db).with_context(|| format!("opening {}", repo.db))?;
    let nodes: Vec<Node> = store
        .load_nodes()?
        .into_iter()
        .map(|mut n| {
            n.qualified_name = prefixed(&repo.name, &n.qualified_name);
            n.file = prefixed(&repo.name, &n.file);
            n
        })
        .collect();

    let edges = store
        .all_edges()?
        .into_iter()
        .map(|e| NewEdge {
            source: prefixed(&repo.name, &e.source),
            target: prefixed(&repo.name, &e.target),
            // The stored `kind`/`tier` are owned Strings; the insert type wants
            // static labels, so map them back onto the known set.
            kind: match e.kind.as_str() {
                "TESTED_BY" => "TESTED_BY",
                _ => "CALLS",
            },
            line: e.line,
            confidence: e.confidence,
            tier: match e.tier.as_str() {
                "EXTRACTED" => "EXTRACTED",
                "AMBIGUOUS" => "AMBIGUOUS",
                _ => "INFERRED",
            },
        })
        .collect();

    let lang_of: HashMap<String, String> = nodes
        .iter()
        .map(|n| (n.qualified_name.clone(), n.language.clone()))
        .collect();
    let calls = store
        .load_raw_calls()?
        .into_iter()
        .filter_map(|rc| {
            let caller = prefixed(&repo.name, &rc.caller);
            Some(DanglingCall {
                language: lang_of.get(&caller)?.clone(),
                caller,
                callee: rc.callee,
                line: rc.line,
                file: prefixed(&repo.name, &rc.file),
            })
        })
        .collect();

    let imports = store
        .load_imports()?
        .into_iter()
        .map(|(f, v)| (prefixed(&repo.name, &f), v))
        .collect();

    Ok(Member {
        name: repo.name.clone(),
        nodes,
        edges,
        calls,
        imports,
    })
}

/// Build the combined graph in `out`. Returns what was joined.
pub fn federate(repos: &[RepoRef], out: &mut Store) -> Result<FederateStats> {
    if repos.len() < 2 {
        return Err(anyhow!("federation needs at least two repos"));
    }
    let members: Vec<Member> = repos.iter().map(load_member).collect::<Result<_>>()?;

    out.clear_all()?;
    for m in &members {
        out.insert_nodes(&m.nodes)?;
    }

    // Index every symbol by short name, remembering which repo it came from.
    let mut by_name: HashMap<&str, Vec<(&str, &Node)>> = HashMap::new();
    for m in &members {
        for n in &m.nodes {
            by_name
                .entry(n.name.as_str())
                .or_default()
                .push((&m.name, n));
        }
    }

    let mut edges: Vec<NewEdge> = members
        .iter()
        .flat_map(|m| m.edges.iter().cloned())
        .collect();
    let mut cross = 0usize;

    for m in &members {
        for c in &m.calls {
            let Some(cands) = by_name.get(c.callee.as_str()) else {
                continue; // truly external — stdlib, third party
            };
            // Only calls the owning repo could not resolve are candidates for a
            // cross-repo edge; anything local was already decided there, and
            // re-deciding it here would second-guess better-informed evidence.
            if cands.iter().any(|(repo, _)| *repo == m.name) {
                continue;
            }
            // **Import evidence is mandatory across a repo boundary.** Two repos
            // that never reference each other will still share common short
            // names (`info`, `run`, `parse`); binding those on uniqueness alone
            // invents a dependency that does not exist. A file must actually
            // have imported the name.
            let evidence: Vec<&chitra_lang::Import> = m
                .imports
                .get(&c.file)
                .map(|v| v.iter().filter(|i| i.name == c.callee).collect())
                .unwrap_or_default();
            if evidence.is_empty() {
                continue;
            }
            let module_known = evidence.iter().any(|i| i.module.is_some());

            // Same guards as the single-repo resolver, plus: never point at a
            // test symbol — a test is nobody's public API.
            let mut matching = cands.iter().filter(|(_, n)| {
                n.language == c.language
                    && !n.is_test
                    && (!module_known
                        || evidence.iter().any(|i| {
                            i.module
                                .as_deref()
                                .is_some_and(|md| crate::build::module_matches(md, &n.file))
                        }))
            });
            let (Some((_, target)), None) = (matching.next(), matching.next()) else {
                continue;
            };
            // Cross-repo evidence is weaker than an in-repo import (no build
            // system was consulted), so confidence stays below the local tiers.
            edges.push(NewEdge {
                source: c.caller.clone(),
                target: target.qualified_name.clone(),
                kind: "CALLS",
                line: c.line,
                confidence: 0.5,
                tier: "INFERRED",
            });
            cross += 1;
        }
    }

    out.replace_edges(&edges)?;
    if let Err(e) = out.rebuild_fts() {
        eprintln!("warn: FTS index skipped: {e}");
    }
    if let Err(e) = crate::structure::postprocess(out) {
        eprintln!("warn: structure postprocess skipped: {e}");
    }
    out.set_meta("last_build_type", "federated")?;
    out.set_meta(
        "federated_repos",
        &members
            .iter()
            .map(|m| m.name.as_str())
            .collect::<Vec<_>>()
            .join(","),
    )?;

    Ok(FederateStats {
        repos: members.len(),
        nodes: out.node_count()? as usize,
        edges: out.edge_count()? as usize,
        cross_edges: cross,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(tag: &str, files: &[(&str, &str)]) -> String {
        let dir = std::env::temp_dir().join(format!("chitra_fed_{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, src) in files {
            std::fs::write(dir.join(name), src).unwrap();
        }
        let db = dir.join("g.db").to_string_lossy().to_string();
        let mut store = Store::open(&db).unwrap();
        crate::build(&mut store, &dir).unwrap();
        db
    }

    /// A service calling into a shared library: the edge exists only once the
    /// two graphs are joined.
    fn two_repo_federation(tag: &str) -> (Store, FederateStats) {
        let lib = repo(
            &format!("{tag}_lib"),
            &[("core.py", "def encrypt(x):\n    return x\n")],
        );
        let svc = repo(
            &format!("{tag}_svc"),
            &[(
                "api.py",
                "from core import encrypt\ndef handle(r):\n    return encrypt(r)\n",
            )],
        );
        let mut out = Store::open_in_memory().unwrap();
        let stats = federate(
            &[
                RepoRef {
                    name: "lib".into(),
                    db: lib,
                },
                RepoRef {
                    name: "svc".into(),
                    db: svc,
                },
            ],
            &mut out,
        )
        .unwrap();
        (out, stats)
    }

    #[test]
    fn a_dangling_call_resolves_across_repos() {
        let (out, stats) = two_repo_federation("basic");
        assert_eq!(stats.repos, 2);
        assert_eq!(stats.cross_edges, 1);

        // The library function's blast radius now reaches the other repository.
        let users = out.impact("lib/core.py::encrypt", 3).unwrap();
        assert_eq!(users, vec!["svc/api.py::handle".to_string()]);
    }

    #[test]
    fn node_ids_are_namespaced_by_repo() {
        let (out, _) = two_repo_federation("ns");
        assert!(out.get_node("lib/core.py::encrypt").unwrap().is_some());
        assert!(out.get_node("core.py::encrypt").unwrap().is_none());
    }

    /// The failure this design must not have: two repositories that never
    /// reference each other still share ordinary short names. Binding those
    /// would invent a dependency, which is the one thing the project refuses to
    /// do anywhere else.
    #[test]
    fn unrelated_repos_sharing_a_name_are_not_linked() {
        let a = repo("unrel_a", &[("log.py", "def info(m):\n    return m\n")]);
        let b = repo(
            "unrel_b",
            // Calls `info()` but never imports it — a local/builtin/attribute
            // call as far as this repo is concerned.
            &[("cli.py", "def run():\n    return info(1)\n")],
        );
        let mut out = Store::open_in_memory().unwrap();
        let stats = federate(
            &[
                RepoRef {
                    name: "a".into(),
                    db: a,
                },
                RepoRef {
                    name: "b".into(),
                    db: b,
                },
            ],
            &mut out,
        )
        .unwrap();
        assert_eq!(
            stats.cross_edges, 0,
            "no import evidence means no cross-repo dependency"
        );
    }

    /// A test symbol is nobody's public API, so it is never a cross-repo target
    /// even when the name is imported.
    #[test]
    fn tests_in_another_repo_are_never_cross_repo_targets() {
        let lib = repo(
            "tgt_lib",
            &[("test_helpers.py", "def helper(x):\n    return x\n")],
        );
        let svc = repo(
            "tgt_svc",
            &[(
                "api.py",
                "from test_helpers import helper\ndef go():\n    return helper(1)\n",
            )],
        );
        let mut out = Store::open_in_memory().unwrap();
        let stats = federate(
            &[
                RepoRef {
                    name: "lib".into(),
                    db: lib,
                },
                RepoRef {
                    name: "svc".into(),
                    db: svc,
                },
            ],
            &mut out,
        )
        .unwrap();
        assert_eq!(stats.cross_edges, 0);
    }

    /// Ambiguity across repos is still ambiguity. Both libraries expose
    /// `core.encrypt`, so even with import evidence the module does not single
    /// one out — and nothing may be asserted.
    #[test]
    fn two_repos_offering_the_same_name_stay_unresolved() {
        let a = repo("amb_a", &[("core.py", "def encrypt(x):\n    return x\n")]);
        let b = repo("amb_b", &[("core.py", "def encrypt(x):\n    return x\n")]);
        let svc = repo(
            "amb_svc",
            &[(
                "api.py",
                "from core import encrypt\ndef handle(r):\n    return encrypt(r)\n",
            )],
        );
        let mut out = Store::open_in_memory().unwrap();
        let stats = federate(
            &[
                RepoRef {
                    name: "a".into(),
                    db: a,
                },
                RepoRef {
                    name: "b".into(),
                    db: b,
                },
                RepoRef {
                    name: "svc".into(),
                    db: svc,
                },
            ],
            &mut out,
        )
        .unwrap();
        assert_eq!(stats.cross_edges, 0, "two candidates must not be asserted");
        assert!(out.impact("a/core.py::encrypt", 3).unwrap().is_empty());
    }

    #[test]
    fn federation_needs_more_than_one_repo() {
        let mut out = Store::open_in_memory().unwrap();
        assert!(federate(&[], &mut out).is_err());
    }

    #[test]
    fn repo_spec_accepts_label_and_bare_path() {
        let db = repo("spec", &[("a.py", "def f():\n    return 1\n")]);
        let r = RepoRef::parse(&format!("mylib={db}")).unwrap();
        assert_eq!(r.name, "mylib");
        assert!(RepoRef::parse("nope=/no/such/graph.db").is_err());
    }
}
