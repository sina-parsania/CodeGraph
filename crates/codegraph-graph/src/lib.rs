//! Graph build layer: turn parsed nodes + raw calls into a petgraph graph and
//! the persisted edge set. M3 lands structural (DEFINES) + intra-file CALLS
//! resolution (Tier-B, same-language only — no cross-language call edges).

use std::collections::{HashMap, HashSet};

use codegraph_core::{Confidence, Edge, EdgeRelation, Metadata, Node, NodeLabel, RawCall, ResolutionTier};
use petgraph::stable_graph::{NodeIndex, StableGraph};

/// Directed graph of node-id → node-id, edge weight = relation name.
pub type CodeGraph = StableGraph<String, String>;

pub struct Built {
    pub graph: CodeGraph,
    pub edges: Vec<Edge>,
}

/// Build the edge set + petgraph from parsed nodes and unresolved calls.
/// - Pass 1 (structural): each File DEFINES every definition in the same file.
/// - Pass 2 (calls): resolve each `RawCall` to a Function in the caller's file
///   by name (intra-language, intra-file) → CALLS edge tagged Tier B.
pub fn build(nodes: &[Node], calls: &[RawCall]) -> Built {
    let by_id: HashMap<&str, &Node> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let file_by_path: HashMap<&str, &str> = nodes
        .iter()
        .filter(|n| n.label == NodeLabel::File)
        .map(|n| (n.file_path.as_str(), n.id.as_str()))
        .collect();
    let mut fn_by_file_name: HashMap<(&str, &str), &str> = HashMap::new();
    for n in nodes.iter().filter(|n| n.label == NodeLabel::Function) {
        fn_by_file_name.insert((n.file_path.as_str(), n.name.as_str()), n.id.as_str());
    }

    let mut edges: Vec<Edge> = Vec::new();
    let mut seen: HashSet<(String, String, EdgeRelation)> = HashSet::new();

    for n in nodes.iter().filter(|n| n.label != NodeLabel::File) {
        if let Some(&file_id) = file_by_path.get(n.file_path.as_str()) {
            push_edge(&mut edges, &mut seen, Edge {
                src: file_id.to_string(),
                dst: n.id.clone(),
                relation: EdgeRelation::Defines,
                tier: ResolutionTier::TreeSitter,
                confidence: Confidence::Extracted,
                src_file: n.file_path.clone(),
                src_line: n.line_start,
                metadata: Metadata::new(),
            });
        }
    }

    for c in calls {
        let Some(caller) = by_id.get(c.caller_id.as_str()) else { continue };
        if let Some(&callee_id) = fn_by_file_name.get(&(caller.file_path.as_str(), c.callee_name.as_str())) {
            push_edge(&mut edges, &mut seen, Edge {
                src: c.caller_id.clone(),
                dst: callee_id.to_string(),
                relation: EdgeRelation::Calls,
                tier: ResolutionTier::TreeSitter,
                confidence: Confidence::Inferred,
                src_file: caller.file_path.clone(),
                src_line: c.line,
                metadata: Metadata::new(),
            });
        }
    }

    let mut graph = CodeGraph::new();
    let mut idx: HashMap<&str, NodeIndex> = HashMap::new();
    for n in nodes {
        idx.insert(n.id.as_str(), graph.add_node(n.id.clone()));
    }
    for e in &edges {
        if let (Some(&a), Some(&b)) = (idx.get(e.src.as_str()), idx.get(e.dst.as_str())) {
            graph.add_edge(a, b, format!("{:?}", e.relation));
        }
    }

    Built { graph, edges }
}

