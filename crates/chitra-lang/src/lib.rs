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

/// A code entity: functions and methods (`Function`, or `Test` when the symbol
/// looks like one), type declarations (`Type`), and a whole-file node (`File`)
/// for languages whose references live outside any definition.
///
/// `Type` nodes are not call targets. They exist so the qualifier in
/// `Foo::new()` has something in the graph to match against — without them a
/// constructor call has no evidence at all and stays ambiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub qualified_name: String, // `file::name`, `#L<line>`-suffixed on collision
    pub kind: String,           // "Function" | "Test" | "Type" | "File"
    pub name: String,
    pub file: String,
    pub line_start: usize,
    pub line_end: usize,
    pub language: String,
    pub signature: String, // first line of the def, whitespace-collapsed (heuristic)
    /// Doc comment attached to the definition, whitespace-collapsed and capped.
    /// Indexed for search: it is the only prose chitra holds, and it describes a
    /// symbol in words its name may not contain.
    pub doc: String,
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
    /// Type/class declaration query, capturing `@name` and `@def`. Optional: a
    /// language without a type system (CSS, HTML) simply has none.
    ///
    /// A type node is not a call target on its own — it exists so that the
    /// qualifier in `Foo::new()` has something in the graph to match against.
    /// Patterns that mark *where a type's code lives* (Rust `impl` blocks) are
    /// as useful here as the declaration itself, and both are captured.
    pub type_query: Option<String>,
    /// Import query capturing `@import` (short imported name) and optionally
    /// `@module` (where it came from). `None` = language resolves fine on
    /// unique-global evidence alone (Go).
    pub import_query: Option<String>,
    /// Name prefixes that mark a symbol as a test (in addition to path heuristics).
    pub test_prefixes: Vec<String>,
    /// Extra definition patterns applied **only to files the path marks as
    /// tests**. A test case declares no function, so without this a test file
    /// yields no nodes; running it everywhere instead would mint nodes inside
    /// ordinary functions and steal their call attribution.
    pub test_function_query: Option<String>,
    /// Attributes/decorators that mark a definition — or any scope enclosing it —
    /// as test code. Rust's dominant idiom puts tests *beside* the code they
    /// cover (`#[cfg(test)] mod tests`), so neither the path nor the name says
    /// "test" and both other heuristics miss it entirely.
    ///
    /// A pattern matches an attribute whose inner text is exactly the pattern,
    /// or is path-qualified with it (`test` matches `#[tokio::test]`). Matching
    /// the whole token rather than a substring is what keeps `#[cfg(not(test))]`
    /// — which marks the opposite — from reading as a test.
    pub test_attributes: Vec<String>,
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
    /// What this language's definitions *are*. Defaults to `Function` because
    /// most are, but a CSS class selector is not a function and typing it as
    /// one is not cosmetic: on a real site 2,910 of 5,658 nodes were selectors
    /// filed as functions, which is what put stylesheets at the top of a
    /// change-risk ranking meant to surface code.
    pub def_kind: String,
    /// The doc comment lives *inside* the definition as its first statement (a
    /// Python docstring) rather than in comments above it.
    pub doc_in_body: bool,
    /// Rewrite the source before parsing, for a file format that *embeds* a
    /// supported language rather than being one (an `.astro` component around
    /// TypeScript). A filter must preserve line numbering — see
    /// [`astro_to_ts`]. Not settable from `languages.toml`: it is code, and a
    /// config file may not introduce code.
    pub source_filter: Option<fn(&str) -> String>,
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
            test_function_query: None,
            test_attributes: Vec::new(),
            callee_separator: None,
            resolve_languages: vec![language.to_string()],
            merge_duplicate_defs: false,
            emit_file_node: false,
            def_kind: "Function".to_string(),
            type_query: None,
            doc_in_body: false,
            source_filter: None,
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
        // `impl Foo` matters as much as `struct Foo` here: the methods a call
        // like `Foo::new()` is looking for live in the impl block, which is
        // often not the file the struct is declared in.
        type_query: Some(
            "(struct_item name: (type_identifier) @name) @def\n\
             (enum_item name: (type_identifier) @name) @def\n\
             (trait_item name: (type_identifier) @name) @def\n\
             (impl_item type: (type_identifier) @name) @def"
                .to_string(),
        ),
        // `use a::b::c;` and `use a::b::{c, d};` — the module half is real
        // resolution evidence in Rust, which has no dynamic import to confuse it.
        import_query: Some(
            "(use_declaration argument: (scoped_identifier path: (_) @module name: (identifier) @import))\n\
             (use_declaration argument: (scoped_use_list path: (_) @module list: (use_list (identifier) @import)))"
                .to_string(),
        ),
        // Rust puts tests beside the code they cover, so `path_is_test` never
        // fires for the dominant idiom: `#[cfg(test)] mod tests` inside an
        // ordinary `src/*.rs`. `test` also covers `#[tokio::test]` and friends.
        test_attributes: strs(&["test", "cfg(test)", "rstest", "proptest"]),
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
        doc_in_body: true,
        type_query: Some("(class_definition name: (identifier) @name) @def".to_string()),
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
        type_query: Some(
            "(type_declaration (type_spec name: (type_identifier) @name)) @def".to_string(),
        ),
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
        type_query: Some(
            "(class_declaration name: (type_identifier) @name) @def\n\
             (interface_declaration name: (type_identifier) @name) @def\n\
             (type_alias_declaration name: (type_identifier) @name) @def"
                .to_string(),
        ),
        import_query: Some(
            // Child order is significant: the grammar puts the clause before `from <source>`.
            "(import_statement (import_clause (named_imports (import_specifier name: (identifier) @import))) source: (string) @module)"
                .to_string(),
        ),
        // A test case is a definition too. `test("…", () => {})` declares
        // nothing, so a test file parsed to zero nodes and no TESTED_BY edge
        // could exist. This runs *only* in files the path marks as tests:
        // `it("x", …)` inside an ordinary function would otherwise mint a node
        // whose byte range steals call attribution from the real enclosing
        // function, silently deleting edges from the graph.
        //
        // `@name` captures the whole `(string)` rather than a `string_fragment`
        // — the grammar splits a literal at every escape, so
        // `test("returns \"ok\" now")` produced three nodes with mangled names.
        // The `.` anchor pins it to the *first* argument.
        test_function_query: Some(
            "((call_expression\n\
                 function: (identifier) @_tfn\n\
                 arguments: (arguments . (string) @name)) @def\n\
              (#match? @_tfn \"^(test|it|bench)$\"))"
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
        // A selector is a styling hook, not a callable.
        def_kind: "Selector".to_string(),
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
        // An `id=` is a link target in the document, not a callable.
        def_kind: "Anchor".to_string(),
        ..LanguageConfig::new(
            "html",
            &["html", "htm"],
            tree_sitter_html::LANGUAGE,
            "((element (start_tag (attribute (attribute_name) @an (quoted_attribute_value (attribute_value) @name)))) @def (#eq? @an \"id\"))",
            "((attribute (attribute_name) @an (quoted_attribute_value (attribute_value) @callee)) (#eq? @an \"class\"))",
        )
    }
}

