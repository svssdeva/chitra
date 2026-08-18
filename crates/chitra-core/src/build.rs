//! Build pipeline: collect -> parse(∥) -> store -> resolve -> postprocess.
//!
//! `build` is a full (re)build; `update` is hash-gated incremental. Both share
//! `sync`: only files whose blake3 hash changed are re-parsed, then edges are
//! re-derived globally (resolution is cheap over an in-memory index; parsing is
//! the expensive part we skip). Resolution is evidence-gated with two guards.

use crate::store::{NewEdge, Store};
use anyhow::Result;
#[cfg(feature = "deep-resolve")]
use chitra_lang::Import;
use chitra_lang::{parse, Node, ParsedFile, Registry};
use ignore::WalkBuilder;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub struct Stats {
    pub files: usize,    // supported files seen this run
    pub reparsed: usize, // files actually re-parsed (changed/new)
    pub nodes: usize,
    pub edges: usize,
}

/// Directories never worth walking (vendored/build output). Cheap perf win on
/// real repos (node_modules alone can dwarf source).
const SKIP_DIRS: &[&str] = &[
    "target",
    ".git",
    ".chitra",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    "dist",
    "build",
    ".mypy_cache",
    ".pytest_cache",
];

/// Full build: reset the store, then sync every file.
pub fn build(store: &mut Store, root: &Path) -> Result<Stats> {
    store.clear_all()?;
    sync(store, root, true)
}

/// Incremental update: re-parse only files whose hash changed; evict deleted.
pub fn update(store: &mut Store, root: &Path) -> Result<Stats> {
    sync(store, root, false)
}

fn sync(store: &mut Store, root: &Path, full: bool) -> Result<Stats> {
    let registry = Registry::load(root); // built-ins + any languages.toml (T4.4)
    let files = collect(root)?;
    // An empty walk is reported, never silent. The most likely cause is now
    // the nested-repo rule: pointed at a *workspace* holding several checkouts,
    // every subdirectory owns its own `.git` and the whole tree is skipped,
    // which would otherwise print `0 nodes, 0 edges` as if the code were gone.
    if files.is_empty() {
        eprintln!(
            "warn: no indexable files under {} — every subdirectory may be a \
             separate git repository (chitra indexes one repo at a time), or \
             .gitignore/.chitraignore may exclude everything",
            root.display()
        );
    }

    // Parse in parallel: read -> blake3 -> tree-sitter. Non-fatal per file.
    let parsed: Vec<(String, String, String, ParsedFile)> = files
        .par_iter()
        .filter_map(|(abs, rel, ext)| read_hash_parse(&registry, abs, rel, ext))
        .collect();

    let stored = if full {
        HashMap::new()
    } else {
        store.file_hashes()?
    };
    let current: HashSet<&str> = parsed.iter().map(|(r, ..)| r.as_str()).collect();

    // Re-store changed/new files (all of them on a full build).
    let mut reparsed = 0usize;
    for (rel, lang, hash, pf) in &parsed {
        if !full && stored.get(rel).map(|h| h == hash).unwrap_or(false) {
            continue; // unchanged -> skip the whole file
        }
        store.replace_file(rel, hash, lang, pf)?;
        reparsed += 1;
    }
    // Evict files that vanished since last build.
    let mut evicted = 0usize;
    if !full {
        for old in stored.keys() {
            if !current.contains(old.as_str()) {
                store.evict_file(old)?;
                evicted += 1;
            }
        }
    }

    // Nothing changed on an incremental run -> skip the global re-resolve (this
    // is what keeps a no-op `update` sub-second on a 3k-file repo).
    if !full && reparsed == 0 && evicted == 0 {
        return Ok(Stats {
            files: parsed.len(),
            reparsed,
            nodes: store.node_count()? as usize,
            edges: store.edge_count()? as usize,
        });
    }

    // Derive edges from the full corpus (nodes + raw_calls + imports).
    let edges = resolve(store, &registry.resolve_map())?;

    // Postprocess (each non-fatal): FTS index, then structure (communities +
    // flows). A broken step warns; it never fails a build.
    if let Err(e) = store.rebuild_fts() {
        eprintln!("warn: FTS index skipped: {e}");
    }
    if let Err(e) = crate::structure::postprocess(store) {
        eprintln!("warn: structure postprocess skipped: {e}");
    }

    store.set_meta("last_build_type", if full { "full" } else { "incremental" })?;
    match git_head(root) {
        Some(sha) => store.set_meta("built_at_commit", &sha)?,
        None => store.set_meta("built_at_commit", "")?, // null when outside git
    }

    Ok(Stats {
        files: parsed.len(),
        reparsed,
        nodes: store.node_count()? as usize,
        edges,
    })
}

