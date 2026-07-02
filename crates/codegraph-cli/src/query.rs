//! Graph traversal/ranking queries shared by the CLI commands.

use std::path::Path;

use anyhow::Result;
use codegraph_core::Node;
use codegraph_graph::LoadedGraph;
use codegraph_store::Store;

pub struct Loaded {
    pub lg: LoadedGraph,
    pub nodes: Vec<Node>,
}

impl Loaded {
    pub fn open(db: &Path) -> Result<Loaded> {
        let store = Store::open(db)?;
        let nodes = store.all_nodes()?;
        let edges = store.all_edges()?;
        let lg = LoadedGraph::load(&nodes, &edges);
        Ok(Loaded { lg, nodes })
    }

    pub fn resolve(&self, name: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.name == name)
    }

    pub fn fmt(&self, id: &str) -> String {
        match self.nodes.iter().find(|n| n.id == id) {
            Some(n) => format!("{:<24} {:?}  {}:{}", n.name, n.label, n.file_path, n.line_start),
            None => id.to_string(),
        }
    }
}

/// Turn a natural-language question into an FTS5 OR-query of identifier-ish tokens.
pub fn fts_query_from(q: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    // Split on every non-identifier char (so `fix(order-flow):` → order, flow) and
    // prefix-match each token (`order*` matches the camelCase token `OrderCheckout…`).
    q.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| t.len() > 2)
        .filter(|t| seen.insert(t.to_lowercase()))
        .take(8)
        .map(|t| format!("{t}*"))
        .collect::<Vec<_>>()
        .join(" OR ")
}


/// Read up to ~12 source lines for a node, for richer `ask` context.
pub fn read_snippet(root: &std::path::Path, file_path: &str, start: u32, end: u32) -> Option<String> {
    let content = std::fs::read_to_string(root.join(file_path)).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    let s = start.saturating_sub(1) as usize;
    if s >= lines.len() {
        return None;
    }
    let e = (end as usize).min(s + 12).min(lines.len()).max(s + 1);
    Some(lines[s..e].join("\n"))
}

/// LLM rerank: ask the model to reorder hits by relevance to the query.
/// Best-effort — falls back to the original order on any parse failure.
pub fn rerank(query: &str, hits: Vec<codegraph_core::Node>) -> Vec<codegraph_core::Node> {
    if hits.len() < 2 {
        return hits;
    }
    let listing: String = hits
        .iter()
        .enumerate()
        .map(|(i, n)| format!("{}. {} ({:?}) {}", i, n.name, n.label, n.file_path))
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = format!(
        "Rank these code symbols by relevance to the query \"{}\". Reply with ONLY the leading numbers, best first, comma-separated.\n\n{}",
        query, listing
    );
    let Some(resp) = codegraph_llm::generate_text(&prompt, 200) else { return hits };
    let order: Vec<usize> = resp
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|t| t.parse::<usize>().ok())
        .filter(|&i| i < hits.len())
        .collect();
    if order.is_empty() {
        return hits;
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for &i in &order {
        if seen.insert(i) {
            out.push(hits[i].clone());
        }
    }
    for (i, n) in hits.iter().enumerate() {
        if !seen.contains(&i) {
            out.push(n.clone());
        }
    }
    out
}