/// Rewrite an `.astro` single-file component into the TypeScript subset of
/// itself, **preserving line numbering exactly** so every reported line still
/// points at the right line of the original file.
///
/// Astro has no first-class grammar here. Rather than link one, the parts that
/// *are* TypeScript — the `---` frontmatter fence and any `<script>` block —
/// are kept and everything else is replaced by a blank line of the same index.
/// The frontmatter is where an Astro page does its work: imports, data
/// fetching, and the calls into shared code that a reviewer needs to see.
///
/// Ceiling: expressions embedded in markup (`{items.map(render)}`) and
/// component usage (`<Card />`) are blanked with the rest of the template, so
/// calls written *only* there are not edges. The imports that introduce those
/// components are still captured, which is what resolution runs on. Lift this
/// by linking a real Astro grammar if markup-level calls start mattering.
/// Does this line open a `<script>` block whose body is executable code?
///
/// Being wrong here is expensive in one direction: treating a data or
/// templated block as code hands tree-sitter a page of markup and loses the
/// whole file to a parse error. So this is deliberately conservative and only
/// accepts a complete, plain opening tag on one line.
///
/// Rejected, each seen in real Astro pages:
/// - `<script type="application/ld+json">` — JSON-LD, structured data
/// - `<script set:html={JSON.stringify(...)}>` — body injected, not written here
/// - `<script is:inline type="..."` continued on the next line — an incomplete
///   tag, where the body does not start on this line at all
fn opens_code_script(t: &str) -> bool {
    let Some(rest) = t.strip_prefix("<script") else {
        return false;
    };
    // Tag-name boundary. `<script-loader>` is a different element, and without
    // this check the shim entered a script block that never closed and handed
    // the whole remaining template to the TypeScript parser.
    if !rest.is_empty() && !rest.starts_with([' ', '\t', '>', '/']) {
        return false;
    }
    if t.contains("/>") || closes_script(t) {
        return false;
    }
    // The opening tag must finish on this line, or we cannot know where the
    // body begins.
    if !t.contains('>') {
        return false;
    }
    script_body_is_code(t)
}

/// Does this line close a script block? `</script >` is legal HTML.
fn closes_script(t: &str) -> bool {
    match t.find("</script") {
        None => false,
        Some(i) => t[i + "</script".len()..].trim_start().starts_with('>'),
    }
}

/// Shared attribute test: is the body of this `<script …>` executable code?
fn script_body_is_code(t: &str) -> bool {
    // `set:html` fills the body from an expression; the text is not source.
    if t.contains("set:html") {
        return false;
    }
    // A `type` other than a JavaScript one means the body is data.
    match t.split("type=").nth(1) {
        None => true,
        Some(rest) => {
            let v = rest
                .trim_start()
                .trim_start_matches(['"', '\''])
                .split(['"', '\'', ' ', '>'])
                .next()
                .unwrap_or("");
            v.is_empty() || v == "module" || v == "text/javascript" || v == "application/javascript"
        }
    }
}

/// `<script>init()</script>` written on one line — returns just the body, which
/// otherwise gets blanked along with the markup.
fn inline_script_body(t: &str) -> Option<&str> {
    let rest = t.strip_prefix("<script")?;
    if !rest.is_empty() && !rest.starts_with([' ', '\t', '>', '/']) {
        return None;
    }
    if t.contains("/>") || !script_body_is_code(t) {
        return None;
    }
    let open_end = t.find('>')?;
    let close = t.find("</script")?;
    if close < open_end {
        return None;
    }
    Some(&t[open_end + 1..close])
}

/// Track the TypeScript lexical state that can hide a `---` from the fence
/// detector: template literals and block comments both span lines, and a
/// frontmatter block that builds a Markdown string contains `---` legitimately.
/// Closing the fence there dropped the rest of the frontmatter and left an
/// unterminated literal, losing every symbol in the file.
fn track_ts_state(line: &str, in_template: &mut bool, in_block_comment: &mut bool) {
    let c: Vec<char> = line.chars().collect();
    let mut i = 0;
    // Ordinary quotes cannot span lines, so this resets each call.
    let mut quote: Option<char> = None;
    while i < c.len() {
        let ch = c[i];
        if *in_block_comment {
            if ch == '*' && c.get(i + 1) == Some(&'/') {
                *in_block_comment = false;
                i += 2;
                continue;
            }
        } else if let Some(q) = quote {
            if ch == '\\' {
                i += 2;
                continue;
            }
            if ch == q {
                quote = None;
            }
        } else if *in_template {
            if ch == '\\' {
                i += 2;
                continue;
            }
            if ch == '`' {
                *in_template = false;
            }
        } else {
            match ch {
                '/' if c.get(i + 1) == Some(&'/') => return, // line comment
                '/' if c.get(i + 1) == Some(&'*') => {
                    *in_block_comment = true;
                    i += 2;
                    continue;
                }
                '`' => *in_template = true,
                '\'' | '"' => quote = Some(ch),
                _ => {}
            }
        }
        i += 1;
    }
}

