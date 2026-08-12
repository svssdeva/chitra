# Getting started

## Install

chitra is a single binary with no runtime dependencies. SQLite is compiled in;
there is no Python, no Node, and nothing to `pip install`.

```sh
cargo install --git https://github.com/svssdeva/chitra
```

Or build from a clone:

```sh
git clone https://github.com/svssdeva/chitra
cd chitra
cargo build --release          # target/release/chitra
```

Rust 1.97 or newer. The toolchain is pinned in `rust-toolchain.toml`, so rustup
fetches the right one automatically.

## Build a graph

Run it in the repository you want to graph:

```sh
chitra build
```

```
built 953 files (953 re-parsed) -> 5478 nodes, 45101 edges  (db: .chitra/graph.db)
```

That is the whole setup. The graph lands in `.chitra/graph.db`, a plain SQLite
file. Add `.chitra/` to your `.gitignore`.

chitra reads your `.gitignore`, so build output and vendored copies stay out of
the graph. That is a correctness feature, not a speed one — see
[configuration](configuration.md#controlling-what-gets-indexed).

To graph a repository you are not standing in:

```sh
chitra build ../other-repo --db ../other-repo/.chitra/graph.db
```

## Ask it something

Symbols are addressed as `path/to/file.rs::name`, using forward slashes on every
platform.

**What breaks if I change this?**

```sh
chitra impact "src/parser.rs::parse" --depth 3
```

```
impact of src/parser.rs::parse (depth 3): 12 dependents
  src/build.rs::run
  src/build.rs::collect
  ...
```

**Who calls it, what does it call, what tests cover it?**

```sh
chitra query callers_of "src/parser.rs::parse"
chitra query callees_of "src/parser.rs::parse"
chitra query tests_for  "src/parser.rs::parse"
```

**Find a symbol when you only remember roughly what it was called:**

```sh
chitra search "parse config"
```

**How risky is changing it?**

```sh
chitra risk "src/parser.rs::parse"
```

Risk combines how many things depend on the symbol, whether tests cover it, how
much of its neighbourhood chitra could not resolve confidently, and how central
it is to an entry-point flow.

## Keep it current

`update` re-parses only files whose contents changed, so it is cheap enough to
run on every save:

```sh
chitra update           # or: chitra watch
```

A no-op update is sub-second. On a 310-file repository a real update takes under
two seconds.

## Review a diff

```sh
chitra detect-changes --base origin/main --format md
```

This diffs against a git ref, maps changed lines to symbols, ranks them by risk,
and prints a Markdown report. It is what the bundled GitHub Action posts on pull
requests — see [CLI reference](cli.md#detect-changes).

## Point your AI assistant at it

```sh
chitra install
```

That registers chitra as an MCP server with every supported agent found on your
machine, so your assistant can query the graph directly instead of reading
files. See [using it from an AI agent](agents.md).

## Look at it

```sh
chitra visualize --out graph.html
```

One self-contained HTML file — no CDN, no build step, works offline. See
[visualizing a graph](visualize.md).
