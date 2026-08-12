//! chitra-lang — config-driven tree-sitter walker.
//!
//! A [`LanguageConfig`] is a table of grammar queries + node-kinds; one generic
//! [`parse`] pass drives any language from it. Phase 1 ships Rust, Python,
//! TypeScript/JS and Go — adding a language is a config entry, not new code.
//! Verified against tree-sitter 0.24.7 and the 0.23.x grammar crates (ADR-0001).

use anyhow::{anyhow, Context, Result};
use std::path::Path;
use streaming_iterator::StreamingIterator; // 0.24: QueryCursor::matches streams
use tree_sitter::{Language, Node as TsNode, Parser, Query, QueryCursor};
use tree_sitter_language::LanguageFn;

/// A code entity. Phase 1: functions/methods (kind `Function`, or `Test` when
/// the symbol looks like a test). Classes/types are deferred (call-graph review
/// is function-centric).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub qualified_name: String, // `file::name`, `#L<line>`-suffixed on collision
    pub kind: String,           // "Function" | "Test"
    pub name: String,
    pub file: String,
    pub line_start: usize,
    pub line_end: usize,
    pub language: String,
    pub signature: String, // first line of the def, whitespace-collapsed (heuristic)
    pub is_test: bool,
}

/// An unresolved call site: bare callee name + where it was called from.
/// Resolution to a target node happens later (chitra-core), evidence-gated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawCall {
    pub caller_qualified: String, // enclosing def, or `file::<file>` at top level
    pub callee_name: String,
    pub line: usize,
    /// The path segment the call was written through, when the language has
    /// one: `math` in `math::add()`, `store` in `store.Fetch()`. This is the
    /// strongest disambiguation signal available without type inference — it is
    /// attached to the *call site*, not merely to the file (T4.1).
    pub qualifier: Option<String>,
}

/// A name imported into a file, with the module it came from when the grammar
/// exposes one. `module` is the deep-resolution evidence (Phase 4, T4.1): with
/// it, two same-named candidates in different files can be told apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    pub name: String,
    pub module: Option<String>,
}

#[derive(Debug, Default)]
pub struct ParsedFile {
    pub nodes: Vec<Node>,
    pub raw_calls: Vec<RawCall>,
    pub imports: Vec<Import>, // names imported into this file (resolution evidence)
    /// Set when tree-sitter's error recovery plausibly cost us symbols. A silent
    /// under-extraction is worse than a noisy one: the graph looks complete and
    /// the missing edges are invisible.
    pub parse_warning: Option<String>,
}

/// Per-language driver: the grammar plus the queries/kinds the walker keys off.
///
/// Fields are owned rather than `&'static str` so a config can also come from a
/// user's `languages.toml` (T4.4). A config is built per file, and building one
/// already allocated, so this costs nothing measurable.
#[derive(Clone)]
pub struct LanguageConfig {
    pub language: String,
    pub extensions: Vec<String>,
    pub grammar: LanguageFn,
    /// Function/method def query: must capture `@name` (identifier) and `@def`
    /// (whole def node). May hold several patterns (e.g. free fn + method).
    pub function_query: String,
    /// Call-site query: must capture `@callee`.
    pub call_query: String,
    /// Import query capturing `@import` (short imported name) and optionally
    /// `@module` (where it came from). `None` = language resolves fine on
    /// unique-global evidence alone (Go).
    pub import_query: Option<String>,
    /// Name prefixes that mark a symbol as a test (in addition to path heuristics).
    pub test_prefixes: Vec<String>,
    /// Split one captured callee into several on this character. HTML's
    /// `class="card title"` is two references, not one symbol named "card title".
    pub callee_separator: Option<char>,
    /// Languages whose nodes this language's calls may bind to. Defaults to the
    /// language itself — the cross-language-family guard. HTML widens it to CSS
    /// deliberately: that edge is the whole point of parsing HTML.
    pub resolve_languages: Vec<String>,
    /// Keep the first definition when a name repeats in a file instead of
    /// suffixing it with `#L<line>`. A CSS class styled by three rule blocks is
    /// one class, not three.
    pub merge_duplicate_defs: bool,
    /// Emit a `<file>` node so references written outside any definition still
    /// have a source. HTML markup is not inside a function.
    pub emit_file_node: bool,
}

impl LanguageConfig {
    /// The common case: definitions and calls, bound only within this language.
    /// Everything else is an override on top.
    fn new(
        language: &str,
        extensions: &[&str],
        grammar: LanguageFn,
        function_query: &str,
        call_query: &str,
    ) -> LanguageConfig {
        LanguageConfig {
            language: language.to_string(),
            extensions: strs(extensions),
            grammar,
            function_query: function_query.to_string(),
            call_query: call_query.to_string(),
            import_query: None,
            test_prefixes: Vec::new(),
            callee_separator: None,
            resolve_languages: vec![language.to_string()],
            merge_duplicate_defs: false,
            emit_file_node: false,
        }
    }
}

// ---- grammars -------------------------------------------------------------
//
// Grammar crates expose a `LanguageFn` (a C function pointer) directly, so a
// config stores that rather than a Rust wrapper. It is `Copy`, and it is also
// exactly the shape a grammar loaded from a shared library has — which is what
// lets static and dynamic grammars share one code path (T4.4).

// ---- configs --------------------------------------------------------------

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