fn astro_to_ts(source: &str) -> String {
    enum Zone {
        /// Before anything meaningful — a leading `---` opens frontmatter.
        Pre,
        Frontmatter,
        Markup,
        Script,
    }
    let mut out = String::with_capacity(source.len());
    let mut zone = Zone::Pre;
    let mut in_template = false;
    let mut in_block_comment = false;
    for (i, raw) in source.lines().enumerate() {
        // A UTF-8 BOM is not whitespace, so without stripping it the opening
        // fence never matches `---` and the entire file reads as markup —
        // silently, with no parse warning. BOMs are routine on Windows.
        let line = if i == 0 {
            raw.trim_start_matches('\u{feff}')
        } else {
            raw
        };
        let t = line.trim();
        let mut keep = false;
        let mut inline: Option<&str> = None;
        match zone {
            Zone::Pre => {
                if t == "---" {
                    zone = Zone::Frontmatter;
                } else if !t.is_empty() {
                    zone = Zone::Markup;
                }
            }
            Zone::Frontmatter => {
                // The fence only closes in plain code, never inside a template
                // literal or block comment that happens to contain `---`.
                if t == "---" && !in_template && !in_block_comment {
                    zone = Zone::Markup;
                } else {
                    keep = true;
                    track_ts_state(line, &mut in_template, &mut in_block_comment);
                }
            }
            Zone::Markup => {}
            Zone::Script => {
                if closes_script(t) {
                    zone = Zone::Markup;
                } else {
                    keep = true;
                }
            }
        }
        if matches!(zone, Zone::Markup) {
            if opens_code_script(t) {
                zone = Zone::Script;
            } else {
                inline = inline_script_body(t);
            }
        }
        match inline {
            Some(body) => out.push_str(body),
            None if keep => out.push_str(line),
            None => {}
        }
        out.push('\n');
    }
    out
}

