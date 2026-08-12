# Configuration

There is no config file to write to get started. Everything on this page is
optional.

## Controlling what gets indexed

chitra honours **`.gitignore`** and `.git/info/exclude`, plus an optional
**`.chitraignore`** with identical syntax — globs, anchoring, and `!` to
whitelist something back in.

```gitignore
# .chitraignore
*.generated.ts
vendor/
!vendor/keep-this.ts     # index this one anyway
fixtures/**/snapshot_*
```

`.chitraignore` is for the things you want in git but out of the graph:
generated clients, vendored third-party source, large fixture corpora.

These directories are skipped whether or not anything ignores them:

```
target  build  dist  node_modules  .venv  venv  __pycache__
.git  .chitra  .mypy_cache  .pytest_cache
```

Your **global** gitignore is deliberately not consulted. Graph contents must not
depend on machine-local configuration, or two people on the same commit get
different graphs.

### Why this is a correctness feature

It is tempting to read this as a speed optimization. It is not.

On a real monorepo chitra indexed 2,042 files where git tracked 954. The excess
was build output and a `.worktrees/` directory holding git worktree checkouts of
*the same source*. Every symbol therefore existed twice, which made its call
sites ambiguous — and ambiguous edges are excluded from impact analysis by
design. A blind filesystem walk quietly degrades the answers rather than merely
slowing things down.

Honouring `.gitignore` on that repository took confidently-resolved edges from
4.0% to 10.7%, and the build from 9.5s to 2.5s. The correctness gain was the
point; the speed was a bonus.

## Adding a language

Drop a `languages.toml` at the repository root. No Rust changes, no rebuild.

### A dialect of a grammar that is already built in

```toml
[[language]]
name = "starlark"
extensions = ["bzl"]
grammar = "python"        # rust | python | go | typescript | tsx | css | html
function_query = "(function_definition name: (identifier) @name) @def"
call_query = "(call function: (identifier) @callee)"
```

`function_query` must capture `@name` and `@def`. `call_query` must capture
`@callee`. These are ordinary tree-sitter queries.

Optional keys:

| Key | Effect |
|---|---|
| `import_query` | Captures `@import`, optionally `@module`. Import evidence is what promotes an edge to a confident tier. |
| `test_prefixes` | Name prefixes that mark a test, for test linking. |
| `callee_separator` | Split one captured callee into several on this character. |
| `resolve_languages` | Which languages this one may resolve calls into. |
| `merge_duplicate_defs` | Treat repeated definitions of a name as one node. |
| `emit_file_node` | Emit a node for the file itself. |

Queries are compiled when the file loads, so a typo warns once rather than once
per file. A malformed file is skipped with a warning and never fails a build.
Built-in extensions cannot be overridden, and the cap is 20 entries.

### A language whose grammar is not compiled in

```toml
[[language]]
name = "zig"
extensions = ["zig"]
grammar = "dynamic"
grammar_library = "/usr/local/lib/libtree-sitter-zig.so"
# grammar_symbol defaults to tree_sitter_<name>
function_query = "..."
call_query = "..."
```

This needs the `dynamic-grammars` build feature **and**
`CHITRA_ALLOW_DYNAMIC_GRAMMARS=1` at run time. Both default to off, and that is
not caution theatre: this file and the library it names live inside the
repository being scanned, and loading a shared library runs its code. Cloning a
repository and running `chitra build` must never be an execution vector. See
[Security](../SECURITY.md).

## Optional build features

All three are off by default, so `cargo install` gives you a lean binary.

```sh
cargo build --features deep-resolve
cargo build --features embeddings
cargo build --features dynamic-grammars
```

### `deep-resolve`

Breaks ties on ambiguous calls using two kinds of evidence: the path a call was
written through (`math::add()`, `store.Fetch()`, `mod.parse()`) and the module a
name was imported from, each matched against a candidate's file stem *or* its
parent directory — so Go packages and Rust `mod.rs` modules resolve, not just
languages with named symbol imports.

On the gold edge set this takes recall from 0.625 to 1.000 with precision held
at 1.000. On a 310-file Python repository it converts about 4% of ambiguous
edges into confident ones.

Stated honestly: **the gain is repository-dependent, and can be zero.** On
chitra's own Rust source it changes nothing, because that code rarely
path-qualifies a call whose bare name is ambiguous. It is a feature flag rather
than a default because it does not always pay.

### `embeddings`

Adds `search --hybrid`, fusing three channels by reciprocal rank: full-text
search, hashed character trigrams (which absorb typos), and **graph context** —
documents expanded with the names of a symbol's neighbours, so `"sign in"` can
find `authenticate_user`.

There is no neural model and no network call. The limit is real and worth
knowing before you enable it: the semantics available are *this repository's*,
not English's. It finds paraphrases your code already implies.

### `dynamic-grammars`

Runtime grammar loading, described above. Requires a second, runtime opt-in.
