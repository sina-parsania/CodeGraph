//! Incremental repo indexing: walk → sha256 → (re)parse changed → persist →
//! rebuild edges from the full persisted graph (so cross-file edges stay correct).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use codegraph_graph::{build_with, LoadedGraph};
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
    /// True when the edge rebuild took the incremental (changed-files-only)
    /// path instead of re-resolving the whole repo.
    pub partial: bool,
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
        index_dir(root, &db, false, None, false, None)?;
    }
    Ok(())
}

pub fn index_dir(root: &Path, db: &Path, full: bool, scip: Option<&Path>, indexstore: bool, ambiguous: Option<bool>) -> Result<IndexStats> {
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
    // RAII: rolls back on early error/panic; committed explicitly below.
    let txn = store.txn()?;
    let project = project_name(root);
    let mut seen: HashSet<String> = HashSet::new();
    let mut files = 0usize;

    // Phase 1: stat-first. Skip unchanged files by mtime (no read); for files
    // whose mtime moved, hash and reparse only if the content actually changed
    // (mtime can move with identical content, e.g. git checkout — refresh the
    // stored mtime so it isn't re-flagged, but don't rebuild).
    // The manifest is loaded ONCE into a map — not one SQL query per file.
    let manifest_map: HashMap<String, codegraph_store::ManifestEntry> = if full {
        HashMap::new()
    } else {
        store.manifest_map()?.into_iter().map(|m| (m.file_path.clone(), m)).collect()
    };
    let mut to_parse: Vec<(String, String, String, i64, bool)> = Vec::new();
    for entry in build_walker(root).filter_map(|e| e.ok()) {
        let Some(is_doc) = classify(&entry) else { continue };
        let path = entry.path();
        let rel = rel_path(root, path);
        let mtime = file_mtime(&entry);
        files += 1;
        seen.insert(rel.clone());
        let manifest = manifest_map.get(&rel);
        if let Some(m) = manifest {
            if m.mtime == mtime && mtime != 0 {
                continue; // unchanged — stat fast-path, no read
            }
        }
        let Ok(source) = std::fs::read_to_string(path) else { continue };
        let sha = sha256(&source);
        if let Some(m) = manifest {
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
                    locals: Vec::new(),
                    imports: Vec::new(),
                }
            } else {
                parse_file(&project, rel, source)
            };
            (rel.clone(), sha.clone(), *mtime, pf)
        })
        .collect();
    let changed = parsed.len();

    // Phase 3: persist sequentially (SQLite writes are serial). FTS stays in
    // sync via the nodes_fts triggers — no manual FTS bookkeeping here.
    //
    // Wave-propagation classification: compare each changed file's pre-edit
    // shape (read from the live tables BEFORE deletion) against its parsed
    // shape. Body-only edits contribute nothing; Function/Method definition
    // changes contribute their names to `dirty_names` (their callers must
    // re-resolve); anything else observable (classes, inherits, fields,
    // routes, doc titles) forces the full rebuild.
    let mut changed_nodes: Vec<Node> = Vec::new();
    let mut changed_files: Vec<String> = Vec::new();
    let mut dirty_names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut shape_beyond_fns = false;
    for (rel, sha, mtime, pf) in parsed {
        let old_shape = store.file_shape(&rel)?;
        let new_shape = parsed_shape(&pf);
        if old_shape.other != new_shape.other {
            shape_beyond_fns = true;
        } else {
            dirty_names.extend(fn_diff_names(&old_shape, &new_shape));
        }
        store.delete_file_data(&rel)?;
        store.save_calls(&rel, &pf.calls)?;
        store.save_inherits(&rel, &pf.inherits)?;
        store.save_fields(&rel, &pf.fields)?;
        store.save_locals(&rel, &pf.locals)?;
        store.save_imports(&rel, &pf.imports)?;
        store.save_manifest(&rel, &sha, mtime)?;
        changed_nodes.extend(pf.nodes);
        changed_files.push(rel);
    }
    store.bulk_upsert_nodes(&changed_nodes)?;

    // Prune files that vanished since last index. A pruned file's definitions
    // are dirty names (their callers must drop their edges); its own edges are
    // stale at every tier.
    let mut pruned = 0usize;
    for mf in store.manifest_files()? {
        if !seen.contains(&mf) {
            let shape = store.file_shape(&mf)?;
            if !shape.other.is_empty() {
                shape_beyond_fns = true;
            }
            dirty_names.extend(shape.fn_defs.into_values());
            store.delete_edges_for_file(&mf)?;
            store.delete_file_data(&mf)?;
            store.delete_manifest(&mf)?;
            pruned += 1;
        }
    }

    // Rebuild ALL edges from the full persisted node + call set (keeps
    // cross-file CALLS correct after a partial update).
    // Nothing changed and not a forced full rebuild: the graph is already current.
    if changed == 0 && pruned == 0 && !full && scip_path(root, scip).is_none() {
        txn.commit()?;
        return Ok(IndexStats {
            files,
            changed,
            pruned,
            nodes: store.node_count()? as usize,
            edges: store.edge_count()? as usize,
            scip_edges: 0,
            partial: false,
        });
    }

    // graph_nodes strips Document chunk text — the edge rebuild reads structure
    // only, and doc text can dominate deserialization cost on doc-heavy repos.
    let nodes = store.graph_nodes()?;
    let inherits = store.all_inherits()?;
    let fields = store.all_fields()?;
    let locals = store.all_locals()?;
    let imports = store.all_imports()?;
    // Sticky ambiguous-tier setting: an explicit flag stamps it; auto-heal reruns
    // read the stamp so the tier survives incremental reindexes.
    let include_ambiguous = match ambiguous {
        Some(v) => {
            let _ = store.meta_set("include_ambiguous", if v { "1" } else { "0" });
            v
        }
        None => store.meta_get("include_ambiguous").ok().flatten().as_deref() == Some("1"),
    };
    // WAVE-PROPAGATION edge rebuild: a body-only edit re-resolves just the
    // changed files; a Function/Method definition change (add/remove/rename/
    // move-between-classes) additionally re-resolves every file with a call
    // site NAMING a dirty definition — the exact set whose resolution can have
    // changed, found via the indexed raw-calls table, no repo scan. Falls back
    // to the full rebuild when the change touches anything else observable
    // (classes/inherits/fields/routes), when a dirty name appears in an inherit
    // clause (name-uniqueness could flip INHERITS edges + hyperedges), on
    // pruning beyond fn defs, SCIP/IndexStore merges, or an --ambiguous flip.
    // A fresh index (no prior manifest) has nothing to be incremental against —
    // the plain full path is cheaper than a wave spanning every file.
    let mut partial_ok = !full
        && !manifest_map.is_empty()
        && !shape_beyond_fns
        && ambiguous.is_none()
        && scip_path(root, scip).is_none()
        && !indexstore
        && !indexstore_wants_remerge(&store, root);
    if partial_ok {
        for n in &dirty_names {
            if store.inherits_name_referenced(n)? {
                partial_ok = false;
                break;
            }
        }
    }
    let (edges, scip_edges) = if partial_ok {
        let mut wave: std::collections::BTreeSet<String> = changed_files.iter().cloned().collect();
        for n in &dirty_names {
            wave.extend(store.files_with_calls_naming(n)?);
        }
        let mut calls_wave = Vec::new();
        for f in &wave {
            calls_wave.extend(store.calls_for_file(f)?);
        }
        let wave_set: HashSet<String> = wave.iter().cloned().collect();
        let new_edges = codegraph_graph::resolve_files(
            &nodes, &calls_wave, &inherits, &fields, &locals, &imports,
            include_ambiguous, &wave_set,
        );
        for f in &wave {
            store.delete_tree_sitter_edges_for_file(f)?;
        }
        // Conflicts can only be surviving compiler-grade (Scip-tier) edges,
        // which outrank tree-sitter — keep them.
        store.bulk_insert_edges_keep_existing(&new_edges)?;
        // Hyperedges + implementer sets depend only on inherits and inherit-name
        // uniqueness, both guaranteed untouched by the gates above.
        (store.graph_edges()?, 0)
    } else {
        let calls = store.all_calls()?;
        let built = build_with(&nodes, &calls, &inherits, &fields, &locals, &imports, include_ambiguous);
        let mut edges = built.edges;
        let scip_edges = merge_scip_edges(root, scip, &nodes, &mut edges);
        // Swift compiler-grade edges are AUTOMATIC when the feature is compiled:
        // fresh Xcode build (store mtime > stamped) -> re-merge; otherwise the
        // previously merged edges are REUSED so auto-heal never drops them.
        // `--indexstore` just forces a re-merge.
        auto_indexstore(&store, root, &nodes, &mut edges, indexstore);
        store.clear_edges()?;
        store.bulk_upsert_edges(&edges)?;
        store.clear_hyperedges()?;
        for (h, members) in &built.hyperedges {
            store.upsert_hyperedge(h, members)?;
        }
        (edges, scip_edges)
    };

    // Community + centrality over the full graph, persisted onto each node.
    let lg = LoadedGraph::load(&nodes, &edges);
    let analytics = lg.analyze();
    // fan_in/fan_out over resolved CALLS edges -> node metadata (with complexity
    // from parse, gives agents per-node risk signals for free).
    let mut fan_in: HashMap<String, u32> = HashMap::new();
    let mut fan_out: HashMap<String, u32> = HashMap::new();
    for e in edges.iter().filter(|e| e.relation == EdgeRelation::Calls) {
        *fan_out.entry(e.src.clone()).or_insert(0) += 1;
        *fan_in.entry(e.dst.clone()).or_insert(0) += 1;
    }
    // Persist analytics via targeted column+json_set UPDATEs — NOT a second full
    // node upsert, which re-serialized every node's JSON (incl. document text)
    // in Rust on every incremental index.
    let items: Vec<(String, u32, f64, f64, u32, u32)> = nodes
        .iter()
        .map(|nd| {
            let (c, pr, bw) = analytics.get(&nd.id).copied().unwrap_or((0, 0.0, 0.0));
            let fi = fan_in.get(&nd.id).copied().unwrap_or(0);
            let fo = fan_out.get(&nd.id).copied().unwrap_or(0);
            (nd.id.clone(), c, pr, bw, fi, fo)
        })
        .collect();
    store.update_analytics(&items)?;
    // Git co-change mining costs a `git log -n 1000` parse — skip it when HEAD
    // hasn't moved since the last index (the common auto-heal case).
    let head = git_head(root);
    if store.meta_get("cochanges_head").ok().flatten() != head || head.is_none() {
        let pairs = compute_cochanges(root);
        if !pairs.is_empty() {
            store.save_cochanges(&pairs)?;
        }
        if let Some(h) = &head {
            store.meta_set("cochanges_head", h)?;
        }
    }
    // Deterministic invariant gate before commit: dangling edges, FQN schema,
    // missing justification tags. Violations are surfaced loudly but never
    // brick the index — a degraded graph beats no graph, and the message tells
    // the user exactly what to report.
    let violations = store.validate_graph()?;
    if !violations.is_empty() {
        eprintln!("codegraph: graph validation found {} issue(s):", violations.len());
        for v in violations.iter().take(10) {
            eprintln!("  - {v}");
        }
    }
    store.bump_generation()?;
    txn.commit()?;
    auto_embed_changed(&store, &changed_nodes);

    Ok(IndexStats { files, changed, pruned, nodes: nodes.len(), edges: edges.len(), scip_edges, partial: partial_ok })
}

