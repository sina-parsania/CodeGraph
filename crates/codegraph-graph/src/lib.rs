//! Graph build layer: turn parsed nodes + raw calls into a petgraph graph and
//! the persisted edge set. M3 lands structural (DEFINES) + intra-file CALLS
//! resolution (Tier-B, same-language only — no cross-language call edges).

use std::collections::{HashMap, HashSet};

use codegraph_core::{
    Confidence, Edge, EdgeRelation, Hyperedge, HyperedgeMember, HyperedgeRelation, InheritKind, Metadata,
    Node, NodeLabel, RawCall, RawInherit, ResolutionTier,
};
use petgraph::stable_graph::{NodeIndex, StableGraph};

/// Directed graph of node-id → node-id, edge weight = relation name.
pub type CodeGraph = StableGraph<String, String>;

pub struct Built {
    pub graph: CodeGraph,
    pub edges: Vec<Edge>,
    pub hyperedges: Vec<(Hyperedge, Vec<HyperedgeMember>)>,
}

/// Build the edge set + petgraph from parsed nodes and unresolved calls.
/// - Pass 1 (structural): each File DEFINES every definition in the same file.
/// - Pass 2 (calls): resolve each `RawCall` to a Function in the caller's file
///   by name (intra-language, intra-file) → CALLS edge tagged Tier B.
pub fn build(nodes: &[Node], calls: &[RawCall], inherits: &[RawInherit]) -> Built {
    let by_id: HashMap<&str, &Node> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let file_by_path: HashMap<&str, &str> = nodes
        .iter()
        .filter(|n| n.label == NodeLabel::File)
        .map(|n| (n.file_path.as_str(), n.id.as_str()))
        .collect();
    let mut fn_by_file_name: HashMap<(&str, &str), &str> = HashMap::new();
    let mut fn_by_name: HashMap<&str, Vec<&str>> = HashMap::new();
    for n in nodes.iter().filter(|n| matches!(n.label, NodeLabel::Function | NodeLabel::Method)) {
        fn_by_file_name.insert((n.file_path.as_str(), n.name.as_str()), n.id.as_str());
        fn_by_name.entry(n.name.as_str()).or_default().push(n.id.as_str());
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
        // 1) same-file resolution wins; 2) else a project-wide UNIQUE name.
        // Ambiguous names (defined in >1 place) are left unresolved - no phantom edge.
        let resolved = fn_by_file_name
            .get(&(caller.file_path.as_str(), c.callee_name.as_str()))
            .copied()
            .or_else(|| match fn_by_name.get(c.callee_name.as_str()) {
                Some(cands) if cands.len() == 1 => Some(cands[0]),
                _ => None,
            });
        if let Some(callee_id) = resolved {
            if callee_id == c.caller_id {
                continue;
            }
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

    // Inheritance edges (resolved by unique project-wide name) + IMPLEMENTS hyperedges.
    let mut node_by_name: HashMap<&str, Vec<&str>> = HashMap::new();
    for n in nodes.iter().filter(|n| n.label != NodeLabel::File) {
        node_by_name.entry(n.name.as_str()).or_default().push(n.id.as_str());
    }
    let mut implementers: HashMap<&str, Vec<String>> = HashMap::new();
    for inh in inherits {
        let imp_id = match node_by_name.get(inh.impl_name.as_str()) {
            Some(v) if v.len() == 1 => v[0],
            _ => continue,
        };
        let sup_id = match node_by_name.get(inh.super_name.as_str()) {
            Some(v) if v.len() == 1 => v[0],
            _ => continue,
        };
        if imp_id == sup_id {
            continue;
        }
        let relation = match inh.kind {
            InheritKind::Extends => EdgeRelation::Inherits,
            InheritKind::Implements => EdgeRelation::Implements,
        };
        push_edge(&mut edges, &mut seen, Edge {
            src: imp_id.to_string(),
            dst: sup_id.to_string(),
            relation,
            tier: ResolutionTier::TreeSitter,
            confidence: Confidence::Extracted,
            src_file: by_id.get(imp_id).map(|n| n.file_path.clone()).unwrap_or_default(),
            src_line: by_id.get(imp_id).map(|n| n.line_start).unwrap_or(1),
            metadata: Metadata::new(),
        });
        if inh.kind == InheritKind::Implements {
            implementers.entry(sup_id).or_default().push(imp_id.to_string());
        }
    }
    let mut hyperedges: Vec<(Hyperedge, Vec<HyperedgeMember>)> = Vec::new();
    let mut sup_ids: Vec<&str> = implementers.keys().copied().collect();
    sup_ids.sort();
    for sup_id in sup_ids {
        let impls = &implementers[sup_id];
        if impls.len() < 2 {
            continue;
        }
        let hid = format!("implements::{}", sup_id);
        let sup_name = by_id.get(sup_id).map(|n| n.name.as_str()).unwrap_or("");
        let h = Hyperedge {
            id: hid.clone(),
            relation: HyperedgeRelation::Implement,
            label: format!("implementers of {}", sup_name),
            confidence: Confidence::Extracted,
            tier: ResolutionTier::TreeSitter,
            metadata: Metadata::new(),
        };
        let mut members: Vec<HyperedgeMember> = impls
            .iter()
            .map(|id| HyperedgeMember { hyperedge_id: hid.clone(), node_id: id.clone(), role: Some("implementer".to_string()) })
            .collect();
        members.push(HyperedgeMember { hyperedge_id: hid.clone(), node_id: sup_id.to_string(), role: Some("interface".to_string()) });
        hyperedges.push((h, members));
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

    Built { graph, edges, hyperedges }
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
        let built = build(&pf.nodes, &pf.calls, &pf.inherits);

        let calls: Vec<_> = built.edges.iter().filter(|e| e.relation == EdgeRelation::Calls).collect();
        assert_eq!(calls.len(), 1, "duplicate calls should dedupe to one edge");
        assert!(calls[0].src.ends_with("main") && calls[0].dst.ends_with("helper"));
        assert!(built.edges.iter().any(|e| e.relation == EdgeRelation::Defines));
        assert_eq!(built.graph.node_count(), pf.nodes.len());
    }

    #[test]
    fn implements_hyperedge_materializes() {
        let pf = codegraph_parse::parse_ts(
            "p", "a.ts",
            "interface Repo {}\nclass SqlRepo implements Repo {}\nclass MemRepo implements Repo {}\n",
        );
        let built = build(&pf.nodes, &pf.calls, &pf.inherits);
        assert!(built.edges.iter().any(|e| e.relation == EdgeRelation::Implements));
        let he = built.hyperedges.iter().find(|(h, _)| h.label.contains("Repo")).expect("hyperedge");
        // 2 implementers + the interface = 3 members
        assert_eq!(he.1.len(), 3);
    }

    #[test]
    fn end_to_end_persist_and_query() {
        let pf = codegraph_parse::parse_rust("proj", "src/main.rs", "fn helper() {}\nfn main() { helper(); }\n");
        let built = build(&pf.nodes, &pf.calls, &pf.inherits);
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
    fn cross_file_unique_name_resolves() {
        // unique callee name across files -> cross-file CALLS edge
        let mut pf = codegraph_parse::parse_rust("proj", "a.rs", "fn main() { ghost(); }\n");
        let other = codegraph_parse::parse_rust("proj", "b.rs", "fn ghost() {}\n");
        pf.nodes.extend(other.nodes);
        pf.calls.extend(other.calls);
        let built = build(&pf.nodes, &pf.calls, &pf.inherits);
        assert!(built.edges.iter().any(|e| e.relation == EdgeRelation::Calls
            && e.src.ends_with(".main") && e.dst.ends_with(".ghost")));
    }

    #[test]
    fn ambiguous_name_not_resolved_cross_file() {
        // same name defined in two files -> a call from a THIRD file stays unresolved
        let mut a = codegraph_parse::parse_rust("proj", "a.rs", "fn dup() {}\n");
        let b = codegraph_parse::parse_rust("proj", "b.rs", "fn dup() {}\n");
        let c = codegraph_parse::parse_rust("proj", "c.rs", "fn caller() { dup(); }\n");
        a.nodes.extend(b.nodes);
        a.nodes.extend(c.nodes);
        a.calls.extend(c.calls);
        let built = build(&a.nodes, &a.calls, &a.inherits);
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

    /// Per-node analytics: (community id, PageRank, betweenness). Computed once
    /// over the whole graph; persisted onto each node at index time.
    pub fn analyze(&self) -> HashMap<String, (u32, f64, f64)> {
        let comm = self.communities();
        let ranks = petgraph::algo::page_rank::page_rank(&self.graph, 0.85_f64, 50);
        let betw = self.betweenness();
        let mut out = HashMap::new();
        for (i, id) in self.ids.iter().enumerate() {
            out.insert(
                id.clone(),
                (
                    comm.get(id).copied().unwrap_or(0),
                    ranks.get(i).copied().unwrap_or(0.0),
                    betw.get(id).copied().unwrap_or(0.0),
                ),
            );
        }
        out
    }

    /// Deterministic one-level Louvain (modularity local-moving); edges treated
    /// as undirected, weight 1. Tie-break to the smaller community id.
    pub fn communities(&self) -> HashMap<String, u32> {
        let n = self.graph.node_count();
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
        for &ni in self.idx.values() {
            let i = ni.index();
            for d in [petgraph::Direction::Outgoing, petgraph::Direction::Incoming] {
                for nb in self.graph.neighbors_directed(ni, d) {
                    adj[i].push(nb.index());
                }
            }
        }
        let deg: Vec<f64> = adj.iter().map(|a| a.len() as f64).collect();
        let m2: f64 = deg.iter().sum();
        let mut comm: Vec<usize> = (0..n).collect();
        if m2 > 0.0 {
            let mut sigma_tot: Vec<f64> = deg.clone();
            let mut improved = true;
            let mut rounds = 0;
            while improved && rounds < 20 {
                improved = false;
                rounds += 1;
                for v in 0..n {
                    if adj[v].is_empty() {
                        continue;
                    }
                    let cv = comm[v];
                    sigma_tot[cv] -= deg[v];
                    let mut k_in: HashMap<usize, f64> = HashMap::new();
                    for &u in &adj[v] {
                        *k_in.entry(comm[u]).or_default() += 1.0;
                    }
                    let mut best_c = cv;
                    let mut best_gain = k_in.get(&cv).copied().unwrap_or(0.0) - deg[v] * sigma_tot[cv] / m2;
                    for (&c, &kin) in &k_in {
                        let gain = kin - deg[v] * sigma_tot[c] / m2;
                        if gain > best_gain + 1e-12 || ((gain - best_gain).abs() <= 1e-12 && c < best_c) {
                            best_gain = gain;
                            best_c = c;
                        }
                    }
                    sigma_tot[best_c] += deg[v];
                    if best_c != cv {
                        comm[v] = best_c;
                        improved = true;
                    }
                }
            }
        }
        let mut remap: HashMap<usize, u32> = HashMap::new();
        let mut next = 0u32;
        let mut out = HashMap::new();
        for (id, &cm) in self.ids.iter().zip(&comm) {
            let c = *remap.entry(cm).or_insert_with(|| {
                let x = next;
                next += 1;
                x
            });
            out.insert(id.clone(), c);
        }
        out
    }

    /// Brandes betweenness centrality (exact for graphs up to 1500 nodes,
    /// else seeded evenly-spaced pivots for an approximation).
    pub fn betweenness(&self) -> HashMap<String, f64> {
        let n = self.graph.node_count();
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
        for &ni in self.idx.values() {
            let i = ni.index();
            for nb in self.graph.neighbors(ni) {
                adj[i].push(nb.index());
            }
        }
        let sources: Vec<usize> = if n <= 1500 {
            (0..n).collect()
        } else {
            (0..n).step_by((n / 500).max(1)).collect()
        };
        let mut bc = vec![0.0f64; n];
        for &s in &sources {
            let mut stack = Vec::new();
            let mut pred: Vec<Vec<usize>> = vec![Vec::new(); n];
            let mut sigma = vec![0.0f64; n];
            sigma[s] = 1.0;
            let mut dist = vec![-1i64; n];
            dist[s] = 0;
            let mut q = std::collections::VecDeque::new();
            q.push_back(s);
            while let Some(v) = q.pop_front() {
                stack.push(v);
                for &w in &adj[v] {
                    if dist[w] < 0 {
                        dist[w] = dist[v] + 1;
                        q.push_back(w);
                    }
                    if dist[w] == dist[v] + 1 {
                        sigma[w] += sigma[v];
                        pred[w].push(v);
                    }
                }
            }
            let mut delta = vec![0.0f64; n];
            while let Some(w) = stack.pop() {
                for &v in &pred[w] {
                    delta[v] += (sigma[v] / sigma[w]) * (1.0 + delta[w]);
                }
                if w != s {
                    bc[w] += delta[w];
                }
            }
        }
        let mut out = HashMap::new();
        for (id, &b) in self.ids.iter().zip(&bc) {
            out.insert(id.clone(), b);
        }
        out
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
        let built = build(&pf.nodes, &pf.calls, &pf.inherits);
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
    fn communities_and_betweenness() {
        use codegraph_core::{Confidence, EdgeRelation, Metadata, NodeLabel, ResolutionTier};
        let mk_n = |x: &str| Node {
            id: x.into(), label: NodeLabel::Function, name: x.into(), file_path: "f".into(),
            line_start: 1, line_end: 1, language: "rust".into(), metadata: Metadata::new(),
            community: None, pagerank: 0.0, betweenness: 0.0,
        };
        let nodes: Vec<Node> = ["a", "b", "c", "d", "e", "f"].iter().map(|x| mk_n(x)).collect();
        let mk_e = |s: &str, d: &str| Edge {
            src: s.into(), dst: d.into(), relation: EdgeRelation::Calls, tier: ResolutionTier::TreeSitter,
            confidence: Confidence::Inferred, src_file: "f".into(), src_line: 1, metadata: Metadata::new(),
        };
        // two triangles {a,b,c} and {d,e,f} bridged by c->d
        let edges = vec![mk_e("a","b"), mk_e("b","c"), mk_e("c","a"), mk_e("c","d"), mk_e("d","e"), mk_e("e","f"), mk_e("f","d")];
        let lg = LoadedGraph::load(&nodes, &edges);
        let a = lg.analyze();
        let comms: std::collections::HashSet<u32> = a.values().map(|v| v.0).collect();
        assert!(comms.len() >= 2, "expected at least two communities");
        assert!(a["c"].2 > 0.0 || a["d"].2 > 0.0, "bridge node should have betweenness");
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
