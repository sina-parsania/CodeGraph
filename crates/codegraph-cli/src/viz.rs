//! Human-facing outputs: `codegraph report` (deterministic Markdown insights —
//! no LLM, every number from the graph) and `codegraph html` (self-contained
//! interactive explorer: canvas force layout, search, communities, table view;
//! zero external requests, works offline and in an email attachment).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::Result;
use codegraph_core::{EdgeRelation, Node, NodeLabel};
use codegraph_graph::{detect_entry_points, LoadedGraph};
use codegraph_store::Store;

const HTML_TEMPLATE: &str = include_str!("viz_template.html");

/// Kinds shown in the explorer / counted as "symbols" in the report.
fn is_symbol(n: &Node) -> bool {
    !matches!(n.label, NodeLabel::File | NodeLabel::Document)
}

fn meta_u64(n: &Node, key: &str) -> u64 {
    n.metadata.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
}

/// Kinds eligible for "most central"/"key symbol" rankings — the subset of
/// `is_symbol` an agent can actually navigate to (matches `LoadedGraph::important`).
fn is_ranked_symbol(n: &Node) -> bool {
    matches!(
        n.label,
        NodeLabel::Function | NodeLabel::Method | NodeLabel::Class | NodeLabel::Interface | NodeLabel::Route
    )
}

/// Hub score from the persisted analytics (same formula as `important`).
fn damped(n: &Node) -> f64 {
    codegraph_graph::hub_score(n.pagerank, meta_u64(n, "fan_in") as u32, meta_u64(n, "fan_out") as u32)
}

