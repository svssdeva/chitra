//! Phase 1 gates as tests (T1.12 precision, T1.10/T1.11 export determinism).
//!
//! A small multi-language fixture with a *hand-labelled* set of correct CALLS
//! edges. The resolver is gated at >=0.85 precision; recall is printed, not
//! gated (RISK-2: under-connect rather than mis-connect). Determinism is
//! asserted by exporting the same build twice and byte-comparing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

fn w(dir: &Path, name: &str, src: &str) {
    let p = dir.join(name);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, src).unwrap();
}

fn fixture(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chitra_gold_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // --- Rust: cross-file + same-file ---
    w(
        &dir,
        "math.rs",
        "fn add(a: i32, b: i32) -> i32 { a + b }\nfn sub(a: i32, b: i32) -> i32 { a - b }\n",
    );
    w(
        &dir,
        "calc.rs",
        "fn helper() -> i32 { add(0, 0) }\n\
         fn run() -> i32 { let _ = add(1, 2); let _ = sub(3, 1); helper() }\n",
    );
    // --- Python: import-promoted cross-file ---
    w(&dir, "lib.py", "def parse():\n    return 1\n");
    w(
        &dir,
        "app.py",
        "from lib import parse\ndef main():\n    parse()\n    return parse()\n",
    );
    // --- Ambiguous: same name in two files -> must NOT be asserted ---
    w(&dir, "a.rs", "fn dup() {}\n");
    w(&dir, "b.rs", "fn dup() {}\n");
    w(&dir, "user.rs", "fn u() { dup(); }\n");
    // --- Module-ambiguous: two `fetch` candidates, but the caller says which
    // module it imported from. Baseline leaves this unresolved (recall miss);
    // `deep-resolve` breaks the tie (T4.1).
    w(&dir, "store.py", "def fetch():\n    return 1\n");
    w(&dir, "cache.py", "def fetch():\n    return 2\n");
    w(
        &dir,
        "svc.py",
        "from store import fetch\ndef load():\n    return fetch()\n",
    );
    // --- Rust: two `render` candidates; the call is written through a path.
    w(&dir, "html_out.rs", "fn render() -> i32 { 1 }\n");
    w(&dir, "text_out.rs", "fn render() -> i32 { 2 }\n");
    w(&dir, "page.rs", "fn draw() -> i32 { html_out::render() }\n");
    // --- Go: two `Encode` candidates; the call goes through the package name,
    // which is the *directory*, not the file stem.
    w(
        &dir,
        "json/enc.go",
        "package json\nfunc Encode() int { return 1 }\n",
    );
    w(
        &dir,
        "xml/enc.go",
        "package xml\nfunc Encode() int { return 2 }\n",
    );
    w(
        &dir,
        "main.go",
        "package main\nfunc Run() int { return json.Encode() }\n",
    );
    dir
}

/// The edges only deep resolution can reach — one per evidence kind, and one per
/// language family, so a regression in any of them is visible.
fn deep_edges() -> Vec<(String, String)> {
    [
        ("svc.py::load", "store.py::fetch"),      // Python: import module
        ("page.rs::draw", "html_out.rs::render"), // Rust: path qualifier
        ("main.go::Run", "json/enc.go::Encode"),  // Go: package = directory
    ]
    .iter()
    .map(|(s, t)| (s.to_string(), t.to_string()))
    .collect()
}

/// Hand-labelled correct CALLS edges (source_qn -> target_qn).
fn truth() -> HashSet<(String, String)> {
    [
        ("calc.rs::run", "math.rs::add"),
        ("calc.rs::run", "math.rs::sub"),
        ("calc.rs::run", "calc.rs::helper"),
        ("calc.rs::helper", "math.rs::add"),
        ("app.py::main", "lib.py::parse"),
        ("svc.py::load", "store.py::fetch"),
        ("page.rs::draw", "html_out.rs::render"),
        ("main.go::Run", "json/enc.go::Encode"),
    ]
    .iter()
    .map(|(s, t)| (s.to_string(), t.to_string()))
    .collect()
}

