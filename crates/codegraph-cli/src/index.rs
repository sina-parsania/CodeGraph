//! Incremental repo indexing: walk → sha256 → (re)parse changed → persist →
//! rebuild edges from the full persisted graph (so cross-file edges stay correct).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use codegraph_graph::{build, LoadedGraph};
use codegraph_parse::{parse_file, ParsedFile};
use codegraph_core::{Edge, EdgeRelation, Metadata, Node, NodeLabel};
use codegraph_store::Store;
use sha2::{Digest, Sha256};
use ignore::WalkBuilder;
use rayon::prelude::*;

const EXTS: &[&str] = &[
    "rs", "py", "pyi", "js", "jsx", "mjs", "cjs", "ts", "mts", "cts", "tsx", "go", "swift", "java",
    "c", "h", "cpp", "cc", "cxx", "hpp", "hh", "hxx", "rb", "cs", "sh", "bash", "kt", "kts",
];

/// Documentation/prose files auto-ingested as searchable Document nodes during
/// `index` (READMEs, docs, changelogs). Data/log files (json, jsonl, log, csv, …)
/// are NOT auto-indexed — ingest them explicitly with `codegraph ingest` to avoid noise.
const DOC_EXTS: &[&str] = &[
    "md", "markdown", "mdx", "rst", "adoc", "asciidoc", "txt",
    // localization keys are commonly searched ("which file has this UI string?")
    "strings", "stringsdict", "po", "xliff", "xlf", "arb",
];

/// Lockfiles / generated manifests we never ingest even if they match an extension.
const SKIP_NAMES: &[&str] = &[
    "package-lock.json", "yarn.lock", "pnpm-lock.yaml", "composer.lock", "poetry.lock",
    "Cargo.lock", "Gemfile.lock", "go.sum", "podfile.lock",
];

/// Directories never indexed (dependencies, build output, caches, VCS).
const EXCLUDE_DIRS: &[&str] = &[
    "target", "node_modules", ".venv", "venv", "env", "Pods", "build", "dist", ".git", ".gradle",
    ".next", ".nuxt", "__pycache__", ".cache", "DerivedData", "vendor", ".idea", ".vscode", "out",
    ".dart_tool", ".mypy_cache", ".pytest_cache", ".tox", "bin", "obj", ".svn", ".hg", ".terraform",
    "coverage", ".codegraph", "Carthage", ".bundle", "bower_components", ".yarn", ".pnp",
];

/// Skip files larger than this (minified bundles, generated blobs) to keep
/// parsing fast and avoid pathological tree-sitter inputs.
const MAX_FILE_BYTES: u64 = 1_500_000;

pub struct IndexStats {
    pub files: usize,
    pub changed: usize,
    pub pruned: usize,
    pub nodes: usize,
    pub edges: usize,
    pub scip_edges: usize,
}

/// Root of the central graph cache: `$CODEGRAPH_CACHE_DIR`, else
/// `$XDG_CACHE_HOME/codegraph`, else `~/.cache/codegraph`. Graphs live here keyed
/// by project path, so source repos stay pristine (no in-repo artifact to commit).
pub fn cache_root() -> PathBuf {
    if let Some(d) = std::env::var_os("CODEGRAPH_CACHE_DIR") {
        return PathBuf::from(d);
    }
    if let Some(x) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(x).join("codegraph");
    }
    if let Some(h) = std::env::var_os("HOME") {
        return PathBuf::from(h).join(".cache").join("codegraph");
    }
    PathBuf::from(".codegraph-cache")
}

/// Absolute path of a project's graph DB inside the central cache, keyed by a
/// hash of the project's absolute path.
pub fn db_path(root: &Path) -> PathBuf {
    let abs = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let id = sha256(abs.to_string_lossy().as_ref());
    cache_root().join(&id[..16]).join("graph.db")
}