pub fn rust_config() -> LanguageConfig {
    LanguageConfig {
        // `use a::b::c;` and `use a::b::{c, d};` — the module half is real
        // resolution evidence in Rust, which has no dynamic import to confuse it.
        import_query: Some(
            "(use_declaration argument: (scoped_identifier path: (_) @module name: (identifier) @import))\n\
             (use_declaration argument: (scoped_use_list path: (_) @module list: (use_list (identifier) @import)))"
                .to_string(),
        ),
        ..LanguageConfig::new(
            "rust",
            &["rs"],
            tree_sitter_rust::LANGUAGE,
            "(function_item name: (identifier) @name) @def",
            // Bare `foo()`; `math::add()` and `a::b::add()` (last path segment is
            // the qualifier); method `x.foo()`.
            "(call_expression function: (identifier) @callee)\n\
             (call_expression function: (scoped_identifier path: (identifier) @qualifier name: (identifier) @callee))\n\
             (call_expression function: (scoped_identifier path: (scoped_identifier name: (identifier) @qualifier) name: (identifier) @callee))\n\
             (call_expression function: (field_expression field: (field_identifier) @callee))",
        )
    }
}

pub fn python_config() -> LanguageConfig {
    LanguageConfig {
        // Pattern 1 carries the module (deep-resolve evidence); 2 keeps relative
        // imports (`from . import x`) that pattern 1 can't match. Duplicates are
        // collapsed in `parse`.
        import_query: Some(
            "(import_from_statement module_name: (dotted_name) @module name: (dotted_name (identifier) @import))\n\
             (import_from_statement name: (dotted_name (identifier) @import))\n\
             (import_statement name: (dotted_name (identifier) @import))"
                .to_string(),
        ),
        test_prefixes: strs(&["test_"]),
        ..LanguageConfig::new(
            "python",
            &["py"],
            tree_sitter_python::LANGUAGE,
            "(function_definition name: (identifier) @name) @def",
            // `foo()`, `mod.foo()` (qualifier captured), `a.b.foo()` (no qualifier
            // — the object is an expression, not a name we can match to a file).
            "(call function: (identifier) @callee)\n\
             (call function: (attribute object: (identifier) @qualifier attribute: (identifier) @callee))\n\
             (call function: (attribute object: (attribute) attribute: (identifier) @callee))",
        )
    }
}

pub fn go_config() -> LanguageConfig {
    LanguageConfig {
        // Go imports bind a package, not individual symbols, so there is no
        // @import to capture — the call-site qualifier carries the evidence.
        test_prefixes: strs(&["Test", "Benchmark", "Fuzz"]),
        ..LanguageConfig::new(
            "go",
            &["go"],
            tree_sitter_go::LANGUAGE,
            "(function_declaration name: (identifier) @name) @def\n\
             (method_declaration name: (field_identifier) @name) @def",
            // `foo()` and `pkg.Foo()` / `x.Foo()` — in Go the qualifier is usually
            // the package name, which by convention names the directory too.
            "(call_expression function: (identifier) @callee)\n\
             (call_expression function: (selector_expression operand: (identifier) @qualifier field: (field_identifier) @callee))\n\
             (call_expression function: (selector_expression operand: (selector_expression) field: (field_identifier) @callee))",
        )
    }
}

fn ts_config_for(extensions: &[&str], grammar: LanguageFn) -> LanguageConfig {
    LanguageConfig {
        import_query: Some(
            // Child order is significant: the grammar puts the clause before `from <source>`.
            "(import_statement (import_clause (named_imports (import_specifier name: (identifier) @import))) source: (string) @module)"
                .to_string(),
        ),
        ..LanguageConfig::new(
            "typescript",
            extensions,
            grammar,
            "(function_declaration name: (identifier) @name) @def\n\
             (method_definition name: (property_identifier) @name) @def",
            "(call_expression function: (identifier) @callee)\n\
             (call_expression function: (member_expression object: (identifier) @qualifier property: (property_identifier) @callee))\n\
             (call_expression function: (member_expression object: (member_expression) property: (property_identifier) @callee))",
        )
    }
}

pub fn ts_config() -> LanguageConfig {
    ts_config_for(&["ts"], tree_sitter_typescript::LANGUAGE_TYPESCRIPT)
}

pub fn tsx_config() -> LanguageConfig {
    // TSX grammar is a superset that also parses JS/JSX.
    ts_config_for(
        &["tsx", "js", "jsx", "mjs", "cjs"],
        tree_sitter_typescript::LANGUAGE_TSX,
    )
}

/// CSS as a def/use graph. Definitions are the things other files reference —
/// class selectors, id selectors, custom properties. Uses are `var(--x)`.
/// Declarations like `color: red` are styling, not structure, and are ignored.
pub fn css_config() -> LanguageConfig {
    LanguageConfig {
        // A class styled in three places is one class.
        merge_duplicate_defs: true,
        ..LanguageConfig::new(
            "css",
            &["css", "scss"],
            tree_sitter_css::LANGUAGE,
            // `@def` is the whole rule set, so a `var()` inside the block
            // attributes to the selector that owns it. Combinators are listed
            // explicitly — tree-sitter patterns cannot match at arbitrary depth,
            // and `:root`'s pseudo-class name parses as a `class_name` that must
            // not become a node.
            "(rule_set (selectors (class_selector (class_name) @name))) @def\n\
             (rule_set (selectors (id_selector (id_name) @name))) @def\n\
             (rule_set (selectors (descendant_selector (class_selector (class_name) @name)))) @def\n\
             (rule_set (selectors (descendant_selector (id_selector (id_name) @name)))) @def\n\
             (rule_set (selectors (child_selector (class_selector (class_name) @name)))) @def\n\
             (rule_set (selectors (pseudo_class_selector (class_selector (class_name) @name)))) @def\n\
             ((declaration (property_name) @name) @def (#match? @name \"^--\"))",
            // `var(--brand)` — the only cross-reference CSS makes to itself.
            "((call_expression (function_name) @fn (arguments (plain_value) @callee)) (#eq? @fn \"var\"))",
        )
    }
}