fn read_hash_parse(
    registry: &Registry,
    abs: &Path,
    rel: &str,
    ext: &str,
) -> Option<(String, String, String, ParsedFile)> {
    let cfg = registry.config_for_extension(ext)?;
    let bytes = std::fs::read(abs).ok()?;
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let src = std::str::from_utf8(&bytes).ok()?; // skip binaries masquerading as source
    match parse(&cfg, rel, src) {
        Ok(pf) => {
            // Surface a recovered-but-lossy parse. Without this a file with a
            // syntax error just contributes fewer symbols and nobody finds out.
            if let Some(w) = &pf.parse_warning {
                eprintln!("warn: {rel}: {w}");
            }
            Some((rel.to_string(), cfg.language.to_string(), hash, pf))
        }
        Err(e) => {
            eprintln!("warn: parse failed for {rel}: {e}");
            None
        }
    }
}

// ---- resolution -----------------------------------------------------------

/// Evidence ladder (data model §Confidence):
/// 1. same-file, single candidate -> EXTRACTED 1.0
/// 2. unique same-language global candidate -> INFERRED 0.6 (promoted to
///    EXTRACTED 0.9 if the callee name is imported here)
/// 3. multiple candidates -> AMBIGUOUS 0.3 (surfaced, excluded from impact)
/// 4. no candidate -> dropped (external / stdlib)
///
/// Guards: single-candidate (never assert on >1), cross-language-family (bind
/// only within the languages a config declares — its own, unless it opts into
/// more, as HTML does for CSS). Returns the resolved edge count.
fn resolve(store: &mut Store, resolve_map: &HashMap<String, Vec<String>>) -> Result<usize> {
    let nodes = store.load_nodes()?;
    let raw = store.load_raw_calls()?;
    let imports = store.load_imports()?;

    let by_qn: HashMap<&str, &Node> = nodes
        .iter()
        .map(|n| (n.qualified_name.as_str(), n))
        .collect();
    let is_test: HashMap<&str, bool> = nodes
        .iter()
        .map(|n| (n.qualified_name.as_str(), n.is_test))
        .collect();
    let mut same_file: HashMap<(&str, &str), Vec<&Node>> = HashMap::new();
    let mut global: HashMap<&str, Vec<&Node>> = HashMap::new();
    // Type name -> every file holding that type's code. In Rust a type is
    // declared in one file and its methods often live in another (`impl Foo`),
    // so this is a set rather than a single file.
    let mut type_files: HashMap<&str, HashSet<&str>> = HashMap::new();
    for n in &nodes {
        if n.kind == "Type" {
            // A type is evidence about where methods live, not something a call
            // site can target, so it is kept out of the candidate maps entirely.
            type_files
                .entry(n.name.as_str())
                .or_default()
                .insert(n.file.as_str());
            continue;
        }
        same_file
            .entry((n.file.as_str(), n.name.as_str()))
            .or_default()
            .push(n);
        global.entry(n.name.as_str()).or_default().push(n);
    }

    // Build all CALLS edges, then derive TESTED_BY from the asserted ones.
    let mut edges: Vec<NewEdge> = Vec::new();
    let mut push = |source: &str, target: &str, line: i64, conf: f64, tier: &'static str| {
        edges.push(NewEdge {
            source: source.to_string(),
            target: target.to_string(),
            kind: "CALLS",
            line,
            confidence: conf,
            tier,
        });
    };

    for rc in &raw {
        let (file, caller, callee, line) = (&rc.file, &rc.caller, &rc.callee, rc.line);
        // A `<file>` caller only resolves when the language emits a file node
        // (HTML markup lives outside any definition); otherwise it is an
        // unattributed top-level call and stays dropped.
        let Some(caller_node) = by_qn.get(caller.as_str()) else {
            continue;
        };
        let lang = caller_node.language.as_str();
        let allowed: &[String] = resolve_map
            .get(lang)
            .map(|v| v.as_slice())
            .unwrap_or(std::slice::from_ref(&caller_node.language));

        // 0. A call through a type that is not in this repository.
        //
        // `Box::new()`, `Math.floor()`, `JSON.parse()` — the qualifier names a
        // type, and it is one we never parsed, so the definition lives outside
        // the repo and there is nothing here to point at. Matching on the bare
        // name anyway is actively wrong: it either invents an INFERRED edge to
        // an unrelated local `new`, or fans out across every one of them. On a
        // real monorepo `Box::new` alone accounted for 593 call sites, all of
        // them noise.
        if names_a_foreign_type(rc, &type_files) {
            continue;
        }

        // 1. same-file
        if let Some(sf) = same_file.get(&(file.as_str(), callee.as_str())) {
            if sf.len() == 1 {
                push(caller, &sf[0].qualified_name, line, 1.0, "EXTRACTED");
                continue;
            }
            if sf.len() > 1 {
                for c in sf {
                    push(caller, &c.qualified_name, line, 0.3, "AMBIGUOUS");
                }
                continue;
            }
        }

        // 2/3. cross-file: candidates in the languages this one may bind to
        let cands: Vec<&&Node> = match global.get(callee.as_str()) {
            Some(v) => v.iter().filter(|n| allowed.contains(&n.language)).collect(),
            None => continue, // external
        };
        match cands.len() {
            0 => continue,
            1 => {
                let imported = imports
                    .get(file)
                    .map(|v| v.iter().any(|i| i.name == *callee))
                    .unwrap_or(false);
                let (conf, tier) = if imported {
                    (0.9, "EXTRACTED")
                } else {
                    (0.6, "INFERRED")
                };
                push(caller, &cands[0].qualified_name, line, conf, tier);
            }
            _ => {
                // The qualifier names a type we have in the graph: `Foo::new()`,
                // `Foo.parse()`. Keep only candidates defined where that type's
                // code lives. This is the constructor case, and on an
                // object-oriented codebase it is most of the ambiguity.
                if let Some(pick) = type_pick(&cands, rc, &type_files) {
                    push(caller, &pick.qualified_name, line, 0.85, "EXTRACTED");
                    continue;
                }
                // Deep resolution (T4.1) gets one attempt to break the tie before
                // the call is written off as ambiguous.
                #[cfg(feature = "deep-resolve")]
                if let Some(pick) = deep_pick(&cands, rc, &imports) {
                    push(caller, &pick.qualified_name, line, 0.85, "EXTRACTED");
                    continue;
                }
                for c in &cands {
                    push(caller, &c.qualified_name, line, 0.3, "AMBIGUOUS");
                }
            }
        }
    }

    // TESTED_BY: a test symbol that calls a production symbol tests it.
    // Stored source=production, target=test (data model convention).
    let mut tested_by: Vec<NewEdge> = Vec::new();
    for e in &edges {
        if e.tier == "AMBIGUOUS" {
            continue;
        }
        let caller_test = is_test.get(e.source.as_str()).copied().unwrap_or(false);
        let target_test = is_test.get(e.target.as_str()).copied().unwrap_or(false);
        if caller_test && !target_test {
            tested_by.push(NewEdge {
                source: e.target.clone(),
                target: e.source.clone(),
                kind: "TESTED_BY",
                line: e.line,
                confidence: 0.7,
                tier: "INFERRED",
            });
        }
    }
    edges.extend(tested_by);

    store.replace_edges(&edges)?;
    Ok(store.edge_count()? as usize)
}