/// The text embedded for one node — shared by `semantic-index` (full pass) and
/// the post-index auto-refresh, so all vectors live in ONE embedding space.
/// Documents embed their chunk CONTENT (capped), not just the title.
pub fn embed_text_for(n: &Node) -> String {
    let mut t = format!("{} {:?} in {}", n.name, n.label, n.file_path);
    if let Some(text) = n.metadata.get("text").and_then(|v| v.as_str()) {
        let cap = text.char_indices().nth(2000).map(|(i, _)| i).unwrap_or(text.len());
        t.push('\n');
        t.push_str(&text[..cap]);
    }
    t
}

/// Keep semantic search fresh WITHOUT a manual `semantic-index` rerun: after a
/// committed index, re-embed just the changed nodes (their stale vectors were
/// already pruned with their files). Opt-in by design — only runs once the user
/// has stamped an embedding model by running `semantic-index`. Skips loudly on
/// model mismatch (two models must never mix in one vector space) or when no
/// embedder is reachable. Runs OUTSIDE the index transaction: embedding can
/// take seconds and must not hold the write lock.
fn auto_embed_changed(store: &Store, changed: &[Node]) {
    // ponytail: inline ceiling — bigger batches (fresh index, git checkout)
    // should go through the explicit, progress-reporting `semantic-index`.
    const AUTO_EMBED_MAX: usize = 2000;
    let Ok(Some(stamped)) = store.meta_get("embed_model") else { return };
    let items: Vec<(&Node, String)> = changed
        .iter()
        .filter(|n| n.label != NodeLabel::File)
        .map(|n| (n, embed_text_for(n)))
        .collect();
    if items.is_empty() {
        return;
    }
    if items.len() > AUTO_EMBED_MAX {
        eprintln!(
            "codegraph: {} changed symbols exceed the inline embed ceiling ({AUTO_EMBED_MAX}) — run `codegraph semantic-index` to refresh semantic search",
            items.len()
        );
        return;
    }
    let texts: Vec<String> = items.iter().map(|(_, t)| t.clone()).collect();
    match codegraph_llm::embed_texts(&texts) {
        Some((vecs, model)) if model == stamped => {
            let rows: Vec<(String, Vec<f32>)> =
                items.iter().zip(vecs).map(|((n, _), v)| (n.id.clone(), v)).collect();
            if let Err(e) = store.upsert_vectors(&rows) {
                eprintln!("codegraph: semantic vector refresh failed: {e}");
            }
        }
        Some((_, model)) => eprintln!(
            "codegraph: vectors are stamped '{stamped}' but the current embedder is '{model}' — refresh skipped; run `codegraph semantic-index` to re-embed"
        ),
        None => eprintln!(
            "codegraph: no embedder reachable — semantic search may be stale for the changed files"
        ),
    }
}

