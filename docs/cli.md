# CLI reference

Every command accepts `--db <path>`, defaulting to `.chitra/graph.db`. Symbols
are written `path/to/file.ext::name` with forward slashes on every platform.

## Building the graph

### `chitra build [dir]`

Parse a repository and write the graph. `dir` defaults to the current directory.

```sh
chitra build
chitra build ../service --db ../service/.chitra/graph.db
```

Honours `.gitignore`, `.git/info/exclude`, and `.chitraignore`. Binary and
non-UTF-8 files are skipped.

### `chitra update [dir]`

Re-parse only files whose contents changed since the last build, then re-resolve.
Safe to run constantly — an update with nothing to do is sub-second.

### `chitra watch [dir] [--interval <ms>]`

Run `update` in a loop. Default interval 1000 ms.

### `chitra daemon --roots <a,b,c> [--interval <ms>]`

Watch several repositories at once. Each keeps its own database inside itself. A
root that fails is isolated, retried on the next tick, and reported once rather
than every tick.

## Asking questions

### `chitra impact <symbol> [--depth <n>]`

Everything that transitively depends on the symbol, up to `--depth` hops
(default 3). This is the blast radius of a change.

Ambiguous edges are excluded, so the answer is a set chitra can defend rather
than everything that might conceivably be related.

### `chitra query <pattern> <symbol>`

| Pattern | Returns |
|---|---|
| `callers_of` | Direct callers |
| `callees_of` | What it calls directly |
| `tests_for` | Tests linked to it |

### `chitra search <query> [--limit <n>] [--hybrid]`

Full-text search over symbol names and signatures. Default limit 20.

`--hybrid` additionally fuses two local channels — character trigrams for typos,
and graph-context expansion so a symbol can be found by the names of its
neighbours. Requires the `embeddings` build feature; see
[configuration](configuration.md#optional-build-features).

### `chitra risk <symbol>`

Change risk on a 0–1 scale, with the contributing terms broken out: fan-in,
test coverage gaps, unresolved neighbourhood, flow criticality, and
cross-module coupling.

### `chitra review <symbol> [--detail minimal|standard]`

A bounded review context as JSON — the symbol, its immediate graph
neighbourhood, its risk, and its tests. `minimal` is the tight version meant for
a token budget; `standard` adds signatures and more neighbours.

## Reviewing a diff

### `chitra detect-changes`

Map a git diff onto the graph and rank the changed symbols by risk.

| Flag | Default | Meaning |
|---|---|---|
| `--base <ref>` | `HEAD` | Git ref to diff against |
| `--root <dir>` | `.` | Repository root |
| `--limit <n>` | `20` | Keep the top *n* by risk; the report carries a `truncated` flag |
| `--format md` | JSON | Markdown instead of JSON |
| `--fail-on-risk` | off | Exit 1 if any high-risk symbol changed |

```sh
chitra detect-changes --base origin/main --format md
chitra detect-changes --base origin/main --fail-on-risk    # CI gate
```

The bound matters. An unbounded dump of everything a diff touches is larger than
the diff itself; the top-N-by-risk report is the point.

## Structure

### `chitra communities` / `chitra community <id>`

Detected modules — clusters of symbols that call each other more than they call
anything else — and their members. Ids are stable across runs.

### `chitra architecture`

An overview: module sizes, their most-depended-upon members, and the coupling
between them.

### `chitra flows` / `chitra flow <id>`

Execution flows traced forward from entry points, with a criticality score.

## Output

### `chitra export [--out <file>]`

Deterministic node-link JSON. Prints to stdout without `--out`.

The same repository at the same commit exports byte-identical JSON on Linux,
macOS, and Windows, which makes the file diffable and safe to commit.

### `chitra visualize [--out <file>] [--mode community|full] [--max-nodes <n>]`

A self-contained interactive HTML map. Defaults: `graph.html`, `community`
mode, 20,000 nodes. See [visualizing a graph](visualize.md).

### `chitra estimate <file>...`

Sum the cl100k token estimate over whole files. Useful for measuring what
reading files costs versus what querying the graph costs.

## Serving and integrating

### `chitra serve [--tools <a,b,c>] [--root <dir>]`

Run the MCP server over stdio. `--tools` restricts the exposed surface. See
[using it from an AI agent](agents.md).

### `chitra install [--platform <id>] [--root <dir>] [--dry-run]`

Register the MCP server with your coding agents. Without `--platform` it
configures every supported agent found on the machine.

### `chitra federate --repos <name=path,...> [--out <file>]`

Join several graphs into one database with namespaced ids. Each path is either a
repository directory or a `graph.db`.

```sh
chitra federate --repos lib=../lib,svc=../svc --out combined.db
chitra impact "lib/crypto.py::encrypt" --db combined.db
```

The result is an ordinary graph database — every command above works on it,
including the MCP server. Cross-repository edges are only created where the
calling file actually imported the name.