/// HTML as the consumer side of that graph: `id=` defines an anchor, `class=`
/// references CSS classes. This is the one place the cross-language guard is
/// deliberately widened — an HTML→CSS edge is the entire reason to parse HTML.
pub fn html_config() -> LanguageConfig {
    LanguageConfig {
        callee_separator: Some(' '), // class="card title" is two references
        resolve_languages: strs(&["css", "html"]),
        emit_file_node: true, // markup lives outside any definition
        ..LanguageConfig::new(
            "html",
            &["html", "htm"],
            tree_sitter_html::LANGUAGE,
            "((element (start_tag (attribute (attribute_name) @an (quoted_attribute_value (attribute_value) @name)))) @def (#eq? @an \"id\"))",
            "((attribute (attribute_name) @an (quoted_attribute_value (attribute_value) @callee)) (#eq? @an \"class\"))",
        )
    }
}

/// Look up a built-in config by file extension. User-defined languages go
/// through [`Registry`] instead.
pub fn config_for_extension(ext: &str) -> Option<LanguageConfig> {
    match ext {
        "rs" => Some(rust_config()),
        "py" => Some(python_config()),
        "go" => Some(go_config()),
        "ts" => Some(ts_config()),
        "tsx" | "js" | "jsx" | "mjs" | "cjs" => Some(tsx_config()),
        "css" | "scss" => Some(css_config()),
        "html" | "htm" => Some(html_config()),
        _ => None,
    }
}

/// Grammars a `languages.toml` entry may name. Grammars are compiled in
/// (ADR-0001), so a user config reuses one of these — it cannot introduce a new
/// grammar without a Rust dependency.
pub fn grammar_by_name(name: &str) -> Option<LanguageFn> {
    match name {
        "rust" => Some(tree_sitter_rust::LANGUAGE),
        "python" => Some(tree_sitter_python::LANGUAGE),
        "go" => Some(tree_sitter_go::LANGUAGE),
        "typescript" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT),
        "tsx" => Some(tree_sitter_typescript::LANGUAGE_TSX),
        "css" => Some(tree_sitter_css::LANGUAGE),
        "html" => Some(tree_sitter_html::LANGUAGE),
        _ => None,
    }
}

/// Extensions the built-in configs own. A `languages.toml` entry may never
/// claim one of these — built-ins are protected (T4.4).
const BUILTIN_EXTENSIONS: &[&str] = &[
    "rs", "py", "go", "ts", "tsx", "js", "jsx", "mjs", "cjs", "css", "scss", "html", "htm",
];

/// Cap on user-defined languages. Each one compiles two tree-sitter queries per
/// file parsed, so an unbounded config is a self-inflicted slowdown.
pub const MAX_CUSTOM_LANGUAGES: usize = 20;

/// Built-in configs plus any user-defined ones from `languages.toml`.
///
/// Grammars are compiled in (ADR-0001), so a `languages.toml` entry attaches new
/// *queries and extensions* to an existing grammar — enough for a dialect
/// (`.bzl` on the Python grammar, `.mts` on TSX) but not for a language whose
/// grammar isn't linked. That limit is deliberate, not an oversight.
#[derive(Default)]
pub struct Registry {
    custom: Vec<LanguageConfig>,
}

impl Registry {
    /// Built-ins only — no config file consulted.
    pub fn builtin_only() -> Registry {
        Registry::default()
    }

    /// Load `<root>/languages.toml` if present. Never fails a build: a malformed
    /// file or entry warns and is skipped (postprocess discipline).
    pub fn load(root: &Path) -> Registry {
        let path = root.join("languages.toml");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Registry::default(); // absent is the normal case
        };
        match parse_languages_toml(&text) {
            Ok((custom, warnings)) => {
                for w in warnings {
                    eprintln!("warn: languages.toml: {w}");
                }
                Registry { custom }
            }
            Err(e) => {
                eprintln!("warn: languages.toml ignored: {e}");
                Registry::default()
            }
        }
    }

    /// language -> the languages its calls may resolve into. Feeds the
    /// cross-language-family guard in the resolver.
    pub fn resolve_map(&self) -> std::collections::HashMap<String, Vec<String>> {
        BUILTIN_EXTENSIONS
            .iter()
            .filter_map(|e| config_for_extension(e))
            .chain(self.custom.iter().cloned())
            .map(|c| (c.language, c.resolve_languages))
            .collect()
    }

    /// Built-ins win; user entries fill the gaps.
    pub fn config_for_extension(&self, ext: &str) -> Option<LanguageConfig> {
        config_for_extension(ext).or_else(|| {
            self.custom
                .iter()
                .find(|c| c.extensions.iter().any(|e| e == ext))
                .cloned()
        })
    }
}

/// Parse + validate the `[[language]]` array. Returns the accepted configs and
/// one warning per rejected entry — a bad entry is dropped, not fatal.
fn parse_languages_toml(text: &str) -> Result<(Vec<LanguageConfig>, Vec<String>)> {
    let table: toml::Table = text.parse().context("not valid TOML")?;
    let entries = match table.get("language") {
        Some(toml::Value::Array(a)) => a.clone(),
        Some(_) => return Err(anyhow!("`language` must be an array of tables")),
        None => Vec::new(),
    };

    let mut out = Vec::new();
    let mut warnings = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        if out.len() >= MAX_CUSTOM_LANGUAGES {
            warnings.push(format!(
                "entry {i}: dropped, cap of {MAX_CUSTOM_LANGUAGES} custom languages reached"
            ));
            continue;
        }
        match language_from_toml(entry) {
            Ok(cfg) => out.push(cfg),
            Err(e) => warnings.push(format!("entry {i}: {e}")),
        }
    }
    Ok((out, warnings))
}

