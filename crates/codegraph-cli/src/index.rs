//! Incremental repo indexing: walk → sha256 → (re)parse changed → persist →
//! rebuild edges from the full persisted graph (so cross-file edges stay correct).

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use codegraph_graph::{build, LoadedGraph};
use codegraph_parse::parse_file;
use codegraph_store::Store;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

const EXTS: &[&str] = &[
    "rs", "py", "pyi", "js", "jsx", "mjs", "cjs", "ts", "mts", "cts", "tsx", "go", "swift", "java",
    "c", "h", "cpp", "cc", "cxx", "hpp", "hh", "hxx", "rb", "cs", "sh", "bash",
];

pub struct IndexStats {
    pub files: usize,
    pub changed: usize,
    pub pruned: usize,
    pub nodes: usize,
    pub edges: usize,
}

pub fn db_path(root: &Path) -> std::path::PathBuf {
    root.join(".codegraph").join("graph.db")
}

pub fn index_dir(root: &Path, db: &Path, full: bool) -> Result<IndexStats> {
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let store = Store::open(db)?;
    let project = project_name(root);
    let mut seen: HashSet<String> = HashSet::new();
    let mut files = 0usize;
    let mut changed = 0usize;

    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        if !EXTS.contains(&ext) || path.components().any(|c| c.as_os_str() == "target") {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(path) else { continue };
        let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/");
        files += 1;
        seen.insert(rel.clone());

        let sha = sha256(&source);
        if !full {
            if let Some(m) = store.manifest_for(&rel)? {
                if m.sha256 == sha {
                    continue; // unchanged
                }
            }
        }
        store.delete_file_data(&rel)?;
        let pf = parse_file(&project, &rel, &source);
        for n in &pf.nodes {
            store.upsert_node(n)?;
        }
        store.save_calls(&rel, &pf.calls)?;
        store.save_manifest(&rel, &sha, 0)?;
        changed += 1;
    }

    // Prune files that vanished since last index.
    let mut pruned = 0usize;
    for mf in store.manifest_files()? {
        if !seen.contains(&mf) {
            store.delete_file_data(&mf)?;
            store.delete_manifest(&mf)?;
            pruned += 1;
        }
    }

    // Rebuild ALL edges from the full persisted node + call set (keeps
    // cross-file CALLS correct after a partial update).
    let nodes = store.all_nodes()?;
    let calls = store.all_calls()?;
    let built = build(&nodes, &calls);
    store.clear_edges()?;
    for e in &built.edges {
        store.upsert_edge(e)?;
    }

    // Community + centrality over the full graph, persisted onto each node.
    let lg = LoadedGraph::load(&nodes, &built.edges);
    let analytics = lg.analyze();
    let mut nodes = nodes;
    for nd in nodes.iter_mut() {
        if let Some(&(c, pr, bw)) = analytics.get(&nd.id) {
            nd.community = Some(c);
            nd.pagerank = pr;
            nd.betweenness = bw;
        }
        store.upsert_node(nd)?;
    }
    store.rebuild_fts()?;

    Ok(IndexStats { files, changed, pruned, nodes: nodes.len(), edges: built.edges.len() })
}

fn sha256(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

fn project_name(root: &Path) -> String {
    root.canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
        .unwrap_or_else(|| "project".to_string())
}
