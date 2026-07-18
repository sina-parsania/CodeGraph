//! SCIP import: turn a compiler-grade `.scip` index into Tier-A graph edges.
//!
//! A `.scip` file (produced by scip-typescript, rust-analyzer, scip-java, …)
//! records, per document, exact occurrences of every symbol with a role
//! (definition vs reference) and a source range. We map those occurrences onto
//! the nodes we already parsed (by file + line) and emit precise CALLS / USES
//! edges at `ResolutionTier::Scip` — the highest-confidence resolution we have.

use std::collections::{HashMap, HashSet};

use codegraph_core::{Confidence, Edge, EdgeRelation, Metadata, Node, NodeLabel, ResolutionTier};
use protobuf::Message;
use scip::types::Index;

const ROLE_DEFINITION: i32 = 1; // SymbolRole::Definition bit

/// Parse `.scip` bytes and resolve precise edges against already-parsed nodes.
/// File paths in the index must be repo-relative (the index was generated at root).
pub fn import_scip(bytes: &[u8], nodes: &[Node]) -> anyhow::Result<Vec<Edge>> {
    let index = Index::parse_from_bytes(bytes)?;

    let mut by_file: HashMap<&str, Vec<&Node>> = HashMap::new();
    for n in nodes {
        by_file.entry(n.file_path.as_str()).or_default().push(n);
    }

    // Pass 1: symbol -> defining node (first definition wins).
    let mut sym_def: HashMap<&str, &Node> = HashMap::new();
    for doc in &index.documents {
        let Some(file_nodes) = by_file.get(doc.relative_path.as_str()) else {
            continue;
        };
        for occ in &doc.occurrences {
            if occ.symbol_roles & ROLE_DEFINITION == 0 || occ.range.is_empty() {
                continue;
            }
            let line = occ.range[0] as u32 + 1;
            if let Some(n) = best_def_node(file_nodes, line) {
                sym_def.entry(occ.symbol.as_str()).or_insert(n);
            }
        }
    }

    // Pass 2: each reference inside a callable -> edge to the symbol's def node.
    let mut edges = Vec::new();
    let mut seen: HashSet<(&str, &str, EdgeRelation)> = HashSet::new();
    for doc in &index.documents {
        let Some(file_nodes) = by_file.get(doc.relative_path.as_str()) else {
            continue;
        };
        for occ in &doc.occurrences {
            if occ.symbol_roles & ROLE_DEFINITION != 0 || occ.range.is_empty() {
                continue;
            }
            let Some(&dst) = sym_def.get(occ.symbol.as_str()) else {
                continue;
            };
            let line = occ.range[0] as u32 + 1;
            let Some(src) = enclosing_callable(file_nodes, line) else {
                continue;
            };
            if src.id == dst.id {
                continue;
            }
            let Some(relation) = edge_relation(dst.label) else {
                continue;
            };
            if !seen.insert((src.id.as_str(), dst.id.as_str(), relation)) {
                continue;
            }
            // justification tags the merge SOURCE (vs "IndexStore") so reuse,
            // audit, and per-tier reporting can tell the compiler tiers apart.
            let mut metadata = Metadata::new();
            metadata.insert(
                "justification".into(),
                serde_json::Value::String("Scip".into()),
            );
            edges.push(Edge {
                src: src.id.clone(),
                dst: dst.id.clone(),
                relation,
                tier: ResolutionTier::Scip,
                confidence: Confidence::Extracted,
                src_file: doc.relative_path.clone(),
                src_line: line,
                metadata,
            });
        }
    }
    Ok(edges)
}

/// A reference to a function/method is a call; to a type is a use. Other targets
/// (locals, params, fields) are not graph-worthy here.
fn edge_relation(label: NodeLabel) -> Option<EdgeRelation> {
    match label {
        NodeLabel::Function | NodeLabel::Method => Some(EdgeRelation::Calls),
        NodeLabel::Class | NodeLabel::Interface | NodeLabel::Enum | NodeLabel::Type => {
            Some(EdgeRelation::UsesType)
        }
        _ => None,
    }
}