/// Astro pages, parsed as the TypeScript they mostly are (see [`astro_to_ts`]).
pub fn astro_config() -> LanguageConfig {
    LanguageConfig {
        language: "astro".to_string(),
        // Frontmatter runs at the top level of the module, so its calls have no
        // enclosing function to attribute to — without a file node they would
        // have no source and be dropped.
        emit_file_node: true,
        // The entire point: an Astro page calls into shared `.ts` modules.
        resolve_languages: strs(&["astro", "typescript"]),
        source_filter: Some(astro_to_ts),
        ..ts_config_for(&["astro"], tree_sitter_typescript::LANGUAGE_TYPESCRIPT)
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
        "astro" => Some(astro_config()),
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
    "rs", "py", "go", "ts", "tsx", "js", "jsx", "mjs", "cjs", "css", "scss", "html", "htm", "astro",
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
        type_query: entry
            .get("type_query")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        test_prefixes: get_list("test_prefixes"),
        test_function_query: entry
            .get("test_function_query")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        test_attributes: get_list("test_attributes"),
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
        def_kind: entry
            .get("def_kind")
            .and_then(|v| v.as_str())
            .unwrap_or("Function")
            .to_string(),
        doc_in_body: entry
            .get("doc_in_body")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        // Deliberately not configurable: a source filter is code, and
        // `languages.toml` is data read from the scanned repo.
        source_filter: None,
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
    if let Some(tq) = &cfg.type_query {
        let q = Query::new(&language, tq).context("type_query")?;
        if q.capture_index_for_name("name").is_none() || q.capture_index_for_name("def").is_none() {
            return Err(anyhow!("type_query must capture both @name and @def"));
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
    // A wrapper format (`.astro`) is reduced to the language it embeds first.
    // The filter preserves line numbering, so everything downstream — line
    // numbers, signatures, byte-containment scoping — reads the same as if the
    // file had been written in that language.
    let filtered = cfg.source_filter.map(|f| f(source));
    let source: &str = filtered.as_deref().unwrap_or(source);
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
            doc: String::new(),
            is_test: file_is_test,
        });
    }

    // Test-case patterns are only in play for a file the path marks as a test.
    let fq_src = match (&cfg.test_function_query, file_is_test) {
        (Some(extra), true) => format!("{}\n{}", cfg.function_query, extra),
        _ => cfg.function_query.clone(),
    };
    let fq = Query::new(&language, &fq_src)?;
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
        // A test case's name is a string literal, so the capture carries its
        // quotes. Identifiers never do, so this is a no-op everywhere else.
        let name = strip_quotes(&name).to_string();
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
                .any(|p| name.starts_with(p.as_str()))
            || has_test_attribute(dn, bytes, &cfg.test_attributes);
        nodes.push(Node {
            qualified_name: qn,
            kind: if is_test {
                "Test".to_string()
            } else {
                cfg.def_kind.clone()
            },
            name,
            file: file.to_string(),
            line_start: s + 1,
            line_end: e + 1,
            language: cfg.language.clone(),
            signature: signature_of(dn, bytes),
            doc: doc_of(dn, bytes, cfg.doc_in_body),
            is_test,
        });
    }

    // --- type declarations ---
    //
    // Deliberately NOT added to `def_ranges`: a call inside a class body should
    // still attribute to the enclosing method, and widening the scope table
    // would silently re-parent existing edges.
    if let Some(tq) = &cfg.type_query {
        let tq = Query::new(&language, tq)?;
        let t_name = tq
            .capture_index_for_name("name")
            .context("type_query missing @name capture")?;
        let t_def = tq
            .capture_index_for_name("def")
            .context("type_query missing @def capture")?;
        let mut tc = QueryCursor::new();
        let mut tm = tc.matches(&tq, root, bytes);
        while let Some(m) = tm.next() {
            let mut name = None;
            let mut def_node = None;
            for c in m.captures.iter() {
                if c.index == t_name {
                    name = Some(c.node.utf8_text(bytes)?.to_string());
                } else if c.index == t_def {
                    def_node = Some(c.node);
                }
            }
            let (Some(name), Some(dn)) = (name, def_node) else {
                continue;
            };
            let (s, e) = (dn.start_position().row, dn.end_position().row);
            // Shares `seen` with functions so one file can never mint two nodes
            // under the same qualified name.
            let base = format!("{file}::{name}");
            let qn = if seen.contains_key(&base) {
                format!("{base}#L{}", s + 1)
            } else {
                base.clone()
            };
            *seen.entry(base).or_insert(0) += 1;
            nodes.push(Node {
                qualified_name: qn,
                kind: "Type".to_string(),
                name,
                file: file.to_string(),
                line_start: s + 1,
                line_end: e + 1,
                language: cfg.language.clone(),
                signature: signature_of(dn, bytes),
                doc: doc_of(dn, bytes, cfg.doc_in_body),
                is_test: file_is_test,
            });
        }
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

/// Doc comment for a definition: the run of comment lines immediately above it,
/// or — for languages that put it inside — the first string in the body.
///
/// Whitespace-collapsed, which also strips the carriage return a CRLF
/// checkout would
/// otherwise leak into the graph and break cross-platform byte equality. Capped,
/// because a 200-line module docstring is not a search term.
fn doc_of(def: TsNode, bytes: &[u8], in_body: bool) -> String {
    const CAP: usize = 400;
    let mut parts: Vec<String> = Vec::new();
    if in_body {
        // A Python docstring: the first statement in the body is a string.
        if let Some(body) = def.child_by_field_name("body") {
            if let Some(first) = body.named_child(0) {
                let n = if first.kind() == "expression_statement" {
                    first.named_child(0)
                } else {
                    Some(first)
                };
                if let Some(n) = n.filter(|n| n.kind() == "string") {
                    parts.push(n.utf8_text(bytes).unwrap_or("").to_string());
                }
            }
        }
    } else {
        // The comment is rarely the immediately preceding sibling. An attribute
        // or decorator usually sits between it and the definition
        // (`/// Doc` / `#[derive(Debug)]` / `struct Foo`), and in TypeScript the
        // definition is *nested inside* the `export` statement the comment sits
        // above. Miss either and the doc channel is empty for most public API,
        // which is exactly the code worth finding.
        let mut node = def;
        'walk: loop {
            let mut cur = node.prev_sibling();
            while let Some(n) = cur {
                if n.kind().contains("comment") {
                    parts.push(n.utf8_text(bytes).unwrap_or("").to_string());
                } else if !is_decoration(n.kind()) {
                    break 'walk;
                }
                cur = n.prev_sibling();
            }
            // Nothing left at this level: the comment may sit above the wrapper.
            // Only climb one that *opens before* the definition — `export class`
            // qualifies, a block does not, and a block is stopped anyway by its
            // `{` being neither comment nor decoration.
            match node.parent() {
                Some(p) if p.start_byte() < node.start_byte() => node = p,
                _ => break,
            }
        }
        parts.reverse();
    }
    let joined = parts.join(" ");
    let cleaned: String = joined
        .split_whitespace()
        .map(|w| w.trim_matches(|c| c == '/' || c == '#' || c == '*' || c == '"'))
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    match cleaned.char_indices().nth(CAP) {
        Some((i, _)) => cleaned[..i].to_string(),
        None => cleaned,
    }
}

/// Node kinds that may sit between a doc comment and the thing it documents.
/// Everything else ends the walk, so a comment is never stolen from across
/// unrelated code.
fn is_decoration(kind: &str) -> bool {
    kind.contains("attribute")
        || matches!(
            kind,
            "decorator" | "export" | "default" | "async" | "abstract" | "declare"
        )
}

/// Path-based test heuristic (covers frameworks that don't use a name prefix).
/// Does `attr` — the raw source of one attribute/decorator — name a test?
///
/// The inner text is taken between the outermost brackets and stripped of
/// whitespace, so `#[ tokio :: test ]` and `#[tokio::test]` compare equal. A
/// pattern matches the whole token or a path-qualified form of it; substring
/// matching would make `#[cfg(not(test))]` a test marker.
fn attribute_names_test(attr: &str, patterns: &[String]) -> bool {
    let inner: String = attr
        .trim()
        .trim_start_matches("#![")
        .trim_start_matches("#[")
        .trim_end_matches(']')
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    // A decorator has no brackets to strip; `@pytest.fixture` arrives as-is.
    let inner = inner.trim_start_matches('@');
    patterns.iter().any(|p| {
        inner == p.as_str()
            || inner.ends_with(&format!("::{p}"))
            || inner.ends_with(&format!(".{p}"))
    })
}