/// The observable shape of a freshly parsed file — the counterpart of
/// `Store::file_shape` (same item formats, so the two compare directly).
/// Ids encode name AND class nesting (B1), so a method moving between classes
/// shows up as a definition change. Locals and imports are excluded: they only
/// influence calls in this same file, which any partial rebuild re-resolves
/// anyway. Line numbers and bodies are excluded by construction — the common
/// "edit a function body" case keeps the shape stable.
fn parsed_shape(pf: &ParsedFile) -> codegraph_store::FileShape {
    let mut shape = codegraph_store::FileShape::default();
    for n in &pf.nodes {
        match n.label {
            codegraph_core::NodeLabel::File => {}
            codegraph_core::NodeLabel::Function | codegraph_core::NodeLabel::Method => {
                shape.fn_defs.insert(n.id.clone(), n.name.clone());
            }
            label => shape.other.push(format!("n\u{1}{}\u{1}{}\u{1}{:?}", n.id, n.name, label)),
        }
    }
    for i in &pf.inherits {
        shape.other.push(format!("i\u{1}{}\u{1}{}\u{1}{:?}", i.impl_name, i.super_name, i.kind));
    }
    for f in &pf.fields {
        shape.other.push(format!("f\u{1}{}\u{1}{}\u{1}{}", f.class_id, f.field_name, f.type_name));
    }
    shape.other.sort_unstable();
    shape
}

