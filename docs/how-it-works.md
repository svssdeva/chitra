# How it works

## The pipeline

```
collect → parse → store → resolve → postprocess
```

**collect** walks the repository, applying `.gitignore`, `.chitraignore`, and a
fixed skip list. Binary and non-UTF-8 files are dropped.

**parse** runs tree-sitter over each file in parallel. Every language is a
`LanguageConfig`: a set of file extensions plus tree-sitter queries for
definitions, call sites, and imports. There is one walker, not one per language,
which is why adding a language usually needs no Rust at all.

Parsing produces definitions and *raw* call sites — a call site at this stage is
just a name and a line number. Nothing is connected yet.

**store** writes each file's rows to SQLite in its own transaction, replacing
that file's previous rows atomically. Each file is hashed with blake3 first, so
`update` re-parses only what actually changed.

**resolve** turns raw call sites into edges. This is where the interesting rules
live — see the confidence model below.

**postprocess** derives what needs the whole graph: signatures, the full-text
index, test links, communities, and flows. Every stage here is non-fatal. A
broken postprocess stage warns and leaves you with a usable graph rather than no
graph.

## The confidence model

A call site names something. Deciding what it names is the entire problem, and
chitra is not a type checker — it will not always know.

Rather than guess and be quietly wrong, every edge carries a tier:

| Tier | Meaning | Used for impact? |
|---|---|---|
| `EXTRACTED` | Proven. The target is defined in the same file, or the file imports the name. | Yes |
| `INFERRED` | Exactly one plausible definition exists anywhere in the repository. | Yes |
| `AMBIGUOUS` | Several definitions match. chitra will not pick one. | **No** |

Three guards sit in front of this. The **single-candidate guard** means a bare
name resolves only when exactly one candidate exists. The
**cross-language-family guard** stops a Python call from resolving to a
same-named Go function. There is exactly one sanctioned cross-language edge —
HTML and CSS, which genuinely are one def/use graph.

The third is the **foreign-type guard**, and on a real codebase it does the most
work. `Box::new()`, `Math.floor()`, `JSON.parse()` are all written through a
type, and it is a type defined outside your repository. Matching them on the
bare name is not merely useless, it is wrong: if the repo happens to contain one
function called `new`, `Box::new()` binds confidently to it. Rust, Go, and
TypeScript all spell types in UpperCamelCase and modules in lower case, so a
capitalised qualifier naming a type chitra never parsed means the definition
isn't here, and the call resolves to nothing.

On a 7,000-file monorepo that one rule removed **24,142 phantom edges** — 60% of
all ambiguity — and took the confidently-resolved share from 10.7% to 23.2%.

### Types are evidence, not targets

chitra parses type declarations — `struct`, `enum`, `trait`, `impl`, `class`,
`interface`, `type` — into `Type` nodes. They are never call targets. They exist
so that the qualifier in `Foo::new()` has something in the graph to match
against: keep only the candidates defined where that type's code lives, and if
exactly one remains, assert it.

Rust needs the `impl` blocks as much as the declaration, because `struct Foo`
and `impl Foo` are frequently in different files and the methods live with the
`impl`.

Being honest about the size of this win: it resolves first-party constructor
calls correctly, but there are fewer of those than you would expect. On that
monorepo only 89 of 989 Rust `new` calls went through a type defined in the
repository at all — the rest were `Box`, `Vec`, `Arc`, `Router`. The value of
type nodes turned out to be mostly in what they let the foreign-type guard
*reject*.

Ambiguous edges are counted and surfaced, never hidden. But they do not drive
impact analysis and they are not drawn in the visualization.

### Why refuse rather than guess

Because the failure modes are not symmetric. A missing edge costs a reviewer one
lookup. An invented edge costs them their trust in every other edge, and they
have no way to tell which kind they are looking at.

This means chitra under-connects, and on some codebases it under-connects a lot.
On a large polyglot monorepo, 77% of edges still come back ambiguous, dominated
by method names shared across dozens of types (`get`, `as_str`) and by CSS class
names repeated across per-component stylesheets. That is the discipline working
as designed, and it is also a real limitation: on that repository, impact
analysis answers less than you would want. See
[known gaps](../README.md#known-gaps).

## Determinism

The same repository at the same commit produces a **byte-identical**
`graph.json` on Linux, macOS, and Windows. CI builds on all three and
byte-compares the exports on every push.

This comes from a handful of unglamorous rules: sorted iteration everywhere,
paths normalized to forward slashes for identity, seeded algorithms including
community detection, and case-insensitive extension matching.

It buys three things. The export is diffable and can be committed. Two people on
the same commit see the same graph, so a disagreement is about the code rather
than about tooling. And because node identity *is* a relative path, the
cross-platform byte comparison turns out to be the sharpest correctness test in
the project — most platform bugs show up there first.

## Incremental updates

`update` hashes every file, re-parses only the changed ones, and re-resolves.
Resolution is global rather than scoped to the change, which is theoretically
wasteful and practically irrelevant: it is sub-second on repositories of this
size, and being correct by construction is worth more than the milliseconds.

A no-op update is sub-second. A real update on a 310-file repository takes
around 1.7 seconds, which is why `watch` polls rather than doing anything
cleverer.

## Storage

One SQLite file, `.chitra/graph.db`, in WAL mode. Nodes and edges are ordinary
tables; impact analysis is a recursive CTE doing a bounded breadth-first search
inside SQLite rather than in application code. Search is FTS5 over four columns:
qualified name, name, signature, and doc comment.

The doc column is the only place prose enters the graph. During parsing, the
comment block immediately above a declaration — or the leading string inside its
body, for Python — is collapsed to a single line and capped at 400 characters.
It is retrieval material, nothing more: docs never create edges and never
influence resolution. Function bodies are not indexed at all, which is what
keeps the index small enough to stay in one file next to the graph.

Schema migrations are forward-only and run on open.

The database is not the artefact you commit — it is binary and not diffable.
`chitra export` produces the committable form.

## Performance

Measured on a polyglot monorepo, 953 indexed files, cold:

| | |
|---|---|
| Full build | 2.5 s → 5,478 nodes, 45,101 edges |
| Incremental update (310-file repo) | 1.7 s |
| No-op update | sub-second |
| Visualization page | 0.14 s |

Single cold runs on one machine, wall clock, not medians.

Against the naive alternative — pasting whole changed files into a prompt — a
14-file diff was 457,386 tokens of source, where the bounded change report was
around 1,600. A minimal review context is roughly 250 tokens.

## Layers

```
chitra-lang    the tree-sitter walker; one LanguageConfig per language
chitra-core    the pipeline and every query
chitra-mcp     JSON-RPC 2.0 over stdio
chitra         the binary
```

`chitra-core` is the product. The CLI, the MCP server, and the GitHub Action are
thin adapters over it — which is what guarantees they give identical answers.