// ---- deep resolution (T4.1, feature `deep-resolve`) -----------------------

/// A same-named symbol in several files is normally AMBIGUOUS and dropped from
/// impact. Two kinds of evidence can break the tie, strongest first:
///
/// 1. **The call-site qualifier** — `math::add()` in Rust, `store.Fetch()` in
///    Go, `mod.parse()` in Python/TS. This is per-call and language-specific,
///    which is what makes it work for Rust and Go, where imports bind modules
///    and packages rather than individual symbols.
/// 2. **The file's import module** — `from lib import parse`,
///    `import { parse } from './lib'`, `use crate::math::add`.
///
/// The single-candidate guard is preserved throughout: a target is returned only
/// when exactly one candidate matches, so recall rises without spending
/// precision.
/// Does this call go through a type that lives outside the repository?
///
/// Rust, Go and TypeScript all spell types in UpperCamelCase and modules in
/// lower case, so a capitalised qualifier is a type with high reliability. If
/// that type is not one chitra parsed, its methods are not here either.
///
/// This only ever *removes* candidates, so it cannot invent an edge. The
/// failure mode is dropping a call to a first-party type whose declaration
/// chitra missed — and a missing edge is the error this codebase prefers.
fn names_a_foreign_type(
    rc: &crate::store::RawCallRow,
    type_files: &HashMap<&str, HashSet<&str>>,
) -> bool {
    let Some(q) = rc.qualifier.as_deref() else {
        return false;
    };
    // `Self` is capitalised but names the impl currently being written, which is
    // in this very file. Treating it as foreign would silently drop every
    // `Self::helper()` call in a Rust codebase; the same-file rule below already
    // resolves it correctly.
    if q == "Self" {
        return false;
    }
    let Some(first) = q.chars().next() else {
        return false;
    };
    first.is_uppercase() && !type_files.contains_key(q)
}

