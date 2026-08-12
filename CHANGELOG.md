# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[semantic versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Graph engine.** Parses Rust, Python, TypeScript/JavaScript, Go, HTML, and CSS
  through one config-driven tree-sitter walker into a SQLite graph of symbols,
  calls, imports, and test links.
- **Confidence tiers.** Every edge is `EXTRACTED`, `INFERRED`, or `AMBIGUOUS`.
  Ambiguous edges are surfaced but excluded from impact analysis and from the
  visualization.
- **Incremental updates.** blake3 hash-gated `update`, with per-file atomic
  replacement. A no-op update is sub-second.
- **Deterministic export.** `graph.json` is byte-identical across Linux, macOS,
  and Windows for the same commit, verified in CI.
- **Review commands.** `impact`, `query`, `risk`, `review`, `search`, and
  `detect-changes` — the last mapping a git diff onto the graph and ranking
  changed symbols by risk.
- **Structure commands.** `communities`, `community`, `architecture`, `flows`,
  and `flow`.
- **MCP server.** `chitra serve` speaks JSON-RPC 2.0 over stdio with six tiered
  tools, per-response token estimates, and a `--tools` allowlist.
- **Agent registration.** `chitra install` writes MCP entries for Claude Code,
  Codex, Cursor, Antigravity, Windsurf, VS Code/Copilot, and Claude Desktop,
  leaving other servers in those files untouched. `--dry-run` shows the diff.
- **GitHub Action.** Graphs the repository, diffs against the PR base, posts a
  sticky risk comment, and optionally gates the build.
- **Visualization.** `chitra visualize` emits one self-contained HTML file that
  renders on WebGL2 with a canvas-2D fallback and fetches nothing at view time.
- **Cross-repository graphs.** `chitra federate` joins per-repository graphs into
  one ordinary database with namespaced ids. Cross-repository edges require
  import evidence.
- **Watch and daemon modes.** `chitra watch` for one repository, `chitra daemon`
  for several, with per-root failure isolation.
- **Ignore rules.** `.gitignore`, `.git/info/exclude`, and `.chitraignore` are
  honoured. Machine-local global gitignore is deliberately not consulted.
- **Custom languages.** A `languages.toml` at the repository root adds a language
  with no Rust changes and no rebuild.
- **Optional features**, all off by default: `deep-resolve` (call-site and import
  evidence for ambiguous calls), `embeddings` (hybrid search over full-text,
  character trigrams, and graph context), and `dynamic-grammars` (runtime grammar
  loading, additionally gated behind `CHITRA_ALLOW_DYNAMIC_GRAMMARS=1`).

[Unreleased]: https://github.com/svssdeva/chitra
