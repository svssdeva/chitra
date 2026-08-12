# chitra documentation

chitra turns a repository into a queryable graph of its symbols and the calls
between them, then answers the questions a code reviewer actually asks — what
breaks if I change this, what is risky about this diff, what should I read
first — in tens of tokens instead of tens of thousands.

Everything runs locally. No network, no API key, no model.

## Start here

- **[Getting started](getting-started.md)** — install, build your first graph, ask your first question.
- **[Using it from an AI agent](agents.md)** — the MCP server, one-command setup for Claude Code, Codex, Cursor, and others.

## Reference

- **[CLI reference](cli.md)** — every command and flag.
- **[Configuration](configuration.md)** — controlling what gets indexed, adding languages, optional build features.
- **[Visualizing a graph](visualize.md)** — the self-contained HTML map.

## Understanding it

- **[How it works](how-it-works.md)** — the pipeline, the confidence model, and why the graph is byte-stable.
- **[Design decisions](design-decisions.md)** — the choices that are easy to disagree with, and the reasoning behind them.

## Contributing

See [CONTRIBUTING.md](../CONTRIBUTING.md). The two rules that are stricter here
than elsewhere — deterministic output and evidence-gated edges — are explained
there and in [how it works](how-it-works.md).