/// Break a tie using the call-site qualifier when it names a **type we parsed**.
///
/// `Foo::new()` carries the qualifier `Foo`. Until types were nodes there was
/// nothing to match it against, so every constructor call in the repository
/// collapsed into one enormous ambiguous bucket — `new` alone accounted for
/// 90,600 ambiguous edges on a real monorepo, and no amount of module-name
/// heuristics touched it, because `Foo` is a type name and not a path.
///
/// The single-candidate guard still holds: a target comes back only when
/// exactly one candidate sits in a file where that type's code lives. This is
/// unconditional rather than feature-gated, because unlike a file-stem
/// heuristic it matches a declaration chitra actually saw.
fn type_pick<'a>(
    cands: &[&&'a Node],
    rc: &crate::store::RawCallRow,
    type_files: &HashMap<&str, HashSet<&str>>,
) -> Option<&'a Node> {
    let files = type_files.get(rc.qualifier.as_deref()?)?;
    let mut matched = cands
        .iter()
        .map(|c| **c)
        .filter(|c| files.contains(c.file.as_str()));
    match (matched.next(), matched.next()) {
        (Some(only), None) => Some(only),
        _ => None, // no match, or several — stay ambiguous
    }
}

#[cfg(feature = "deep-resolve")]
fn deep_pick<'a>(
    cands: &[&&'a Node],
    rc: &crate::store::RawCallRow,
    imports: &HashMap<String, Vec<Import>>,
) -> Option<&'a Node> {
    let unique_match = |evidence: &[&str]| -> Option<&'a Node> {
        let mut matched = cands
            .iter()
            .map(|c| **c)
            .filter(|c| evidence.iter().any(|m| module_matches(m, &c.file)));
        match (matched.next(), matched.next()) {
            (Some(only), None) => Some(only),
            _ => None, // no evidence, or evidence for more than one — stay honest
        }
    };

    if let Some(q) = rc.qualifier.as_deref() {
        if let Some(pick) = unique_match(&[q]) {
            return Some(pick);
        }
    }
    let modules: Vec<&str> = imports
        .get(&rc.file)?
        .iter()
        .filter(|i| i.name == rc.callee)
        .filter_map(|i| i.module.as_deref())
        .collect();
    if modules.is_empty() {
        return None;
    }
    unique_match(&modules)
}

/// Does a module path or call qualifier designate this file? Compares its last
/// segment (`pkg.lib`, `./lib`, `crate::math`, `../pkg/lib` → `lib` / `math`)
/// against the file stem, and against the parent directory — Go packages and
/// Rust `mod.rs` modules are named by their directory, not their file.
///
/// Deliberately shallow: no package-root resolution, no `__init__` re-export
/// chasing. It only ever *narrows* a candidate set, and only a unique narrowing
/// is acted on.
///
/// ponytail: stem/dir match; a real module-path resolver is the upgrade if a
/// repo's re-export layers make this miss.
pub(crate) fn module_matches(module: &str, path: &str) -> bool {
    let Some(last) = module
        .rsplit(['.', '/', ':'])
        .find(|s| !s.is_empty() && *s != ".")
    else {
        return false;
    };
    let p = Path::new(path);
    let stem = p.file_stem().and_then(|s| s.to_str());
    if stem == Some(last) {
        return true;
    }
    // `store/fetch.go` in package `store`; `math/mod.rs` in module `math`.
    p.parent()
        .and_then(|d| d.file_name())
        .and_then(|d| d.to_str())
        == Some(last)
}

// ---- collect --------------------------------------------------------------