fn language_from_toml(entry: &toml::Value) -> Result<LanguageConfig> {
    let get_str = |k: &str| -> Result<String> {
        entry
            .get(k)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("missing string field `{k}`"))
    };
    let get_list = |k: &str| -> Vec<String> {
        entry
            .get(k)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };

    let language = get_str("name")?;
    let grammar_name = get_str("grammar")?;
    let grammar = if grammar_name == "dynamic" {
        let lib = get_str("grammar_library")
            .context("`grammar = \"dynamic\"` needs `grammar_library`")?;
        let symbol = entry
            .get("grammar_symbol")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("tree_sitter_{language}"));
        load_dynamic_grammar(&lib, &symbol)?
    } else {
        grammar_by_name(&grammar_name).ok_or_else(|| {
            anyhow!("unknown grammar `{grammar_name}` (use a built-in name, or `dynamic`)")
        })?
    };

    let extensions = get_list("extensions");
    if extensions.is_empty() {
        return Err(anyhow!("`extensions` must list at least one extension"));
    }
    if let Some(clash) = extensions
        .iter()
        .find(|e| BUILTIN_EXTENSIONS.contains(&e.as_str()))
    {
        return Err(anyhow!(
            "extension `{clash}` belongs to a built-in language"
        ));
    }

    let resolve_languages = match get_list("resolve_languages") {
        v if v.is_empty() => vec![language.clone()],
        v => v,
    };
    let cfg = LanguageConfig {
        language,
        extensions,
        grammar,
        function_query: get_str("function_query")?,
        call_query: get_str("call_query")?,
        import_query: entry
            .get("import_query")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        test_prefixes: get_list("test_prefixes"),
        callee_separator: entry
            .get("callee_separator")
            .and_then(|v| v.as_str())
            .and_then(|s| s.chars().next()),
        resolve_languages,
        merge_duplicate_defs: entry
            .get("merge_duplicate_defs")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        emit_file_node: entry
            .get("emit_file_node")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    };
    // Compile the queries now: a typo caught here is one warning, not one per
    // file parsed.
    validate_queries(&cfg)?;
    Ok(cfg)
}

/// Environment variable that must be set for any shared library to be loaded.
///
/// This is a **trust boundary**, not a convenience toggle. `languages.toml` and
/// the library it names both live in the repository being scanned, so loading
/// one means running code that a repository author chose, inside whoever builds
/// the graph. Cloning a repo and running `chitra build` must never be a
/// code-execution vector, so the default is deny even when the feature is
/// compiled in.
pub const DYNAMIC_GRAMMAR_ENV: &str = "CHITRA_ALLOW_DYNAMIC_GRAMMARS";

/// Load a tree-sitter grammar from a shared library at runtime (T4.4).
///
/// This is what lets a genuinely new language — one whose grammar is not linked
/// into the binary — be added with no Rust changes: point at a compiled
/// `libtree-sitter-<lang>.{so,dylib,dll}` and give the queries in TOML.
#[cfg(feature = "dynamic-grammars")]
fn load_dynamic_grammar(path: &str, symbol: &str) -> Result<LanguageFn> {
    if std::env::var(DYNAMIC_GRAMMAR_ENV).unwrap_or_default() != "1" {
        return Err(anyhow!(
            "refusing to load `{path}`: loading a grammar runs code from the scanned \
             repository. Set {DYNAMIC_GRAMMAR_ENV}=1 only for libraries you trust"
        ));
    }
    // SAFETY: dlopen/LoadLibrary runs the library's initialisers. The caller has
    // explicitly opted in via the environment variable above; there is no way to
    // validate arbitrary native code beyond that consent.
    let lib = unsafe { libloading::Library::new(path) }
        .with_context(|| format!("cannot load grammar library `{path}`"))?;
    // SAFETY: the symbol is required to be a tree-sitter grammar entry point,
    // `extern "C" fn() -> *const TSLanguage`. A wrong symbol type here is
    // undefined behaviour, which is exactly why the opt-in above exists.
    let func = unsafe {
        let sym: libloading::Symbol<unsafe extern "C" fn() -> *const ()> = lib
            .get(symbol.as_bytes())
            .with_context(|| format!("`{path}` has no symbol `{symbol}`"))?;
        *sym
    };
    // The Language borrows code and static tables owned by the library, so the
    // library must outlive every parse. It is deliberately leaked: grammars are
    // loaded once at startup and live for the process.
    std::mem::forget(lib);
    // SAFETY: `func` is the grammar entry point resolved above.
    Ok(unsafe { LanguageFn::from_raw(func) })
}

#[cfg(not(feature = "dynamic-grammars"))]
fn load_dynamic_grammar(_path: &str, _symbol: &str) -> Result<LanguageFn> {
    Err(anyhow!(
        "runtime grammar loading needs the `dynamic-grammars` feature: \
         cargo install chitra --features dynamic-grammars"
    ))
}

fn validate_queries(cfg: &LanguageConfig) -> Result<()> {
    let language: Language = cfg.grammar.into();
    let fq = Query::new(&language, &cfg.function_query).context("function_query")?;
    if fq.capture_index_for_name("name").is_none() || fq.capture_index_for_name("def").is_none() {
        return Err(anyhow!("function_query must capture both @name and @def"));
    }
    let cq = Query::new(&language, &cfg.call_query).context("call_query")?;
    if cq.capture_index_for_name("callee").is_none() {
        return Err(anyhow!("call_query must capture @callee"));
    }
    if let Some(iq) = &cfg.import_query {
        let q = Query::new(&language, iq).context("import_query")?;
        if q.capture_index_for_name("import").is_none() {
            return Err(anyhow!("import_query must capture @import"));
        }
    }
    Ok(())
}