/// Asserted (non-AMBIGUOUS) CALLS edges from a fresh build of the fixture.
fn asserted_edges(tag: &str) -> HashSet<(String, String)> {
    let dir = fixture(tag);
    let mut store = chitra_core::Store::open_in_memory().unwrap();
    chitra_core::build(&mut store, &dir).unwrap();
    store
        .all_edges()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "CALLS" && e.tier != "AMBIGUOUS")
        .map(|e| (e.source, e.target))
        .collect()
}

#[test]
fn precision_at_least_085_recall_reported() {
    // Asserted CALLS edges (EXTRACTED/INFERRED only; AMBIGUOUS is excluded).
    let asserted = asserted_edges("prec");
    let truth = truth();
    let correct = asserted.intersection(&truth).count();
    let precision = correct as f64 / asserted.len().max(1) as f64;
    let recall = correct as f64 / truth.len() as f64;
    println!(
        "gold: asserted={} correct={correct} precision={precision:.3} recall={recall:.3}",
        asserted.len()
    );

    // The ambiguous `dup()` call must not have been asserted to either target.
    assert!(!asserted.contains(&("user.rs::u".into(), "a.rs::dup".into())));
    assert!(!asserted.contains(&("user.rs::u".into(), "b.rs::dup".into())));

    assert!(
        precision >= 0.85,
        "precision {precision:.3} < 0.85 (RISK-2 gate)"
    );
    let _ = recall; // reported, not gated
}

/// T4.1 acceptance: deep resolution must lift recall on the gold set while
/// precision stays at the gate. Which side of the assertion runs depends on the
/// feature, so both configurations are pinned by the same fixture.
#[test]
fn deep_resolve_lifts_recall_and_holds_precision() {
    let asserted = asserted_edges("deep");
    let truth = truth();
    let correct = asserted.intersection(&truth).count();
    let precision = correct as f64 / asserted.len().max(1) as f64;
    let recall = correct as f64 / truth.len() as f64;
    println!("deep-resolve gold: precision={precision:.3} recall={recall:.3}");

    assert!(precision >= 0.85, "precision {precision:.3} < 0.85");

    // The tie-break must never fire on the candidate the evidence did NOT name.
    for wrong in [
        ("svc.py::load", "cache.py::fetch"),
        ("page.rs::draw", "text_out.rs::render"),
        ("main.go::Run", "xml/enc.go::Encode"),
    ] {
        assert!(
            !asserted.contains(&(wrong.0.to_string(), wrong.1.to_string())),
            "resolved {wrong:?} — the evidence pointed elsewhere"
        );
    }

    #[cfg(feature = "deep-resolve")]
    {
        for e in deep_edges() {
            assert!(asserted.contains(&e), "deep-resolve should resolve {e:?}");
        }
        assert_eq!(recall, 1.0, "expected full recall with deep-resolve");
    }
    #[cfg(not(feature = "deep-resolve"))]
    {
        for e in deep_edges() {
            assert!(
                !asserted.contains(&e),
                "baseline resolver must leave {e:?} unasserted"
            );
        }
        assert!(recall < 1.0, "baseline is expected to miss the tie-breaks");
    }
}

#[test]
fn export_is_deterministic() {
    let dir = fixture("det");
    let export_once = || {
        let mut store = chitra_core::Store::open_in_memory().unwrap();
        chitra_core::build(&mut store, &dir).unwrap();
        chitra_core::export_json(&store).unwrap()
    };
    let a = export_once();
    let b = export_once();
    assert_eq!(
        a, b,
        "graph.json export must be byte-identical across builds"
    );
    // sanity: it's real JSON with our shape
    assert!(a.contains("\"directed\": true"));
    assert!(a.contains("\"tier\": \"EXTRACTED\""));
}
