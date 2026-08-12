//! Deterministic `graph.json` export (node-link shape, graphify-compatible).
//!
//! Byte-stable across platforms: nodes/edges are read back sorted, keys are
//! emitted in a fixed order, floats are formatted fixed-width. The SQLite
//! `graph.db` is git-ignored; this JSON is the shareable, diffable artifact.

use crate::store::Store;
use anyhow::Result;
use serde_json::{json, Value};

/// Serialize the graph to a stable JSON string (sorted, fixed key order).
pub fn export_json(store: &Store) -> Result<String> {
    let nodes = store.load_nodes()?; // already ORDER BY qualified_name
    let edges = store.all_edges()?; // already ORDER BY source,target,kind,line
    let community = store.community_map()?; // deterministic ids → byte-stable

    let node_vals: Vec<Value> = nodes
        .iter()
        .map(|n| {
            json!({
                "id": n.qualified_name,
                "kind": n.kind,
                "language": n.language,
                "file": n.file,
                "line": n.line_start,
                "signature": n.signature,
                "is_test": n.is_test,
                "community": community.get(&n.qualified_name).copied(),
            })
        })
        .collect();

    let edge_vals: Vec<Value> = edges
        .iter()
        .map(|e| {
            json!({
                "source": e.source,
                "target": e.target,
                "kind": e.kind,
                "line": e.line,
                "confidence": round2(e.confidence),
                "tier": e.tier,
            })
        })
        .collect();

    let commit = store.get_meta("built_at_commit")?.unwrap_or_default();
    let doc = json!({
        "directed": true,
        "schema_version": crate::store::SCHEMA_VERSION,
        "built_at_commit": if commit.is_empty() { Value::Null } else { Value::String(commit) },
        "nodes": node_vals,
        "edges": edge_vals,
    });

    // Pretty-print for git-diffability; serde_json preserves our insertion order
    // and sorts nothing else, so identical inputs => identical bytes.
    Ok(serde_json::to_string_pretty(&doc)?)
}

/// Fixed 2-dp float so 0.6 vs 0.6000001 never diverge across platforms.
fn round2(x: f64) -> Value {
    let s = format!("{x:.2}");
    json!(s.parse::<f64>().unwrap_or(x))
}
