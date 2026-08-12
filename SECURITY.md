# Security

## Reporting a vulnerability

Report privately through
[GitHub Security Advisories](https://github.com/svssdeva/chitra/security/advisories/new).
Please do not open a public issue for a vulnerability.

Include what you did, what happened, and what you expected. A repository that
reproduces the problem is the most useful thing you can send. Expect a first
reply within a week.

## Threat model

chitra runs locally against source code you have already checked out. It makes
no network requests, sends no telemetry, and needs no API key. The graph and
every artefact it produces stay on your machine.

The interesting question is therefore: **what can a repository do to you when
you run `chitra build` on it?** Cloning an untrusted repository and graphing it
must not execute that repository's code.

### What a scanned repository controls

| Input | Effect | Guard |
|---|---|---|
| Source files | Parsed by tree-sitter. Parsers are data-driven and do not execute the input. | A file that fails to parse is skipped with a warning, never fatal. |
| `.chitraignore` | Excludes paths from the graph. | Cannot include paths outside the scan root. |
| `languages.toml` | Adds a language definition: extensions and tree-sitter queries. | Capped at 20 entries. Built-in extensions cannot be overridden. Queries are compiled at load; a malformed file is skipped with a warning. |
| `languages.toml` with `grammar = "dynamic"` | **Loads a shared library from a path the repository chose, which executes that library's code.** | Double-locked: requires the `dynamic-grammars` build feature *and* `CHITRA_ALLOW_DYNAMIC_GRAMMARS=1` at run time. Both are off by default. |

Runtime grammar loading is the one place chitra will run code that came from
the scanned repository. That is why it takes two deliberate opt-ins rather than
one, and why the default binary cannot do it at all. Do not enable it for
repositories you do not trust.

### What chitra writes

`chitra build` writes only to the database path you give it — `.chitra/graph.db`
by default. It does not modify the source it reads.

`chitra install` edits agent configuration files. It merges a single entry,
leaves every other server untouched, and skips any file it cannot parse rather
than overwriting it. Use `--dry-run` to see exactly what would change.

### Graph contents are not secrets, but they are structure

An exported `graph.json` contains symbol names, file paths, and signatures — no
file bodies. It still describes your codebase's shape. Treat it with the same
care as the source it came from before publishing it.
