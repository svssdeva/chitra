//! `chitra visualize` — a self-contained HTML map of the graph.
//!
//! Three decisions shape this, each taken from a problem an upstream already
//! hit:
//!
//! 1. **Nothing is fetched at view time.** No CDN, no external script. A tool
//!    that sells "local-first, no network" cannot require the internet to look
//!    at its own output (code-review-graph shipped a bug for exactly this and
//!    had to vendor D3).
//! 2. **The layout is computed here, in Rust, deterministically.** The browser
//!    only draws. That keeps the page trivial, makes the output byte-stable like
//!    every other artefact chitra emits, and means a big graph does not melt a
//!    tab running a force simulation.
//! 3. **Communities are the default view, not the whole graph.** graphify's own
//!    flag documentation warns viz is unusable past ~5,000 nodes — and a real
//!    monorepo graph here was 5,478 nodes and 45,101 edges. Rendering all of it
//!    at once is not a performance problem to solve, it is a legibility problem:
//!    a 45,000-edge hairball is unreadable at any frame rate. So the top level is
//!    the ~450-community architecture map, and detail appears on zoom.

use crate::store::Store;
use anyhow::Result;
use std::collections::HashMap;
use std::f64::consts::PI;

/// What to draw.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Communities as discs, members placed inside them. The readable default.
    Community,
    /// Every node, laid out by community. Honest about being a hairball.
    Full,
}

impl Mode {
    pub fn parse(s: &str) -> Mode {
        match s {
            "full" => Mode::Full,
            _ => Mode::Community,
        }
    }
}

struct Placed {
    id: String,
    x: f64,
    y: f64,
    module: u16,
    /// Fan-in: how many asserted callers this symbol has. Drawn as magnitude,
    /// so the brightest points on the plate are the most depended-upon code.
    magnitude: i64,
    language: String,
}

/// Walk a region's members breadth-first from its most-depended-upon symbol, so
/// consecutive slots are usually neighbours. Ties break on the qualified name,
/// which keeps the whole layout deterministic.
fn connected_order(
    group: &[usize],
    adj: &HashMap<usize, Vec<usize>>,
    fan_in: &HashMap<String, i64>,
    nodes: &[chitra_lang::Node],
) -> Vec<usize> {
    let mut remaining: Vec<usize> = group.to_vec();
    // Highest fan-in first: hubs land at the centre of the disc, where the
    // sunflower's first slots are.
    remaining.sort_by(|a, b| {
        let (fa, fb) = (
            fan_in.get(&nodes[*a].qualified_name).copied().unwrap_or(0),
            fan_in.get(&nodes[*b].qualified_name).copied().unwrap_or(0),
        );
        fb.cmp(&fa)
            .then_with(|| nodes[*a].qualified_name.cmp(&nodes[*b].qualified_name))
    });

    let in_group: std::collections::HashSet<usize> = group.iter().copied().collect();
    let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut order = Vec::with_capacity(group.len());
    for seed in remaining {
        if seen.contains(&seed) {
            continue;
        }
        let mut queue = std::collections::VecDeque::from([seed]);
        seen.insert(seed);
        while let Some(cur) = queue.pop_front() {
            order.push(cur);
            if let Some(ns) = adj.get(&cur) {
                for n in ns {
                    if in_group.contains(n) && seen.insert(*n) {
                        queue.push_back(*n);
                    }
                }
            }
        }
    }
    order
}

/// Sectors larger than this keep their seeded positions. The relaxation is
/// O(n^2) per pass, fine at the sizes real modules reach and a problem only if
/// one ever got pathological.
const RELAX_MAX: usize = 4000;

/// Fruchterman–Reingold relaxation, confined to one module's wedge.
///
/// Ordering alone cannot do this job: no 1-D arrangement puts a hub next to all
/// fifty of its callers. Seeded from the analytic placement and run without any
/// randomness, so the same graph always yields the same console.
fn relax_wedge(
    points: &mut [(f64, f64)],
    edges: &[(usize, usize)],
    a0: f64,
    a1: f64,
    r_in: f64,
    r_out: f64,
) {
    let n = points.len();
    if !(3..=RELAX_MAX).contains(&n) {
        return;
    }
    // Ideal separation for n points spread over this wedge's area.
    let area = 0.5 * (a1 - a0).abs() * (r_out * r_out - r_in * r_in);
    let k = 0.9 * (area / n as f64).sqrt();
    let mut temp = (r_out - r_in) * 0.22;

    let mut disp = vec![(0.0, 0.0); n];
    for _ in 0..70 {
        for d in disp.iter_mut() {
            *d = (0.0, 0.0);
        }
        for i in 0..n {
            for j in (i + 1)..n {
                let (dx, dy) = (points[i].0 - points[j].0, points[i].1 - points[j].1);
                let dist = (dx * dx + dy * dy).sqrt().max(0.01);
                let f = k * k / dist;
                let (ux, uy) = (dx / dist * f, dy / dist * f);
                disp[i].0 += ux;
                disp[i].1 += uy;
                disp[j].0 -= ux;
                disp[j].1 -= uy;
            }
        }
        for (a, b) in edges {
            let (dx, dy) = (points[*a].0 - points[*b].0, points[*a].1 - points[*b].1);
            let dist = (dx * dx + dy * dy).sqrt().max(0.01);
            let f = dist * dist / k;
            let (ux, uy) = (dx / dist * f, dy / dist * f);
            disp[*a].0 -= ux;
            disp[*a].1 -= uy;
            disp[*b].0 += ux;
            disp[*b].1 += uy;
        }
        for i in 0..n {
            let d = (disp[i].0 * disp[i].0 + disp[i].1 * disp[i].1)
                .sqrt()
                .max(1e-9);
            let step = d.min(temp);
            points[i].0 += disp[i].0 / d * step;
            points[i].1 += disp[i].1 / d * step;
            // Stay inside the wedge: a symbol must never drift into another
            // module's sector, or the picture lies about where code lives.
            let (x, y) = points[i];
            let r = (x * x + y * y).sqrt().clamp(r_in, r_out);
            // Unwrap around the wedge's *middle*, not its start: a big module
            // spans more than PI radians, and an a0-centred window then throws
            // legitimate interior angles outside and clamps them to the edge.
            let mid = (a0 + a1) / 2.0;
            let mut a = y.atan2(x);
            while a < mid - PI {
                a += 2.0 * PI;
            }
            while a > mid + PI {
                a -= 2.0 * PI;
            }
            let a = a.clamp(a0.min(a1), a0.max(a1));
            points[i] = (r * a.cos(), r * a.sin());
        }
        temp *= 0.93;
    }
}

/// Top-level path segment: `frontend/projects/admin/x.ts::foo` -> `frontend`.
/// A monorepo's real architecture is its directory tree, and unlike an
/// auto-detected community it comes with a name a human already recognises.
fn module_of(qualified: &str) -> &str {
    let file = qualified.split("::").next().unwrap_or(qualified);
    match file.split_once('/') {
        Some((head, _)) if !head.is_empty() => head,
        _ => "(root)",
    }
}