/// Is this definition marked as test code by an attribute on itself or on any
/// scope enclosing it?
///
/// Attributes are *preceding siblings* of the item they decorate in both the
/// Rust and Python grammars, not children of it — verified against
/// tree-sitter-rust, where `#[test] fn f()` parses as `(attribute_item)
/// (function_item)`. Climbing parents is what catches `#[cfg(test)] mod tests`,
/// under which plain helper functions are still test code.
fn has_test_attribute(node: tree_sitter::Node, bytes: &[u8], patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let mut scope = Some(node);
    while let Some(n) = scope {
        let mut sib = n.prev_sibling();
        while let Some(s) = sib {
            let kind = s.kind();
            let is_attr = kind.contains("attribute") || kind.contains("decorator");
            // Doc comments may sit between an attribute and its item; anything
            // else means we have left the decoration block.
            if !is_attr && !kind.contains("comment") {
                break;
            }
            if is_attr {
                if let Ok(text) = s.utf8_text(bytes) {
                    if attribute_names_test(text, patterns) {
                        return true;
                    }
                }
            }
            sib = s.prev_sibling();
        }
        scope = n.parent();
    }
    false
}

/// Remove one matching pair of surrounding quotes, if present.
fn strip_quotes(s: &str) -> &str {
    let mut c = s.chars();
    match (c.next(), s.chars().next_back()) {
        (Some(a), Some(b)) if a == b && matches!(a, '"' | '\'' | '`') && s.len() >= 2 => {
            &s[a.len_utf8()..s.len() - b.len_utf8()]
        }
        _ => s,
    }
}