/// Most common `a/b` directory prefix of a set of nodes — the human name for a
/// community ("crates/codegraph-store", "src/auth", …).
fn dominant_prefix(nodes: &[&Node]) -> String {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for n in nodes {
        let mut parts = n.file_path.split('/');
        let prefix = match (parts.next(), parts.next(), parts.next()) {
            (Some(a), Some(b), Some(_)) => format!("{a}/{b}"),
            (Some(a), Some(_), None) => a.to_string(),
            (Some(a), None, _) => a.to_string(),
            _ => continue,
        };
        *counts.entry(prefix).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
        .map(|(p, _)| p)
        .unwrap_or_else(|| "misc".to_string())
}

pub fn report(root: &Path, db: &Path) -> Result<String> {
    let store = Store::open(db)?;
    let nodes = store.graph_nodes()?;
    let edges = store.graph_edges()?;
    let project = root
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "project".into());

    let mut out = String::new();
    let mut w = |s: String| {
        out.push_str(&s);
        out.push('\n');
    };

    w(format!("# CodeGraph report — {project}"));
    w(format!(
        "\n_Deterministic snapshot (no LLM involved) — codegraph v{}, graph generation {}._\n",
        codegraph_core::VERSION,
        codegraph_store::generation(db)
    ));

    // ---- overview ----
    let mut by_label: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_lang: BTreeMap<String, usize> = BTreeMap::new();
    for n in &nodes {
        *by_label.entry(format!("{:?}", n.label)).or_default() += 1;
        if is_symbol(n) && !n.language.is_empty() {
            *by_lang.entry(n.language.clone()).or_default() += 1;
        }
    }
    let mut by_rel: BTreeMap<String, usize> = BTreeMap::new();
    for e in &edges {
        *by_rel.entry(format!("{:?}", e.relation)).or_default() += 1;
    }
    w("## Overview".into());
    w(format!("- **{} nodes**, **{} edges**", nodes.len(), edges.len()));
    w(format!(
        "- nodes by kind: {}",
        by_label.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(" · ")
    ));
    w(format!(
        "- edges by relation: {}",
        by_rel.iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(" · ")
    ));
    w(format!(
        "- languages: {}",
        by_lang.iter().map(|(k, v)| format!("{k} ({v})")).collect::<Vec<_>>().join(", ")
    ));

    // ---- resolution quality ----
    let total_calls = store.all_calls()?.len();
    let external_calls = store.external_bound_call_sites().unwrap_or(0);
    let internal_calls = total_calls.saturating_sub(external_calls);
    let resolved_calls = edges.iter().filter(|e| e.relation == EdgeRelation::Calls).count();
    w("\n## Call-resolution quality".into());
    if internal_calls > 0 {
        w(format!(
            "- {resolved_calls} resolved CALLS edges from {internal_calls} resolvable call sites \
             ({:.1}% — {external_calls} sites are excluded as UNRESOLVABLE in-repo: bound to an \
             external-package import or naming no in-repo definition; the rest are ambiguous or \
             unresolved; precision over recall by design)",
            resolved_calls as f64 * 100.0 / internal_calls as f64
        ));
    }
    if let Ok((_, rows)) = codegraph_store::query_readonly(
        db,
        "SELECT json_extract(data,'$.metadata.justification') j, COUNT(*) c \
         FROM edges WHERE relation='Calls' GROUP BY j ORDER BY c DESC, j",
        20,
    ) {
        let parts: Vec<String> = rows
            .iter()
            .filter(|r| !r[0].is_empty())
            .map(|r| format!("{} {}", r[0], r[1]))
            .collect();
        if !parts.is_empty() {
            w(format!("- by resolution tier: {}", parts.join(" · ")));
        }
    }
    // ---- measured precision (codegraph audit) ----
    if let Ok(Some(raw)) = store.meta_get("audit_result") {
        if let Ok(audit) = serde_json::from_str::<serde_json::Value>(&raw) {
            w("\n## Measured precision (vs compiler oracle — `codegraph audit`)".into());
            if let Some(langs) = audit.get("oracle_languages").and_then(|l| l.as_array()) {
                let langs: Vec<&str> = langs.iter().filter_map(|v| v.as_str()).collect();
                w(format!(
                    "_Lower bound, measured on oracle-covered code only (oracle languages: {}); tiers with checked=0 are not verified by this oracle._",
                    if langs.is_empty() { "?".into() } else { langs.join(", ") }
                ));
            }
            let audited_gen = audit.get("generation").and_then(|g| g.as_u64()).unwrap_or(0);
            let current_gen = codegraph_store::generation(db);
            if audited_gen < current_gen {
                w(format!(
                    "> ⚠ audit ran at graph generation {audited_gen}, current is {current_gen} — re-run `codegraph audit` to refresh."
                ));
            }
            if let Some(tiers) = audit.get("tiers").and_then(|t| t.as_object()) {
                w("| tier | checked | confirmed | precision |".into());
                w("|---|---:|---:|---:|".into());
                for (tier, t) in tiers {
                    let p = t
                        .get("precision")
                        .and_then(|p| p.as_f64())
                        .map(|p| format!("{:.1}%", p * 100.0))
                        .unwrap_or_else(|| "—".into());
                    w(format!(
                        "| {tier} | {} | {} | {p} |",
                        t.get("checked").and_then(|v| v.as_u64()).unwrap_or(0),
                        t.get("confirmed").and_then(|v| v.as_u64()).unwrap_or(0),
                    ));
                }
            }
            if let Some(overall) = audit.get("overall").and_then(|o| o.get("precision")).and_then(|p| p.as_f64()) {
                w(format!("- overall: **{:.1}%** (sampled {} edges, seed {})",
                    overall * 100.0,
                    audit.get("sampled").and_then(|v| v.as_u64()).unwrap_or(0),
                    audit.get("seed").and_then(|v| v.as_u64()).unwrap_or(0)));
            }
        }
    }

    // ---- most central ----
    w("\n## Most central symbols (PageRank)".into());
    w("| symbol | kind | location | fan-in | fan-out |".into());
    w("|---|---|---|---:|---:|".into());
    let mut central: Vec<(&Node, f64)> = nodes
        .iter()
        .filter(|n| is_ranked_symbol(n))
        .map(|n| (n, damped(n)))
        .collect();
    central.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.id.cmp(&b.0.id)));
    for (n, _) in central.iter().take(15) {
        w(format!(
            "| `{}` | {:?} | {}:{} | {} | {} |",
            n.name, n.label, n.file_path, n.line_start, meta_u64(n, "fan_in"), meta_u64(n, "fan_out")
        ));
    }

    // ---- hotspots ----
    w("\n## Hotspots (high fan-in × complexity, test-gap flagged)".into());
    w("| symbol | location | fan-in | complexity | tested |".into());
    w("|---|---|---:|---:|---|".into());
    let mut hot: Vec<(&Node, u64)> = nodes
        .iter()
        .filter(|n| matches!(n.label, NodeLabel::Function | NodeLabel::Method))
        .map(|n| (n, meta_u64(n, "fan_in") * (1 + meta_u64(n, "complexity"))))
        .filter(|(_, s)| *s > 0)
        .collect();
    hot.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.id.cmp(&b.0.id)));
    for (n, _) in hot.iter().take(10) {
        let tested = store.has_test_reference(&n.name).unwrap_or(false);
        w(format!(
            "| `{}` | {}:{} | {} | {} | {} |",
            n.name, n.file_path, n.line_start,
            meta_u64(n, "fan_in"), meta_u64(n, "complexity"),
            if tested { "yes" } else { "**NO**" }
        ));
    }

    // ---- communities ----
    w("\n## Communities (structural clusters)".into());
    let mut comms: HashMap<u32, Vec<&Node>> = HashMap::new();
    for n in nodes.iter().filter(|n| is_symbol(n)) {
        if let Some(c) = n.community {
            comms.entry(c).or_default().push(n);
        }
    }
    let mut comms: Vec<(u32, Vec<&Node>)> = comms.into_iter().collect();
    comms.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
    for (cid, members) in comms.iter().take(8) {
        // Key symbols: real ranked symbols under the same sink damping as
        // `important` — a community keyed by its most-called leaf helper says
        // nothing about what the cluster does.
        let mut top: Vec<(&&Node, f64)> =
            members.iter().filter(|n| is_ranked_symbol(n)).map(|n| (n, damped(n))).collect();
        top.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.id.cmp(&b.0.id)));
        let names: Vec<String> = top.iter().take(3).map(|(n, _)| format!("`{}`", n.name)).collect();
        w(format!(
            "- **{}** — {} symbols (community {cid}); key: {}",
            dominant_prefix(members),
            members.len(),
            names.join(", ")
        ));
    }

    // ---- flows ----
    let lg = LoadedGraph::load(&nodes, &edges);
    let entries = detect_entry_points(&nodes);
    let mut flows: Vec<(String, &'static str, usize, f64)> = entries
        .iter()
        .filter_map(|(n, kind)| {
            let body = lg.flow_from(&n.id, 6);
            if body.is_empty() {
                return None;
            }
            let by_id: f64 = body
                .iter()
                .filter_map(|id| nodes.iter().find(|x| &x.id == id))
                .map(|x| x.pagerank)
                .sum();
            Some((codegraph_core::display_label(n), *kind, body.len(), by_id * (1.0 + body.len() as f64).ln()))
        })
        .collect();
    flows.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    if !flows.is_empty() {
        w("\n## Critical execution flows (entry points by reach × centrality)".into());
        for (label, kind, reach, _) in flows.iter().take(10) {
            w(format!("- `{label}` [{kind}] — reaches {reach} symbols"));
        }
    }

    // ---- API surface ----
    let mut routes = store.nodes_by_label("Route")?;
    routes.sort_by(|a, b| a.name.cmp(&b.name));
    if !routes.is_empty() {
        w(format!("\n## API surface ({} routes)", routes.len()));
        for r in routes.iter().take(20) {
            w(format!("- `{}` — {}:{}", r.name, r.file_path, r.line_start));
        }
        if routes.len() > 20 {
            w(format!("- … and {} more (`codegraph routes`)", routes.len() - 20));
        }
    }

    // ---- dead code ----
    let dead = store.dead_code_candidates(10)?;
    if !dead.is_empty() {
        w("\n## Dead-code candidates (verify before deleting — static view)".into());
        for n in &dead {
            w(format!("- `{}` — {}:{}", n.name, n.file_path, n.line_start));
        }
    }

    // ---- co-change ----
    let pairs = store.top_cochanges(10)?;
    if !pairs.is_empty() {
        w("\n## Frequently co-changed files (git history)".into());
        for (a, b, n) in &pairs {
            w(format!("- {a} ⇄ {b} ({n}×)"));
        }
    }

    // ---- health ----
    w("\n## Graph health".into());
    let violations = store.validate_graph()?;
    if violations.is_empty() {
        w("- ✓ no invariant violations (dangling edges, FQN schema, justification tags)".into());
    } else {
        for v in violations.iter().take(10) {
            w(format!("- ✗ {v}"));
        }
    }

    Ok(out)
}

