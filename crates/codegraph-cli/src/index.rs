//! Repo indexing orchestration: walk → parse → build → persist.

use std::path::Path;

use anyhow::Result;
use codegraph_core::Node;
use codegraph_graph::build;
use codegraph_parse::parse_file;

const EXTS: &[&str] = &[
    "rs", "py", "pyi", "js", "jsx", "mjs", "cjs", "ts", "mts", "cts", "tsx", "go",
];
use codegraph_store::Store;
use walkdir::WalkDir;

pub struct IndexStats {
    pub files: usize,
    pub nodes: usize,
    pub edges: usize,
}

pub fn db_path(root: &Path) -> std::path::PathBuf {
    root.join(".codegraph").join("graph.db")
}

pub fn index_dir(root: &Path, db: &Path) -> Result<IndexStats> {
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let store = Store::open(db)?;
    let project = project_name(root);
    let mut nodes: Vec<Node> = Vec::new();
    let mut calls = Vec::new();
    let mut files = 0usize;

    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        if !EXTS.contains(&ext) {
            continue;
        }
        if path.components().any(|c| c.as_os_str() == "target") {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(path) else { continue };
        let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/");
        let pf = parse_file(&project, &rel, &source);
        nodes.extend(pf.nodes);
        calls.extend(pf.calls);
        files += 1;
    }

    let built = build(&nodes, &calls);
    for n in &nodes {
        store.upsert_node(n)?;
    }
    for e in &built.edges {
        store.upsert_edge(e)?;
    }
    store.rebuild_fts()?;
    Ok(IndexStats { files, nodes: nodes.len(), edges: built.edges.len() })
}

fn project_name(root: &Path) -> String {
    root.canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
        .unwrap_or_else(|| "project".to_string())
}