/// Convenience: parse a Rust source string.
pub fn parse_rust(file: &str, source: &str) -> Result<ParsedFile> {
    parse(&rust_config(), file, source)
}

// ---- the generic walk -----------------------------------------------------

/// Generic walk: source + config -> nodes + raw call sites + imported names.
pub fn parse(cfg: &LanguageConfig, file: &str, source: &str) -> Result<ParsedFile> {
    let language: Language = cfg.grammar.into();
    let mut parser = Parser::new();
    parser.set_language(&language)?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("tree-sitter failed to parse {file}"))?;
    let bytes = source.as_bytes();
    let root = tree.root_node();
    let file_is_test = path_is_test(file);

    // --- definitions ---
    let mut nodes: Vec<Node> = Vec::new();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    // (start_byte, end_byte, qualified_name) per definition — how a call site is
    // attributed to its enclosing definition. Byte containment rather than an
    // AST climb, so it works for languages whose "definition" node carries no
    // `name` field (a CSS rule set, an HTML element).
    let mut def_ranges: Vec<(usize, usize, String)> = Vec::new();

    if cfg.emit_file_node {
        nodes.push(Node {
            qualified_name: format!("{file}::<file>"),
            kind: "File".to_string(),
            name: file.to_string(),
            file: file.to_string(),
            line_start: 1,
            line_end: source.lines().count().max(1),
            language: cfg.language.clone(),
            signature: String::new(),
            is_test: file_is_test,
        });
    }

    let fq = Query::new(&language, &cfg.function_query)?;
    let f_name = fq
        .capture_index_for_name("name")
        .context("function_query missing @name capture")?;
    let f_def = fq
        .capture_index_for_name("def")
        .context("function_query missing @def capture")?;
    let mut fc = QueryCursor::new();
    let mut fm = fc.matches(&fq, root, bytes);
    while let Some(m) = fm.next() {
        let mut name = None;
        let mut def_node = None;
        for c in m.captures.iter() {
            if c.index == f_name {
                name = Some(c.node.utf8_text(bytes)?.to_string());
            } else if c.index == f_def {
                def_node = Some(c.node);
            }
        }
        let (Some(name), Some(dn)) = (name, def_node) else {
            continue;
        };
        let (s, e) = (dn.start_position().row, dn.end_position().row);
        // Collision policy (data model): later same-name symbols get `#L<line>`,
        // unless the language says repeats are the same thing (CSS selectors).
        let base = format!("{file}::{name}");
        let repeat = seen.contains_key(&base);
        if repeat && cfg.merge_duplicate_defs {
            // Still a valid enclosing scope for calls inside it.
            def_ranges.push((dn.start_byte(), dn.end_byte(), base));
            continue;
        }
        let qn = if repeat {
            format!("{base}#L{}", s + 1)
        } else {
            base.clone()
        };
        *seen.entry(base).or_insert(0) += 1;
        def_ranges.push((dn.start_byte(), dn.end_byte(), qn.clone()));
        let is_test = file_is_test
            || cfg
                .test_prefixes
                .iter()
                .any(|p| name.starts_with(p.as_str()));
        nodes.push(Node {
            qualified_name: qn,
            kind: if is_test { "Test" } else { "Function" }.to_string(),
            name,
            file: file.to_string(),
            line_start: s + 1,
            line_end: e + 1,
            language: cfg.language.clone(),
            signature: signature_of(dn, bytes),
            is_test,
        });
    }

    // --- call sites ---
    let mut raw_calls = Vec::new();
    let cq = Query::new(&language, &cfg.call_query)?;
    let c_idx = cq
        .capture_index_for_name("callee")
        .context("call_query missing @callee capture")?;
    let q_idx = cq.capture_index_for_name("qualifier");
    let mut cc = QueryCursor::new();
    let mut cm = cc.matches(&cq, root, bytes);
    while let Some(m) = cm.next() {
        // The qualifier belongs to the same match as its callee.
        let qualifier = match q_idx {
            Some(qi) => m
                .captures
                .iter()
                .find(|c| c.index == qi)
                .and_then(|c| c.node.utf8_text(bytes).ok())
                .map(str::to_string),
            None => None,
        };
        for c in m.captures.iter().filter(|c| c.index == c_idx) {
            let text = c.node.utf8_text(bytes)?;
            let line = c.node.start_position().row + 1;
            let caller = enclosing(&def_ranges, c.node.start_byte(), file);
            // One capture may hold several references (`class="card title"`).
            let callees: Vec<&str> = match cfg.callee_separator {
                Some(sep) => text.split(sep).filter(|s| !s.is_empty()).collect(),
                None => vec![text],
            };
            for callee in callees {
                raw_calls.push(RawCall {
                    caller_qualified: caller.clone(),
                    callee_name: callee.to_string(),
                    line,
                    qualifier: qualifier.clone(),
                });
            }
        }
    }

    // --- imports (resolution evidence; best-effort) ---
    let mut imports: Vec<Import> = Vec::new();
    if let Some(iq) = &cfg.import_query {
        let q = Query::new(&language, iq)?;
        if let Some(idx) = q.capture_index_for_name("import") {
            let mod_idx = q.capture_index_for_name("module");
            let mut ic = QueryCursor::new();
            let mut im = ic.matches(&q, root, bytes);
            while let Some(m) = im.next() {
                // One match may bind several names to a single module
                // (`from lib import a, b` / `import { a, b } from './lib'`).
                let module = match mod_idx {
                    Some(mi) => m
                        .captures
                        .iter()
                        .find(|c| c.index == mi)
                        .and_then(|c| c.node.utf8_text(bytes).ok())
                        .map(normalize_module),
                    None => None,
                };
                for c in m.captures.iter().filter(|c| c.index == idx) {
                    imports.push(Import {
                        name: c.node.utf8_text(bytes)?.to_string(),
                        module: module.clone(),
                    });
                }
            }
        }
    }
    dedup_imports(&mut imports);

    // What this file actually contributed. The synthesized `<file>` node is not
    // a definition, and a template with no ids but many class references has
    // yielded plenty — both would look like "empty" if we counted nodes alone.
    let extracted = nodes.iter().filter(|n| n.kind != "File").count() + raw_calls.len();
    let parse_warning = partial_parse_warning(root, extracted);

    Ok(ParsedFile {
        nodes,
        raw_calls,
        imports,
        parse_warning,
    })
}