/// A Barnes-Hut quadtree cell (flat-arena node): a leaf holds one point, an
/// internal cell holds its 4 children + center-of-mass. `theta` decides when a
/// far cell is summarised by its COM instead of recursed — O(n log n) repulsion.
#[derive(Clone, Copy)]
struct BhCell {
    // bounds
    x: f32,
    y: f32,
    half: f32,
    // center of mass + count
    cx: f32,
    cy: f32,
    mass: f32,
    child: [i32; 4], // arena indices, -1 = empty
    point: i32,      // point index for a leaf, -1 otherwise
}

impl BhCell {
    fn empty(x: f32, y: f32, half: f32) -> Self {
        BhCell { x, y, half, cx: 0.0, cy: 0.0, mass: 0.0, child: [-1; 4], point: -1 }
    }
    fn quadrant(&self, px: f32, py: f32) -> usize {
        (if px >= self.x { 1 } else { 0 }) | (if py >= self.y { 2 } else { 0 })
    }
}

fn bh_insert(arena: &mut Vec<BhCell>, cell: usize, p: usize, pos: &[(f32, f32)]) {
    let (px, py) = pos[p];
    arena[cell].cx += px; // accumulate COM as we descend
    arena[cell].cy += py;
    arena[cell].mass += 1.0;
    if arena[cell].point == -1 && arena[cell].child == [-1; 4] {
        arena[cell].point = p as i32; // empty leaf → hold the point
        return;
    }
    if arena[cell].point != -1 {
        if arena[cell].half < 0.5 {
            return; // coincident/degenerate: merge into COM, don't subdivide forever
        }
        let existing = arena[cell].point; // split: push the existing point down
        arena[cell].point = -1;
        bh_place(arena, cell, existing as usize, pos);
    }
    bh_place(arena, cell, p, pos);
}