/// The ignore-aware walker shared by the indexer AND the staleness probe — they
/// MUST use the same file set or they disagree and reintroduce false positives.
fn build_walker(root: &Path) -> ignore::Walk {
    WalkBuilder::new(root)
        .git_ignore(true)
        .git_global(true)
        .add_custom_ignore_filename(".codegraphignore")
        .filter_entry(|e| {
            !e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                || !EXCLUDE_DIRS.contains(&e.file_name().to_str().unwrap_or(""))
        })
        .build()
}

/// Some(is_doc) if a walked entry is indexable, None to skip. Shared predicate.
fn classify(entry: &ignore::DirEntry) -> Option<bool> {
    if entry.file_type().map(|t| t.is_dir()).unwrap_or(true) {
        return None;
    }
    let path = entry.path();
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    let is_code = EXTS.contains(&ext);
    let is_doc = DOC_EXTS.contains(&ext);
    if !is_code && !is_doc {
        return None;
    }
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if SKIP_NAMES.iter().any(|s| s.eq_ignore_ascii_case(name)) {
        return None;
    }
    if entry.metadata().map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(false) {
        return None;
    }
    Some(is_doc)
}

fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

/// File mtime as nanoseconds since epoch (0 if unavailable). The cheap staleness signal.
fn file_mtime(entry: &ignore::DirEntry) -> i64 {
    entry
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Read-only staleness probe: does the graph match the working tree right now?
/// Walks with the indexer's exact filters and compares mtimes against the
/// manifest (add/delete via set membership). ANY difference => stale. Cheap
/// (stat-only, no file reads). This is what makes "auto-heal before query" viable.
pub fn is_stale(root: &Path) -> bool {
    let db = db_path(root);
    if !db.exists() {
        return true;
    }
    let Ok(store) = Store::open(&db) else { return true };
    let Ok(rows) = store.manifest_map() else { return true };
    let mut prev: std::collections::HashMap<String, i64> =
        rows.into_iter().map(|m| (m.file_path, m.mtime)).collect();
    for entry in build_walker(root).filter_map(|e| e.ok()) {
        if classify(&entry).is_none() {
            continue;
        }
        let rel = rel_path(root, entry.path());
        match prev.remove(&rel) {
            None => return true,                                   // added file
            Some(prev_mtime) if prev_mtime != file_mtime(&entry) => return true, // changed/touched
            Some(_) => {}
        }
    }
    !prev.is_empty() // anything left in the manifest was deleted on disk
}

/// Make the graph match the working tree before serving a query: build it if
/// missing, incrementally reindex if anything changed. The clean path is the
/// stat-only probe above. This is the guarantee that queries never serve stale
/// results (no false positives after edits / add / delete / git checkout).
pub fn ensure_fresh(root: &Path) -> Result<()> {
    if is_stale(root) {
        let db = db_path(root);
        index_dir(root, &db, false, None)?;
    }
    Ok(())
}

pub fn index_dir(root: &Path, db: &Path, full: bool, scip: Option<&Path>) -> Result<IndexStats> {
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent)?;
        // Self-describe the cache entry (which project it belongs to) for
        // discoverability + `codegraph projects`.
        let abs = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let _ = std::fs::write(parent.join("source"), abs.to_string_lossy().as_bytes());
    }
    // Migration: graphs now live in the central cache. Remove any legacy in-repo
    // `.codegraph/` we created so source trees go back to pristine.
    let legacy = root.join(".codegraph");
    if legacy.join("graph.db").exists() {
        let _ = std::fs::remove_dir_all(&legacy);
    }
    let store = Store::open(db)?;
    store.begin()?;
    let project = project_name(root);
    let mut seen: HashSet<String> = HashSet::new();
    let mut files = 0usize;

    // Phase 1: stat-first. Skip unchanged files by mtime (no read); for files
    // whose mtime moved, hash and reparse only if the content actually changed
    // (mtime can move with identical content, e.g. git checkout — refresh the
    // stored mtime so it isn't re-flagged, but don't rebuild).
    let mut to_parse: Vec<(String, String, String, i64, bool)> = Vec::new();
    for entry in build_walker(root).filter_map(|e| e.ok()) {
        let Some(is_doc) = classify(&entry) else { continue };
        let path = entry.path();
        let rel = rel_path(root, path);
        let mtime = file_mtime(&entry);
        files += 1;
        seen.insert(rel.clone());
        let manifest = if full { None } else { store.manifest_for(&rel)? };
        if let Some(m) = &manifest {
            if m.mtime == mtime && mtime != 0 {
                continue; // unchanged — stat fast-path, no read
            }
        }
        let Ok(source) = std::fs::read_to_string(path) else { continue };
        let sha = sha256(&source);
        if let Some(m) = &manifest {
            if m.sha256 == sha {
                store.save_manifest(&rel, &sha, mtime)?; // touched but identical: refresh mtime only
                continue;
            }
        }
        to_parse.push((rel, source, sha, mtime, is_doc));
    }

    // Phase 2: process changed files in parallel — code → tree-sitter parse,
    // docs → Document chunks (CPU-bound, no shared state).
    let parsed: Vec<(String, String, i64, ParsedFile)> = to_parse
        .par_iter()
        .map(|(rel, source, sha, mtime, is_doc)| {
            let pf = if *is_doc {
                let ctype = rel.rsplit('.').next().unwrap_or("text");
                ParsedFile {
                    nodes: document_nodes(rel, ctype, source),
                    calls: Vec::new(),
                    inherits: Vec::new(),
                    fields: Vec::new(),
                }
            } else {
                parse_file(&project, rel, source)
            };
            (rel.clone(), sha.clone(), *mtime, pf)
        })
        .collect();
    let changed = parsed.len();

    // Phase 3: persist sequentially (SQLite writes are serial).
    let mut changed_nodes: Vec<Node> = Vec::new();
    for (rel, sha, mtime, pf) in parsed {
        store.delete_file_data(&rel)?;
        store.save_calls(&rel, &pf.calls)?;
        store.save_inherits(&rel, &pf.inherits)?;
        store.save_fields(&rel, &pf.fields)?;
        store.save_manifest(&rel, &sha, mtime)?;
        changed_nodes.extend(pf.nodes);
    }
    store.bulk_upsert_nodes(&changed_nodes)?;

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
    // Nothing changed and not a forced full rebuild: the graph is already current.
    if changed == 0 && pruned == 0 && !full && scip_path(root, scip).is_none() {
        store.commit()?;
        return Ok(IndexStats {
            files,
            changed,
            pruned,
            nodes: store.node_count()? as usize,
            edges: store.edge_count()? as usize,
            scip_edges: 0,
        });
    }

    let nodes = store.all_nodes()?;
    let calls = store.all_calls()?;
    let inherits = store.all_inherits()?;
    let fields = store.all_fields()?;
    let built = build(&nodes, &calls, &inherits, &fields);
    let mut edges = built.edges;
    let scip_edges = merge_scip_edges(root, scip, &nodes, &mut edges);
    store.clear_edges()?;
    store.bulk_upsert_edges(&edges)?;
    store.clear_hyperedges()?;
    for (h, members) in &built.hyperedges {
        store.upsert_hyperedge(h, members)?;
    }

    // Community + centrality over the full graph, persisted onto each node.
    let lg = LoadedGraph::load(&nodes, &edges);
    let analytics = lg.analyze();
    let mut nodes = nodes;
    for nd in nodes.iter_mut() {
        if let Some(&(c, pr, bw)) = analytics.get(&nd.id) {
            nd.community = Some(c);
            nd.pagerank = pr;
            nd.betweenness = bw;
        }
    }
    store.bulk_upsert_nodes(&nodes)?;
    store.rebuild_fts()?;
    store.commit()?;

    Ok(IndexStats { files, changed, pruned, nodes: nodes.len(), edges: edges.len(), scip_edges })
}

