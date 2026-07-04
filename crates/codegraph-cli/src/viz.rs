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
    let resolved_calls = edges.iter().filter(|e| e.relation == EdgeRelation::Calls).count();
    w("\n## Call-resolution quality".into());
    if total_calls > 0 {
        w(format!(
            "- {resolved_calls} resolved CALLS edges from {total_calls} textual call sites \
             ({:.1}% — the rest are external, ambiguous, or unresolved; precision over recall by design)",
            resolved_calls as f64 * 100.0 / total_calls as f64
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

    // ---- most central ----
    w("\n## Most central symbols (PageRank)".into());
    w("| symbol | kind | location | fan-in | fan-out |".into());
    w("|---|---|---|---:|---:|".into());
    let mut central: Vec<&Node> = nodes.iter().filter(|n| is_symbol(n)).collect();
    central.sort_by(|a, b| b.pagerank.partial_cmp(&a.pagerank).unwrap_or(std::cmp::Ordering::Equal).then(a.id.cmp(&b.id)));
    for n in central.iter().take(15) {
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
        let mut top = members.clone();
        top.sort_by(|a, b| b.pagerank.partial_cmp(&a.pagerank).unwrap_or(std::cmp::Ordering::Equal).then(a.id.cmp(&b.id)));
        let names: Vec<String> = top.iter().take(3).map(|n| format!("`{}`", n.name)).collect();
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
            Some((n.name.clone(), *kind, body.len(), by_id * (1.0 + body.len() as f64).ln()))
        })
        .collect();
    flows.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    if !flows.is_empty() {
        w("\n## Critical execution flows (entry points by reach × centrality)".into());
        for (name, kind, reach, _) in flows.iter().take(10) {
            w(format!("- `{name}` [{kind}] — reaches {reach} symbols"));
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

pub fn html(root: &Path, db: &Path, limit: usize) -> Result<String> {
    let store = Store::open(db)?;
    let all = store.graph_nodes()?;
    let edges = store.graph_edges()?;
    let project = root
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "project".into());

    // Top `limit` symbols by PageRank — the graph a human can actually read.
    let mut picked: Vec<&Node> = all.iter().filter(|n| is_symbol(n)).collect();
    picked.sort_by(|a, b| b.pagerank.partial_cmp(&a.pagerank).unwrap_or(std::cmp::Ordering::Equal).then(a.id.cmp(&b.id)));
    picked.truncate(limit);
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

    let nodes_json: Vec<serde_json::Value> = picked
        .iter()
        .map(|n| {
            serde_json::json!({
                "n": n.name, "l": format!("{:?}", n.label), "f": n.file_path, "ln": n.line_start,
                "c": n.community.and_then(|c| slot_of.get(&c).copied()).unwrap_or(8),
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
