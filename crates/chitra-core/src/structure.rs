//! Phase 3 structure: communities + flows + architecture overview.
//!
//! Communities use **deterministic label propagation** (seeded by qualified
//! name, sorted async updates, lexicographic tie-breaks) rather than Leiden —
//! Leiden from scratch is disproportionate, and label-prop is adequate for the
//! architecture overview and the risk-v2 coupling term. This is a deliberate
//! simplification (see the roadmap note); Leiden is a possible future upgrade.
//! Determinism matters: community ids leak into the byte-stable `graph.json`.

use crate::store::{FlowRow, Store};
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

/// Run all structural postprocessing (non-fatal at the call site).
pub fn postprocess(store: &mut Store) -> Result<()> {
    detect_communities(store)?;
    detect_flows(store)?;
    Ok(())
}

/// Undirected adjacency over asserted CALLS edges.
fn undirected_adjacency(store: &Store) -> Result<HashMap<String, Vec<String>>> {
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    for e in store.all_edges()? {
        if e.kind != "CALLS" || e.tier == "AMBIGUOUS" || e.source == e.target {
            continue;
        }
        adj.entry(e.source.clone())
            .or_default()
            .push(e.target.clone());
        adj.entry(e.target.clone())
            .or_default()
            .push(e.source.clone());
    }
    Ok(adj)
}

