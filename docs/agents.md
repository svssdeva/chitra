# Using it from an AI agent

An AI assistant reviewing a change usually starts by reading files, because
that is the only tool it has. Reading files is expensive and mostly wasted — the
answer to "what does this change affect" lives in a handful of relationships,
not in the thousands of lines those relationships are spread across.

chitra exposes those relationships over MCP, so an assistant can ask the graph
instead. A minimal review context is a few hundred tokens.

## Setup

```sh
chitra build      # the server needs a graph to serve
chitra install
```

`install` writes an MCP server entry for every supported agent it finds:

| Agent | Config file | Scope |
|---|---|---|
| Claude Code | `.mcp.json` | project |
| Cursor | `.cursor/mcp.json` | project |
| VS Code / Copilot | `.vscode/mcp.json` | project |
| Codex | `~/.codex/config.toml` | user |
| Antigravity | `~/.gemini/antigravity/mcp_config.json` | user |
| Windsurf | `~/.codeium/windsurf/mcp_config.json` | user |
| Claude Desktop | platform config directory | user |

Restrict it to one:

```sh
chitra install --platform codex
```

See exactly what would change without changing it:

```sh
chitra install --dry-run
```

Existing servers in those files are never touched, comments in TOML configs
survive, re-running changes nothing, and a config file that fails to parse is
skipped with a message rather than overwritten.

To wire it up by hand instead, the entry is a command:

```json
{
  "mcpServers": {
    "chitra": {
      "type": "stdio",
      "command": "chitra",
      "args": ["serve", "--db", "/absolute/path/to/.chitra/graph.db"]
    }
  }
}
```

The database path must be absolute — an agent starts the server from a directory
you do not control.

## Tools

JSON-RPC 2.0 over stdio, protocol version `2024-11-05`. Seven tools:

| Tool | Use it for |
|---|---|
| `get_minimal_context` | The smallest useful orientation for a repository. Around 250 tokens. |
| `search` | Finding symbols from words. **Start here** when you don't already know an exact symbol. |
| `get_review_context` | Everything needed to review one symbol: neighbourhood, risk, tests. Takes `detail_level`. |
| `detect_changes` | Diff against a base ref, ranked by risk. |
| `impact` | Transitive dependents of a symbol, bounded by depth. |
| `query_graph` | One tool, six patterns: `callers_of`, `callees_of`, `tests_for`, `impact_of`, `community_of`, `file_summary`. |
| `architecture` | Module-level overview. |

`search` matters more than its size suggests. Every other tool takes an exact
`file.rs::Symbol` id, so an assistant holding only a description — "where do we
handle sign-in?" — has no way in without it. It returns qualified names, which
are exactly what the other tools want.

It searches **names and signatures**, never function bodies or comments. On an
`embeddings` build it uses the same hybrid channels as `chitra search --hybrid`
and reports `"mode": "hybrid"`; otherwise `"mode": "fts"`. Either way, it finds
what your code *names*, so it works best when your naming is honest.

Every response carries `_meta.token_estimate` — a cl100k estimate, labelled as
an estimate, so an assistant can budget before it spends.

### Trimming the surface

More tools is not better; each one costs context in the agent's system prompt
whether or not it is called. Expose only what you need:

```sh
chitra serve --tools get_minimal_context,detect_changes,impact
```

## Keeping the graph fresh

The server reads whatever is in the database. If the code moves and the graph
does not, the answers go stale. Either run `chitra watch` alongside your editing
session, or have your agent run `chitra update` before it queries.

## Transport

stdio only. Streamable HTTP is not implemented.