fn bh_place(arena: &mut Vec<BhCell>, cell: usize, p: usize, pos: &[(f32, f32)]) {
    let (px, py) = pos[p];
    let q = arena[cell].quadrant(px, py);
    let mut child = arena[cell].child[q];
    if child == -1 {
        let (cx0, cy0, half) = (arena[cell].x, arena[cell].y, arena[cell].half * 0.5);
        let nx = cx0 + if q & 1 == 1 { half } else { -half };
        let ny = cy0 + if q & 2 == 2 { half } else { -half };
        arena.push(BhCell::empty(nx, ny, half));
        child = (arena.len() - 1) as i32;
        arena[cell].child[q] = child;
    }
    bh_insert(arena, child as usize, p, pos);
}

/// Repulsion on point `p` from the tree, Barnes-Hut approximation (theta²=0.81).
/// `strength` = k²·alpha (ideal-length² × cooling); force = strength·mass/d².
fn bh_force(arena: &[BhCell], cell: usize, p: usize, pos: &[(f32, f32)], strength: f32, acc: &mut (f32, f32)) {
    let c = &arena[cell];
    if c.mass == 0.0 {
        return;
    }
    let (px, py) = pos[p];
    let (mx, my) = (c.cx / c.mass, c.cy / c.mass);
    let mut dx = px - mx;
    let mut dy = py - my;
    let mut d2 = dx * dx + dy * dy;
    let is_leaf = c.point != -1;
    if is_leaf && c.point as usize == p {
        return;
    }
    // Far enough (cell width² < theta² · dist²) OR a leaf → summarise by COM.
    if is_leaf || (2.0 * c.half) * (2.0 * c.half) < 0.81 * d2 {
        if d2 < 1.0 {
            dx = ((p % 3) as f32) - 1.0;
            dy = 0.7;
            d2 = 1.0;
        }
        let f = strength * c.mass / d2;
        let d = d2.sqrt();
        acc.0 += dx / d * f;
        acc.1 += dy / d * f;
    } else {
        for &ch in &c.child {
            if ch != -1 {
                bh_force(arena, ch as usize, p, pos, strength, acc);
            }
        }
    }
}