fn push_edge(
    edges: &mut Vec<Edge>,
    seen: &mut HashSet<(String, String, EdgeRelation)>,
    e: Edge,
) {
    let key = (e.src.clone(), e.dst.clone(), e.relation);
    if seen.insert(key) {
        edges.push(e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codegraph_store::Store;

    #[test]
    fn structural_and_call_edges() {
        let pf = codegraph_parse::parse_rust("proj", "src/main.rs", "fn helper() {}\nfn main() { helper(); helper(); }\n");
        let built = build(&pf.nodes, &pf.calls);

        let calls: Vec<_> = built.edges.iter().filter(|e| e.relation == EdgeRelation::Calls).collect();
        assert_eq!(calls.len(), 1, "duplicate calls should dedupe to one edge");
        assert!(calls[0].src.ends_with("main") && calls[0].dst.ends_with("helper"));
        assert!(built.edges.iter().any(|e| e.relation == EdgeRelation::Defines));
        assert_eq!(built.graph.node_count(), pf.nodes.len());
    }

    #[test]
    fn end_to_end_persist_and_query() {
        let pf = codegraph_parse::parse_rust("proj", "src/main.rs", "fn helper() {}\nfn main() { helper(); }\n");
        let built = build(&pf.nodes, &pf.calls);
        let store = Store::open_in_memory().unwrap();
        for n in &pf.nodes {
            store.upsert_node(n).unwrap();
        }
        for e in &built.edges {
            store.upsert_edge(e).unwrap();
        }
        let main_id = pf.nodes.iter().find(|n| n.name == "main").unwrap().id.clone();
        let edges = store.get_edges_for_node(&main_id).unwrap();
        assert!(edges.iter().any(|e| e.relation == EdgeRelation::Calls && e.dst.ends_with("helper")));
    }

    #[test]
    fn no_cross_file_call_resolution() {
        // a call whose name matches a function in a DIFFERENT file must NOT resolve
        let mut pf = codegraph_parse::parse_rust("proj", "a.rs", "fn main() { ghost(); }\n");
        let other = codegraph_parse::parse_rust("proj", "b.rs", "fn ghost() {}\n");
        pf.nodes.extend(other.nodes);
        let built = build(&pf.nodes, &pf.calls);
        assert!(!built.edges.iter().any(|e| e.relation == EdgeRelation::Calls));
    }
}

/// An in-memory graph loaded from the persisted store, with id↔index mapping,
/// for traversal and ranking queries (trace_path, blast-radius, callees, PageRank).
pub struct LoadedGraph {
    graph: CodeGraph,
    idx: HashMap<String, petgraph::stable_graph::NodeIndex>,
    ids: Vec<String>,
}

impl LoadedGraph {
    pub fn load(nodes: &[Node], edges: &[Edge]) -> Self {
        let mut graph = CodeGraph::new();
        let mut idx = HashMap::new();
        for n in nodes {
            idx.insert(n.id.clone(), graph.add_node(n.id.clone()));
        }
        for e in edges {
            if let (Some(&a), Some(&b)) = (idx.get(&e.src), idx.get(&e.dst)) {
                graph.add_edge(a, b, format!("{:?}", e.relation));
            }
        }
        let mut ids = vec![String::new(); graph.node_count()];
        for (id, ni) in &idx {
            ids[ni.index()] = id.clone();
        }
        LoadedGraph { graph, idx, ids }
    }

    /// Shortest dependency path (any edge) between two node ids, as an id list.
    pub fn shortest_path(&self, from: &str, to: &str) -> Option<Vec<String>> {
        let (s, g) = (*self.idx.get(from)?, *self.idx.get(to)?);
        let (_, path) = petgraph::algo::astar(&self.graph, s, |n| n == g, |_| 1, |_| 0)?;
        Some(path.into_iter().map(|ni| self.ids[ni.index()].clone()).collect())
    }

    /// Reverse reachability (who depends on `target`) up to `max_depth` hops.
    pub fn blast_radius(&self, target: &str, max_depth: usize) -> Vec<String> {
        let Some(&start) = self.idx.get(target) else { return Vec::new() };
        let mut visited: HashSet<_> = HashSet::from([start]);
        let mut frontier = vec![start];
        let mut out = Vec::new();
        for _ in 0..max_depth {
            let mut next = Vec::new();
            for &n in &frontier {
                for pred in self.graph.neighbors_directed(n, petgraph::Direction::Incoming) {
                    if visited.insert(pred) {
                        next.push(pred);
                        out.push(self.ids[pred.index()].clone());
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        out
    }

    /// Direct callees (outgoing CALLS edges) of a node id.
    pub fn callees(&self, of: &str) -> Vec<String> {
        use petgraph::visit::EdgeRef;
        let Some(&n) = self.idx.get(of) else { return Vec::new() };
        self.graph
            .edges(n)
            .filter(|e| e.weight() == "Calls")
            .map(|e| self.ids[e.target().index()].clone())
            .collect()
    }

    /// Top-k most central nodes by PageRank (deterministic id tiebreaker).
    pub fn pagerank_top(&self, k: usize) -> Vec<(String, f64)> {
        let ranks = petgraph::algo::page_rank::page_rank(&self.graph, 0.85_f64, 50);
        let mut scored: Vec<(String, f64)> = self
            .ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.clone(), ranks.get(i).copied().unwrap_or(0.0)))
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0))
        });
        scored.truncate(k);
        scored
    }
}

#[cfg(test)]
mod traversal_tests {
    use super::*;

    fn fixture() -> (Vec<Node>, Vec<Edge>) {
        let pf = codegraph_parse::parse_rust(
            "p",
            "src/lib.rs",
            "fn a() { b(); }\nfn b() { c(); }\nfn c() {}\nfn lonely() {}\n",
        );
        let built = build(&pf.nodes, &pf.calls);
        (pf.nodes, built.edges)
    }

    #[test]
    fn shortest_path_and_blast_radius() {
        let (nodes, edges) = fixture();
        let lg = LoadedGraph::load(&nodes, &edges);
        let a = nodes.iter().find(|n| n.name == "a").unwrap().id.clone();
        let c = nodes.iter().find(|n| n.name == "c").unwrap().id.clone();
        let path = lg.shortest_path(&a, &c).unwrap();
        assert_eq!(path.first().unwrap(), &a);
        assert_eq!(path.last().unwrap(), &c);
        // who depends on c? a and b (transitively) plus the File via DEFINES
        let blast = lg.blast_radius(&c, 5);
        assert!(blast.iter().any(|id| id.ends_with(".b")));
        assert!(blast.iter().any(|id| id.ends_with(".a")));
    }

    #[test]
    fn callees_and_pagerank() {
        let (nodes, edges) = fixture();
        let lg = LoadedGraph::load(&nodes, &edges);
        let a = nodes.iter().find(|n| n.name == "a").unwrap().id.clone();
        let callees = lg.callees(&a);
        assert!(callees.iter().any(|id| id.ends_with(".b")));
        let top = lg.pagerank_top(3);
        assert_eq!(top.len(), 3);
    }
}
