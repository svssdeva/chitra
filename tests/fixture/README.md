Multi-language corpus for the cross-OS determinism gate (RISK-4).

Every language chitra parses appears here, in nested directories, so the
byte-identical `graph.json` comparison across Linux/macOS/Windows actually
exercises each language's node-identity path — not just Rust. Path identities
are the thing most likely to diverge between platforms, so they are the thing
the gate must cover.

Keep it small and keep it valid: a syntax error here would trip the
partial-parse warning on every CI run.