/// Locate a `.scip` index: explicit path, else `index.scip`, else any `*.scip` at root.
fn scip_path(root: &Path, explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return p.exists().then(|| p.to_path_buf());
    }
    let cand = root.join("index.scip");
    if cand.exists() {
        return Some(cand);
    }
    std::fs::read_dir(root)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "scip"))
}

/// Merge compiler-grade SCIP edges in: they supersede the tree-sitter edge for
/// the same (src, dst, relation) and add precise edges tree-sitter missed.
fn merge_scip_edges(root: &Path, explicit: Option<&Path>, nodes: &[Node], edges: &mut Vec<Edge>) -> usize {
    let Some(path) = scip_path(root, explicit) else { return 0 };
    let Ok(bytes) = std::fs::read(&path) else { return 0 };
    let Ok(scip) = codegraph_resolve::import_scip(&bytes, nodes) else { return 0 };
    if scip.is_empty() {
        return 0;
    }
    let superseded: HashSet<(String, String, EdgeRelation)> =
        scip.iter().map(|e| (e.src.clone(), e.dst.clone(), e.relation)).collect();
    edges.retain(|e| !superseded.contains(&(e.src.clone(), e.dst.clone(), e.relation)));
    let n = scip.len();
    edges.extend(scip);
    n
}