fn is_definable(label: NodeLabel) -> bool {
    edge_relation(label).is_some() || matches!(label, NodeLabel::Module)
}

/// Node whose definition starts at `line` (exact), else the innermost definable
/// node that contains it.
fn best_def_node<'a>(file_nodes: &[&'a Node], line: u32) -> Option<&'a Node> {
    let mut best: Option<&Node> = None;
    for &n in file_nodes {
        if !is_definable(n.label) {
            continue;
        }
        if n.line_start == line {
            return Some(n);
        }
        if n.line_start <= line
            && line <= n.line_end
            && best.is_none_or(|b| n.line_start > b.line_start)
        {
            best = Some(n);
        }
    }
    best
}

/// Innermost function/method whose span contains `line` (the caller).
fn enclosing_callable<'a>(file_nodes: &[&'a Node], line: u32) -> Option<&'a Node> {
    let mut best: Option<&Node> = None;
    for &n in file_nodes {
        if !matches!(n.label, NodeLabel::Function | NodeLabel::Method) {
            continue;
        }
        if n.line_start <= line
            && line <= n.line_end
            && best.is_none_or(|b| n.line_start > b.line_start)
        {
            best = Some(n);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use scip::types::{Document, Occurrence};

    fn node(id: &str, name: &str, label: NodeLabel, start: u32, end: u32) -> Node {
        Node {
            id: id.into(),
            label,
            name: name.into(),
            file_path: "a.ts".into(),
            line_start: start,
            line_end: end,
            language: "typescript".into(),
            metadata: Metadata::new(),
            community: None,
            pagerank: 0.0,
            betweenness: 0.0,
        }
    }

    fn occ(line: i32, symbol: &str, role: i32) -> Occurrence {
        let mut o = Occurrence::new();
        o.range = vec![line, 4, 9];
        o.symbol = symbol.into();
        o.symbol_roles = role;
        o
    }

    #[test]
    fn resolves_reference_to_definition() {
        // foo defined lines 1-3, bar defined lines 5-8; bar references foo at line 6.
        let mut doc = Document::new();
        doc.relative_path = "a.ts".into();
        doc.occurrences = vec![
            occ(0, "foo#", ROLE_DEFINITION),
            occ(4, "bar#", ROLE_DEFINITION),
            occ(5, "foo#", 0), // reference, 0-indexed line 5 = line 6
        ];
        let mut index = Index::new();
        index.documents = vec![doc];
        let bytes = index.write_to_bytes().unwrap();

        let nodes = vec![
            node("a.ts.foo", "foo", NodeLabel::Function, 1, 3),
            node("a.ts.bar", "bar", NodeLabel::Function, 5, 8),
        ];
        let edges = import_scip(&bytes, &nodes).unwrap();

        assert_eq!(edges.len(), 1);
        let e = &edges[0];
        assert_eq!(e.src, "a.ts.bar");
        assert_eq!(e.dst, "a.ts.foo");
        assert_eq!(e.relation, EdgeRelation::Calls);
        assert_eq!(e.tier, ResolutionTier::Scip);
    }

    #[test]
    fn ignores_unmapped_files_and_symbols() {
        let mut doc = Document::new();
        doc.relative_path = "other.ts".into(); // no nodes for this file
        doc.occurrences = vec![occ(0, "x#", ROLE_DEFINITION), occ(1, "x#", 0)];
        let mut index = Index::new();
        index.documents = vec![doc];
        let bytes = index.write_to_bytes().unwrap();
        let nodes = vec![node("a.ts.foo", "foo", NodeLabel::Function, 1, 3)];
        assert!(import_scip(&bytes, &nodes).unwrap().is_empty());
    }
}