/// Build the page. Returns HTML with the graph embedded.
pub fn visualize_html(store: &Store, mode: Mode, max_nodes: usize) -> Result<String> {
    let nodes = store.load_nodes()?; // sorted by qualified_name
    let fan_in = store.fan_in_map()?;

    let keep: HashMap<&str, usize> = nodes
        .iter()
        .take(max_nodes)
        .enumerate()
        .map(|(i, n)| (n.qualified_name.as_str(), i))
        .collect();
    let n_kept = nodes.len().min(max_nodes);

    // --- edges: directed, so the console can tell callers from callees ---
    let mut ambiguous_edges = 0usize;
    let mut adj: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut directed: Vec<[usize; 2]> = Vec::new();
    for e in store.all_edges()? {
        if e.tier == "AMBIGUOUS" {
            ambiguous_edges += 1;
            continue; // never draw an edge the resolver refused to assert
        }
        let (Some(&s), Some(&t)) = (keep.get(e.source.as_str()), keep.get(e.target.as_str()))
        else {
            continue;
        };
        if s == t {
            continue;
        }
        adj.entry(s).or_default().push(t);
        adj.entry(t).or_default().push(s);
        directed.push([s, t]);
    }
    directed.sort_unstable();
    directed.dedup();
    for v in adj.values_mut() {
        v.sort_unstable();
        v.dedup();
    }

    // --- group by module, largest first ---
    let mut members: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, n) in nodes.iter().take(max_nodes).enumerate() {
        members
            .entry(module_of(&n.qualified_name))
            .or_default()
            .push(i);
    }
    let mut modules: Vec<(&str, usize)> = members.iter().map(|(m, v)| (*m, v.len())).collect();
    modules.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));

    // --- one wedge per module, span proportional to how much code it holds ---
    const R_INNER: f64 = 260.0; // the middle stays clear for the readout
    const R_OUTER: f64 = 1180.0;
    const GAP: f64 = 0.012; // radians of breathing room between wedges
    let total = n_kept.max(1) as f64;
    let mut spans: HashMap<&str, (f64, f64)> = HashMap::new();
    let mut cursor = -PI / 2.0; // start at twelve o'clock
    for (m, count) in &modules {
        let span = 2.0 * PI * (*count as f64) / total;
        spans.insert(m, (cursor + GAP, cursor + span - GAP));
        cursor += span;
    }

    // --- place inside the wedge: hubs inward, traversal order around ---
    let mut point: HashMap<usize, (f64, f64)> = HashMap::new();
    for (m, group) in &members {
        let (a0, a1) = spans.get(m).copied().unwrap_or((0.0, 2.0 * PI));
        let order = connected_order(group, &adj, &fan_in, &nodes);
        let n = order.len().max(1);
        // Radius by fan-in rank: the most depended-upon code sits closest to the
        // centre, which is the one thing a reader should be able to see instantly.
        let mut by_fan: Vec<usize> = order.clone();
        by_fan.sort_by(|a, b| {
            let f = |i: &usize| fan_in.get(&nodes[*i].qualified_name).copied().unwrap_or(0);
            f(b).cmp(&f(a))
                .then_with(|| nodes[*a].qualified_name.cmp(&nodes[*b].qualified_name))
        });
        let rank: HashMap<usize, usize> = by_fan.iter().enumerate().map(|(r, i)| (*i, r)).collect();

        let mut pts: Vec<(f64, f64)> = Vec::with_capacity(n);
        for (slot, i) in order.iter().enumerate() {
            let t = (rank.get(i).copied().unwrap_or(0) as f64 + 0.5) / n as f64;
            let r = R_INNER + (R_OUTER - R_INNER) * t.sqrt();
            let a = a0 + (a1 - a0) * ((slot as f64 + 0.5) / n as f64);
            pts.push((r * a.cos(), r * a.sin()));
        }
        let local: HashMap<usize, usize> = order.iter().enumerate().map(|(p, i)| (*i, p)).collect();
        let local_edges: Vec<(usize, usize)> = directed
            .iter()
            .filter_map(|[s, t]| match (local.get(s), local.get(t)) {
                (Some(a), Some(b)) => Some((*a, *b)),
                _ => None,
            })
            .collect();
        relax_wedge(&mut pts, &local_edges, a0, a1, R_INNER, R_OUTER);
        for (p, i) in order.iter().enumerate() {
            point.insert(*i, pts[p]);
        }
    }

    let module_index: HashMap<&str, u16> = modules
        .iter()
        .enumerate()
        .map(|(i, (m, _))| (*m, i as u16))
        .collect();

    let truncated = nodes.len() > max_nodes;
    let mut placed: Vec<Placed> = Vec::new();
    for (i, n) in nodes.iter().take(max_nodes).enumerate() {
        let (x, y) = point.get(&i).copied().unwrap_or((0.0, 0.0));
        placed.push(Placed {
            id: n.qualified_name.clone(),
            x,
            y,
            module: module_index
                .get(module_of(&n.qualified_name))
                .copied()
                .unwrap_or(0),
            magnitude: fan_in.get(&n.qualified_name).copied().unwrap_or(0),
            language: n.language.clone(),
        });
    }

    // --- module-to-module coupling, drawn as chords across the middle ---
    let mut chord: HashMap<(u16, u16), u32> = HashMap::new();
    for [s, t] in &directed {
        let (a, b) = (placed[*s].module, placed[*t].module);
        if a != b {
            *chord.entry((a.min(b), a.max(b))).or_insert(0) += 1;
        }
    }
    let mut chords: Vec<((u16, u16), u32)> = chord.into_iter().collect();
    chords.sort();

    // --- serialise: geometry binary, text as JSON ---
    let mut languages: Vec<&str> = placed.iter().map(|p| p.language.as_str()).collect();
    languages.sort_unstable();
    languages.dedup();
    let lang_index: HashMap<&str, u8> = languages
        .iter()
        .enumerate()
        .map(|(i, l)| (*l, i as u8))
        .collect();

    let mut pos_bytes = Vec::with_capacity(placed.len() * 8);
    let mut mod_bytes = Vec::with_capacity(placed.len() * 2);
    let mut mag_bytes = Vec::with_capacity(placed.len() * 2);
    let mut lang_bytes = Vec::with_capacity(placed.len());
    for p in &placed {
        pos_bytes.extend_from_slice(&(p.x as f32).to_le_bytes());
        pos_bytes.extend_from_slice(&(p.y as f32).to_le_bytes());
        mod_bytes.extend_from_slice(&p.module.to_le_bytes());
        mag_bytes.extend_from_slice(&(p.magnitude.clamp(0, u16::MAX as i64) as u16).to_le_bytes());
        lang_bytes.push(lang_index.get(p.language.as_str()).copied().unwrap_or(0));
    }
    // Direction is preserved: the console distinguishes what a symbol calls from
    // what calls it, which is the question the whole tool exists to answer.
    let mut edge_bytes = Vec::with_capacity(directed.len() * 8);
    for [s, t] in &directed {
        edge_bytes.extend_from_slice(&(*s as u32).to_le_bytes());
        edge_bytes.extend_from_slice(&(*t as u32).to_le_bytes());
    }

    let labels_json: String = placed
        .iter()
        .map(|p| json_str(short_label(&p.id)))
        .collect::<Vec<_>>()
        .join(",");
    let ids_json: String = placed
        .iter()
        .map(|p| json_str(&p.id))
        .collect::<Vec<_>>()
        .join(",");
    let langs_json: String = languages
        .iter()
        .map(|l| json_str(l))
        .collect::<Vec<_>>()
        .join(",");
    let modules_json: String = modules
        .iter()
        .map(|(m, count)| {
            let (a0, a1) = spans.get(m).copied().unwrap_or((0.0, 0.0));
            // Six places: at the outer radius a rounded bound would visibly
            // detach the sector rule from the symbols it contains.
            format!("[{},{a0:.6},{a1:.6},{count}]", json_str(m))
        })
        .collect::<Vec<_>>()
        .join(",");
    let chords_json: String = chords
        .iter()
        .map(|((a, b), w)| format!("[{a},{b},{w}]"))
        .collect::<Vec<_>>()
        .join(",");

    let payload = format!(
        "{{\"mode\":{},\"count\":{},\"pos\":{},\"mod\":{},\"mag\":{},\"lang\":{},         \"languages\":[{}],\"modules\":[{}],\"chords\":[{}],\"edges\":{},\"edgeCount\":{},         \"ambiguous\":{},\"rInner\":{R_INNER},\"rOuter\":{R_OUTER},         \"labels\":[{}],\"ids\":[{}],\"truncated\":{}}}",
        json_str(if mode == Mode::Full { "full" } else { "community" }),
        placed.len(),
        json_str(&b64(&pos_bytes)),
        json_str(&b64(&mod_bytes)),
        json_str(&b64(&mag_bytes)),
        json_str(&b64(&lang_bytes)),
        langs_json,
        modules_json,
        chords_json,
        json_str(&b64(&edge_bytes)),
        directed.len(),
        ambiguous_edges,
        labels_json,
        ids_json,
        truncated
    );

    if truncated {
        eprintln!(
            "warn: graph truncated to {max_nodes} of {} nodes for display (raise --max-nodes)",
            nodes.len()
        );
    }
    if placed.is_empty() {
        eprintln!("warn: this graph has no symbols — run `chitra build` first, or check --db");
    }

    Ok(TEMPLATE.replace("/*__DATA__*/", &payload))
}

/// `crates/chitra-core/src/build.rs::resolve` -> `resolve`, with the file kept
/// for the tooltip. Long ids make an unreadable canvas.
fn short_label(qualified: &str) -> &str {
    qualified.rsplit("::").next().unwrap_or(qualified)
}