/// Deterministic force-directed layout computed HERE (Rust), not in the browser:
/// a 40k-node live sim freezes a page, but native Barnes-Hut (O(n log n)) converges
/// in ~1s and the browser then just draws static coordinates (smooth pan/zoom at
/// any size). Deterministic: index-seeded start, fixed traversal order, no RNG.
fn layout(n: usize, edges: &[[usize; 3]], comm: &[usize], n_slots: usize) -> Vec<(f32, f32)> {
    if n == 0 {
        return Vec::new();
    }
    let _ = (comm, n_slots); // community drives COLOR, not position — layout is pure force
    // Fruchterman-Reingold style: repulsion between all nodes (Barnes-Hut),
    // attraction along edges, gentle centering to keep the drawing framed.
    let k = 90.0f32; // ideal edge length
    let mut pos: Vec<(f32, f32)> = (0..n)
        .map(|i| {
            let a = i as f32 * 2.399963; // golden-angle disk seed (deterministic)
            let r = (i as f32).sqrt() * 8.0 + 1.0;
            (a.cos() * r, a.sin() * r)
        })
        .collect();
    let mut vel = vec![(0.0f32, 0.0f32); n];
    let iters = 260;
    let mut alpha = 1.0f32;
    for _ in 0..iters {
        // Attraction along edges: f_a = d²/k toward each other (FR).
        for e in edges {
            let (s, t) = (e[0], e[1]);
            let dx = pos[t].0 - pos[s].0;
            let dy = pos[t].1 - pos[s].1;
            let d = (dx * dx + dy * dy).sqrt().max(0.01);
            let f = d / k * alpha; // proportional pull (spring), scaled by cooling
            let (ux, uy) = (dx / d, dy / d);
            vel[s].0 += ux * f;
            vel[s].1 += uy * f;
            vel[t].0 -= ux * f;
            vel[t].1 -= uy * f;
        }
        // Barnes-Hut repulsion (f_r = k²/d): build the quadtree over positions.
        let (mut lo, mut hi) = (f32::MAX, f32::MIN);
        for &(x, y) in &pos {
            lo = lo.min(x).min(y);
            hi = hi.max(x).max(y);
        }
        let half = ((hi - lo) * 0.5).max(1.0) + 1.0;
        let mid = (lo + hi) * 0.5;
        let mut arena: Vec<BhCell> = Vec::with_capacity(n * 2);
        arena.push(BhCell::empty(mid, mid, half));
        for p in 0..n {
            bh_insert(&mut arena, 0, p, &pos);
        }
        let k2 = k * k;
        for i in 0..n {
            let mut acc = (0.0f32, 0.0f32);
            bh_force(&arena, 0, i, &pos, k2 * alpha, &mut acc);
            vel[i].0 += acc.0;
            vel[i].1 += acc.1;
            vel[i].0 -= pos[i].0 * 0.006 * alpha; // gentle centering (no artificial anchors)
            vel[i].1 -= pos[i].1 * 0.006 * alpha;
        }
        // Cooling: cap displacement so it settles instead of oscillating.
        let cap = 40.0 + 260.0 * alpha;
        for i in 0..n {
            let (vx, vy) = (vel[i].0, vel[i].1);
            let vm = (vx * vx + vy * vy).sqrt().max(0.001);
            let s = vm.min(cap) / vm;
            pos[i].0 += vx * s;
            pos[i].1 += vy * s;
            vel[i].0 *= 0.85;
            vel[i].1 *= 0.85;
        }
        alpha = (alpha - 1.0 / iters as f32).max(0.02);
    }
    pos
}