/// Decide whether a recovered parse is worth reporting.
///
/// `has_error()` alone is far too noisy to act on: tree-sitter sets it for tiny
/// fully-recovered slips — a stray `&` in a JSX attribute, a missing semicolon —
/// where every symbol is still extracted. graphify shipped that unguarded
/// version and had to walk it back (their #2551 → #2610), so chitra takes the
/// gated form straight away: report only when recovery plausibly *cost* us
/// something.
///
/// `extracted` is definitions **plus references**, excluding the synthesized
/// `<file>` node. Measuring nodes alone misfired on 134 Angular templates in a
/// real monorepo: they carry no `id` attributes, so their only node was the file
/// node, while they had in fact yielded dozens of class references each.
fn partial_parse_warning(root: TsNode, extracted: usize) -> Option<String> {
    if !root.has_error() {
        return None;
    }
    let (widest, total) = widest_error_span(root);
    if extracted <= 1 {
        return Some(format!(
            "syntax errors; recovered only {extracted} symbol(s) or reference(s) — the graph for this file is probably incomplete"
        ));
    }
    if widest > 1 {
        return Some(format!(
            "syntax errors; {total} unparsable region(s), the largest spanning {widest} lines — symbols there are missing"
        ));
    }
    None // recovered cleanly enough to stay quiet
}

/// (widest ERROR region in lines, number of ERROR regions).
fn widest_error_span(root: TsNode) -> (usize, usize) {
    let mut cursor = root.walk();
    let mut stack = vec![root];
    let (mut widest, mut count) = (0usize, 0usize);
    while let Some(n) = stack.pop() {
        if n.is_error() || n.is_missing() {
            count += 1;
            let lines = n.end_position().row.saturating_sub(n.start_position().row) + 1;
            widest = widest.max(lines);
            continue; // no need to descend into a region already counted
        }
        if n.has_error() {
            stack.extend(n.children(&mut cursor));
        }
    }
    (widest, count)
}

/// Strip the noise a grammar leaves on a module reference: quotes around a JS
/// string, and the `./` of a relative specifier.
fn normalize_module(raw: &str) -> String {
    raw.trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .to_string()
}

/// Overlapping import patterns can bind the same name twice (once with a module,
/// once without). Keep the module-bearing row — it is the stronger evidence.
fn dedup_imports(imports: &mut Vec<Import>) {
    let with_module: std::collections::HashSet<String> = imports
        .iter()
        .filter(|i| i.module.is_some())
        .map(|i| i.name.clone())
        .collect();
    imports.retain(|i| i.module.is_some() || !with_module.contains(&i.name));
    let mut seen = std::collections::HashSet::new();
    imports.retain(|i| seen.insert((i.name.clone(), i.module.clone())));
}

/// The innermost definition whose source range contains this call site, or the
/// file itself. Narrowest range wins, so a call in a nested function attributes
/// to the inner one.
///
/// ponytail: linear scan over the file's definitions — a few dozen on real
/// files. Sort + binary search if a generated file ever makes this show up in a
/// profile.
fn enclosing(def_ranges: &[(usize, usize, String)], pos: usize, file: &str) -> String {
    def_ranges
        .iter()
        .filter(|(s, e, _)| *s <= pos && pos < *e)
        .min_by_key(|(s, e, _)| e - s)
        .map(|(_, _, qn)| qn.clone())
        .unwrap_or_else(|| format!("{file}::<file>"))
}