/// Names of Function/Method definitions present in exactly one of the two
/// shapes — the "dirty" names whose call sites (anywhere) must re-resolve.
fn fn_diff_names(old: &codegraph_store::FileShape, new: &codegraph_store::FileShape) -> Vec<String> {
    let mut out = Vec::new();
    for (id, name) in &old.fn_defs {
        if new.fn_defs.get(id) != Some(name) {
            out.push(name.clone());
        }
    }
    for (id, name) in &new.fn_defs {
        if old.fn_defs.get(id) != Some(name) {
            out.push(name.clone());
        }
    }
    out
}

/// Would `auto_indexstore` re-merge on this run (fresh Xcode build)? The
/// partial edge rebuild must fall back to the full path in that case.
#[cfg(feature = "indexstore")]
fn indexstore_wants_remerge(db: &Store, root: &Path) -> bool {
    let Some(store_path) = find_index_store(root) else { return false };
    let mtime = std::fs::metadata(&store_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default();
    let stamped = db.meta_get("indexstore_mtime").ok().flatten().unwrap_or_default();
    !mtime.is_empty() && mtime != stamped
}

#[cfg(not(feature = "indexstore"))]
fn indexstore_wants_remerge(_db: &Store, _root: &Path) -> bool {
    false
}

/// Current HEAD sha (None outside a git repo) — the cochanges cache key.
fn git_head(root: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", &root.to_string_lossy(), "rev-parse", "HEAD"])
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Git co-change pairs: files that changed together in the last 1000 commits
/// (unordered pairs, ≥2 occurrences; mega-commits >30 files skipped as noise).
/// Deterministic for a given HEAD. Empty when not a git repo.
fn compute_cochanges(root: &Path) -> Vec<(String, String, u32)> {
    const COMMITS: &str = "1000";
    const MAX_FILES_PER_COMMIT: usize = 30;
    const MIN_PAIR_COUNT: u32 = 2;
    const MAX_PAIRS: usize = 20_000;
    let Ok(out) = std::process::Command::new("git")
        .args(["-C", &root.to_string_lossy(), "log", "--no-merges", "--name-only", "--pretty=format:%x00", "-n", COMMITS])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut counts: HashMap<(String, String), u32> = HashMap::new();
    for block in text.split('\0') {
        let files: Vec<&str> = block.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        if files.len() < 2 || files.len() > MAX_FILES_PER_COMMIT {
            continue;
        }
        for i in 0..files.len() {
            for j in (i + 1)..files.len() {
                let (a, b) = if files[i] < files[j] { (files[i], files[j]) } else { (files[j], files[i]) };
                *counts.entry((a.to_string(), b.to_string())).or_insert(0) += 1;
            }
        }
    }
    let mut pairs: Vec<(String, String, u32)> =
        counts.into_iter().filter(|(_, n)| *n >= MIN_PAIR_COUNT).map(|((a, b), n)| (a, b, n)).collect();
    pairs.sort_by(|x, y| y.2.cmp(&x.2).then(x.0.cmp(&y.0)).then(x.1.cmp(&y.1)));
    pairs.truncate(MAX_PAIRS);
    pairs
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
/// Swift compiler-grade tier — SMART + automatic (no manual step):
/// - picks the DerivedData store MATCHING this repo's .xcodeproj/.xcworkspace name
///   (never a random other project's store);
/// - re-merges only when the store is NEWER than the stamped mtime (or forced);
/// - otherwise REUSES the previously merged edges from the DB, so incremental /
///   auto-heal reindexes never silently drop compiler edges.
#[cfg(feature = "indexstore")]
fn auto_indexstore(db: &Store, root: &Path, nodes: &[Node], edges: &mut Vec<Edge>, force: bool) {
    let extend_superseding = |edges: &mut Vec<Edge>, new: Vec<Edge>| {
        let superseded: HashSet<(String, String, EdgeRelation)> =
            new.iter().map(|e| (e.src.clone(), e.dst.clone(), e.relation)).collect();
        edges.retain(|e| !superseded.contains(&(e.src.clone(), e.dst.clone(), e.relation)));
        edges.extend(new);
    };
    let reuse = |edges: &mut Vec<Edge>| {
        if let Ok(prev) = db.edges_by_justification("IndexStore") {
            if !prev.is_empty() {
                let n = prev.len();
                extend_superseding(edges, prev);
                eprintln!("indexstore: reused {n} compiler-grade edges (no new Xcode build)");
            }
        }
    };
    let Some(store) = find_index_store(root) else {
        reuse(edges);
        return;
    };
    let mtime = std::fs::metadata(&store)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default();
    let stamped = db.meta_get("indexstore_mtime").ok().flatten().unwrap_or_default();
    if !force && !mtime.is_empty() && mtime == stamped {
        reuse(edges);
        return;
    }
    match codegraph_indexstore::import_indexstore(&store, nodes, root) {
        Ok(is_edges) if !is_edges.is_empty() => {
            let n = is_edges.len();
            extend_superseding(edges, is_edges);
            let _ = db.meta_set("indexstore_mtime", &mtime);
            eprintln!("indexstore: merged {n} compiler-grade edges (fresh Xcode build detected)");
        }
        Ok(_) => reuse(edges),
        Err(e) => {
            eprintln!("indexstore: {e}");
            reuse(edges);
        }
    }
}

/// DerivedData store for THIS repo: dir names are `<XcodeProjectName>-<hash>`, so
/// only stores whose prefix matches an .xcodeproj/.xcworkspace found in the repo
/// qualify (never another project's store). Freshest match wins.
#[cfg(feature = "indexstore")]
fn find_index_store(root: &Path) -> Option<PathBuf> {
    let stems = xcode_project_stems(root);
    if stems.is_empty() {
        return None;
    }
    let home = std::env::var_os("HOME")?;
    let dd = Path::new(&home).join("Library/Developer/Xcode/DerivedData");
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(&dd).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !stems.iter().any(|s| name.starts_with(&format!("{s}-"))) {
            continue;
        }
        let store = entry.path().join("Index.noindex/DataStore");
        if !store.is_dir() {
            continue;
        }
        let Ok(m) = std::fs::metadata(&store).and_then(|md| md.modified()) else { continue };
        if best.as_ref().map(|(t, _)| m > *t).unwrap_or(true) {
            best = Some((m, store));
        }
    }
    best.map(|(_, p)| p)
}

/// Names of .xcodeproj / .xcworkspace bundles in the repo (shallow walk, 3 levels).
#[cfg(feature = "indexstore")]
fn xcode_project_stems(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut frontier = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = frontier.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".xcodeproj") || name.ends_with(".xcworkspace") {
                if let Some(stem) = name.rsplit_once('.').map(|(s, _)| s.to_string()) {
                    if !out.contains(&stem) {
                        out.push(stem);
                    }
                }
            } else if depth < 3 && p.is_dir() && !name.starts_with('.') && name != "node_modules" && name != "Pods" {
                frontier.push((p, depth + 1));
            }
        }
    }
    out
}