pub fn html(root: &Path, db: &Path, limit: usize) -> Result<String> {
    let store = Store::open(db)?;
    let all = store.graph_nodes()?;
    let edges = store.graph_edges()?;
    let project = root
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "project".into());

    // `limit == 0` renders the WHOLE graph (grid-bucketed force sim handles it);
    // otherwise the top `limit` symbols by PageRank — a graph a human can read.
    let mut picked: Vec<&Node> = all.iter().filter(|n| is_symbol(n)).collect();
    picked.sort_by(|a, b| b.pagerank.partial_cmp(&a.pagerank).unwrap_or(std::cmp::Ordering::Equal).then(a.id.cmp(&b.id)));
    if limit > 0 {
        picked.truncate(limit);
    }
    let idx: HashMap<&str, usize> = picked.iter().enumerate().map(|(i, n)| (n.id.as_str(), i)).collect();

    const RELS: [EdgeRelation; 5] = [
        EdgeRelation::Calls, EdgeRelation::Inherits, EdgeRelation::Implements,
        EdgeRelation::HttpCalls, EdgeRelation::Tests,
    ];
    let mut e_out: Vec<[usize; 3]> = Vec::new();
    for e in &edges {
        let Some(r) = RELS.iter().position(|r| *r == e.relation) else { continue };
        if let (Some(&s), Some(&t)) = (idx.get(e.src.as_str()), idx.get(e.dst.as_str())) {
            e_out.push([s, t, r]);
        }
    }

    // Communities present among the picked nodes → display slots 0..7 by size,
    // everything else folds into slot 8 ("other") — fixed order, never cycled.
    let mut sizes: HashMap<u32, usize> = HashMap::new();
    for n in &picked {
        if let Some(c) = n.community {
            *sizes.entry(c).or_default() += 1;
        }
    }
    let mut ranked: Vec<(u32, usize)> = sizes.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let slot_of: HashMap<u32, usize> =
        ranked.iter().take(8).enumerate().map(|(slot, (c, _))| (*c, slot)).collect();
    let mut legend: Vec<String> = Vec::new();
    for (c, _) in ranked.iter().take(8) {
        let members: Vec<&Node> =
            picked.iter().filter(|n| n.community == Some(*c)).copied().collect();
        let mut label = dominant_prefix(&members);
        if legend.contains(&label) {
            // several clusters inside one directory — disambiguate by the
            // cluster's most central symbol
            if let Some(top) = members.first() {
                label = format!("{label} · {}", top.name);
            }
        }
        legend.push(label);
    }
    legend.push("other".into());

    // Precompute the layout server-side (see `layout`): the browser draws static
    // coordinates instead of running a physics sim that would freeze on 40k nodes.
    let comm: Vec<usize> =
        picked.iter().map(|n| n.community.and_then(|c| slot_of.get(&c).copied()).unwrap_or(8)).collect();
    let pos = layout(picked.len(), &e_out, &comm, legend.len());

    let nodes_json: Vec<serde_json::Value> = picked
        .iter()
        .enumerate()
        .map(|(i, n)| {
            serde_json::json!({
                "n": n.name, "l": format!("{:?}", n.label), "f": n.file_path, "ln": n.line_start,
                "c": comm[i], "x": pos[i].0, "y": pos[i].1,
                "pr": n.pagerank, "fi": meta_u64(n, "fan_in"), "fo": meta_u64(n, "fan_out"),
            })
        })
        .collect();
    let data = serde_json::json!({
        "nodes": nodes_json,
        "edges": e_out,
        "rels": RELS.iter().map(|r| format!("{r:?}")).collect::<Vec<_>>(),
        "legend": legend,
    });

    Ok(HTML_TEMPLATE
        .replace("__CODEGRAPH_TITLE__", &project)
        .replace("__CODEGRAPH_DATA__", &serde_json::to_string(&data)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_and_html_generate_from_indexed_repo() {
        let tmp = std::env::temp_dir().join(format!("cg_viz_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("a.py"),
            "def helper():\n    return 1\n\ndef caller():\n    helper()\n",
        )
        .unwrap();
        let db = tmp.join("g.db");
        crate::index::index_dir(&tmp, &db, false, None, false, None).unwrap();

        let md = report(&tmp, &db).unwrap();
        assert!(md.contains("# CodeGraph report"));
        assert!(md.contains("Most central symbols"));
        assert!(md.contains("helper"));
        assert!(md.contains("no invariant violations"), "healthy graph reports clean:\n{md}");

        let page = html(&tmp, &db, 100).unwrap();
        assert!(!page.contains("__CODEGRAPH_DATA__"), "data placeholder substituted");
        assert!(!page.contains("__CODEGRAPH_TITLE__"), "title placeholder substituted");
        assert!(page.contains("\"helper\""), "symbol embedded in data");
        assert!(page.contains("<canvas"), "canvas explorer present");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