/// Base64, so binary buffers can ride inside the HTML. Written out rather than
/// pulled in: it is twelve lines and the page must stay dependency-free.
fn b64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            A[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            A[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Minimal JSON string escaping — enough for identifiers and file paths.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The page, as a celestial survey plate.
///
/// The metaphor is load-bearing, not decoration: a star chart exists to make a
/// vast undifferentiated field navigable by grouping points into named regions,
/// which is exactly what community detection does to a codebase. Regions are
/// communities, stars are symbols, magnitude is fan-in, and the constellation
/// lines are cross-community coupling. Colour is constrained to one spectral
/// ramp so the plate reads as a single sky rather than a category rainbow.
///
/// No web fonts: nothing may be fetched at view time, so the type is system
/// stacks — serif for the chart's own chrome, monospace for the code it charts.
const TEMPLATE: &str = r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Chitra — dependency console</title>
<style>
  :root{
    color-scheme: dark;
    --void:#04070D; --deep:#070C15; --panel:#0A121Ce6;
    --line:#12303D; --fg:#CFE9F2; --dim:#5E7C8A; --faint:#33505C;
    --live:#46E8FF; --up:#FFB454; --down:#7CFFB2; --path:#C77DFF;
    --mono: ui-monospace, "SF Mono", "Cascadia Mono", Menlo, Consolas, monospace;
  }
  *{box-sizing:border-box}
  html,body{margin:0;height:100%;background:var(--void);color:var(--fg);
    font-family:var(--mono);font-size:12px;overflow:hidden;font-variant-numeric:tabular-nums}
  #stage{position:fixed;inset:0;background:
    radial-gradient(ellipse 72% 72% at 50% 50%,#0A1622 0%,var(--deep) 58%,var(--void) 100%)}
  #gl,#ui{position:absolute;inset:0;display:block}
  #ui{cursor:crosshair} #ui.drag{cursor:grabbing}

  .br{position:fixed;width:15px;height:15px;border:1px solid var(--live);opacity:.5;pointer-events:none}
  .br.tl{top:10px;left:10px;border-right:0;border-bottom:0}
  .br.tr{top:10px;right:10px;border-left:0;border-bottom:0}
  .br.bl{bottom:10px;left:10px;border-right:0;border-top:0}
  .br.brr{bottom:10px;right:10px;border-left:0;border-top:0}

  .panel{position:fixed;z-index:3;background:var(--panel);border:1px solid var(--line);
    padding:11px 13px;backdrop-filter:blur(4px)}
  .hd{font-size:9px;letter-spacing:.22em;text-transform:uppercase;color:var(--live);
    margin:0 0 8px;display:flex;gap:10px;align-items:center}
  .hd::after{content:"";flex:1;height:1px;background:var(--line)}
  .row{display:flex;justify-content:space-between;gap:14px;padding:2px 0;color:var(--dim)}
  .row b{font-weight:400;color:var(--fg)}
  kbd{border:1px solid var(--line);padding:0 4px;color:var(--dim);font:inherit;font-size:10px}

  #sys{top:22px;left:22px;width:246px}
  #legend{margin-top:9px;padding-top:8px;border-top:1px solid var(--line);display:grid;gap:3px}
  .k{display:flex;align-items:center;gap:7px;color:var(--dim);font-size:10px}
  .k i{width:14px;height:2px;flex:none}
  #sel{top:22px;right:22px;width:334px;display:none;max-height:calc(100vh - 44px);
    overflow:auto;overscroll-behavior:contain}
  #sel h2{font-size:12px;margin:0 0 2px;font-weight:400;word-break:break-all;line-height:1.45}
  #sel .path{color:var(--dim);font-size:10px;word-break:break-all;margin-bottom:9px}
  .lists{display:grid;gap:9px;margin-top:10px}
  .lst h3{font-size:9px;letter-spacing:.18em;text-transform:uppercase;margin:0 0 4px;font-weight:400;
    display:flex;justify-content:space-between}
  .lst.up h3{color:var(--up)} .lst.down h3{color:var(--down)}
  .lst button{display:block;width:100%;text-align:left;background:none;border:0;
    border-left:2px solid transparent;color:var(--dim);font:inherit;font-size:11px;
    padding:2px 0 2px 7px;cursor:pointer;word-break:break-all}
  .lst.up button:hover{color:var(--up);border-left-color:var(--up)}
  .lst.down button:hover{color:var(--down);border-left-color:var(--down)}
  .empty{color:var(--faint);font-size:11px;font-style:italic}
  #trace{margin-top:10px;padding-top:9px;border-top:1px solid var(--line);display:none}
  #trace h3{font-size:9px;letter-spacing:.18em;text-transform:uppercase;margin:0 0 5px;
    font-weight:400;color:var(--path)}
  #trace ol{margin:0;padding-left:16px;color:var(--dim);font-size:11px;line-height:1.7}
  #trace li::marker{color:var(--path)}

  #filters{position:fixed;left:22px;bottom:22px;z-index:3;display:flex;flex-direction:column;gap:4px}
  .chip{display:flex;align-items:center;gap:8px;background:var(--panel);border:1px solid var(--line);
    padding:5px 9px;cursor:pointer;font:inherit;font-size:10px;letter-spacing:.1em;
    text-transform:uppercase;color:var(--dim);min-width:196px}
  .chip:hover{border-color:var(--live)} .chip[aria-pressed="true"]{color:var(--fg)}
  .chip[aria-pressed="false"]{opacity:.35}
  .chip .sw{width:8px;height:8px;flex:none} .chip .n{margin-left:auto;letter-spacing:0;color:var(--dim)}
  .chip:focus-visible,#q:focus,button:focus-visible{outline:1px solid var(--live);outline-offset:1px}

  #cmd{position:fixed;bottom:22px;left:50%;transform:translateX(-50%);z-index:3;
    display:flex;border:1px solid var(--line);background:var(--panel)}
  #q{background:none;border:0;color:var(--fg);font:inherit;padding:9px 12px;width:340px}
  #q::placeholder{color:var(--faint)}
  #cmd button{background:none;border:0;border-left:1px solid var(--line);color:var(--dim);
    font:inherit;font-size:10px;letter-spacing:.12em;text-transform:uppercase;padding:9px 12px;cursor:pointer}
  #cmd button:hover{color:var(--live)}
  #crumb{position:fixed;top:22px;left:50%;transform:translateX(-50%);z-index:3;display:none;
    background:var(--panel);border:1px solid var(--line);padding:6px 11px;font-size:10px;
    letter-spacing:.14em;text-transform:uppercase;color:var(--live);cursor:pointer}
  #note{position:fixed;bottom:62px;left:50%;transform:translateX(-50%);z-index:3;
    color:var(--up);font-size:11px;display:none}
  @media (prefers-reduced-motion: no-preference){
    #stage{animation:boot .55s ease-out both}
    @keyframes boot{from{opacity:0}to{opacity:1}}
  }
  @media (max-width:940px){#sys{display:none}#sel{width:270px}#q{width:190px}}
</style></head><body>
<div id="stage"><canvas id="gl"></canvas><canvas id="ui"></canvas></div>
<i class="br tl"></i><i class="br tr"></i><i class="br bl"></i><i class="br brr"></i>

<section class="panel" id="sys">
  <h1 class="hd">Chitra</h1>
  <div class="row"><span>Symbols</span><b id="n-sym">—</b></div>
  <div class="row"><span>Modules</span><b id="n-mod">—</b></div>
  <div class="row"><span>Links drawn</span><b id="n-edge">—</b></div>
  <div class="row"><span>Unresolved</span><b id="n-amb">—</b></div>
  <div class="row"><span>Renderer</span><b id="n-gpu">—</b></div>
  <div id="legend">
    <div class="k"><i style="background:var(--up)"></i>Breaks if changed</div>
    <div class="k"><i style="background:var(--down)"></i>Depended on</div>
    <div class="k"><i style="background:var(--path)"></i>Traced path</div>
    <div class="k" style="margin-top:4px"><kbd>click</kbd> select · <kbd>shift</kbd>+click trace</div>
    <div class="k"><kbd>/</kbd> search · <kbd>esc</kbd> clear</div>
  </div>
</section>

<section class="panel" id="sel" aria-live="polite">
  <h1 class="hd">Selection</h1>
  <h2 id="s-name"></h2>
  <div class="path" id="s-path"></div>
  <div class="row"><span>Module</span><b id="s-mod"></b></div>
  <div class="row"><span>Type</span><b id="s-lang"></b></div>
  <div class="row"><span>Direct callers</span><b id="s-in"></b></div>
  <div class="row"><span>Direct calls</span><b id="s-out"></b></div>
  <div class="row"><span>Blast radius</span><b id="s-blast"></b></div>
  <div id="trace"><h3>Call path</h3><ol id="t-steps"></ol></div>
  <div class="lists">
    <div class="lst up"><h3><span>Breaks if this changes</span><span id="c-up"></span></h3><div id="l-up"></div></div>
    <div class="lst down"><h3><span>This depends on</span><span id="c-down"></span></h3><div id="l-down"></div></div>
  </div>
</section>

<div id="filters" role="group" aria-label="Filter by file type"></div>
<button id="crumb"></button>
<div id="cmd">
  <input id="q" placeholder="Search symbols…   /" autocomplete="off" spellcheck="false" aria-label="Search symbols">
  <button id="reset">Reset</button>
</div>
<div id="note"></div>

<script>
const G = /*__DATA__*/;
const REDUCED = matchMedia('(prefers-reduced-motion: reduce)').matches;
function bin(b){const s=atob(b),a=new Uint8Array(s.length);
  for(let i=0;i<s.length;i++)a[i]=s.charCodeAt(i);return a;}
const POS=new Float32Array(bin(G.pos).buffer);
const MOD=new Uint16Array(bin(G.mod).buffer);
const MAG=new Uint16Array(bin(G.mag).buffer);
const LANG=bin(G.lang);
const EDGE=new Uint32Array(bin(G.edges).buffer);
const N=G.count;

const callers=Array.from({length:N},()=>[]), calls=Array.from({length:N},()=>[]);
for(let e=0;e<EDGE.length;e+=2){calls[EDGE[e]].push(EDGE[e+1]);callers[EDGE[e+1]].push(EDGE[e]);}

const stage=document.getElementById('stage'), glc=document.getElementById('gl');
const uic=document.getElementById('ui'), ui=uic.getContext('2d');
let W=0,H=0,DPR=1,scale=.34,ox=0,oy=0,filter='',hits=null,hitAt=0,
    sel=-1,hover=-1,focusMod=-1,path=null,cascade=null,t0=performance.now();
const langOn=new Set(G.languages.map((_,i)=>i));
const shown=i=>langOn.has(LANG[i]) && (focusMod<0 || MOD[i]===focusMod);

function modColour(m){const t=G.modules.length<2?0:m/(G.modules.length-1);
  return `hsl(${186-t*54} ${72-t*10}% ${52+t*8}%)`;}
function modRGB(m){const c=modColour(m).match(/[\d.]+/g).map(Number);
  const[h,s,l]=[c[0]/360,c[1]/100,c[2]/100];const a=s*Math.min(l,1-l);
  const f=n=>{const k=(n+h*12)%12;return l-a*Math.max(-1,Math.min(k-3,Math.min(9-k,1)));};
  return [f(0),f(8),f(4)];}
const sx=x=>x*scale+ox+W/2, sy=y=>y*scale+oy+H/2;
const fmt=n=>n.toLocaleString('en-US');
const set=(id,v)=>{document.getElementById(id).textContent=v;};

// ---------------- GPU ----------------
const gl=glc.getContext('webgl2',{antialias:true,alpha:true});
const VS_PT=`#version 300 es
layout(location=0) in vec2 a_pos; layout(location=1) in vec3 a_col; layout(location=2) in float a_mag;
uniform vec2 u_off; uniform float u_scale; uniform vec2 u_size; uniform float u_dpr;
uniform float u_pulse;
out vec3 v_col; out float v_hub;
void main(){
  vec2 p=a_pos*u_scale+u_off+u_size*0.5;
  gl_Position=vec4((p/u_size)*2.0-1.0,0.0,1.0); gl_Position.y=-gl_Position.y;
  float base = 1.2+min(4.8, log2(1.0+a_mag)*1.0);
  // Hubs breathe: size carries fan-in, and the biggest ones read as alive.
  float hub = clamp((a_mag-8.0)/40.0, 0.0, 1.0);
  gl_PointSize = a_mag<0.0 ? 0.0 : (base*(1.0+hub*u_pulse*0.22))*2.0*u_dpr;
  v_col=a_col; v_hub=hub;
}`;
const FS_PT=`#version 300 es
precision mediump float; in vec3 v_col; in float v_hub; out vec4 o; uniform float u_alpha;
void main(){
  vec2 d=gl_PointCoord-vec2(0.5); float r=length(d);
  if(r>0.5) discard;
  float core=smoothstep(0.5,0.30,r);
  float halo=smoothstep(0.5,0.0,r)*v_hub*0.5;      // glow only where it means something
  o=vec4(v_col, (core+halo)*u_alpha);
}`;
const VS_LN=`#version 300 es
layout(location=0) in vec2 a_pos; layout(location=1) in vec3 a_col; layout(location=2) in float a_t;
uniform vec2 u_off; uniform float u_scale; uniform vec2 u_size;
out vec3 v_col; out float v_t;
void main(){
  vec2 p=a_pos*u_scale+u_off+u_size*0.5;
  gl_Position=vec4((p/u_size)*2.0-1.0,0.0,1.0); gl_Position.y=-gl_Position.y;
  v_col=a_col; v_t=a_t;
}`;
const FS_LN=`#version 300 es
precision mediump float; in vec3 v_col; in float v_t; out vec4 o; uniform float u_alpha;
void main(){
  // Direction is the meaning: faint at the caller, bright at the callee.
  o=vec4(v_col, mix(0.02,0.20,v_t)*u_alpha);
}`;
let pPt,pLn,vaoPt,vaoLn,magBuf,lnPos,lnCol,lnT,lnCount=0,gpu=false;
const sh=(t,s)=>{const x=gl.createShader(t);gl.shaderSource(x,s);gl.compileShader(x);
  if(!gl.getShaderParameter(x,gl.COMPILE_STATUS))throw new Error(gl.getShaderInfoLog(x));return x;};
const pr=(v,f)=>{const p=gl.createProgram();gl.attachShader(p,sh(gl.VERTEX_SHADER,v));
  gl.attachShader(p,sh(gl.FRAGMENT_SHADER,f));gl.linkProgram(p);
  if(!gl.getProgramParameter(p,gl.LINK_STATUS))throw new Error(gl.getProgramInfoLog(p));return p;};
function buf(loc,data,size,usage){const b=gl.createBuffer();
  gl.bindBuffer(gl.ARRAY_BUFFER,b);gl.bufferData(gl.ARRAY_BUFFER,data,usage||gl.STATIC_DRAW);
  gl.enableVertexAttribArray(loc);gl.vertexAttribPointer(loc,size,gl.FLOAT,false,0,0);return b;}
function buildLines(){
  const keep=[];
  for(let e=0;e<EDGE.length;e+=2) if(shown(EDGE[e])&&shown(EDGE[e+1])) keep.push(e);
  lnCount=keep.length;
  const P=new Float32Array(lnCount*4), C=new Float32Array(lnCount*6), T=new Float32Array(lnCount*2);
  keep.forEach((e,n)=>{
    const s=EDGE[e], t=EDGE[e+1], c=modRGB(MOD[t]);
    P[n*4]=POS[s*2];   P[n*4+1]=POS[s*2+1];
    P[n*4+2]=POS[t*2]; P[n*4+3]=POS[t*2+1];
    C[n*6]=c[0];C[n*6+1]=c[1];C[n*6+2]=c[2];
    C[n*6+3]=c[0];C[n*6+4]=c[1];C[n*6+5]=c[2];
    T[n*2]=0; T[n*2+1]=1;
  });
  if(gpu){
    gl.bindVertexArray(vaoLn);
    gl.bindBuffer(gl.ARRAY_BUFFER,lnPos); gl.bufferData(gl.ARRAY_BUFFER,P,gl.DYNAMIC_DRAW);
    gl.bindBuffer(gl.ARRAY_BUFFER,lnCol); gl.bufferData(gl.ARRAY_BUFFER,C,gl.DYNAMIC_DRAW);
    gl.bindBuffer(gl.ARRAY_BUFFER,lnT);   gl.bufferData(gl.ARRAY_BUFFER,T,gl.DYNAMIC_DRAW);
    gl.bindVertexArray(null);
  }
}
function initGL(){
  if(!gl) return false;
  pPt=pr(VS_PT,FS_PT); pLn=pr(VS_LN,FS_LN);
  const col=new Float32Array(N*3);
  for(let i=0;i<N;i++){const c=modRGB(MOD[i]);col[i*3]=c[0];col[i*3+1]=c[1];col[i*3+2]=c[2];}
  const mag=new Float32Array(N); for(let i=0;i<N;i++) mag[i]=MAG[i];
  vaoPt=gl.createVertexArray(); gl.bindVertexArray(vaoPt);
  buf(0,POS,2); buf(1,col,3); magBuf=buf(2,mag,1,gl.DYNAMIC_DRAW);
  vaoLn=gl.createVertexArray(); gl.bindVertexArray(vaoLn);
  lnPos=buf(0,new Float32Array(0),2,gl.DYNAMIC_DRAW);
  lnCol=buf(1,new Float32Array(0),3,gl.DYNAMIC_DRAW);
  lnT=buf(2,new Float32Array(0),1,gl.DYNAMIC_DRAW);
  gl.bindVertexArray(null);
  gl.enable(gl.BLEND); gl.blendFunc(gl.SRC_ALPHA,gl.ONE_MINUS_SRC_ALPHA);
  return true;
}
try{gpu=initGL();}catch(e){gpu=false;console.warn('WebGL unavailable:',e);}
function uni(p,alpha,pulse){
  gl.useProgram(p);
  gl.uniform2f(gl.getUniformLocation(p,'u_off'),ox,oy);
  gl.uniform1f(gl.getUniformLocation(p,'u_scale'),scale);
  gl.uniform2f(gl.getUniformLocation(p,'u_size'),W,H);
  const d=gl.getUniformLocation(p,'u_dpr'); if(d) gl.uniform1f(d,DPR);
  const a=gl.getUniformLocation(p,'u_alpha'); if(a) gl.uniform1f(a,alpha);
  const u=gl.getUniformLocation(p,'u_pulse'); if(u) gl.uniform1f(u,pulse);
}

function resize(){
  DPR=Math.min(devicePixelRatio||1,2); W=stage.clientWidth; H=stage.clientHeight;
  for(const c of [glc,uic]){c.style.width=W+'px';c.style.height=H+'px';c.width=W*DPR;c.height=H*DPR;}
  ui.setTransform(DPR,0,0,DPR,0,0);
  if(gpu) gl.viewport(0,0,glc.width,glc.height);
  grid(); draw();
}
addEventListener('resize',resize);

// ---------------- draw ----------------
function draw(){
  const now=(performance.now()-t0)/1000;
  const pulse=REDUCED?0:(Math.sin(now*1.9)*0.5+0.5);
  const dim=(sel>=0||path)?0.20:1.0;

  if(gpu){
    gl.clearColor(0,0,0,0); gl.clear(gl.COLOR_BUFFER_BIT);
    if(lnCount){ gl.bindVertexArray(vaoLn); uni(pLn,dim,0);
      gl.drawArrays(gl.LINES,0,lnCount*2); }
    gl.bindVertexArray(vaoPt); uni(pPt,dim,pulse); gl.drawArrays(gl.POINTS,0,N);
    gl.bindVertexArray(null);
  }

  ui.clearRect(0,0,W,H);
  drawDial(now);
  if(scale<=0.5) drawChords();
  if(!gpu) drawPointsFallback();
  if(cascade) drawCascade(now);
  if(path) drawPath(now);
  if(sel>=0) drawLock(now);
  if(hits) drawHits(); else drawLandmarks();
  if(hover>=0&&hover!==sel) drawHover(hover);

  let vis=0; for(let i=0;i<N;i++) if(shown(i)) vis++;
  set('n-sym', vis===N?fmt(N):fmt(vis)+' / '+fmt(N));
  set('n-mod', focusMod<0?fmt(G.modules.length):'1 / '+fmt(G.modules.length));
  set('n-edge', fmt(lnCount)); set('n-amb', fmt(G.ambiguous));
  set('n-gpu', gpu?'WEBGL2':'CANVAS');
}

let raf=null;
function animate(){ draw();
  raf = (!REDUCED && (sel>=0||path||true)) ? requestAnimationFrame(animate) : null; }

function drawDial(now){
  const cx=sx(0),cy=sy(0),ri=G.rInner*scale,ro=G.rOuter*scale;
  ui.lineWidth=1;
  G.modules.forEach(([name,a0,a1,count],mi)=>{
    const on = focusMod<0||focusMod===mi;
    ui.strokeStyle=on?'rgba(70,232,255,0.17)':'rgba(70,232,255,0.05)';
    for(const a of [a0,a1]){ ui.beginPath();
      ui.moveTo(cx+Math.cos(a)*ri,cy+Math.sin(a)*ri);
      ui.lineTo(cx+Math.cos(a)*ro,cy+Math.sin(a)*ro); ui.stroke(); }
    const col=modColour(mi);
    ui.strokeStyle=on?col.replace('hsl','hsla').replace(')',' / 0.45)')
                    :'rgba(51,80,92,0.25)';
    ui.lineWidth=on?1.4:1; ui.beginPath(); ui.arc(cx,cy,ro,a0,a1); ui.stroke(); ui.lineWidth=1;
    const mid=(a0+a1)/2, lx=cx+Math.cos(mid)*(ro+17), ly=cy+Math.sin(mid)*(ro+17);
    if(lx>-90&&lx<W+90&&ly>-40&&ly<H+40){
      ui.save(); ui.translate(lx,ly); ui.rotate(Math.cos(mid)<0?mid+Math.PI:mid);
      ui.textAlign=Math.cos(mid)<0?'right':'left'; ui.textBaseline='middle';
      ui.font='10px '+getComputedStyle(document.body).fontFamily;
      ui.fillStyle=on?'rgba(207,233,242,0.92)':'rgba(51,80,92,0.7)';
      ui.fillText(name.toUpperCase()+'  '+count,0,0); ui.restore();
    }
  });
  ui.strokeStyle='rgba(18,48,61,0.85)';
  for(const f of [0.25,0.5,0.75]){ui.beginPath();ui.arc(cx,cy,ri+(ro-ri)*f,0,6.283);ui.stroke();}
  ui.strokeStyle='rgba(70,232,255,0.3)'; ui.beginPath(); ui.arc(cx,cy,ri,0,6.283); ui.stroke();
  // slow sweep — the only ambient motion, and it doubles as a "live" cue
  if(!REDUCED){
    const a=(now*0.36)%(Math.PI*2);
    const grad=ui.createLinearGradient(cx,cy,cx+Math.cos(a)*ro,cy+Math.sin(a)*ro);
    grad.addColorStop(0,'rgba(70,232,255,0)'); grad.addColorStop(1,'rgba(70,232,255,0.20)');
    ui.strokeStyle=grad; ui.lineWidth=1.5;
    ui.beginPath(); ui.moveTo(cx+Math.cos(a)*ri,cy+Math.sin(a)*ri);
    ui.lineTo(cx+Math.cos(a)*ro,cy+Math.sin(a)*ro); ui.stroke(); ui.lineWidth=1;
  }
  if(scale<=0.5&&!cascade&&!path){
    ui.fillStyle='rgba(94,124,138,0.85)';ui.textAlign='center';ui.textBaseline='middle';
    ui.font='9px '+getComputedStyle(document.body).fontFamily;
    ui.fillText('MOST DEPENDED-UPON',cx,cy-7);
    ui.fillText('ZOOM OR CLICK A SYMBOL',cx,cy+7);
  }
}
function drawChords(){
  const cx=sx(0),cy=sy(0),ri=G.rInner*scale;
  const max=G.chords.reduce((m,c)=>Math.max(m,c[2]),1);
  for(const [a,b,w] of G.chords){
    const A=G.modules[a],B=G.modules[b]; if(!A||!B) continue;
    const ma=(A[1]+A[2])/2, mb=(B[1]+B[2])/2;
    ui.strokeStyle=`rgba(70,232,255,${0.10+0.5*(w/max)})`; ui.lineWidth=0.5+2.5*(w/max);
    ui.beginPath(); ui.moveTo(cx+Math.cos(ma)*ri,cy+Math.sin(ma)*ri);
    ui.quadraticCurveTo(cx,cy,cx+Math.cos(mb)*ri,cy+Math.sin(mb)*ri); ui.stroke();
  }
  ui.lineWidth=1;
}
function drawPointsFallback(){
  for(let i=0;i<N;i++){ if(!shown(i)) continue;
    const X=sx(POS[i*2]),Y=sy(POS[i*2+1]); if(X<0||X>W||Y<0||Y>H) continue;
    ui.beginPath();ui.arc(X,Y,1.2+Math.min(4.8,Math.log2(1+MAG[i])),0,6.283);
    ui.fillStyle=modColour(MOD[i]);ui.fill(); }
}

/* Landmarks: the few symbols everything leans on, labelled at every zoom so the
   map is never an anonymous field of dots. Boxes are collision-checked. */
let landmarks=null;
function computeLandmarks(){
  const idx=[...Array(N).keys()].filter(i=>shown(i)&&MAG[i]>0);
  idx.sort((a,b)=>MAG[b]-MAG[a]);
  landmarks=idx.slice(0,44);
}
function drawLandmarks(){
  if(!landmarks) computeLandmarks();
  ui.font='10px '+getComputedStyle(document.body).fontFamily;
  ui.textAlign='left'; ui.textBaseline='middle';
  const taken=[];
  for(const i of landmarks){
    if(!shown(i)) continue;
    const X=sx(POS[i*2]),Y=sy(POS[i*2+1]);
    if(X<40||X>W-40||Y<20||Y>H-20) continue;
    const label=G.labels[i], w=ui.measureText(label).width;
    const box=[X+9,Y-6,w+8,12];
    if(taken.some(t=>!(box[0]>t[0]+t[2]||box[0]+box[2]<t[0]||box[1]>t[1]+t[3]||box[1]+box[3]<t[1]))) continue;
    taken.push(box);
    ui.strokeStyle='rgba(70,232,255,0.5)'; ui.lineWidth=1;
    ui.beginPath(); ui.moveTo(X+3,Y); ui.lineTo(X+8,Y); ui.stroke();
    ui.fillStyle='rgba(4,7,13,0.72)'; ui.fillRect(box[0],box[1],box[2],box[3]);
    ui.fillStyle='rgba(207,233,242,0.92)'; ui.fillText(label,X+13,Y);
  }
}

/* The blast radius, actually drawn: concentric hops out from the selection. */
function buildCascade(i){
  const level=new Map([[i,0]]); let front=[i];
  for(let d=1;d<=3;d++){
    const next=[];
    for(const n of front) for(const c of callers[n])
      if(!level.has(c)&&shown(c)){ level.set(c,d); next.push(c); }
    front=next; if(!front.length) break;
  }
  return level;
}
function drawCascade(now){
  const wave=REDUCED?1:((now*0.55)%1);
  for(const [node,d] of cascade){
    if(d===0||!shown(node)) continue;
    const X=sx(POS[node*2]),Y=sy(POS[node*2+1]);
    if(X<-30||X>W+30||Y<-30||Y>H+30) continue;
    const a=[0,0.85,0.5,0.28][d];
    ui.beginPath(); ui.arc(X,Y,3.4,0,6.283);
    ui.fillStyle=`rgba(255,180,84,${a})`; ui.fill();
    // one expanding ring per hop, so depth reads as time
    const p=(wave+d*0.18)%1;
    ui.beginPath(); ui.arc(X,Y,3.4+p*9,0,6.283);
    ui.strokeStyle=`rgba(255,180,84,${a*(1-p)*0.5})`; ui.lineWidth=1; ui.stroke();
  }
}
function drawLock(now){
  const X=sx(POS[sel*2]),Y=sy(POS[sel*2+1]);
  const arm=(list,col)=>{ui.strokeStyle=col;ui.fillStyle=col;ui.lineWidth=1.1;
    for(const j of list){ if(!shown(j)) continue;
      const x=sx(POS[j*2]),y=sy(POS[j*2+1]);
      ui.beginPath();ui.moveTo(X,Y);ui.lineTo(x,y);ui.stroke();
      ui.beginPath();ui.arc(x,y,3,0,6.283);ui.fill(); }};
  arm(calls[sel],'rgba(124,255,178,0.75)');
  arm(callers[sel],'rgba(255,180,84,0.9)');
  const r=REDUCED?9:9+Math.sin(now*3.2)*1.6;
  ui.beginPath();ui.arc(X,Y,r,0,6.283);ui.strokeStyle='#46E8FF';ui.lineWidth=1.5;ui.stroke();
  ui.beginPath();ui.arc(X,Y,2.8,0,6.283);ui.fillStyle='#46E8FF';ui.fill();
  ui.beginPath();
  for(const [dx,dy] of [[0,-1],[0,1],[-1,0],[1,0]]){
    ui.moveTo(X+dx*(r+4),Y+dy*(r+4)); ui.lineTo(X+dx*(r+10),Y+dy*(r+10)); }
  ui.stroke(); ui.lineWidth=1;
}
function drawPath(now){
  ui.strokeStyle='rgba(199,125,255,0.95)'; ui.lineWidth=2;
  ui.beginPath();
  for(let k=0;k<path.length;k++){
    const X=sx(POS[path[k]*2]),Y=sy(POS[path[k]*2+1]);
    k?ui.lineTo(X,Y):ui.moveTo(X,Y);
  }
  ui.stroke(); ui.lineWidth=1;
  // a marker running the route, so direction is unmistakable
  if(!REDUCED&&path.length>1){
    const t=((now*0.55)%1)*(path.length-1), k=Math.floor(t), f=t-k;
    const a=path[k],b=path[Math.min(k+1,path.length-1)];
    const X=sx(POS[a*2])+(sx(POS[b*2])-sx(POS[a*2]))*f;
    const Y=sy(POS[a*2+1])+(sy(POS[b*2+1])-sy(POS[a*2+1]))*f;
    ui.beginPath();ui.arc(X,Y,3.6,0,6.283);ui.fillStyle='#C77DFF';ui.fill();
  }
  for(const n of path){
    const X=sx(POS[n*2]),Y=sy(POS[n*2+1]);
    ui.beginPath();ui.arc(X,Y,4,0,6.283);
    ui.strokeStyle='#C77DFF';ui.lineWidth=1.4;ui.stroke();
  }
  ui.lineWidth=1;
}
function drawHits(){
  ui.font='10px '+getComputedStyle(document.body).fontFamily;ui.textAlign='left';ui.textBaseline='middle';
  for(const i of hits.slice(0,140)){
    if(!shown(i)) continue;
    const X=sx(POS[i*2]),Y=sy(POS[i*2+1]);
    if(X<-40||X>W+40||Y<-20||Y>H+20) continue;
    ui.beginPath();ui.arc(X,Y,5,0,6.283);ui.strokeStyle='#46E8FF';ui.lineWidth=1.1;ui.stroke();
    ui.fillStyle='rgba(4,7,13,0.7)';
    const w=ui.measureText(G.labels[i]).width; ui.fillRect(X+8,Y-6,w+6,12);
    ui.fillStyle='rgba(207,233,242,0.95)';ui.fillText(G.labels[i],X+11,Y);
  }
}
function drawHover(i){
  const X=sx(POS[i*2]),Y=sy(POS[i*2+1]);
  ui.strokeStyle='rgba(70,232,255,0.75)';ui.lineWidth=1;
  ui.beginPath();ui.arc(X,Y,7,0,6.283);ui.stroke();
  ui.font='10px '+getComputedStyle(document.body).fontFamily;ui.textAlign='left';ui.textBaseline='middle';
  const txt=G.ids[i], sub=callers[i].length+' callers · '+calls[i].length+' calls';
  const w=Math.max(ui.measureText(txt).width,ui.measureText(sub).width)+12;
  const bx=Math.min(X+11,W-w-8), by=Math.min(Y-20,H-40);
  ui.fillStyle='rgba(4,7,13,0.88)';ui.fillRect(bx,by,w,32);
  ui.strokeStyle='rgba(18,48,61,1)';ui.strokeRect(bx,by,w,32);
  ui.fillStyle='#CFE9F2';ui.fillText(txt,bx+6,by+11);
  ui.fillStyle='#5E7C8A';ui.fillText(sub,bx+6,by+24);
}

// ---------------- selection ----------------
function shortestPath(a,b){
  if(a===b) return [a];
  const prev=new Map([[a,-1]]); let front=[a];
  while(front.length){
    const next=[];
    for(const n of front) for(const c of calls[n]){
      if(prev.has(c)||!shown(c)) continue;
      prev.set(c,n); if(c===b){ const out=[b]; let p=n;
        while(p!==-1){out.push(p);p=prev.get(p);} return out.reverse(); }
      next.push(c);
    }
    front=next;
  }
  return null;
}
function select(i,{trace=false}={}){
  const panel=document.getElementById('sel'), tr=document.getElementById('trace');
  if(trace&&sel>=0&&i>=0&&i!==sel){
    const p=shortestPath(sel,i);
    path=p; tr.style.display=p?'block':'none';
    const ol=document.getElementById('t-steps'); ol.innerHTML='';
    if(p) for(const n of p){const li=document.createElement('li');li.textContent=G.ids[n];ol.appendChild(li);}
    else{ const note=document.getElementById('note'); note.style.display='block';
      note.textContent='No call path from the selection to that symbol.'; }
    draw(); return;
  }
  path=null; document.getElementById('trace').style.display='none';
  sel=i;
  if(i<0){ cascade=null; panel.style.display='none'; draw(); return; }
  cascade=buildCascade(i);
  const id=G.ids[i], cut=id.lastIndexOf('::');
  set('s-name',cut<0?id:id.slice(cut+2));
  set('s-path',cut<0?'':id.slice(0,cut));
  set('s-mod',G.modules[MOD[i]]?G.modules[MOD[i]][0]:'—');
  set('s-lang',G.languages[LANG[i]]);
  set('s-in',fmt(callers[i].length)); set('s-out',fmt(calls[i].length));
  set('s-blast',fmt(cascade.size-1)+' within 3 hops');
  set('c-up',fmt(callers[i].filter(shown).length));
  set('c-down',fmt(calls[i].filter(shown).length));
  fill('l-up',callers[i],'up'); fill('l-down',calls[i],'down');
  panel.style.display='block'; panel.scrollTop=0;
  draw();
}
function fill(where,list,kind){
  const host=document.getElementById(where); host.innerHTML='';
  const vis=list.filter(shown);
  if(!vis.length){ const p=document.createElement('div'); p.className='empty';
    p.textContent=kind==='up'?'Nothing depends on this.':'Calls nothing tracked here.';
    host.appendChild(p); return; }
  vis.sort((a,b)=>MAG[b]-MAG[a]);
  for(const j of vis.slice(0,60)){
    const b=document.createElement('button'); b.type='button'; b.textContent=G.ids[j];
    b.addEventListener('click',()=>{centreOn(j);select(j);});
    host.appendChild(b);
  }
  if(vis.length>60){const m=document.createElement('div');m.className='empty';
    m.textContent='+ '+fmt(vis.length-60)+' more';host.appendChild(m);}
}
function centreOn(i){ scale=Math.max(scale,1.15);
  ox=-POS[i*2]*scale; oy=-POS[i*2+1]*scale; grid(); }
function focusModule(m){
  focusMod=m; landmarks=null; buildLines();
  const crumb=document.getElementById('crumb');
  if(m<0){ crumb.style.display='none'; scale=.34; ox=0; oy=0; }
  else{
    crumb.style.display='block';
    crumb.textContent='◂ '+G.modules[m][0]+' — show all modules';
    const [,a0,a1]=G.modules[m], mid=(a0+a1)/2, r=(G.rInner+G.rOuter)/2;
    scale=Math.min(2.2, 1.5/Math.max(0.35,(a1-a0)));
    ox=-Math.cos(mid)*r*scale; oy=-Math.sin(mid)*r*scale;
  }
  grid(); draw();
}

// ---------------- picking ----------------
let cells=null,cell=0;
function grid(){ cell=70/Math.max(scale,1e-4); cells=new Map();
  for(let i=0;i<N;i++){ const k=((POS[i*2]/cell)|0)+','+((POS[i*2+1]/cell)|0);
    let b=cells.get(k); if(!b){b=[];cells.set(k,b);} b.push(i); } }
function pick(px,py){
  const wx=(px-ox-W/2)/scale, wy=(py-oy-H/2)/scale;
  const gx=(wx/cell)|0, gy=(wy/cell)|0;
  let best=-1,bd=(16/scale)**2;
  for(let dx=-1;dx<=1;dx++)for(let dy=-1;dy<=1;dy++){
    const b=cells.get((gx+dx)+','+(gy+dy)); if(!b) continue;
    for(const i of b){ if(!shown(i)) continue;
      const ex=POS[i*2]-wx,ey=POS[i*2+1]-wy,d=ex*ex+ey*ey;
      // prefer the more important symbol when two overlap
      if(d<bd||(d<bd*1.6&&best>=0&&MAG[i]>MAG[best]*2)){bd=Math.min(bd,d);best=i;} }
  }
  return best;
}
function moduleAt(px,py){
  const wx=(px-ox-W/2)/scale, wy=(py-oy-H/2)/scale;
  const r=Math.hypot(wx,wy);
  if(r<G.rInner||r>G.rOuter*1.06) return -1;
  let a=Math.atan2(wy,wx);
  for(let m=0;m<G.modules.length;m++){
    const [,a0,a1]=G.modules[m], mid=(a0+a1)/2;
    let x=a; while(x<mid-Math.PI)x+=2*Math.PI; while(x>mid+Math.PI)x-=2*Math.PI;
    if(x>=Math.min(a0,a1)&&x<=Math.max(a0,a1)) return m;
  }
  return -1;
}

// ---------------- input ----------------
let drag=false,lx=0,ly=0,moved=false;
uic.addEventListener('mousedown',e=>{drag=true;moved=false;lx=e.clientX;ly=e.clientY;uic.classList.add('drag');});
addEventListener('mouseup',e=>{
  uic.classList.remove('drag');
  if(drag&&!moved){
    const r=uic.getBoundingClientRect(), px=e.clientX-r.left, py=e.clientY-r.top;
    const i=pick(px,py);
    if(i>=0) select(i,{trace:e.shiftKey});
    else{
      const m=moduleAt(px,py);
      if(m>=0&&focusMod<0) focusModule(m); else select(-1);
    }
  }
  drag=false;
});
addEventListener('mousemove',e=>{
  if(drag){ if(Math.abs(e.clientX-lx)+Math.abs(e.clientY-ly)>3) moved=true;
    ox+=e.clientX-lx;oy+=e.clientY-ly;lx=e.clientX;ly=e.clientY;draw();return; }
  const r=uic.getBoundingClientRect();
  const i=pick(e.clientX-r.left,e.clientY-r.top);
  if(i!==hover){hover=i;draw();}
});
uic.addEventListener('wheel',e=>{
  e.preventDefault();
  const k=Math.exp(-e.deltaY*0.0016), r=uic.getBoundingClientRect();
  const mx=e.clientX-r.left-W/2-ox, my=e.clientY-r.top-H/2-oy;
  ox-=mx*(k-1);oy-=my*(k-1);
  scale=Math.max(0.06,Math.min(30,scale*k));
  grid();draw();
},{passive:false});

// ---------------- filters / search ----------------
function applyFilter(){
  if(gpu){ const mag=new Float32Array(N);
    for(let i=0;i<N;i++) mag[i]=shown(i)?MAG[i]:-1;
    gl.bindVertexArray(vaoPt); gl.bindBuffer(gl.ARRAY_BUFFER,magBuf);
    gl.bufferSubData(gl.ARRAY_BUFFER,0,mag); gl.bindVertexArray(null); }
  buildLines(); landmarks=null;
  if(sel>=0&&!shown(sel)) select(-1); else if(sel>=0) select(sel);
  draw();
}
(function chips(){
  const host=document.getElementById('filters'), count=new Map();
  for(let i=0;i<N;i++) count.set(LANG[i],(count.get(LANG[i])||0)+1);
  for(const [li,n] of [...count.entries()].sort((a,b)=>b[1]-a[1])){
    const b=document.createElement('button');
    b.className='chip';b.type='button';b.setAttribute('aria-pressed','true');
    let rep=0; for(let i=0;i<N;i++) if(LANG[i]===li){rep=MOD[i];break;}
    b.innerHTML='<span class="sw" style="background:'+modColour(rep)+'"></span>'
      +G.languages[li]+'<span class="n">'+fmt(n)+'</span>';
    b.addEventListener('click',()=>{
      if(langOn.has(li))langOn.delete(li);else langOn.add(li);
      if(!langOn.size){langOn.add(li);return;}
      b.setAttribute('aria-pressed',langOn.has(li)?'true':'false');
      applyFilter();
    });
    host.appendChild(b);
  }
})();
const qEl=document.getElementById('q');
qEl.addEventListener('input',e=>{
  filter=e.target.value.toLowerCase().trim(); hitAt=0;
  const note=document.getElementById('note');
  if(!filter){hits=null;note.style.display='none';draw();return;}
  hits=[]; for(let i=0;i<N;i++) if(shown(i)&&G.ids[i].toLowerCase().includes(filter)) hits.push(i);
  hits.sort((a,b)=>MAG[b]-MAG[a]);
  if(!hits.length){note.style.display='block';note.textContent='No symbol matches “'+filter+'”.';}
  else{note.style.display='block';
    note.textContent=fmt(hits.length)+' match'+(hits.length>1?'es':'')+' · enter to jump';}
  draw();
});
qEl.addEventListener('keydown',e=>{
  if(!hits||!hits.length) return;
  if(e.key==='Enter'){ e.preventDefault();
    const i=hits[hitAt%hits.length]; hitAt++; centreOn(i); select(i); }
});
document.getElementById('reset').addEventListener('click',()=>{
  scale=.34;ox=0;oy=0;select(-1);focusModule(-1);qEl.value='';filter='';hits=null;
  document.getElementById('note').style.display='none';grid();draw();});
document.getElementById('crumb').addEventListener('click',()=>focusModule(-1));
addEventListener('keydown',e=>{
  if(e.key==='/'&&document.activeElement!==qEl){e.preventDefault();qEl.focus();qEl.select();}
  if(e.key==='Escape'){qEl.value='';filter='';hits=null;
    document.getElementById('note').style.display='none';
    if(path||sel>=0) select(-1); else if(focusMod>=0) focusModule(-1);
    qEl.blur();}
});
if(G.truncated){const n=document.getElementById('note');n.style.display='block';
  n.textContent='Showing '+fmt(N)+' symbols. Raise --max-nodes for the rest.';}
buildLines(); resize();
if(!REDUCED) animate();
</script></body></html>
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn built(tag: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("chitra_viz_{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.py"),
            "def helper():\n    return 1\n\ndef main():\n    return helper()\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("b.py"),
            "from a import helper\n\ndef other():\n    return helper()\n",
        )
        .unwrap();
        let mut store = Store::open_in_memory().unwrap();
        crate::build(&mut store, &PathBuf::from(&dir)).unwrap();
        store
    }

    #[test]
    fn page_is_self_contained() {
        let html = visualize_html(&built("self"), Mode::Community, 10_000).unwrap();
        assert!(html.starts_with("<!doctype html>"));
        // The whole point: nothing is fetched when the page opens.
        for bad in ["http://", "https://", "src=", "cdn"] {
            assert!(!html.contains(bad), "page must not reference {bad}");
        }
    }

    #[test]
    fn graph_data_is_embedded() {
        let html = visualize_html(&built("data"), Mode::Community, 10_000).unwrap();
        assert!(
            !html.contains("/*__DATA__*/"),
            "payload was not substituted"
        );
        assert!(html.contains("a.py::helper"));
        assert!(html.contains("\"modules\""));
    }

    /// Same store, same page — the export discipline applies to the viz too.
    #[test]
    fn output_is_deterministic() {
        let store = built("det");
        let a = visualize_html(&store, Mode::Community, 10_000).unwrap();
        let b = visualize_html(&store, Mode::Community, 10_000).unwrap();
        assert_eq!(a, b);
    }

    /// A cap must be visible in the output, never a silent truncation.
    #[test]
    fn truncation_is_declared() {
        let html = visualize_html(&built("trunc"), Mode::Community, 1).unwrap();
        assert!(html.contains("\"truncated\":true"));
    }

    /// The binary payload has to survive the round trip, or the page renders a
    /// scrambled sky. Decode it back the way the browser does.
    #[test]
    fn binary_payload_decodes_to_the_right_geometry() {
        let store = built("bin");
        let html = visualize_html(&store, Mode::Community, 10_000).unwrap();
        let count: usize = grab(&html, "\"count\":").parse().unwrap();
        let pos = unb64(&grab_str(&html, "\"pos\":\""));
        assert_eq!(pos.len(), count * 8, "two f32 per node");
        assert_eq!(unb64(&grab_str(&html, "\"mod\":\"")).len(), count * 2);
        assert_eq!(unb64(&grab_str(&html, "\"mag\":\"")).len(), count * 2);
        let edge_count: usize = grab(&html, "\"edgeCount\":").parse().unwrap();
        assert_eq!(
            unb64(&grab_str(&html, "\"edges\":\"")).len(),
            edge_count * 8
        );

        // First node's x must decode to a finite coordinate, not garbage.
        let x = f32::from_le_bytes([pos[0], pos[1], pos[2], pos[3]]);
        assert!(x.is_finite(), "decoded coordinate is not finite: {x}");
    }

    fn grab(h: &str, key: &str) -> String {
        let i = h.find(key).expect(key) + key.len();
        h[i..].chars().take_while(|c| c.is_ascii_digit()).collect()
    }
    fn grab_str(h: &str, key: &str) -> String {
        let i = h.find(key).expect(key) + key.len();
        h[i..].chars().take_while(|c| *c != '"').collect()
    }
    /// Mirror of the page's decoder, so the test fails if `b64` ever drifts.
    fn unb64(s: &str) -> Vec<u8> {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let idx = |c: u8| A.iter().position(|&a| a == c).unwrap() as u32;
        let raw: Vec<u8> = s.bytes().filter(|c| *c != b'=').collect();
        let mut out = Vec::new();
        for chunk in raw.chunks(4) {
            let mut n = 0u32;
            for (i, c) in chunk.iter().enumerate() {
                n |= idx(*c) << (18 - 6 * i);
            }
            let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
            out.extend_from_slice(&bytes[..chunk.len() - 1]);
        }
        out
    }

    /// Regression: cross-region links used to live only in the aggregate trunks,
    /// which draw when zoomed out — so zooming in made real connections vanish.
    /// Every asserted edge must be in the index buffer the renderer uses.
    #[test]
    fn cross_region_links_are_in_the_drawable_edge_set() {
        let store = built("cross");
        let asserted = store
            .all_edges()
            .unwrap()
            .into_iter()
            .filter(|e| e.tier != "AMBIGUOUS" && e.source != e.target)
            .map(|e| {
                let (a, b) = (e.source, e.target);
                if a < b {
                    (a, b)
                } else {
                    (b, a)
                }
            })
            .collect::<std::collections::HashSet<_>>();
        let html = visualize_html(&store, Mode::Community, 10_000).unwrap();
        let drawn: usize = grab(&html, "\"edgeCount\":").parse().unwrap();
        assert_eq!(
            drawn,
            asserted.len(),
            "every asserted link must be drawable, not just the intra-region ones"
        );
    }

    /// The count of links deliberately withheld has to be stated, or the plate
    /// implies the graph is denser and more certain than it is.
    #[test]
    fn omitted_ambiguous_links_are_declared() {
        let dir = std::env::temp_dir().join("chitra_viz_declared");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.py"), "def dup():\n    return 1\n").unwrap();
        std::fs::write(dir.join("y.py"), "def dup():\n    return 2\n").unwrap();
        std::fs::write(dir.join("z.py"), "def call():\n    return dup()\n").unwrap();
        let mut store = Store::open_in_memory().unwrap();
        crate::build(&mut store, &dir).unwrap();
        let html = visualize_html(&store, Mode::Community, 10_000).unwrap();
        let declared: usize = grab(&html, "\"ambiguous\":").parse().unwrap();
        assert!(
            declared > 0,
            "ambiguous links exist but the page claims none"
        );
    }

    /// Placement must follow the graph. Alphabetical order put connected symbols
    /// 1.34x *further* apart than random pairs from the same region.
    #[test]
    fn layout_places_connected_symbols_closer_than_random() {
        let dir = std::env::temp_dir().join("chitra_viz_layout");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A chain a..z where each calls the next: neighbours in the graph, but
        // adjacent in the alphabet too, so shuffle the names against the order.
        let mut src = String::new();
        for i in 0..26u8 {
            let name = (b'a' + i) as char;
            let next = (b'a' + (25 - i)) as char;
            src.push_str(&format!("def z{name}():\n    return z{next}()\n"));
        }
        std::fs::write(dir.join("chain.py"), src).unwrap();
        let mut store = Store::open_in_memory().unwrap();
        crate::build(&mut store, &dir).unwrap();

        let html = visualize_html(&store, Mode::Community, 10_000).unwrap();
        let pos = unb64(&grab_str(&html, "\"pos\":\""));
        let edges = unb64(&grab_str(&html, "\"edges\":\""));
        let xy = |i: usize| {
            let f =
                |o: usize| f32::from_le_bytes([pos[o], pos[o + 1], pos[o + 2], pos[o + 3]]) as f64;
            (f(i * 8), f(i * 8 + 4))
        };
        let idx = |o: usize| {
            u32::from_le_bytes([edges[o], edges[o + 1], edges[o + 2], edges[o + 3]]) as usize
        };
        let n = pos.len() / 8;
        let mut edge_total = 0.0;
        let mut edge_n = 0.0;
        for o in (0..edges.len()).step_by(8) {
            let (a, b) = (xy(idx(o)), xy(idx(o + 4)));
            edge_total += (a.0 - b.0).hypot(a.1 - b.1);
            edge_n += 1.0;
        }
        // Mean distance over all pairs — the null hypothesis.
        let mut all_total = 0.0;
        let mut all_n = 0.0;
        for i in 0..n {
            for j in (i + 1)..n {
                let (a, b) = (xy(i), xy(j));
                all_total += (a.0 - b.0).hypot(a.1 - b.1);
                all_n += 1.0;
            }
        }
        let ratio = (edge_total / edge_n) / (all_total / all_n);
        println!("connected/random distance ratio = {ratio:.2}");
        assert!(
            ratio < 0.9,
            "layout must place connected symbols closer than average, got {ratio:.2}"
        );
    }

    /// The console is only worth shipping if you can interrogate it. A template
    /// edit that quietly drops a control should fail here, not in someone's
    /// browser.
    #[test]
    fn the_console_keeps_its_controls() {
        let html = visualize_html(&built("controls"), Mode::Community, 10_000).unwrap();
        for hook in [
            "id=\"q\"",       // search
            "id=\"filters\"", // file-type chips
            "id=\"sel\"",     // selection readout
            "id=\"trace\"",   // call-path trace
            "id=\"crumb\"",   // module focus breadcrumb
            "s-blast",        // blast radius figure
            "shortestPath",   // path finding
            "buildCascade",   // hop-by-hop blast radius
            "prefers-reduced-motion",
            "aria-pressed",
        ] {
            assert!(html.contains(hook), "console lost `{hook}`");
        }
    }

    #[test]
    fn ambiguous_edges_are_never_drawn() {
        let dir = std::env::temp_dir().join("chitra_viz_amb");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.py"), "def dup():\n    return 1\n").unwrap();
        std::fs::write(dir.join("y.py"), "def dup():\n    return 2\n").unwrap();
        std::fs::write(dir.join("z.py"), "def call():\n    return dup()\n").unwrap();
        let mut store = Store::open_in_memory().unwrap();
        crate::build(&mut store, &dir).unwrap();
        let html = visualize_html(&store, Mode::Community, 10_000).unwrap();
        // `dup()` is ambiguous, so no edge for it may reach the renderer.
        assert!(html.contains("\"edgeCount\":0"), "ambiguous edge was drawn");
    }
}
