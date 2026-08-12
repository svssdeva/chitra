# Contributing

Thanks for looking. This file covers what you need to run the code, the two
rules that are stricter here than in most repositories, and the shortest paths
to a useful first change.

## Setup

You need Rust. The toolchain is pinned in `rust-toolchain.toml`, so rustup
installs the right one on first build — there is nothing else to install. SQLite
is compiled in.

```sh
git clone <your-fork>
cd chitra
cargo test --workspace          # what a user gets from `cargo install`
```

Before opening a pull request, run what CI runs:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
cargo test --workspace --all-features --locked
```

Warnings are denied. Both feature sets are gated, because the optional features
add to the default build rather than replacing it — a change that only compiles
with `--all-features` is a broken default build.

## The two hard rules

### 1. Output must be deterministic

The same repository at the same commit must produce a byte-identical
`graph.json` on Linux, macOS, and Windows. CI builds the graph on all three and
byte-compares the exports. Node identity is a relative path, which makes this
the sharpest test of cross-platform correctness in the project — and the easiest
thing to break by accident.

In practice:

- Iterate over sorted collections. Never over a `HashMap` in a way that reaches
  the output.
- Normalize paths to forward slashes for identity. Do not use the platform
  separator in an id.
- Seed anything that could vary between runs. Community detection is
  deterministic by construction for this reason.
- Compare file extensions case-insensitively.

If you add a stage that produces output, add it to the export in sorted order.

### 2. Confidence is not optional

Every edge carries a tier: `EXTRACTED` (proven by a same-file definition or an
import), `INFERRED` (a single plausible candidate), or `AMBIGUOUS` (several
candidates, so chitra refuses to pick). Ambiguous edges are surfaced in counts
but excluded from impact analysis and never drawn in the visualization.

Do not add a resolver that guesses. A change that raises recall by lowering the
evidence bar will be measured against the gold edge set in `chitra-core`, and a
precision regression is a rejection. Under-connecting is the intended failure
mode: a missing edge costs a reviewer a lookup, an invented edge costs them
trust in every other edge.

Postprocessing stages are non-fatal by the same logic — a broken stage warns and
the build still produces a usable graph.

## Layout

```
crates/chitra-lang   config-driven tree-sitter walker; one LanguageConfig per language
crates/chitra-core   collect → parse → store → resolve → postprocess, plus every query
crates/chitra-mcp    JSON-RPC 2.0 server over stdio; thin adapter over core
src/main.rs          the `chitra` binary; argument parsing and printing, no logic
crates/toy-grammar   test fixture only — a cdylib for the runtime grammar loader
tests/fixture/       a small six-language corpus used by the determinism check
```

The core library is the product. The CLI, the MCP server, and the GitHub Action
are thin adapters over it, which is what guarantees they return the same answers.
Logic that lands in `main.rs` instead of `chitra-core` will be asked to move.

## Good first changes

**Add a language.** Most languages need no Rust at all — a `languages.toml`
entry with two tree-sitter queries is enough. See
[docs/configuration.md](docs/configuration.md). If you want it compiled in
instead, add the grammar crate to the workspace and a `LanguageConfig` in
`chitra-lang`, and extend `tests/fixture/` so the determinism check covers it.

**Improve a query.** Language support is only as good as its tree-sitter
queries. Missed definitions and missed call sites are concrete, testable bugs.

**Open items.** The larger known gaps are listed at the end of the
[README](README.md#known-gaps) — Class and Type nodes are the highest-value one,
because constructor calls (`Foo::new()`) currently resolve to nothing on large
codebases.

## Tests

Tests live next to the code they test, in `mod tests`. Two conventions matter:

- **Give every fixture its own directory.** Tests in one binary run in parallel;
  a shared temp directory means one test's cleanup lands inside another test's
  setup. This has caused three separate flaky tests here. Take a tag parameter
  and use it in the path.
- **Assert on measured behaviour, not on shape.** A test that a resolver returns
  *some* edges is worth little. A test that it returns exactly these edges at
  exactly these tiers is worth a lot.

## Pull requests

Describe what changed and how you checked it. If you changed resolution, say
what happened to precision and recall on the gold set. If you changed output
format, say why the change is worth breaking byte-compatibility for.

Small and focused merges faster than large and comprehensive.

## Licensing of contributions

chitra is [Apache-2.0](LICENSE). Under section 5 of that license, anything you
deliberately submit for inclusion is contributed under the same terms unless you
say otherwise in writing. There is no CLA to sign.

You keep the copyright on what you write. Adding yourself to `NOTICE` is
reasonable for a substantial contribution — ask in the pull request.

The **chitra name** is a trademark and is not covered by the license. Forking is
welcome; naming your fork chitra is not. See [TRADEMARK.md](TRADEMARK.md), which
also lists the things you explicitly do not need permission for.
