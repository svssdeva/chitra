//! detect-changes over a real (temporary) git repo — covers the diff→symbol→
//! risk ranking and the top-N bound (T2.3).

use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .expect("git runs")
        .success();
    assert!(ok, "git {args:?} failed");
}

#[test]
fn detect_changes_ranks_and_bounds() {
    let dir = std::env::temp_dir().join("chitra_dc_it");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // baseline commit
    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "user.email", "t@t.t"]);
    git(&dir, &["config", "user.name", "t"]);
    std::fs::write(dir.join("core.rs"), "fn a() {}\nfn b() {}\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "base"]);

    // edit core.rs on disk: add a hot function called by several sites
    std::fs::write(
        dir.join("core.rs"),
        "fn a() {}\nfn b() {}\nfn hot() {}\n\
         fn c() { hot(); }\nfn d() { hot(); }\nfn e() { hot(); }\n",
    )
    .unwrap();

    let mut store = chitra_core::Store::open_in_memory().unwrap();
    chitra_core::build(&mut store, &dir).unwrap();

    let report = chitra_core::detect_changes(&store, &dir, "HEAD", 20).unwrap();
    assert_eq!(report["changed_files"][0], "core.rs");
    let syms = report["changed_symbols"].as_array().unwrap();
    assert!(!syms.is_empty());
    // hot() has the highest fan-in -> ranked first.
    assert_eq!(syms[0]["symbol"], "core.rs::hot");
    assert_eq!(report["truncated"], false);

    // bound to 1 -> truncated flag set, one symbol shown, full count retained.
    let bounded = chitra_core::detect_changes(&store, &dir, "HEAD", 1).unwrap();
    assert_eq!(bounded["changed_symbols"].as_array().unwrap().len(), 1);
    assert_eq!(bounded["truncated"], true);
    assert!(bounded["summary"]["symbols"].as_u64().unwrap() > 1);

    // markdown render: sticky marker + a table row for the top symbol.
    let md = chitra_core::changes_markdown(&report);
    assert!(md.starts_with("<!-- chitra-review -->"));
    assert!(md.contains("core.rs::hot"));
    assert!(md.contains("| risk |"));
}