#[cfg(not(feature = "indexstore"))]
#[allow(clippy::ptr_arg)] // signature must match the feature-on variant (which needs Vec)
fn auto_indexstore(_db: &Store, _root: &Path, _nodes: &[Node], _edges: &mut Vec<Edge>, force: bool) {
    if force {
        eprintln!("indexstore: rebuild with `--features indexstore` (macOS + Xcode) to enable this tier");
    }
}

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

    /// Body edit (stable interface signature) must take the PARTIAL edge-rebuild
    /// path and still produce a graph byte-identical to a from-scratch full
    /// index of the same tree — the determinism guarantee extended to the
    /// incremental path.
    #[test]
    fn partial_edge_rebuild_matches_full_index() {
        let tmp = std::env::temp_dir().join(format!("cg_partial_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // cross-file edge b.other -> a.helper (globally-unique name)
        std::fs::write(tmp.join("a.py"), "def helper():\n    return 1\n\ndef caller():\n    helper()\n").unwrap();
        std::fs::write(tmp.join("b.py"), "def other():\n    helper()\n").unwrap();
        let db1 = tmp.join("g1.db");
        let s1 = index_dir(&tmp, &db1, false, None, false, None).unwrap();
        assert!(!s1.partial, "first index is a full build");

        // BODY-only edit: same defs, caller() drops its call — the stale
        // caller->helper edge must disappear via the partial path.
        std::fs::write(tmp.join("a.py"), "def helper():\n    return 2\n\ndef caller():\n    return 3\n").unwrap();
        let s2 = index_dir(&tmp, &db1, false, None, false, None).unwrap();
        assert!(s2.partial, "stable interface signature must take the partial path");

        // Fresh full index of the same tree → byte-identical canonical graph.
        let db2 = tmp.join("g2.db");
        let s3 = index_dir(&tmp, &db2, false, None, false, None).unwrap();
        assert!(!s3.partial);
        let h_incremental = Store::open(&db1).unwrap().canonical_hash().unwrap();
        let h_full = Store::open(&db2).unwrap().canonical_hash().unwrap();
        assert_eq!(h_incremental, h_full, "partial rebuild must equal a full rebuild byte-for-byte");

        // The dropped call really is gone, the cross-file edge really remains.
        let store = Store::open(&db1).unwrap();
        let helper_callers: Vec<String> =
            store.callers_of("helper").unwrap().into_iter().map(|n| n.name).collect();
        assert_eq!(helper_callers, vec!["other"], "only b.other still calls helper");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Renaming a function is a definition-only change: the WAVE path must
    /// re-resolve the callers' files (found via the raw-calls index, no repo
    /// scan) and end up byte-identical to a from-scratch full index.
    #[test]
    fn wave_rename_matches_full_index() {
        let tmp = std::env::temp_dir().join(format!("cg_wave_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("a.py"), "def helper():\n    return 1\n").unwrap();
        std::fs::write(tmp.join("b.py"), "def other():\n    helper()\n").unwrap();
        // c.py is untouched by the wave (names nothing dirty) — its rows must survive
        std::fs::write(tmp.join("c.py"), "def lonely():\n    return 2\n\ndef c_caller():\n    lonely()\n").unwrap();
        let db = tmp.join("g.db");
        index_dir(&tmp, &db, false, None, false, None).unwrap();
        assert_eq!(Store::open(&db).unwrap().callers_of("helper").unwrap().len(), 1);

        // rename helper -> freshname: b.py's call must stop resolving — the wave
        // reaches b.py through the dirty name, NOT through a full rebuild.
        std::fs::write(tmp.join("a.py"), "def freshname():\n    return 1\n").unwrap();
        let s = index_dir(&tmp, &db, false, None, false, None).unwrap();
        assert!(s.partial, "definition-only change must take the wave path");
        {
            let store = Store::open(&db).unwrap();
            assert!(store.callers_of("helper").unwrap().is_empty(), "stale edge to renamed def is gone");
            assert_eq!(store.callers_of("lonely").unwrap().len(), 1, "untouched file keeps its edges");
        }
        // byte-identical to a fresh full index of the same tree
        let db2 = tmp.join("g2.db");
        index_dir(&tmp, &db2, false, None, false, None).unwrap();
        let h1 = Store::open(&db).unwrap().canonical_hash().unwrap();
        let h2 = Store::open(&db2).unwrap().canonical_hash().unwrap();
        assert_eq!(h1, h2, "wave rebuild must equal a full rebuild byte-for-byte");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A deleted file's definitions are dirty names: its callers drop their
    /// edges via the wave, without a full rebuild.
    #[test]
    fn wave_handles_pruned_file() {
        let tmp = std::env::temp_dir().join(format!("cg_prune_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("a.py"), "def helper():\n    return 1\n").unwrap();
        std::fs::write(tmp.join("b.py"), "def other():\n    helper()\n").unwrap();
        let db = tmp.join("g.db");
        index_dir(&tmp, &db, false, None, false, None).unwrap();
        std::fs::remove_file(tmp.join("a.py")).unwrap();
        let s = index_dir(&tmp, &db, false, None, false, None).unwrap();
        assert!(s.partial, "fn-only prune takes the wave path");
        assert_eq!(s.pruned, 1);
        let store = Store::open(&db).unwrap();
        assert!(store.callers_of("helper").unwrap().is_empty());
        assert!(store.validate_graph().unwrap().is_empty(), "no dangling edges after prune");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A class/inherit change is beyond the wave invariant — full rebuild.
    #[test]
    fn class_change_falls_back_to_full_rebuild() {
        let tmp = std::env::temp_dir().join(format!("cg_cls_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("a.py"), "def helper():\n    return 1\n").unwrap();
        let db = tmp.join("g.db");
        index_dir(&tmp, &db, false, None, false, None).unwrap();
        std::fs::write(tmp.join("a.py"), "def helper():\n    return 1\n\nclass NewThing:\n    pass\n").unwrap();
        let s = index_dir(&tmp, &db, false, None, false, None).unwrap();
        assert!(!s.partial, "class addition must force the full path");
        assert!(Store::open(&db).unwrap().validate_graph().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

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
