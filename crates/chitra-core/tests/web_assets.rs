//! HTML + CSS as a def/use graph: which markup depends on which style.
//!
//! This is the one place the cross-language-family guard is widened, so it gets
//! its own test — including the negative case that the guard still holds
//! everywhere else.

use std::path::{Path, PathBuf};

fn w(dir: &Path, name: &str, src: &str) {
    let p = dir.join(name);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, src).unwrap();
}

/// Per-test fixture directory — these tests run in parallel and would otherwise
/// wipe each other's files.
fn site(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chitra_web_assets_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    w(
        &dir,
        "styles.css",
        ":root { --brand: #09f; }\n\
         .card { color: var(--brand); }\n\
         .card { border: 0; }\n\
         .unused { color: red; }\n",
    );
    w(
        &dir,
        "index.html",
        "<div id=\"hero\" class=\"card\">a</div>\n\
         <span class=\"card\">b</span>\n",
    );
    w(&dir, "about.html", "<p class=\"card\">c</p>\n");
    // A Python file with a function named `card` — the guard must keep it out of
    // the HTML resolution, since Python is not in HTML's resolve_languages.
    w(&dir, "app.py", "def card():\n    return 1\n");
    dir
}

fn built(tag: &str) -> chitra_core::Store {
    let mut store = chitra_core::Store::open_in_memory().unwrap();
    chitra_core::build(&mut store, &site(tag)).unwrap();
    store
}

#[test]
fn html_class_use_reaches_the_css_rule_that_defines_it() {
    let store = built("reach");
    let users = store.impact("styles.css::card", 3).unwrap();
    assert!(
        users.contains(&"index.html::<file>".to_string()),
        "index.html should depend on .card, got {users:?}"
    );
    assert!(
        users.contains(&"about.html::<file>".to_string()),
        "about.html should depend on .card, got {users:?}"
    );
}

#[test]
fn a_css_class_nothing_references_has_no_dependents() {
    let store = built("unused");
    assert!(store.impact("styles.css::unused", 3).unwrap().is_empty());
}

/// Editing a custom property reaches the rule that reads it *and*, transitively,
/// the markup that uses that rule — the blast radius a reviewer actually wants.
#[test]
fn custom_property_blast_radius_reaches_the_markup() {
    let store = built("prop");
    let users = store.impact("styles.css::--brand", 3).unwrap();
    assert!(users.contains(&"styles.css::card".to_string()));
    assert!(users.contains(&"index.html::<file>".to_string()));
    assert!(users.contains(&"about.html::<file>".to_string()));
    // The element carrying the class is reached too, not just its file.
    assert!(users.contains(&"index.html::hero".to_string()));
}

/// The widened guard is scoped to HTML→CSS. A Python `card()` must not be
/// dragged in, and CSS must not resolve into HTML.
#[test]
fn cross_language_guard_still_holds_outside_the_html_css_pair() {
    let store = built("guard");
    let py_users = store.impact("app.py::card", 3).unwrap();
    assert!(
        py_users.is_empty(),
        "python card() must not be a target of HTML markup, got {py_users:?}"
    );
}

/// A repeated selector is one node, so the reference resolves instead of going
/// ambiguous across the two rule blocks.
#[test]
fn repeated_selector_is_a_single_node() {
    let store = built("merge");
    assert!(store.get_node("styles.css::card").unwrap().is_some());
    assert!(
        store.get_node("styles.css::card#L3").unwrap().is_none(),
        "repeated `.card` rule should merge, not collide"
    );
}