/// Build one searchable Document node from an ingested chunk. Shared by `index`
/// (doc auto-ingest) and the explicit `ingest` command so the shape stays identical.
pub fn document_node_from_chunk(ch: &codegraph_ingest::DocChunk, i: usize) -> Node {
    let safe: String = ch
        .source
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let title: String = ch
        .text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(&ch.source)
        .chars()
        .take(60)
        .collect();
    let mut meta = Metadata::new();
    meta.insert("text".to_string(), serde_json::Value::String(ch.text.clone()));
    meta.insert("content_type".to_string(), serde_json::Value::String(ch.content_type.clone()));
    Node {
        id: format!("doc.{safe}.{i}"),
        label: NodeLabel::Document,
        name: if title.trim().is_empty() { format!("{} #{i}", ch.source) } else { title },
        file_path: ch.source.clone(),
        line_start: 1,
        line_end: 1,
        language: ch.content_type.clone(),
        metadata: meta,
        community: None,
        pagerank: 0.0,
        betweenness: 0.0,
    }
}

/// Chunk a text document and build its Document nodes (used by the index walk).
pub fn document_nodes(source: &str, content_type: &str, text: &str) -> Vec<Node> {
    codegraph_ingest::chunk_text(text, content_type, source)
        .iter()
        .enumerate()
        .map(|(i, ch)| document_node_from_chunk(ch, i))
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_fresh_detects_every_change_class() {
        let tmp = std::env::temp_dir().join(format!("cg_fresh_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // isolate the cache so the test never touches the real ~/.cache
        std::env::set_var("CODEGRAPH_CACHE_DIR", tmp.join("cache"));
        std::fs::write(tmp.join("a.py"), "def foo():\n    return 1\n").unwrap();

        assert!(is_stale(&tmp), "never-indexed project is stale");
        ensure_fresh(&tmp).unwrap();
        assert!(!is_stale(&tmp), "clean right after index");

        std::fs::write(tmp.join("a.py"), "def bar():\n    return 2\n").unwrap();
        assert!(is_stale(&tmp), "edit detected");
        ensure_fresh(&tmp).unwrap();
        assert!(!is_stale(&tmp), "clean after heal");

        std::fs::write(tmp.join("b.py"), "def baz():\n    pass\n").unwrap();
        assert!(is_stale(&tmp), "added file detected");
        ensure_fresh(&tmp).unwrap();

        std::fs::remove_file(tmp.join("b.py")).unwrap();
        assert!(is_stale(&tmp), "deleted file detected");

        std::env::remove_var("CODEGRAPH_CACHE_DIR");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