/// Is this path a test file?
///
/// Paths here are always root-relative and `/`-separated, so a repo-root
/// `tests/` directory has no leading slash — the original `/tests/` check
/// missed it, along with Jest's canonical `__tests__/` and vitest's
/// `*.bench.*`. Every miss puts test code into the graph as production code.
fn path_is_test(file: &str) -> bool {
    let f = file.to_ascii_lowercase();
    let dir_named = |d: &str| f.starts_with(&format!("{d}/")) || f.contains(&format!("/{d}/"));
    dir_named("tests")
        || dir_named("test")
        || dir_named("__tests__")
        || dir_named("spec")
        || dir_named("__mocks__")
        || f.contains("_test.")
        || f.contains(".test.")
        || f.contains(".spec.")
        || f.contains(".bench.")
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

    /// Rust's dominant idiom keeps tests in the same file as the code, so
    /// neither the path nor the function name says "test". Missing them makes
    /// every covered function look untested, and `test_gap` is 30% of risk v1.
    #[test]
    fn rust_test_attributes_mark_tests() {
        let src = "fn prod() -> i32 { 1 }\n\
                   #[cfg(test)]\n\
                   mod tests {\n\
                       use super::*;\n\
                       fn helper() -> i32 { 2 }\n\
                       #[test]\n\
                       fn covers_prod() { assert_eq!(prod(), 1); }\n\
                   }\n\
                   #[tokio::test]\n\
                   async fn covers_async() {}\n";
        let pf = parse(&rust_config(), "src/lib.rs", src).unwrap();
        let by = |n: &str| pf.nodes.iter().find(|x| x.name == n).cloned().unwrap();

        assert!(!by("prod").is_test, "production code must stay production");
        assert_eq!(by("prod").kind, "Function");

        let t = by("covers_prod");
        assert!(t.is_test && t.kind == "Test", "#[test] must mark a test");

        let a = by("covers_async");
        assert!(a.is_test, "#[tokio::test] is path-qualified `test`");

        // A plain helper inside `#[cfg(test)] mod tests` is test code too —
        // otherwise it shows up as an untested production function.
        assert!(
            by("helper").is_test,
            "a function under #[cfg(test)] is test code"
        );
    }

    /// A modern TS test file declares nothing: it is a list of
    /// `test("...", () => {})` calls. Matching only declarations meant such a
    /// file parsed to *zero* nodes, so nothing could ever be TESTED_BY, and
    /// `test_gap` — 30% of risk v1 — was pinned at 1.0 for every symbol in the
    /// repository.
    #[test]
    fn ts_test_callbacks_become_test_nodes() {
        let src = "import { enrichProjects } from './github';\n\
                   test(\"enriches stars from a successful fetch\", async () => {\n\
                       await enrichProjects(base);\n\
                   });\n\
                   describe(\"group\", () => {\n\
                       it(\"handles the empty case\", () => { enrichProjects([]); });\n\
                   });\n";
        let pf = parse(&ts_config(), "data/github.test.ts", src).unwrap();

        let names: Vec<&str> = pf.nodes.iter().map(|n| n.name.as_str()).collect();
        assert!(
            names.contains(&"enriches stars from a successful fetch"),
            "got {names:?}"
        );
        assert!(names.contains(&"handles the empty case"), "got {names:?}");
        assert_eq!(names.len(), 2, "one node per test case, got {names:?}");

        // The path already says `.test.`, so these are Test nodes.
        assert!(pf.nodes.iter().all(|n| n.is_test && n.kind == "Test"));

        // And the call inside the callback is attributed to it, which is what
        // makes the TESTED_BY edge derivable.
        assert!(
            pf.raw_calls
                .iter()
                .any(|c| c.callee_name == "enrichProjects"
                    && c.caller_qualified
                        == "data/github.test.ts::enriches stars from a successful fetch"),
            "got {:?}",
            pf.raw_calls
        );
    }

    /// Test-case synthesis must not fire in ordinary source. A node minted
    /// inside a real function takes over call attribution for that function's
    /// byte range, so `run()`'s calls silently vanish from the graph.
    #[test]
    fn test_pattern_does_not_fire_in_production_files() {
        let src = "export function run() { it(\"items\", () => { helper(); }); other(); }\n";
        let pf = parse(&ts_config(), "src/prod.ts", src).unwrap();
        let names: Vec<&str> = pf.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["run"], "no synthetic test node in prod code");
        // Both calls still belong to `run`, not to a phantom `items` node.
        for c in &pf.raw_calls {
            assert_eq!(c.caller_qualified, "src/prod.ts::run", "{c:?}");
        }
    }

    /// Layouts that are unmistakably tests but that the original path check
    /// missed, putting test code into the graph as production code.
    #[test]
    fn test_paths_cover_common_layouts() {
        for p in [
            "src/__tests__/github.ts",
            "tests/a.ts",
            "src/parse.bench.ts",
            "spec/thing.ts",
            "pkg/tests/x.rs",
            "a/b.test.ts",
        ] {
            assert!(path_is_test(p), "{p} should be a test path");
        }
        for p in [
            "src/data/github.ts",
            "src/latest.ts",
            "src/contest/entry.ts",
            "src/protests.ts",
        ] {
            assert!(!path_is_test(p), "{p} should NOT be a test path");
        }
    }

    /// The grammar splits a string literal at every escape, so capturing
    /// `string_fragment` produced one node per fragment with mangled names.
    /// Capturing the whole `(string)`, anchored to the first argument, is one
    /// node per test.
    #[test]
    fn test_names_survive_escapes_and_extra_arguments() {
        let src = "test(\"returns \\\"ok\\\" always\", () => { go(); });\n\
                   test(\"first\", \"second\", fn);\n";
        let pf = parse(&ts_config(), "a.test.ts", src).unwrap();
        assert_eq!(pf.nodes.len(), 2, "got {:?}", pf.nodes);
        // Quotes stripped, escapes left as written in source.
        assert_eq!(pf.nodes[0].name, "returns \\\"ok\\\" always");
        assert_eq!(
            pf.nodes[1].name, "first",
            "only the first argument names it"
        );
    }

    /// A UTF-8 BOM is not whitespace. Without stripping it the opening `---`
    /// never matched, and a routine Windows-authored page contributed nothing
    /// to the graph — silently, with no parse warning.
    #[test]
    fn astro_shim_tolerates_a_byte_order_mark() {
        let src = "\u{feff}---\nimport { a } from './a';\nconst x = a();\n---\n<div/>\n";
        let pf = parse(&astro_config(), "p.astro", src).unwrap();
        assert!(
            pf.imports.iter().any(|i| i.name == "a"),
            "BOM must not hide the frontmatter, got {:?}",
            pf.imports
        );
        assert!(pf.raw_calls.iter().any(|c| c.callee_name == "a"));
    }

    /// `---` is legal inside a template literal or block comment in
    /// frontmatter (a page that builds Markdown). Closing the fence there
    /// dropped the rest of the frontmatter and left an unterminated literal,
    /// losing every symbol in the file.
    #[test]
    fn astro_fence_ignores_dashes_inside_strings_and_comments() {
        let tpl = "---\nimport { render } from './md';\nconst tpl = `\n---\ntitle: hi\n---\n`;\nconst out = render(tpl);\n---\n<div/>\n";
        let pf = parse(&astro_config(), "p.astro", tpl).unwrap();
        assert!(
            pf.raw_calls.iter().any(|c| c.callee_name == "render"),
            "call after the embedded fence was lost: {:?}",
            pf.raw_calls
        );
        assert!(pf.imports.iter().any(|i| i.name == "render"));

        let cmt = "---\n/*\n---\n*/\nimport { z } from './z';\nconst q = z();\n---\n<div/>\n";
        let pf = parse(&astro_config(), "c.astro", cmt).unwrap();
        assert!(
            pf.raw_calls.iter().any(|c| c.callee_name == "z"),
            "call after a commented fence was lost: {:?}",
            pf.raw_calls
        );
    }

    /// `<script-loader>` is a different element. Matching it as `<script`
    /// opened a block that never closed, handing the whole remaining template
    /// to the TypeScript parser and losing the file to a parse error.
    #[test]
    fn astro_script_tag_name_has_a_boundary() {
        assert!(!opens_code_script("<script-loader url=\"/x.js\">"));
        assert!(!opens_code_script("<scriptish-thing>"));
        assert!(opens_code_script("<script>"));

        let src = "---\nconst a = 1;\n---\n<script-loader url=\"/x.js\">\n<div class=\"a\">hi &amp; bye</div>\n<p>more</p>\n";
        let ts = astro_to_ts(src);
        assert!(!ts.contains("<div"), "markup leaked into TS: {ts}");
        let pf = parse(&astro_config(), "p.astro", src).unwrap();
        assert!(
            pf.parse_warning.is_none(),
            "should parse cleanly, got {:?}",
            pf.parse_warning
        );
    }

    /// `</script >` with whitespace is legal and must close the block;
    /// otherwise the rest of the document leaks into the TypeScript source.
    #[test]
    fn astro_script_close_tolerates_whitespace() {
        assert!(closes_script("</script>"));
        assert!(closes_script("</script >"));
        assert!(!closes_script("</scriptfoo>"));

        let src = "---\nconst a = 1;\n---\n<script>\n  boot();\n</script >\n<main>\n  <p>text</p>\n</main>\n";
        let ts = astro_to_ts(src);
        assert!(ts.contains("boot();"));
        assert!(!ts.contains("<main"), "markup leaked past close: {ts}");
    }

    /// A one-line `<script>init()</script>` is real code and was being blanked
    /// with the markup.
    #[test]
    fn astro_inline_script_body_is_kept() {
        let src = "---\nconst a = 1;\n---\n<script>init();</script>\n";
        let ts = astro_to_ts(src);
        assert_eq!(ts.lines().count(), src.lines().count());
        assert!(ts.contains("init();"), "got {ts}");
        // A data one-liner is still skipped.
        let data = "<script type=\"application/ld+json\">{\"a\":1}</script>\n";
        assert!(!astro_to_ts(data).contains("\"a\""));
    }

    /// The shim must preserve line numbering exactly: a symbol reported on the
    /// wrong line sends a reviewer to the wrong place.
    #[test]
    fn astro_shim_preserves_line_numbers() {
        let src = "---\nimport { a } from './a';\nconst x = a();\n---\n<div>markup</div>\n<script>\n  init();\n</script>\n<style>.c{color:red}</style>\n";
        let ts = astro_to_ts(src);
        assert_eq!(
            ts.lines().count(),
            src.lines().count(),
            "line count must be preserved"
        );
        let l: Vec<&str> = ts.lines().collect();
        assert_eq!(l[0].trim(), "", "the --- fence is not TypeScript");
        assert_eq!(l[1].trim(), "import { a } from './a';");
        assert_eq!(l[2].trim(), "const x = a();");
        assert_eq!(l[4].trim(), "", "markup must be blanked");
        assert_eq!(l[6].trim(), "init();", "client script is real code");
        assert_eq!(l[8].trim(), "", "style blocks are not TypeScript");
    }

    /// The bug this fixes: an Astro page is where the call to a helper actually
    /// lives, and `.astro` was not in the extension table at all — so the whole
    /// page, and every edge out of it, was missing from the graph.
    #[test]
    fn astro_frontmatter_yields_imports_and_calls() {
        let src = "---\nimport { enrichProjects } from '../../data/github';\nconst enriched = await enrichProjects(projects);\n---\n<h1>{enriched.length}</h1>\n";
        let pf = parse(&astro_config(), "pages/projects/index.astro", src).unwrap();

        assert!(
            pf.imports
                .iter()
                .any(|i| i.name == "enrichProjects"
                    && i.module.as_deref() == Some("../../data/github")),
            "the import is the resolution evidence, got {:?}",
            pf.imports
        );
        // Frontmatter is all top level, so the call hangs off the file node.
        assert!(
            pf.raw_calls
                .iter()
                .any(|c| c.callee_name == "enrichProjects"
                    && c.caller_qualified == "pages/projects/index.astro::<file>"),
            "got {:?}",
            pf.raw_calls
        );
        assert!(
            pf.nodes.iter().any(|n| n.kind == "File"),
            "a file node must exist for those calls to hang off"
        );
    }

    /// A JSON-LD block opened across two lines swallowed 120 lines of markup
    /// into the "TypeScript" it handed the parser, and the whole page was lost
    /// to one parse error. Found on a real page, not imagined.
    #[test]
    fn astro_data_script_blocks_are_not_treated_as_code() {
        assert!(opens_code_script("<script>"));
        assert!(opens_code_script(r#"<script type="module">"#));
        assert!(!opens_code_script(r#"<script type="application/ld+json">"#));
        assert!(
            !opens_code_script(
                r#"<script is:inline type="application/ld+json" set:html={JSON.stringify({"#
            ),
            "an incomplete opening tag must not start a code block"
        );
        assert!(!opens_code_script(r#"<script src="a.js" />"#));
        assert!(!opens_code_script("<script>init()</script>"));

        // End to end: the markup after a JSON-LD block stays blanked.
        let src = "---\nconst a = 1;\n---\n<script is:inline type=\"application/ld+json\" set:html={JSON.stringify({\n  \"@type\": \"Blog\",\n})} />\n<div>{oops()}</div>\n";
        let ts = astro_to_ts(src);
        assert_eq!(ts.lines().count(), src.lines().count());
        assert!(
            !ts.contains("@type"),
            "JSON-LD payload must not be parsed as code: {ts}"
        );
        let pf = parse(&astro_config(), "p.astro", src).unwrap();
        assert!(
            pf.parse_warning.is_none(),
            "should parse cleanly, got {:?}",
            pf.parse_warning
        );
    }

    /// An `.astro` file with no frontmatter at all is legal and must not panic
    /// or invent nodes.
    #[test]
    fn astro_without_frontmatter_is_harmless() {
        let src = "<html>\n  <body>plain markup</body>\n</html>\n";
        let pf = parse(&astro_config(), "p.astro", src).unwrap();
        assert!(pf.raw_calls.is_empty());
        assert!(pf.imports.is_empty());
    }

    /// A CSS selector is not a callable. Typing it as one let stylesheets
    /// dominate a change-risk ranking that exists to surface code.
    #[test]
    fn css_selectors_are_not_functions() {
        let src = ".card { color: var(--brand); }\n\
                   #hero { padding: 0; }\n\
                   :root { --brand: red; }\n";
        let pf = parse(&css_config(), "a.css", src).unwrap();
        assert!(!pf.nodes.is_empty(), "css should still yield nodes");
        for n in &pf.nodes {
            assert_eq!(
                n.kind, "Selector",
                "css def {} must not be typed as a function",
                n.name
            );
        }
    }

    /// Rust is unaffected — the default is still `Function`.
    #[test]
    fn rust_definitions_are_still_functions() {
        let pf = parse(&rust_config(), "m.rs", "fn f() {}\n").unwrap();
        assert_eq!(pf.nodes[0].kind, "Function");
    }

    /// `#[cfg(not(test))]` marks the *opposite* of a test. Substring matching on
    /// "test" would invert it, which is why patterns match whole tokens.
    #[test]
    fn cfg_not_test_is_not_a_test() {
        assert!(attribute_names_test("#[test]", &strs(&["test"])));
        assert!(attribute_names_test("#[tokio::test]", &strs(&["test"])));
        assert!(attribute_names_test("#[ tokio :: test ]", &strs(&["test"])));
        assert!(attribute_names_test("#[cfg(test)]", &strs(&["cfg(test)"])));
        assert!(!attribute_names_test(
            "#[cfg(not(test))]",
            &strs(&["test", "cfg(test)"])
        ));
        assert!(!attribute_names_test("#[derive(Debug)]", &strs(&["test"])));
        // No patterns configured: nothing is a test by attribute.
        assert!(!attribute_names_test("#[test]", &[]));
    }

    /// A `#[cfg(not(test))]` module must not have its contents flagged.
    #[test]
    fn rust_cfg_not_test_module_stays_production() {
        let src = "#[cfg(not(test))]\n\
                   mod prod {\n\
                       fn only_in_release() -> i32 { 1 }\n\
                   }\n";
        let pf = parse(&rust_config(), "src/lib.rs", src).unwrap();
        let n = pf
            .nodes
            .iter()
            .find(|x| x.name == "only_in_release")
            .unwrap();
        assert!(!n.is_test, "#[cfg(not(test))] is not a test marker");
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

#[cfg(test)]
mod type_node_tests {
    use super::*;

    fn type_names(pf: &ParsedFile) -> Vec<(&str, &str)> {
        pf.nodes
            .iter()
            .filter(|n| n.kind == "Type")
            .map(|n| (n.name.as_str(), n.qualified_name.as_str()))
            .collect()
    }

    #[test]
    fn rust_types_include_impl_blocks() {
        let src = "struct Config { a: i32 }\n\
                   enum Mode { On }\n\
                   trait Run { fn go(&self); }\n\
                   impl Config { fn new() -> Self { Config { a: 1 } } }\n";
        let pf = parse(&rust_config(), "c.rs", src).unwrap();
        let names: Vec<&str> = type_names(&pf).iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"Config"), "struct: {names:?}");
        assert!(names.contains(&"Mode"), "enum: {names:?}");
        assert!(names.contains(&"Run"), "trait: {names:?}");
        // `impl Config` is a second node for the same name in one file, so the
        // collision policy suffixes it rather than dropping it.
        assert_eq!(
            names.iter().filter(|n| **n == "Config").count(),
            2,
            "struct and impl both mark where Config's code lives: {names:?}"
        );
        // The method inside the impl is still a Function, attributed normally.
        assert!(pf
            .nodes
            .iter()
            .any(|n| n.name == "new" && n.kind == "Function"));
    }

    #[test]
    fn python_classes_are_types() {
        let pf = parse(
            &python_config(),
            "m.py",
            "class Store:\n    def get(self): pass\n",
        )
        .unwrap();
        assert_eq!(type_names(&pf), vec![("Store", "m.py::Store")]);
        assert!(pf
            .nodes
            .iter()
            .any(|n| n.name == "get" && n.kind == "Function"));
    }

    #[test]
    fn typescript_classes_interfaces_and_aliases() {
        let src = "class Client {}\ninterface Opts {}\ntype Id = string;\n";
        let pf = parse(
            &ts_config_for(&["ts"], tree_sitter_typescript::LANGUAGE_TYPESCRIPT),
            "a.ts",
            src,
        )
        .unwrap();
        let names: Vec<&str> = type_names(&pf).iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["Client", "Opts", "Id"]);
    }

    #[test]
    fn go_type_declarations() {
        let pf = parse(&go_config(), "s.go", "type Server struct { n int }\n").unwrap();
        assert_eq!(type_names(&pf), vec![("Server", "s.go::Server")]);
    }

    fn doc_for<'a>(pf: &'a ParsedFile, name: &str) -> &'a str {
        pf.nodes
            .iter()
            .find(|n| n.name == name)
            .map(|n| n.doc.as_str())
            .unwrap_or("<missing node>")
    }

    /// Almost every documented Rust item carries an attribute between the doc
    /// and the definition, so a walk that stops at the first non-comment sibling
    /// indexes nothing that matters.
    #[test]
    fn a_doc_comment_survives_an_attribute() {
        let src = "/// Retries with exponential backoff.\n\
                   #[derive(Debug)]\n\
                   pub struct Config { a: i32 }\n\
                   \n\
                   /// Parses a descriptor.\n\
                   #[inline]\n\
                   pub fn parse_it() -> i32 { 1 }\n";
        let pf = parse(&rust_config(), "c.rs", src).unwrap();
        assert!(
            doc_for(&pf, "Config").contains("backoff"),
            "struct doc: {:?}",
            doc_for(&pf, "Config")
        );
        assert!(
            doc_for(&pf, "parse_it").contains("descriptor"),
            "fn doc: {:?}",
            doc_for(&pf, "parse_it")
        );
    }

    /// In TypeScript the definition is nested inside the `export` statement, so
    /// the JSDoc is the sibling of the *wrapper*, not of the class.
    #[test]
    fn a_jsdoc_survives_an_export_wrapper() {
        let src = "/** Sends a payload to the queue. */\n\
                   export class Sender { run() { return 1; } }\n\
                   \n\
                   /** Computes a score. */\n\
                   export function score() { return 2; }\n";
        let pf = parse(
            &ts_config_for(&["ts"], tree_sitter_typescript::LANGUAGE_TYPESCRIPT),
            "a.ts",
            src,
        )
        .unwrap();
        assert!(
            doc_for(&pf, "Sender").contains("payload"),
            "class doc: {:?}",
            doc_for(&pf, "Sender")
        );
        assert!(
            doc_for(&pf, "score").contains("Computes"),
            "fn doc: {:?}",
            doc_for(&pf, "score")
        );
    }

    /// The climb out of a wrapper must not reach across unrelated code: a
    /// comment above an enclosing function does not document what is inside it.
    #[test]
    fn a_comment_is_not_stolen_from_an_enclosing_block() {
        let src = "/// Documents the outer function only.\n\
                   fn outer() {\n\
                       fn inner() -> i32 { 1 }\n\
                   }\n";
        let pf = parse(&rust_config(), "n.rs", src).unwrap();
        assert!(doc_for(&pf, "outer").contains("outer function"));
        assert_eq!(doc_for(&pf, "inner"), "", "inner must have no doc");
    }

    /// CSS and HTML have no type system; the query is absent, not empty.
    #[test]
    fn languages_without_types_emit_none() {
        let pf = parse(&css_config(), "a.css", ".card { color: red }\n").unwrap();
        assert!(type_names(&pf).is_empty());
    }
}