/// Deterministic label propagation → community assignment. Singleton/isolated
/// nodes get no community (NULL); communities are groups of size ≥ 2.
fn detect_communities(store: &mut Store) -> Result<()> {
    let mut nodes: Vec<String> = store
        .load_nodes()?
        .into_iter()
        .map(|n| n.qualified_name)
        .collect();
    nodes.sort();
    let adj = undirected_adjacency(store)?;

    // init label = own name
    let mut label: HashMap<String, String> = nodes.iter().map(|n| (n.clone(), n.clone())).collect();

    // async LP in sorted order; bounded passes; converges deterministically
    for _ in 0..10 {
        let mut changed = false;
        for n in &nodes {
            let Some(neigh) = adj.get(n) else { continue };
            if neigh.is_empty() {
                continue;
            }
            // tally neighbour labels
            let mut tally: HashMap<&str, usize> = HashMap::new();
            for m in neigh {
                *tally.entry(label[m].as_str()).or_insert(0) += 1;
            }
            // pick max count, tie -> lexicographically smallest label
            let mut best: Vec<(&str, usize)> = tally.into_iter().collect();
            best.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
            let winner = best[0].0.to_string();
            if winner != label[n] {
                label.insert(n.clone(), winner);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // group by final label, keep groups of size >= 2
    let mut groups: HashMap<String, Vec<String>> = HashMap::new();
    for n in &nodes {
        groups.entry(label[n].clone()).or_default().push(n.clone());
    }
    let mut kept: Vec<Vec<String>> = groups
        .into_values()
        .filter(|members| members.len() >= 2)
        .collect();
    for m in &mut kept {
        m.sort();
    }
    // deterministic id remap: size desc, then smallest member name
    kept.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a[0].cmp(&b[0])));

    let mut assignments: Vec<(String, i64)> = Vec::new();
    let mut communities: Vec<(i64, i64, String)> = Vec::new();
    for (id, members) in kept.iter().enumerate() {
        let id = id as i64;
        communities.push((id, members.len() as i64, members[0].clone())); // label = smallest member
        for m in members {
            assignments.push((m.clone(), id));
        }
    }
    store.replace_communities(&assignments, &communities)?;
    Ok(())
}

/// Flows: entry points (no callers, has callees) → forward reachable set.
/// Criticality = flow size normalized to [0,1] by the largest flow.
fn detect_flows(store: &mut Store) -> Result<()> {
    // forward adjacency + fan-in over asserted CALLS
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let mut has_caller: HashSet<String> = HashSet::new();
    for e in store.all_edges()? {
        if e.kind != "CALLS" || e.tier == "AMBIGUOUS" {
            continue;
        }
        out.entry(e.source.clone())
            .or_default()
            .push(e.target.clone());
        has_caller.insert(e.target.clone());
    }
    // entry points: have callees, no callers. Sorted for stable flow ids.
    let mut entries: Vec<String> = out
        .keys()
        .filter(|s| !has_caller.contains(*s))
        .cloned()
        .collect();
    entries.sort();

    let mut raw: Vec<(String, Vec<String>)> = Vec::new(); // (entry, members)
    for entry in &entries {
        // forward BFS, deterministic (sorted frontier)
        let mut seen: HashSet<String> = HashSet::new();
        let mut stack = vec![entry.clone()];
        while let Some(cur) = stack.pop() {
            if !seen.insert(cur.clone()) {
                continue;
            }
            if let Some(next) = out.get(&cur) {
                let mut n = next.clone();
                n.sort();
                n.dedup();
                for t in n {
                    if !seen.contains(&t) {
                        stack.push(t);
                    }
                }
            }
        }
        if seen.len() >= 2 {
            let mut members: Vec<String> = seen.into_iter().collect();
            members.sort();
            raw.push((entry.clone(), members));
        }
    }

    let max_size = raw.iter().map(|(_, m)| m.len()).max().unwrap_or(1) as f64;
    let mut flows: Vec<FlowRow> = Vec::new();
    let mut memberships: Vec<(i64, String)> = Vec::new();
    for (id, (entry, members)) in raw.iter().enumerate() {
        let id = id as i64;
        let size = members.len() as i64;
        flows.push(FlowRow {
            id,
            entry: entry.clone(),
            size,
            criticality: round2(members.len() as f64 / max_size),
        });
        for m in members {
            memberships.push((id, m.clone()));
        }
    }
    store.replace_flows(&flows, &memberships)?;
    Ok(())
}

/// Architecture overview: communities with size, top members (by fan-in), and
/// cross-community coupling (edges leaving the community).
pub fn architecture(store: &Store) -> Result<Value> {
    let comms = store.communities()?;
    let comm_map = store.community_map()?;
    let fan_in = store.fan_in_map()?;

    // external coupling per community: asserted CALLS edges crossing a boundary.
    let mut external: HashMap<i64, i64> = HashMap::new();
    for e in store.all_edges()? {
        if e.kind != "CALLS" || e.tier == "AMBIGUOUS" {
            continue;
        }
        let (cs, ct) = (comm_map.get(&e.source), comm_map.get(&e.target));
        if let (Some(&s), Some(&t)) = (cs, ct) {
            if s != t {
                *external.entry(s).or_insert(0) += 1;
                *external.entry(t).or_insert(0) += 1;
            }
        }
    }

    let mut out = Vec::new();
    for (id, size, label) in &comms {
        let mut members = store.community_members(*id)?;
        members.sort_by(|a, b| {
            fan_in
                .get(b)
                .unwrap_or(&0)
                .cmp(fan_in.get(a).unwrap_or(&0))
                .then_with(|| a.cmp(b))
        });
        members.truncate(3);
        out.push(json!({
            "id": id,
            "size": size,
            "label": label,
            "external_coupling": external.get(id).copied().unwrap_or(0),
            "top_members": members,
        }));
    }
    Ok(json!({
        "communities": out,
        "summary": { "communities": comms.len(), "clustered_nodes": comm_map.len() },
    }))
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn build(tag: &str, files: &[(&str, &str)]) -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("chitra_struct_{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, src) in files {
            std::fs::write(dir.join(name), src).unwrap();
        }
        let mut store = Store::open_in_memory().unwrap();
        crate::build(&mut store, Path::new(&dir)).unwrap();
        (store, dir)
    }

    #[test]
    fn two_clusters_become_two_communities() {
        // cluster A: a1<->a2<->a3 ; cluster B: b1<->b2<->b3 ; no A-B edges.
        let (store, _d) = build(
            "clusters",
            &[(
                "m.rs",
                "fn a1() { a2(); }\nfn a2() { a3(); a1(); }\nfn a3() { a2(); }\n\
                 fn b1() { b2(); }\nfn b2() { b3(); b1(); }\nfn b3() { b2(); }\n",
            )],
        );
        let comms = store.communities().unwrap();
        assert_eq!(comms.len(), 2, "expected two communities, got {comms:?}");
        assert_eq!(comms[0].1, 3); // each has 3 members
                                   // determinism: a second build yields identical community ids/labels.
        let (store2, _d2) = build(
            "clusters2",
            &[(
                "m.rs",
                "fn a1() { a2(); }\nfn a2() { a3(); a1(); }\nfn a3() { a2(); }\n\
                 fn b1() { b2(); }\nfn b2() { b3(); b1(); }\nfn b3() { b2(); }\n",
            )],
        );
        assert_eq!(store.communities().unwrap(), store2.communities().unwrap());
    }

    #[test]
    fn flow_from_entry_point_covers_the_chain() {
        // main -> step1 -> step2 (main is the only entry: nothing calls it)
        let (store, _d) = build(
            "flow",
            &[(
                "m.rs",
                "fn step2() {}\nfn step1() { step2(); }\nfn main() { step1(); }\n",
            )],
        );
        let flows = store.flows().unwrap();
        assert_eq!(flows.len(), 1, "one entry point -> one flow: {flows:?}");
        assert_eq!(flows[0].entry, "m.rs::main");
        assert_eq!(flows[0].size, 3); // main, step1, step2
        assert!((flows[0].criticality - 1.0).abs() < 1e-9);
        let members = store.flow_members(flows[0].id).unwrap();
        assert!(members.contains(&"m.rs::step2".to_string()));
    }

    #[test]
    fn architecture_reports_coupling() {
        // two clusters joined by a single cross edge a1 -> b1
        let (store, _d) = build(
            "arch",
            &[(
                "m.rs",
                "fn a1() { a2(); b1(); }\nfn a2() { a1(); }\n\
                 fn b1() { b2(); }\nfn b2() { b1(); }\n",
            )],
        );
        let arch = architecture(&store).unwrap();
        let comms = arch["communities"].as_array().unwrap();
        assert_eq!(comms.len(), 2);
        // the cross edge shows up as external coupling on both.
        assert!(comms
            .iter()
            .all(|c| c["external_coupling"].as_i64().unwrap() >= 1));
    }
}