/// Walk the repository, honouring `.gitignore` and `.chitraignore`.
///
/// What a graph indexes is a correctness question, not a convenience one.
/// Walking the filesystem blindly pulls in build output and vendored or
/// worktree copies of the same source; every duplicated symbol then makes its
/// call sites ambiguous, and ambiguous edges are excluded from impact. Measured
/// on a real monorepo: 2,042 files walked against 954 that git actually tracks,
/// and half of all ambiguous edges traced to `.worktrees/` copies that
/// `.gitignore` had already excluded.
///
/// The rules come from the `ignore` crate (ripgrep's), so `.chitraignore` gets
/// real gitignore semantics — globs, anchoring, and `!` negation to whitelist
/// something back in — instead of the prefix match it had before.
fn collect(root: &Path) -> Result<Vec<(PathBuf, String, String)>> {
    let walker = WalkBuilder::new(root)
        .git_ignore(true)
        .git_exclude(true)
        // Deliberately ignore the user's *global* gitignore: graph contents must
        // not depend on a machine-local config file (RISK-4, determinism).
        .git_global(false)
        // Honour .gitignore even outside a git repo — tarball checkouts and
        // exported trees are still ordinary repositories to a reader.
        .require_git(false)
        .follow_links(false) // loops, escapes
        .add_custom_ignore_filename(".chitraignore")
        .filter_entry(|e| {
            if !e.file_type().is_some_and(|t| t.is_dir()) {
                return true;
            }
            if SKIP_DIRS.contains(&e.file_name().to_string_lossy().as_ref()) {
                return false;
            }
            // A nested `.git` means a different repository owns these files:
            // a submodule (where `.git` is a file) or a plain nested clone
            // (where it is a directory). `git ls-files` reports a submodule as
            // a single gitlink, never its contents, and indexing them anyway
            // duplicates third-party symbols into our graph. Depth 0 is the
            // scan root, whose own `.git` is precisely the repo we want.
            e.depth() == 0 || !e.path().join(".git").exists()
        })
        .build();

    let mut out = Vec::new();
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            // An unreadable directory is not a reason to fail a whole build.
            Err(e) => {
                eprintln!("warn: walk: {e}");
                continue;
            }
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let p = entry.into_path();
        let rel = relative_id(root, &p);
        // Extensions are compared lower-case: `.PY` and `.CSS` are ordinary on
        // case-insensitive filesystems (Windows, macOS) and legal on all.
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        out.push((p, rel, ext));
    }
    // The walk order is filesystem order; sorting keeps the file list identical
    // across platforms and runs.
    out.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(out)
}