/// ponytail: signature = first line of the def, whitespace-collapsed. Cheap and
/// honest; a full param/return normalizer is Phase 4 if search demands it.
fn signature_of(def: TsNode, bytes: &[u8]) -> String {
    let text = def.utf8_text(bytes).unwrap_or("");
    let end = text
        .find('{')
        .or_else(|| text.find('\n'))
        .unwrap_or(text.len());
    text[..end].split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Path-based test heuristic (covers frameworks that don't use a name prefix).
fn path_is_test(file: &str) -> bool {
    let f = file.to_ascii_lowercase();
    f.contains("/tests/")
        || f.contains("/test/")
        || f.contains("_test.")
        || f.contains(".test.")
        || f.contains(".spec.")
        || f.starts_with("test_")
        || f.contains("/test_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_functions_and_calls_rust() {
        let src = "fn helper() -> i32 { 42 }\n\
                   fn compute() -> i32 { helper() + helper() }\n\
                   fn main() { let _ = compute(); }\n";
        let pf = parse_rust("m.rs", src).unwrap();
        let names: Vec<&str> = pf.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["helper", "compute", "main"]);
        assert_eq!(pf.nodes[0].qualified_name, "m.rs::helper");
        assert_eq!(pf.nodes[1].line_start, 2);
        assert_eq!(pf.nodes[0].signature, "fn helper() -> i32");

        let calls: Vec<(&str, &str)> = pf
            .raw_calls
            .iter()
            .map(|c| (c.caller_qualified.as_str(), c.callee_name.as_str()))
            .collect();
        assert!(calls.contains(&("m.rs::compute", "helper")));
        assert!(calls.contains(&("m.rs::main", "compute")));
        assert_eq!(calls.iter().filter(|(_, c)| *c == "helper").count(), 2);
    }

    #[test]
    fn python_functions_calls_imports() {
        let src = "from mod import helper\n\
                   def compute():\n    return helper()\n\
                   def test_compute():\n    assert compute()\n";
        let pf = parse(&python_config(), "m.py", src).unwrap();
        let names: Vec<&str> = pf.nodes.iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains(&"compute"));
        assert_eq!(
            pf.imports,
            vec![Import {
                name: "helper".to_string(),
                module: Some("mod".to_string()),
            }]
        );
        // test_compute is a Test node.
        let t = pf.nodes.iter().find(|n| n.name == "test_compute").unwrap();
        assert!(t.is_test && t.kind == "Test");
        assert!(pf
            .raw_calls
            .iter()
            .any(|c| c.caller_qualified == "m.py::compute" && c.callee_name == "helper"));
    }

    #[test]
    fn go_functions_and_methods() {
        let src = "package p\nfunc Helper() int { return 1 }\n\
                   func (s S) Run() int { return Helper() }\n";
        let pf = parse(&go_config(), "m.go", src).unwrap();
        let names: Vec<&str> = pf.nodes.iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains(&"Helper"));
        assert!(names.contains(&"Run"));
        assert!(pf
            .raw_calls
            .iter()
            .any(|c| c.caller_qualified == "m.go::Run" && c.callee_name == "Helper"));
    }

    #[test]
    fn typescript_functions_and_calls() {
        let src = "import { helper } from './m';\n\
                   function compute() { return helper(); }\n\
                   function main() { compute(); }\n";
        let pf = parse(&ts_config(), "m.ts", src).unwrap();
        let names: Vec<&str> = pf.nodes.iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains(&"compute"));
        assert!(names.contains(&"main"));
        assert_eq!(
            pf.imports,
            vec![Import {
                name: "helper".to_string(),
                module: Some("./m".to_string()),
            }]
        );
        assert!(pf
            .raw_calls
            .iter()
            .any(|c| c.caller_qualified == "m.ts::main" && c.callee_name == "compute"));
    }

    // ---- partial parses (upstream lesson: graphify #2551 -> #2610) ----

    #[test]
    fn a_broken_file_that_loses_symbols_warns() {
        let src = "fn good() -> i32 { 1 }\n\
                   fn broken( -> { @@@@@\n\
                   fn also_good() -> i32 { good() }\n";
        let pf = parse_rust("b.rs", src).unwrap();
        let w = pf
            .parse_warning
            .expect("a file this broken must not fail silently");
        assert!(w.contains("syntax errors"), "{w}");
    }

    /// Regression from a real monorepo: 134 Angular templates warned because
    /// their only node was the synthesized `<file>` node. They carry no `id`
    /// attributes but plenty of class references — nothing was lost.
    #[test]
    fn a_template_with_references_but_no_definitions_stays_quiet() {
        // `@if (...) { }` is Angular control flow; tree-sitter-html flags it.
        let src = "<div class=\"page-container\">\n\
                   @if (build(); as b) {\n\
                   <div class=\"detail-grid card\"><span class=\"title\">x</span></div>\n\
                   }\n\
                   </div>\n";
        let pf = parse(&html_config(), "t.html", src).unwrap();
        assert!(
            pf.raw_calls.len() > 2,
            "the class references should still be extracted: {:?}",
            pf.raw_calls
        );
        assert_eq!(
            pf.parse_warning, None,
            "a template that yielded many references must not be called incomplete"
        );
    }

    #[test]
    fn a_clean_file_never_warns() {
        let pf = parse_rust("ok.rs", "fn a() {}\nfn b() { a(); }\n").unwrap();
        assert_eq!(pf.parse_warning, None);
    }

    /// The regression graphify had to ship twice: a tiny recovered slip that
    /// costs no symbols must stay quiet, or the warning is noise and gets
    /// ignored when it matters.
    #[test]
    fn a_recovered_slip_that_costs_no_symbols_stays_quiet() {
        // Stray token between two complete definitions: tree-sitter flags an
        // error, but both functions still extract.
        let src = "fn one() -> i32 { 1 }\n@\nfn two() -> i32 { 2 }\n";
        let pf = parse_rust("s.rs", src).unwrap();
        assert_eq!(pf.nodes.len(), 2, "both definitions should survive");
        assert_eq!(
            pf.parse_warning, None,
            "a one-line recovered error that cost nothing must not warn"
        );
    }

    #[test]
    fn rust_path_call_captures_the_qualifier() {
        let pf = parse_rust(
            "p.rs",
            "fn draw() { html::render(); a::b::emit(); local(); }\n",
        )
        .unwrap();
        let q: Vec<(&str, Option<&str>)> = pf
            .raw_calls
            .iter()
            .map(|c| (c.callee_name.as_str(), c.qualifier.as_deref()))
            .collect();
        assert!(q.contains(&("render", Some("html"))));
        assert!(q.contains(&("emit", Some("b")))); // last path segment
        assert!(q.contains(&("local", None)));
    }

    #[test]
    fn rust_use_declaration_is_import_evidence() {
        let pf = parse_rust("p.rs", "use crate::math::add;\nuse a::b::{c, d};\n").unwrap();
        assert!(pf.imports.contains(&Import {
            name: "add".to_string(),
            module: Some("crate::math".to_string()),
        }));
        assert!(pf.imports.contains(&Import {
            name: "c".to_string(),
            module: Some("a::b".to_string()),
        }));
    }

    #[test]
    fn go_selector_call_captures_the_package() {
        let pf = parse(
            &go_config(),
            "m.go",
            "package m\nfunc R() { json.Encode() }\n",
        )
        .unwrap();
        let c = &pf.raw_calls[0];
        assert_eq!(
            (c.callee_name.as_str(), c.qualifier.as_deref()),
            ("Encode", Some("json"))
        );
    }

    // ---- HTML + CSS ----

    #[test]
    fn css_defines_selectors_and_uses_custom_properties() {
        let src = ":root { --brand: red; }\n\
                   .card { color: var(--brand); }\n\
                   .card { padding: 0; }\n\
                   #hero .title { margin: 0; }\n";
        let pf = parse(&css_config(), "s.css", src).unwrap();
        let names: Vec<&str> = pf.nodes.iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains(&"--brand"));
        assert!(names.contains(&"card"));
        assert!(names.contains(&"hero"));
        assert!(names.contains(&"title")); // nested in a descendant selector
                                           // `.card` is styled twice but is one class.
        assert_eq!(names.iter().filter(|n| **n == "card").count(), 1);

        // `var(--brand)` is a use, attributed to the rule it sits in.
        let call = pf
            .raw_calls
            .iter()
            .find(|c| c.callee_name == "--brand")
            .expect("var() use not captured");
        assert_eq!(call.caller_qualified, "s.css::card");
    }

    #[test]
    fn html_ids_define_and_class_lists_reference() {
        let src = "<div id=\"hero\" class=\"card title\">x</div>\n";
        let pf = parse(&html_config(), "i.html", src).unwrap();
        let names: Vec<&str> = pf.nodes.iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains(&"hero"));
        assert!(names.contains(&"i.html")); // the file node

        // class="card title" is two references, not one symbol.
        let callees: Vec<&str> = pf
            .raw_calls
            .iter()
            .map(|c| c.callee_name.as_str())
            .collect();
        assert_eq!(callees, vec!["card", "title"]);
    }

    // ---- T4.4: languages.toml ----

    const STARLARK_TOML: &str = r#"
