# chitra

[![CI](https://github.com/svssdeva/chitra/actions/workflows/ci.yml/badge.svg)](https://github.com/svssdeva/chitra/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.97%2B-orange.svg)](rust-toolchain.toml)
[![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20macOS%20%7C%20Windows-lightgrey.svg)](#platform-support)

**Tell your AI reviewer exactly which functions a change affects, how risky they
are, and why — in hundreds of tokens instead of hundreds of thousands.**

chitra parses your repository into a graph of symbols and the calls between
them, then answers the questions a reviewer actually asks. It runs locally as a
single binary: no network, no API key, no model, nothing to install alongside
it.

```sh
chitra build
chitra impact "src/parser.rs::parse"
```

```
impact of src/parser.rs::parse (depth 3): 12 dependents
  src/build.rs::run
  src/build.rs::collect
  ...
```

## Why

An AI assistant asked to review a change starts by reading files, because that
is the only tool it has. Most of that reading is wasted — the answer to "what
does this break" lives in a handful of relationships, not in the thousands of
lines those relationships are spread across.

On a recent 14-file diff, pasting the changed files whole came to **457,386
tokens**. The same change as a risk-ranked report from the graph was **about
1,600**. Orientation for a whole repository is roughly **250**.

## Install

```sh
cargo install --git https://github.com/svssdeva/chitra
```

Rust 1.97+. SQLite is compiled in. That is the entire dependency list.

## Use it

```sh
chitra build                                   # graph the repo -> .chitra/graph.db
chitra update                                  # re-parse only what changed

chitra impact "src/api.rs::handler"            # what breaks if I change this
chitra query callers_of "src/api.rs::handler"  # also: callees_of, tests_for
chitra risk "src/api.rs::handler"              # how risky is changing it
chitra search "parse config"                   # find it by name or signature

chitra detect-changes --base origin/main --format md   # review a diff
chitra visualize --out graph.html              # look at the whole thing
chitra install                                 # register with your AI agents
```

Full reference: **[docs/cli.md](docs/cli.md)**.

## From your AI assistant

```sh
chitra install
```

That registers chitra as an MCP server with every supported agent on your
machine — Claude Code, Codex, Cursor, Antigravity, Windsurf, VS Code/Copilot,
Claude Desktop. Existing servers in those config files are left alone; `--dry-run`
shows you exactly what would change.

Your assistant then gets seven tools: `search` (find a symbol from words),
`get_minimal_context` (orient in a new repository), `detect_changes` (review a
diff by risk), `impact` (blast radius), and three more. Every response is
stamped with a token estimate so the assistant can budget before it spends.

See **[docs/agents.md](docs/agents.md)**.

## On pull requests

The bundled GitHub Action graphs the repository, diffs against the PR base, and
posts a sticky risk comment.

```yaml
- uses: svssdeva/chitra@v0
  with:
    fail-on-risk: false   # set true to gate the build on high-risk changes
```

## What it understands

Rust, Python, TypeScript/JavaScript, Go, HTML, and CSS — one config-driven
tree-sitter walker, functions and methods as nodes, call sites and imports as
edges.

HTML and CSS are a real part of the graph rather than a checkbox: CSS selectors
and custom properties are definitions, `class="..."` and `var(--x)` are uses. So
`impact` answers "which pages break if I change this rule" and "is this CSS
dead".

Adding a language usually needs **no Rust and no rebuild** — a `languages.toml`
entry with two tree-sitter queries. See
**[docs/configuration.md](docs/configuration.md)**.

## The thing that makes it trustworthy

Every edge carries a confidence tier. `EXTRACTED` is proven by a same-file
definition or an import. `INFERRED` means exactly one plausible candidate exists.
`AMBIGUOUS` means several do — and chitra refuses to pick one.

Ambiguous edges are counted and surfaced, but they never drive impact analysis
and are never drawn in the visualization.

This means chitra under-connects rather than over-connects, sometimes
significantly. That is deliberate. A missing edge costs a reviewer one lookup;
an invented edge costs them their trust in every other edge, and they have no way
to tell the two apart.

The same discipline shows up as byte-identical output: the same commit exports
the same `graph.json` on Linux, macOS, and Windows, verified in CI on every push.

## Documentation

| | |
|---|---|
| [Getting started](docs/getting-started.md) | Install, first graph, first query |
| [CLI reference](docs/cli.md) | Every command and flag |
| [Using it from an AI agent](docs/agents.md) | MCP setup and tools |
| [Configuration](docs/configuration.md) | Ignore rules, adding languages, optional features |
| [Visualizing a graph](docs/visualize.md) | The offline HTML map |
| [How it works](docs/how-it-works.md) | Pipeline, confidence model, determinism |
| [Design decisions](docs/design-decisions.md) | The choices worth arguing with |

## Optional features

Off by default, so the binary you install stays lean.

```sh
cargo build --features deep-resolve      # break ties on ambiguous calls using call-site evidence
cargo build --features embeddings        # hybrid search: full-text + typo tolerance + graph context
cargo build --features dynamic-grammars  # load a grammar that is not compiled in
```

Each has a real cost as well as a benefit — [docs/configuration.md](docs/configuration.md#optional-build-features)
covers both, including the case where `deep-resolve` does nothing at all.

## Platform support

Linux, macOS, and Windows, from one codebase with no platform-specific
constructs. Every push runs fmt, clippy, and both test suites on all three, then
builds the graph on each and byte-compares the three `graph.json` exports.

That last job is the interesting one. Node identity is a relative path, so it is
the most likely thing to diverge between platforms — which makes the comparison
the sharpest correctness test in the project rather than a formality.

## Known gaps

Stated plainly, because you will hit some of these.

- **Method calls on values are unresolved.** `x.parse()` carries no evidence
  about what `x` is, and chitra does not infer types. Calls written through a
  type (`Foo::new()`, `Foo.parse()`) resolve; calls through a variable do not.
- **No framework awareness.** Event publishers, HTTP route handlers, and dependency
  injection are invisible — chitra sees calls, not conventions.
- **MCP transport is stdio only.** No streamable HTTP.
- **Largest repository measured is 7,000 files.** It handles that comfortably;
  beyond it is untested.
- **Federation is not manifest-aware.** It joins graphs you point it at rather
  than reading your workspace or lockfile.

> [!NOTE]
> chitra is a call-graph engine for code review. It does not index documents,
> images, or prose, and it will not answer questions about your codebase in
> natural language. It tells you what depends on what, deterministically and
> fast.

## Name

**चित्र** — *image, map, picture.* From *Chitragupta*, who in Hindu tradition
keeps the complete ledger of every deed and weighs each one: a fitting namesake
for something that holds the structural map of a codebase and judges the risk of
each change.

The code is Apache-2.0; the name is a trademark. Forking is welcome — see
[TRADEMARK.md](TRADEMARK.md) for the short list of things that need a different
name, and the longer list of things that need no permission at all.
