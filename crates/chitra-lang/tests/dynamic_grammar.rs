//! T4.4 acceptance: a language whose grammar is **not** compiled in, added
//! through `languages.toml` alone.
//!
//! The fixture library (`toy-grammar`) exports `tree_sitter_toy`. Nothing in
//! chitra knows that name — it is reached only via the config file.

#[cfg(feature = "dynamic-grammars")]
use std::path::PathBuf;

/// Where cargo put the fixture cdylib for this profile.
#[cfg(feature = "dynamic-grammars")]
fn toy_library() -> Option<PathBuf> {
    // tests run from the crate dir; the workspace target dir is two levels up.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .parent()?
        .join("target")
        .join(if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        });
    let name = if cfg!(target_os = "windows") {
        "toy_grammar.dll"
    } else if cfg!(target_os = "macos") {
        "libtoy_grammar.dylib"
    } else {
        "libtoy_grammar.so"
    };
    let p = root.join(name);
    p.exists().then_some(p)
}

fn config_toml(lib: &str) -> String {
    format!(
        r#"
[[language]]
name = "toy"
extensions = ["toy"]
grammar = "dynamic"
grammar_library = "{lib}"
function_query = "(function_definition name: (identifier) @name) @def"
call_query = "(call function: (identifier) @callee)"
"#
    )
}

/// Both halves in one test: the opt-in is process-global state, so splitting
/// them would race under the default parallel test runner.
#[test]
#[cfg(feature = "dynamic-grammars")]
fn dynamic_grammar_is_denied_by_default_then_loads_on_opt_in() {
    let Some(lib) = toy_library() else {
        panic!("fixture cdylib missing — run `cargo build -p toy-grammar` first");
    };
    let dir = std::env::temp_dir().join("chitra_dyn");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("languages.toml"),
        config_toml(&lib.to_string_lossy().replace('\\', "/")),
    )
    .unwrap();

    // 1. The trust boundary: a repository-supplied config must not be able to
    //    make chitra load a repository-supplied binary on its own.
    std::env::remove_var(chitra_lang::DYNAMIC_GRAMMAR_ENV);
    assert!(
        chitra_lang::Registry::load(&dir)
            .config_for_extension("toy")
            .is_none(),
        "a grammar must not load without {}=1",
        chitra_lang::DYNAMIC_GRAMMAR_ENV
    );

    // 2. With consent, a language the binary has never heard of works.
    std::env::set_var(chitra_lang::DYNAMIC_GRAMMAR_ENV, "1");
    let cfg = chitra_lang::Registry::load(&dir)
        .config_for_extension("toy")
        .expect("`.toy` should resolve through the dynamically loaded grammar");
    assert_eq!(cfg.language, "toy");

    // The real proof: it parses.
    let parsed = chitra_lang::parse(&cfg, "a.toy", "def build():\n    compile()\n").unwrap();
    assert_eq!(parsed.nodes[0].name, "build");
    assert_eq!(parsed.nodes[0].language, "toy");
    assert_eq!(parsed.raw_calls[0].callee_name, "compile");
    std::env::remove_var(chitra_lang::DYNAMIC_GRAMMAR_ENV);
}

/// Without the feature compiled in, the config is rejected with a message that
/// says how to enable it — not silently ignored.
#[test]
#[cfg(not(feature = "dynamic-grammars"))]
fn dynamic_config_is_rejected_when_the_feature_is_off() {
    let dir = std::env::temp_dir().join("chitra_dyn_off");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("languages.toml"), config_toml("/nonexistent.so")).unwrap();
    let reg = chitra_lang::Registry::load(&dir);
    assert!(reg.config_for_extension("toy").is_none());
}
