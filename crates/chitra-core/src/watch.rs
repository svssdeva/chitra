//! T4.3 — `watch` (one repo) and `daemon` (several), both a poll loop over
//! [`crate::update`].
//!
//! Polling rather than filesystem events: `update` is hash-gated, so a tick over
//! an unchanged repo is a directory walk plus a blake3 per file — measured
//! sub-second on the dogfood repos. At the 1s default that puts a saved file in
//! the graph in under 2s without an OS-event dependency to get wrong per
//! platform.
//!
//! ponytail: poll loop; move to the `notify` crate only if sub-second freshness
//! or very large repos make the walk the bottleneck.

use crate::store::Store;
use crate::{update, Stats};
use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default poll interval. Half the 2s freshness target, so a save is picked up
/// with time to spare for the update itself.
pub const DEFAULT_INTERVAL_MS: u64 = 1000;

/// One repository the daemon keeps fresh.
#[derive(Debug, Clone)]
pub struct WatchRoot {
    pub root: PathBuf,
    pub db: String,
}

impl WatchRoot {
    /// A root using its own in-repo database (`<root>/.chitra/graph.db`).
    pub fn new(root: impl Into<PathBuf>) -> WatchRoot {
        let root = root.into();
        let db = root.join(".chitra/graph.db").to_string_lossy().to_string();
        WatchRoot { root, db }
    }

    pub fn with_db(root: impl Into<PathBuf>, db: impl Into<String>) -> WatchRoot {
        WatchRoot {
            root: root.into(),
            db: db.into(),
        }
    }
}

/// Update one root. Kept separate from [`tick`] so the failure of one repo is a
/// value, not a panic that takes the daemon down.
fn update_root(r: &WatchRoot) -> Result<Stats> {
    if !r.root.is_dir() {
        return Err(anyhow!("root {} is not a directory", r.root.display()));
    }
    if let Some(parent) = Path::new(&r.db).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut store = Store::open(&r.db)?;
    update(&mut store, &r.root)
}

/// One pass over every root, in order. Each root's result is independent: a root
/// that fails this tick is simply retried on the next one, which is what
/// "restart dead watchers" means for a design with no long-lived per-root state.
pub fn tick(roots: &[WatchRoot]) -> Vec<(PathBuf, Result<Stats>)> {
    roots
        .iter()
        .map(|r| (r.root.clone(), update_root(r)))
        .collect()
}

/// Poll forever. Only changes and *new* failures are printed — a quiet repo
/// stays quiet, and a root that is broken for an hour says so once rather than
/// once per tick. Never returns; stopping is the caller's signal to handle.
pub fn watch_loop(roots: &[WatchRoot], interval: Duration) -> Result<()> {
    if roots.is_empty() {
        return Err(anyhow!("nothing to watch"));
    }
    for r in roots {
        println!("watching {} -> {}", r.root.display(), r.db);
    }
    let mut reported: std::collections::HashMap<PathBuf, String> = std::collections::HashMap::new();
    loop {
        for (root, result) in tick(roots) {
            match result {
                Ok(s) => {
                    if let Some(prev) = reported.remove(&root) {
                        println!("{}: recovered ({prev} resolved)", root.display());
                    }
                    if s.reparsed > 0 {
                        println!(
                            "{}: {} file(s) re-parsed -> {} nodes, {} edges",
                            root.display(),
                            s.reparsed,
                            s.nodes,
                            s.edges
                        );
                    }
                }
                // Logged, never fatal: the next tick retries this root.
                Err(e) => {
                    let msg = e.to_string();
                    if reported.get(&root) != Some(&msg) {
                        eprintln!("warn: {}: {msg}", root.display());
                        reported.insert(root, msg);
                    }
                }
            }
        }
        std::thread::sleep(interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A broken root must not stop the healthy one, this tick or the next.
    #[test]
    fn tick_isolates_a_failing_root_and_retries_it() {
        let good = fresh("chitra_watch_good");
        std::fs::write(good.join("a.rs"), "fn a() {}\n").unwrap();
        // A file, not a directory: stands in for a root that was replaced or removed.
        let bad = fresh("chitra_watch_bad").join("not_a_dir");
        std::fs::write(&bad, "").unwrap();

        let roots = [WatchRoot::new(&good), WatchRoot::new(&bad)];

        let first = tick(&roots);
        assert_eq!(first[0].1.as_ref().unwrap().reparsed, 1);
        assert!(first[1].1.is_err(), "bad root should report a failure");

        // Second pass: good root still served, bad root retried (still failing).
        std::fs::write(good.join("b.rs"), "fn b() { a(); }\n").unwrap();
        let second = tick(&roots);
        assert_eq!(second[0].1.as_ref().unwrap().reparsed, 1);
        assert!(second[1].1.is_err());
    }

    /// A tick over an unchanged repo re-parses nothing — what keeps polling cheap.
    #[test]
    fn tick_on_unchanged_repo_reparses_nothing() {
        let dir = fresh("chitra_watch_noop");
        std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
        let roots = [WatchRoot::new(&dir)];

        assert_eq!(tick(&roots)[0].1.as_ref().unwrap().reparsed, 1);
        assert_eq!(tick(&roots)[0].1.as_ref().unwrap().reparsed, 0);
    }

    #[test]
    fn root_gets_its_own_in_repo_database() {
        let r = WatchRoot::new("/tmp/repo");
        assert!(r.db.replace('\\', "/").ends_with("repo/.chitra/graph.db"));
    }
}