/// A node's file identity: the path relative to the root, always `/`-separated.
///
/// Built from path *components* rather than by rewriting separators in the
/// string. On Unix a backslash is a legal character in a filename, so
/// `replace('\\', "/")` would silently invent a directory boundary inside a
/// legitimate name — and identities are the one thing that must be stable and
/// identical on every platform (RISK-4).
fn relative_id(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn git_head(root: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C"])
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, src: &str) {
        std::fs::write(dir.join(name), src).unwrap();
    }

    fn fresh(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The point of indexing doc comments: prose describes a symbol in words its
    /// name does not contain. Neither "retry" nor "backoff" appears in
    /// `attempt_once`, so before this the search could not find it at all.
    #[test]
    fn a_symbol_is_findable_by_its_doc_comment() {
        let dir = fresh("chitra_doc_search");
        write(
            &dir,
            "net.rs",
            "/// Retry the request with exponential backoff.
fn attempt_once() -> i32 { 1 }
",
        );
        write(
            &dir,
            "other.rs",
            "fn unrelated() -> i32 { 2 }
",
        );
        let mut store = Store::open_in_memory().unwrap();
        build(&mut store, &dir).unwrap();
        assert_eq!(
            store.search("backoff", 10).unwrap(),
            vec!["net.rs::attempt_once".to_string()]
        );
        assert_eq!(
            store.search("exponential", 10).unwrap(),
            vec!["net.rs::attempt_once".to_string()]
        );
    }

    /// Python puts its prose inside the definition, not above it.
    #[test]
    fn a_python_docstring_is_indexed() {
        let dir = fresh("chitra_doc_py");
        write(
            &dir,
            "auth.py",
            "def check(u):
    \"\"\"Verify the caller is who they claim to be.\"\"\"
    return True
",
        );
        let mut store = Store::open_in_memory().unwrap();
        build(&mut store, &dir).unwrap();
        assert_eq!(
            store.search("caller claim", 10).unwrap(),
            vec!["auth.py::check".to_string()]
        );
    }

    /// The constructor case. Two types each expose `new`, so the bare name is
    /// ambiguous and stays that way under every name-based heuristic. The
    /// qualifier `Config` / `Client` is a *type*, and only a graph that holds
    /// types can use it.
    #[test]
    fn a_qualified_constructor_resolves_to_its_own_type() {
        let dir = fresh("chitra_type_ctor");
        write(
            &dir,
            "config.rs",
            "pub struct Config { pub a: i32 }
impl Config { pub fn new() -> Config { Config { a: 1 } } }
",
        );
        write(
            &dir,
            "client.rs",
            "pub struct Client { pub b: i32 }
impl Client { pub fn new() -> Client { Client { b: 2 } } }
",
        );
        write(
            &dir,
            "app.rs",
            "fn boot() { let c = Config::new(); let d = Client::new(); }
",
        );
        let mut store = Store::open_in_memory().unwrap();
        build(&mut store, &dir).unwrap();

        let out = store.callees_of("app.rs::boot").unwrap();
        assert!(
            out.contains(&"config.rs::new".to_string()),
            "Config::new() must bind to config.rs, got {out:?}"
        );
        assert!(
            out.contains(&"client.rs::new".to_string()),
            "Client::new() must bind to client.rs, got {out:?}"
        );
        // And the reverse direction is what impact analysis actually reads.
        let dependents = store.impact("config.rs::new", 3).unwrap();
        assert_eq!(dependents, vec!["app.rs::boot".to_string()]);
    }

    /// The dominant source of ambiguity on a real repo was not first-party
    /// constructors but `Box::new`, `Vec::new`, `Arc::new` — calls through
    /// types defined outside the repository. Matching those on the bare name
    /// invents an edge to an unrelated local function.
    #[test]
    fn a_call_through_a_foreign_type_resolves_to_nothing() {
        let dir = fresh("chitra_foreign_type");
        // Exactly one local `new`, so without the guard this call would be
        // asserted with full confidence to entirely the wrong function.
        write(
            &dir,
            "thing.rs",
            "pub struct Thing;
impl Thing { pub fn new() -> Thing { Thing } }
",
        );
        write(
            &dir,
            "app.rs",
            "fn run() { let b = Box::new(1); }
",
        );
        let mut store = Store::open_in_memory().unwrap();
        build(&mut store, &dir).unwrap();
        assert!(
            store.callees_of("app.rs::run").unwrap().is_empty(),
            "Box is not a type in this repo, so Box::new must bind to nothing"
        );
        // A lower-case qualifier is a module path, not a type, and is left alone.
        assert!(store.get_node("thing.rs::new").unwrap().is_some());
    }

    /// `Self::` is capitalised, so a naive foreign-type check kills it — and it
    /// is one of the most common ways a Rust method calls its neighbour.
    #[test]
    fn a_self_qualified_call_still_resolves() {
        let dir = fresh("chitra_self_qual");
        write(
            &dir,
            "a.rs",
            "pub struct Config { pub a: i32 }
impl Config {
    pub fn helper() -> i32 { 3 }
    pub fn go() -> i32 { Self::helper() }
}
",
        );
        let mut store = Store::open_in_memory().unwrap();
        build(&mut store, &dir).unwrap();
        assert_eq!(
            store.callees_of("a.rs::go").unwrap(),
            vec!["a.rs::helper".to_string()],
            "Self:: names the impl in this file, not a foreign type"
        );
    }

    /// The guard has to hold in the other direction too: a qualifier naming a
    /// type whose file holds *several* matching candidates is still ambiguous.
    #[test]
    fn an_unresolvable_constructor_stays_ambiguous() {
        let dir = fresh("chitra_type_ambig");
        write(
            &dir,
            "a.rs",
            "pub struct Thing { x: i32 }
fn new() -> i32 { 1 }
",
        );
        write(
            &dir,
            "b.rs",
            "fn new() -> i32 { 2 }
",
        );
        write(
            &dir,
            "c.rs",
            "fn go() { Missing::new(); }
",
        );
        let mut store = Store::open_in_memory().unwrap();
        build(&mut store, &dir).unwrap();
        // `Missing` is not a type chitra saw, so nothing is asserted.
        assert!(
            store.callees_of("c.rs::go").unwrap().is_empty(),
            "an unknown qualifier must not resolve"
        );
    }

    /// End-to-end full build: nodes, cross-file edges, impact, TESTED_BY.
    #[test]
    fn e2e_build_impact_tested_by() {
        let dir = fresh("chitra_e2e_build");
        write(&dir, "util.rs", "fn helper() -> i32 { 42 }\n");
        write(
            &dir,
            "app.rs",
            "fn compute() -> i32 { helper() }\nfn main() { let _ = compute(); }\n",
        );
        // a test file that calls compute -> TESTED_BY(compute, the test)
        write(&dir, "app_test.rs", "fn check() { let _ = compute(); }\n");

        let mut store = Store::open_in_memory().unwrap();
        let s = build(&mut store, &dir).unwrap();
        assert_eq!(s.reparsed, 3);

        // cross-file: compute (app.rs) -> helper (util.rs), unique global -> INFERRED
        let deps = store.impact("util.rs::helper", 5).unwrap();
        assert!(deps.contains(&"app.rs::compute".to_string()));
        assert!(deps.contains(&"app.rs::main".to_string()));

        // app_test.rs is a test file -> its check() is a Test node -> TESTED_BY
        let tests = store.tests_for("app.rs::compute").unwrap();
        assert!(
            tests.contains(&"app_test.rs::check".to_string()),
            "{tests:?}"
        );
    }

    /// Incremental: an untouched file is not re-parsed; a changed one is.
    #[test]
    fn incremental_skips_unchanged() {
        let dir = fresh("chitra_e2e_incr");
        write(&dir, "a.rs", "fn a() {}\n");
        write(&dir, "b.rs", "fn b() { a(); }\n");

        let db = dir.join("g.db");
        let db = db.to_string_lossy().to_string();
        {
            let mut store = Store::open(&db).unwrap();
            let s = build(&mut store, &dir).unwrap();
            assert_eq!(s.reparsed, 2);
        }
        {
            let mut store = Store::open(&db).unwrap();
            let s = update(&mut store, &dir).unwrap();
            assert_eq!(s.reparsed, 0); // nothing changed
        }
        // change b.rs only
        write(&dir, "b.rs", "fn b() { a(); a(); }\n");
        {
            let mut store = Store::open(&db).unwrap();
            let s = update(&mut store, &dir).unwrap();
            assert_eq!(s.reparsed, 1); // just b.rs
        }
    }

    // ---- what gets indexed at all ----

    /// Indexing build output and vendored copies is not a cosmetic problem: on a
    /// real monorepo it doubled the input and made half of all call sites
    /// ambiguous, because every symbol existed twice.
    #[test]
    fn gitignored_files_are_not_indexed() {
        let dir = fresh("chitra_ignore_git");
        std::fs::write(dir.join(".gitignore"), "dist/\n*.min.js\n").unwrap();
        write(&dir, "app.js", "function real() { return 1; }\n");
        std::fs::create_dir_all(dir.join("dist")).unwrap();
        write(&dir, "dist/app.js", "function built() { return 1; }\n");
        write(&dir, "vendor.min.js", "function vendored() { return 1; }\n");

        let mut store = Store::open_in_memory().unwrap();
        build(&mut store, &dir).unwrap();

        assert!(store.get_node("app.js::real").unwrap().is_some());
        assert!(
            store.get_node("dist/app.js::built").unwrap().is_none(),
            "a gitignored directory must not be indexed"
        );
        assert!(
            store.get_node("vendor.min.js::vendored").unwrap().is_none(),
            "a gitignored glob must not be indexed"
        );
    }

    /// `.chitraignore` gets full gitignore syntax, including globs.
    #[test]
    fn chitraignore_supports_globs() {
        let dir = fresh("chitra_ignore_glob");
        std::fs::write(dir.join(".chitraignore"), "*.generated.ts\ngenerated/\n").unwrap();
        write(&dir, "hand.ts", "function kept() { return 1; }\n");
        write(
            &dir,
            "api.generated.ts",
            "function dropped() { return 1; }\n",
        );

        let mut store = Store::open_in_memory().unwrap();
        build(&mut store, &dir).unwrap();
        assert!(store.get_node("hand.ts::kept").unwrap().is_some());
        assert!(store
            .get_node("api.generated.ts::dropped")
            .unwrap()
            .is_none());
    }

    /// The whitelist half: `!` re-includes something a broader rule excluded.
    #[test]
    fn chitraignore_negation_re_includes_a_file() {
        let dir = fresh("chitra_ignore_negate");
        std::fs::write(
            dir.join(".chitraignore"),
            "*.generated.ts\n!keep.generated.ts\n",
        )
        .unwrap();
        write(&dir, "drop.generated.ts", "function gone() { return 1; }\n");
        write(
            &dir,
            "keep.generated.ts",
            "function stays() { return 1; }\n",
        );

        let mut store = Store::open_in_memory().unwrap();
        build(&mut store, &dir).unwrap();
        assert!(store.get_node("drop.generated.ts::gone").unwrap().is_none());
        assert!(
            store
                .get_node("keep.generated.ts::stays")
                .unwrap()
                .is_some(),
            "a `!` rule should whitelist the file back in"
        );
    }

    /// node_modules is skipped even in a repo that never gitignored it.
    #[test]
    fn vendored_directories_are_skipped_without_any_ignore_file() {
        let dir = fresh("chitra_ignore_vendor");
        write(&dir, "app.js", "function real() { return 1; }\n");
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        write(
            &dir,
            "node_modules/pkg/i.js",
            "function dep() { return 1; }\n",
        );

        let mut store = Store::open_in_memory().unwrap();
        build(&mut store, &dir).unwrap();
        assert!(store.get_node("app.js::real").unwrap().is_some());
        assert!(store
            .get_node("node_modules/pkg/i.js::dep")
            .unwrap()
            .is_none());
    }

    /// A git submodule is a *separate repository*: `git ls-files` reports it as
    /// one gitlink entry (mode 160000), not as its contents. Walking into it
    /// indexes third-party source as if it were ours — measured on a real Astro
    /// monorepo, 59 vendored files, and the largest "community" in the
    /// architecture overview was named after one of them.
    ///
    /// The marker is a nested `.git`, which is a *file* in a submodule checkout
    /// and a *directory* in a plain nested clone. Both mean the same thing:
    /// those files belong to another repository.
    #[test]
    fn nested_git_repositories_are_not_indexed() {
        let dir = fresh("chitra_ignore_submodule");
        write(&dir, "app.js", "function real() { return 1; }\n");

        // A submodule checkout: `.git` is a file pointing at the parent's
        // .git/modules/<name>.
        std::fs::create_dir_all(dir.join("vendored")).unwrap();
        std::fs::write(
            dir.join("vendored/.git"),
            "gitdir: ../.git/modules/vendored\n",
        )
        .unwrap();
        write(&dir, "vendored/lib.js", "function theirs() { return 1; }\n");

        // A plain nested clone: `.git` is a directory.
        std::fs::create_dir_all(dir.join("nested/.git")).unwrap();
        write(
            &dir,
            "nested/lib.js",
            "function alsoTheirs() { return 1; }\n",
        );

        let mut store = Store::open_in_memory().unwrap();
        build(&mut store, &dir).unwrap();

        assert!(store.get_node("app.js::real").unwrap().is_some());
        assert!(
            store.get_node("vendored/lib.js::theirs").unwrap().is_none(),
            "a git submodule must not be indexed"
        );
        assert!(
            store
                .get_node("nested/lib.js::alsoTheirs")
                .unwrap()
                .is_none(),
            "a nested git clone must not be indexed"
        );
    }

    /// The root of the scan always has a `.git` of its own — skipping any
    /// directory with one would index nothing at all.
    #[test]
    fn the_repository_root_is_still_indexed_when_it_has_a_git_dir() {
        let dir = fresh("chitra_ignore_root_git");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        write(&dir, "app.js", "function real() { return 1; }\n");

        let mut store = Store::open_in_memory().unwrap();
        build(&mut store, &dir).unwrap();
        assert!(
            store.get_node("app.js::real").unwrap().is_some(),
            "the scanned repo's own .git must not exclude the repo"
        );
    }

    /// Node identity must be `/`-separated on every platform — this is what the
    /// cross-OS byte-identical export gate (RISK-4) rests on.
    #[test]
    fn relative_id_is_always_posix_separated() {
        let root = Path::new("repo");
        let nested = root.join("src").join("deep").join("mod.rs");
        assert_eq!(relative_id(root, &nested), "src/deep/mod.rs");
    }

    /// On Unix a backslash is a legal filename character. Rewriting separators
    /// in the string would split this into a fake directory; building from
    /// components does not.
    #[test]
    #[cfg(unix)]
    fn relative_id_does_not_split_a_unix_filename_containing_a_backslash() {
        let root = Path::new("repo");
        let odd = root.join(r"we\ird.rs");
        assert_eq!(relative_id(root, &odd), r"we\ird.rs");
    }

    /// `.PY` is an ordinary filename on a case-insensitive filesystem and legal
    /// everywhere; matching extensions case-sensitively silently skipped them.
    #[test]
    fn uppercase_extensions_are_recognised() {
        let dir = fresh("chitra_ext_case");
        write(&dir, "Shouty.PY", "def loud():\n    return 1\n");
        let mut store = Store::open_in_memory().unwrap();
        let s = build(&mut store, &dir).unwrap();
        assert_eq!(s.reparsed, 1, "an uppercase extension should still parse");
        assert!(store.get_node("Shouty.PY::loud").unwrap().is_some());
    }

    /// Ambiguity: same name in two files, no import evidence -> AMBIGUOUS,
    /// so it never appears in impact.
    #[test]
    fn ambiguous_not_asserted() {
        let dir = fresh("chitra_e2e_ambig");
        write(&dir, "x.rs", "fn dup() {}\n");
        write(&dir, "y.rs", "fn dup() {}\n");
        write(&dir, "z.rs", "fn caller() { dup(); }\n");

        let mut store = Store::open_in_memory().unwrap();
        build(&mut store, &dir).unwrap();
        // dup() call is ambiguous across x/y -> caller reaches neither via impact
        assert!(store.impact("x.rs::dup", 5).unwrap().is_empty());
        assert!(store.impact("y.rs::dup", 5).unwrap().is_empty());
    }
}