[[language]]
name = "starlark"
extensions = ["bzl"]
grammar = "python"
function_query = "(function_definition name: (identifier) @name) @def"
call_query = "(call function: (identifier) @callee)"
"#;

    #[test]
    fn custom_language_parses_without_rust_changes() {
        let (cfgs, warnings) = parse_languages_toml(STARLARK_TOML).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(cfgs.len(), 1);
        let pf = parse(&cfgs[0], "rules.bzl", "def build():\n    compile()\n").unwrap();
        assert_eq!(pf.nodes[0].name, "build");
        assert_eq!(pf.nodes[0].language, "starlark");
        assert_eq!(pf.raw_calls[0].callee_name, "compile");
    }

    #[test]
    fn custom_language_cannot_claim_a_builtin_extension() {
        let toml = STARLARK_TOML.replace(r#"extensions = ["bzl"]"#, r#"extensions = ["py"]"#);
        let (cfgs, warnings) = parse_languages_toml(&toml).unwrap();
        assert!(cfgs.is_empty());
        assert!(warnings[0].contains("built-in"), "{warnings:?}");
    }

    #[test]
    fn unknown_grammar_is_rejected_with_a_warning() {
        let toml = STARLARK_TOML.replace(r#"grammar = "python""#, r#"grammar = "cobol""#);
        let (cfgs, warnings) = parse_languages_toml(&toml).unwrap();
        assert!(cfgs.is_empty());
        assert!(warnings[0].contains("unknown grammar"), "{warnings:?}");
    }

    #[test]
    fn malformed_query_is_rejected_at_load_not_per_file() {
        let toml = STARLARK_TOML.replace(
            "(call function: (identifier) @callee)",
            "(call function: (identifier) @wrong)",
        );
        let (cfgs, warnings) = parse_languages_toml(&toml).unwrap();
        assert!(cfgs.is_empty());
        assert!(warnings[0].contains("@callee"), "{warnings:?}");
    }

    #[test]
    fn custom_languages_are_capped() {
        let mut toml = String::new();
        for i in 0..MAX_CUSTOM_LANGUAGES + 3 {
            toml.push_str(&STARLARK_TOML.replace(r#"["bzl"]"#, &format!(r#"["bzl{i}"]"#)));
        }
        let (cfgs, warnings) = parse_languages_toml(&toml).unwrap();
        assert_eq!(cfgs.len(), MAX_CUSTOM_LANGUAGES);
        assert_eq!(warnings.len(), 3);
        assert!(warnings[0].contains("cap"), "{warnings:?}");
    }

    #[test]
    fn registry_prefers_builtin_over_custom() {
        let (custom, _) = parse_languages_toml(STARLARK_TOML).unwrap();
        let reg = Registry { custom };
        assert_eq!(reg.config_for_extension("py").unwrap().language, "python");
        assert_eq!(
            reg.config_for_extension("bzl").unwrap().language,
            "starlark"
        );
        assert!(reg.config_for_extension("cob").is_none());
    }

    #[test]
    fn collision_gets_line_suffix() {
        let src = "#[cfg(a)]\nfn f() {}\n#[cfg(b)]\nfn f() {}\n";
        let pf = parse_rust("m.rs", src).unwrap();
        let qns: Vec<&str> = pf.nodes.iter().map(|n| n.qualified_name.as_str()).collect();
        assert_eq!(qns[0], "m.rs::f");
        assert!(qns[1].starts_with("m.rs::f#L"));
    }
}
