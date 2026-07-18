//! Incremental repo indexing: walk → sha256 → (re)parse changed → persist →
//! rebuild edges from the full persisted graph (so cross-file edges stay correct).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use codegraph_core::{Edge, EdgeRelation, Metadata, Node, NodeLabel};
use codegraph_graph::{build_with, LoadedGraph};
use codegraph_parse::{parse_file, ParsedFile};
use codegraph_store::Store;
use ignore::WalkBuilder;
use rayon::prelude::*;
use sha2::{Digest, Sha256};

const EXTS: &[&str] = &[
    "rs", "py", "pyi", "js", "jsx", "mjs", "cjs", "ts", "mts", "cts", "tsx", "go", "swift", "java",
    "c", "h", "cpp", "cc", "cxx", "hpp", "hh", "hxx", "rb", "cs", "sh", "bash", "kt", "kts",
];

/// Documentation/prose files auto-ingested as searchable Document nodes during
/// `index` (READMEs, docs, changelogs). Data/log files (json, jsonl, log, csv, …)
/// are NOT auto-indexed — ingest them explicitly with `codegraph ingest` to avoid noise.
const DOC_EXTS: &[&str] = &[
    "md",
    "markdown",
    "mdx",
    "rst",
    "adoc",
    "asciidoc",
    "txt",
    // localization keys are commonly searched ("which file has this UI string?")
    "strings",
    "stringsdict",
    "po",
    "xliff",
    "xlf",
    "arb",
];

/// Lockfiles / generated manifests we never ingest even if they match an extension.
const SKIP_NAMES: &[&str] = &[
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "composer.lock",
    "poetry.lock",
    "Cargo.lock",
    "Gemfile.lock",
    "go.sum",
    "podfile.lock",
];

/// Directories never indexed (dependencies, build output, caches, VCS).
const EXCLUDE_DIRS: &[&str] = &[
    "target",
    "node_modules",
    ".venv",
    "venv",
    "env",
    "Pods",
    "build",
    "dist",
    ".git",
    ".gradle",
    ".next",
    ".nuxt",
    "__pycache__",
    ".cache",
    "DerivedData",
    "vendor",
    ".idea",
    ".vscode",
    "out",
    ".dart_tool",
    ".mypy_cache",
    ".pytest_cache",
    ".tox",
    "bin",
    "obj",
    ".svn",
    ".hg",
    ".terraform",
    "coverage",
    ".codegraph",
    "Carthage",
    ".bundle",
    "bower_components",
    ".yarn",
    ".pnp",
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

/// Full rebuild key: PARSER_VERSION catches deliberate parse-behavior bumps
/// during development; the release version catches everything a human forgot
/// to bump (resolver changes, new tiers) — every upgraded binary rebuilds
/// automatically, so a stale-engine graph can never survive an upgrade.
fn engine_version() -> String {
    format!(
        "{}+{}",
        codegraph_parse::PARSER_VERSION,
        env!("CARGO_PKG_VERSION")
    )
}

/// Cross-process index lock: the MCP server, its watcher thread, and parallel
/// CLI runs can all decide to rebuild the same graph at once — serialize them
/// so the work happens once (the loser re-checks and finds nothing changed).
/// OS advisory flock on a persistent `.lock` file (std `File::try_lock`,
/// stable since 1.89 — no dependency): the KERNEL releases it the instant the
/// owner dies (kill -9 included) — no PID stamps, no staleness windows, no
/// steal logic. The file is never deleted (deleting a flocked path
/// reintroduces the very races flock removes).
struct IndexLock {
    _file: std::fs::File, // closing the fd releases the lock
}

impl IndexLock {
    fn acquire(db: &Path) -> Option<IndexLock> {
        const MAX_WAIT_SECS: u64 = 600;
        let path = db.with_extension("lock");
        std::fs::create_dir_all(path.parent()?).ok()?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .ok()?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(MAX_WAIT_SECS);
        loop {
            if file.try_lock().is_ok() {
                return Some(IndexLock { _file: file });
            }
            if std::time::Instant::now() >= deadline {
                // ponytail: a >10-min holder is hung — proceed UNLOCKED rather
                // than brick; SQLite still serializes the writes, so the cost
                // is duplicated work, never a wrong graph.
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
}

/// The ignore-aware walker shared by the indexer AND the staleness probe — they
/// MUST use the same file set or they disagree and reintroduce false positives.
pub(crate) fn build_walker(root: &Path) -> ignore::Walk {
    WalkBuilder::new(root)
        .git_ignore(true)
        .git_global(true)
        // dot-dirs carry real content (.claude/ agent docs, .github/ configs)
        // and git enumerates them — hiding them here would make the two
        // enumeration paths disagree AND drop real symbols. Junk dot-dirs
        // (.git, .venv, .idea, …) are excluded by EXCLUDE_DIRS below.
        .hidden(false)
        .add_custom_ignore_filename(".codegraphignore")
        .filter_entry(|e| {
            !e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                || !EXCLUDE_DIRS.contains(&e.file_name().to_str().unwrap_or(""))
        })
        .build()
}

/// ONE file-enumeration source shared by the indexer AND the staleness probe —
/// they MUST use the same file set or they disagree and reintroduce false
/// positives. Fast path: `git ls-files` (tracked + untracked-unignored, nested
/// .gitignore handled by git itself — no directory traversal, big win on large
/// repos since the probe runs before every query). Walker fallback for non-git
/// trees and for repos whose semantics ls-files can't reproduce.
pub(crate) fn list_files(root: &Path) -> Vec<PathBuf> {
    if let Some(v) = git_ls_files(root) {
        return v;
    }
    build_walker(root)
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| !t.is_dir()).unwrap_or(false))
        .map(|e| e.into_path())
        .collect()
}

/// `git ls-files` enumeration, or None when the walker must be used instead
/// (not a git repo, or submodules exist — ls-files shows one gitlink entry
/// where the walker descends, and dropping submodule symbols would be a false
/// negative).
///
/// STRICTLY BETTER than the walker where it applies: git knows which files are
/// TRACKED, and tracked beats gitignore — a `docs/*`-ignored dir whose files
/// were committed anyway is part of the repo; the walker can only see the
/// ignore pattern and silently drops them (measured: 26 real docs on a live
/// monorepo). `.codegraphignore` is applied by us on the git listing
/// (root-level file; a NESTED .codegraphignore is walker-only).
///
/// NESTED PLAIN REPOS (monorepo of independent .git checkouts, no .gitmodules —
/// e.g. a parent repo with backend/, web/, ios/ each their own repo): ls-files
/// silently skips their entire subtree, which the walker would index. Untracked
/// directories carrying a `.git` are therefore enumerated RECURSIVELY; if any
/// level can't take the git path, the whole enumeration falls back to the
/// walker — losing a subproject's symbols is exactly the false negative this
/// tool promises never to produce.
fn git_ls_files(root: &Path) -> Option<Vec<PathBuf>> {
    if root.join(".gitmodules").exists() {
        return None;
    }
    let cgi = {
        let f = root.join(".codegraphignore");
        if f.exists() {
            let mut b = ignore::gitignore::GitignoreBuilder::new(root);
            if b.add(&f).is_some() {
                return None; // unparseable custom ignore -> walker decides
            }
            Some(b.build().ok()?)
        } else {
            None
        }
    };
    // Both listings spawn CONCURRENTLY, and nested repos enumerate in
    // parallel — this probe runs before every query, so spawn latency is
    // user-visible (measured: 12 sequential spawns ≈ 150 ms on a monorepo
    // of 6 repos; parallel ≈ one spawn's worth per nesting level).
    let spawn_git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()
    };
    let child_files = spawn_git(&[
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
    ])?;
    // dirs collapsed: the ONLY place nested repos are visible
    let child_dirs = spawn_git(&[
        "ls-files",
        "-z",
        "--others",
        "--exclude-standard",
        "--directory",
        "--no-empty-directory",
    ])?;
    let take = |child: std::process::Child| {
        let out = child.wait_with_output().ok()?;
        out.status.success().then_some(out.stdout)
    };
    let listed = take(child_files)?;
    let dirs = take(child_dirs)?;
    let mut files: Vec<PathBuf> = listed
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|s| std::str::from_utf8(s).ok())
        // tracked files under excluded dirs (vendored deps, build output)
        // are skipped by the walker's filter — mirror it exactly
        .filter(|rel| !rel.split('/').any(|c| EXCLUDE_DIRS.contains(&c)))
        .map(|rel| root.join(rel))
        .collect();
    let subs: Vec<PathBuf> = dirs
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|s| std::str::from_utf8(s).ok())
        .filter(|rel| rel.ends_with('/'))
        .filter(|rel| !rel.split('/').any(|c| EXCLUDE_DIRS.contains(&c)))
        .map(|rel| root.join(rel.trim_end_matches('/')))
        .filter(|sub| sub.join(".git").exists())
        .collect();
    let nested: Vec<Option<Vec<PathBuf>>> = subs.par_iter().map(|sub| git_ls_files(sub)).collect();
    for n in nested {
        files.extend(n?); // None anywhere => walker everywhere
    }
    // apply THIS level's .codegraphignore over everything below it (own files +
    // nested-repo files) — same cascade the walker's custom-ignore gives
    if let Some(g) = cgi {
        files.retain(|p| !g.matched_path_or_any_parents(p, false).is_ignore());
    }
    Some(files)
}

/// Some(is_doc) if a file is indexable, None to skip. Shared predicate.
fn classify(path: &Path, meta: &std::fs::Metadata) -> Option<bool> {
    if !meta.is_file() {
        return None;
    }
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
    if meta.len() > MAX_FILE_BYTES {
        return None;
    }
    Some(is_doc)
}

/// tsconfig*.json — tracked for staleness (alias-map input), never parsed.
fn is_tsconfig(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy())
        .unwrap_or_default();
    name.starts_with("tsconfig") && name.ends_with(".json")
}

fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// File mtime as nanoseconds since epoch (0 if unavailable). The cheap staleness signal.
fn file_mtime(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
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
    let Ok(store) = Store::open(&db) else {
        return true;
    };
    // a binary with different parse/resolve behavior means EVERY file is
    // effectively stale — without this, ensure_fresh/MCP would serve an
    // old-engine graph forever after an upgrade (index_dir's gate only helps
    // if a reindex runs)
    if store.meta_get("parser_version").ok().flatten().as_deref() != Some(engine_version().as_str())
    {
        return true;
    }
    // a new/updated .scip on disk (e.g. a background auto_scip run finished)
    // must be merged before the next query is served
    if scip_file_changed(&store, root, None) {
        return true;
    }
    let Ok(rows) = store.manifest_map() else {
        return true;
    };
    let prev: std::collections::HashMap<String, i64> =
        rows.into_iter().map(|m| (m.file_path, m.mtime)).collect();
    // Parallel stat sweep: this probe runs before EVERY query — per-file
    // stat latency (8k+ files, external disks) is the visible cost.
    let entries: Vec<(String, i64)> = list_files(root)
        .par_iter()
        .filter_map(|path| {
            // stat failure = listed-but-gone (git still tracks a deleted file)
            // — not part of the working tree, so not part of the comparison set
            let meta = std::fs::metadata(path).ok()?;
            if classify(path, &meta).is_none() && !is_tsconfig(path) {
                return None;
            }
            Some((rel_path(root, path), file_mtime(&meta)))
        })
        .collect();
    for (rel, mtime) in &entries {
        match prev.get(rel) {
            None => return true,                                    // added file
            Some(prev_mtime) if prev_mtime != mtime => return true, // changed/touched
            Some(_) => {}
        }
    }
    // every entry matched something in the manifest; a count mismatch means
    // the manifest has files that vanished on disk
    entries.len() != prev.len()
}

/// Identity gate: refuse to answer from a graph built for a DIFFERENT repo (or
/// written by a different tool into our cache slot) — a wrong graph is worse
/// than no graph. Read-only; never mutates a foreign DB. Legacy graphs without
/// a stamp pass (the next index stamps them).
pub fn check_identity(root: &Path, db: &Path) -> Result<()> {
    if !db.exists() {
        return Ok(());
    }
    let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    match codegraph_store::query_readonly(db, "SELECT value FROM meta WHERE key = 'repo_root'", 1) {
        Ok((_, rows)) => match rows.first().and_then(|r| r.first()) {
            Some(stamped) if *stamped != canon.to_string_lossy() => anyhow::bail!(
                "graph at {} was built for {}, not {} — run `codegraph index {}`",
                db.display(),
                stamped,
                canon.display(),
                canon.display()
            ),
            _ => Ok(()),
        },
        Err(_) => anyhow::bail!(
            "{} is not a codegraph graph (no meta table — another tool may have written it). Delete it and re-run `codegraph index`",
            db.display()
        ),
    }
}

/// Make the graph match the working tree before serving a query: build it if
/// missing, incrementally reindex if anything changed. The clean path is the
/// stat-only probe above. This is the guarantee that queries never serve stale
/// results (no false positives after edits / add / delete / git checkout).
pub fn ensure_fresh(root: &Path) -> Result<()> {
    // Long-lived MCP sessions only stamp the registry at startup; a month-long
    // session would look idle to the TTL sweep of a command run in ANOTHER
    // repo, which could delete this graph out from under the server. Touching
    // (throttled) marks the project as live. 15 min ≪ any sane TTL.
    {
        use std::sync::atomic::{AtomicU64, Ordering};
        static LAST_TOUCH: AtomicU64 = AtomicU64::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if now.saturating_sub(LAST_TOUCH.load(Ordering::Relaxed)) > 900 {
            LAST_TOUCH.store(now, Ordering::Relaxed);
            crate::registry::housekeeping(Some((root, &db_path(root), false)));
        }
    }
    if is_stale(root) {
        let db = db_path(root);
        index_dir(root, &db, false, None, false, None)?;
    }
    Ok(())
}

pub fn index_dir(
    root: &Path,
    db: &Path,
    full: bool,
    scip: Option<&Path>,
    indexstore: bool,
    ambiguous: Option<bool>,
) -> Result<IndexStats> {
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
    // Serialize concurrent indexers (MCP server / watcher / CLI). Whoever
    // waited here re-diffs against the manifest below and finds the work
    // already done — one rebuild, not N.
    let _lock = IndexLock::acquire(db);
    let store = Store::open(db)?;
    // RAII: rolls back on early error/panic; committed explicitly below.
    let txn = store.txn()?;
    // Identity stamp (checked by `check_identity` before every query). Meta is
    // excluded from canonical_hash, so this can't break determinism.
    let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    store.meta_set("repo_root", &canon.to_string_lossy())?;
    // Engine-version gate: a binary with different parse OR resolve behavior
    // must rebuild from scratch — mixing old and new interpretations in one
    // graph breaks the incremental==full invariant.
    let parser_v = engine_version();
    let mut full = full
        || store.meta_get("parser_version").ok().flatten().as_deref() != Some(parser_v.as_str());
    store.meta_set("parser_version", &parser_v)?;
    // tsconfig `paths` aliases: rewritten imports of UNCHANGED files depend on
    // the alias map, so an alias-map change (content hash) forces a full
    // reparse — a manifest sha can't see it.
    // ponytail: this walks the repo once more on every index (~tens of ms); it
    // must run BEFORE the no-change early return because tsconfig-only edits
    // don't bump `changed`. Gate behind a manifest lookup if profiles complain.
    let (aliases, ts_hash) = crate::tsconfig::load_alias_maps(root);
    full =
        full || store.meta_get("tsconfig_hash").ok().flatten().as_deref() != Some(ts_hash.as_str());
    store.meta_set("tsconfig_hash", &ts_hash)?;
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
        store
            .manifest_map()?
            .into_iter()
            .map(|m| (m.file_path.clone(), m))
            .collect()
    };
    let mut to_parse: Vec<(String, String, String, i64, bool)> = Vec::new();
    for path in list_files(root) {
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        // tsconfig files are tracked in the manifest (so is_stale sees their
        // edits) but never parsed — their CONTENT is consumed by `aliases`.
        if is_tsconfig(&path) {
            let rel = rel_path(root, &path);
            seen.insert(rel.clone());
            let mtime = file_mtime(&meta);
            if manifest_map.get(&rel).map(|m| m.mtime) != Some(mtime) {
                if let Ok(source) = std::fs::read_to_string(&path) {
                    store.save_manifest(&rel, &sha256(&source), mtime)?;
                }
            }
            continue;
        }
        let Some(is_doc) = classify(&path, &meta) else {
            continue;
        };
        let rel = rel_path(root, &path);
        let mtime = file_mtime(&meta);
        files += 1;
        seen.insert(rel.clone());
        let manifest = manifest_map.get(&rel);
        if let Some(m) = manifest {
            if m.mtime == mtime && mtime != 0 {
                continue; // unchanged — stat fast-path, no read
            }
        }
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
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
            // Binary sniff: NUL in the first 8KB = generated/minified binary
            // masquerading as text (NUL is valid UTF-8, so read_to_string
            // passes it). Keep it MANIFESTED (the staleness probe stays
            // stat-only and never re-flags it) but contribute nothing.
            let binary = source.as_bytes().iter().take(8192).any(|&b| b == 0);
            let pf = if binary {
                ParsedFile {
                    nodes: Vec::new(),
                    calls: Vec::new(),
                    inherits: Vec::new(),
                    fields: Vec::new(),
                    locals: Vec::new(),
                    imports: Vec::new(),
                    type_refs: Vec::new(),
                }
            } else if *is_doc {
                let ctype = rel.rsplit('.').next().unwrap_or("text");
                ParsedFile {
                    nodes: document_nodes(rel, ctype, source),
                    calls: Vec::new(),
                    inherits: Vec::new(),
                    fields: Vec::new(),
                    locals: Vec::new(),
                    imports: Vec::new(),
                    type_refs: Vec::new(),
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
        store.save_type_refs(&rel, &pf.type_refs)?;
        // TS/JS non-relative imports: expand tsconfig aliases to root-relative
        // `/dir/file` modules. Whatever doesn't match stays with its package
        // specifier — the resolver ignores it, but coverage uses it as
        // EXTERNALITY EVIDENCE (a call bound to an external import can never
        // resolve in-repo, so it doesn't belong in the recall denominator).
        let is_ts_js = matches!(
            rel.rsplit('.').next().unwrap_or(""),
            "ts" | "tsx" | "js" | "jsx" | "mjs"
        );
        let imports: Vec<codegraph_core::RawImport> = pf
            .imports
            .iter()
            .map(|im| {
                if is_ts_js && !im.module.starts_with('.') {
                    if let Some(module) = aliases.resolve(&rel, &im.module) {
                        return codegraph_core::RawImport {
                            module,
                            ..im.clone()
                        };
                    }
                }
                im.clone()
            })
            .collect();
        store.save_imports(&rel, &imports)?;
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
    // `--indexstore` must reach the merge path even with zero file changes —
    // the flag EXISTS to force a re-merge (field bug: it early-returned here).
    if changed == 0 && pruned == 0 && !full && !indexstore && !scip_file_changed(&store, root, scip)
    {
        // Self-heal graphs committed by older binaries (pre-heal dangling edges).
        heal_dangling(&store);
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
        None => {
            store
                .meta_get("include_ambiguous")
                .ok()
                .flatten()
                .as_deref()
                == Some("1")
        }
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
        && !scip_file_changed(&store, root, scip)
        && !indexstore
        && !indexstore_wants_remerge(&store, root)
        && !scip_wants_reacquire(&store, root);
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
            &nodes,
            &calls_wave,
            &inherits,
            &fields,
            &locals,
            &imports,
            include_ambiguous,
            &wave_set,
        );
        for f in &wave {
            store.delete_tree_sitter_edges_for_file(f)?;
        }
        // Conflicts can only be surviving compiler-grade (Scip-tier) edges,
        // which outrank tree-sitter — keep them.
        store.bulk_insert_edges_keep_existing(&new_edges)?;
        // A spared compiler-grade edge can orphan when the reparse renamed its
        // endpoint — heal BEFORE analytics read the edge set.
        heal_dangling(&store);
        // Hyperedges + implementer sets depend only on inherits and inherit-name
        // uniqueness, both guaranteed untouched by the gates above.
        (store.graph_edges()?, 0)
    } else {
        let calls = store.all_calls()?;
        let built = build_with(
            &nodes,
            &calls,
            &inherits,
            &fields,
            &locals,
            &imports,
            include_ambiguous,
        );
        let mut edges = built.edges;
        // Capture the artifact's mtime BEFORE reading it: if the background
        // indexer finishes mid-merge, stamping a post-merge mtime would mark
        // the final content as examined without ever merging it.
        let scip_seen = scip_path(root, None)
            .map(|p| mtime_secs(&p))
            .filter(|m| !m.is_empty());
        let scip_edges = merge_scip_edges(root, scip, &nodes, &mut edges);
        // SCIP tier is STICKY: once merged, full rebuilds reuse the persisted
        // compiler-grade edges (filtered against current nodes), and if the
        // user opted in (ran `codegraph scip` once) a moved HEAD auto-reruns
        // the indexer — same contract as the Xcode IndexStore tier.
        auto_scip(&store, root, &nodes, &mut edges, scip_edges, scip_seen);
        // Swift compiler-grade edges are AUTOMATIC when the feature is compiled:
        // fresh Xcode build (store mtime > stamped) -> re-merge; otherwise the
        // previously merged edges are REUSED so auto-heal never drops them.
        // `--indexstore` just forces a re-merge.
        auto_indexstore(&store, root, &nodes, &mut edges, indexstore);
        // Belt-and-braces: no merge source may contribute an edge whose endpoint
        // isn't in the current node set (zero-phantom invariant).
        let valid: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        edges.retain(|e| valid.contains(e.src.as_str()) && valid.contains(e.dst.as_str()));
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
    // missing justification tags. Dangling edges are auto-healed first (dropping
    // is always precision-safe); anything validate still finds is a real bug and
    // stays loud. Violations never brick the index — a degraded graph beats no
    // graph, and the message tells the user exactly what to report.
    if !partial_ok {
        // the partial branch already healed (before analytics read the edges)
        heal_dangling(&store);
    }
    let violations = store.validate_graph()?;
    if !violations.is_empty() {
        eprintln!(
            "codegraph: graph validation found {} issue(s):",
            violations.len()
        );
        for v in violations.iter().take(10) {
            eprintln!("  - {v}");
        }
    }
    store.bump_generation()?;
    txn.commit()?;
    auto_embed_changed(&store, root, &changed_nodes);

    Ok(IndexStats {
        files,
        changed,
        pruned,
        nodes: nodes.len(),
        edges: edges.len(),
        scip_edges,
        partial: partial_ok,
    })
}

/// The text embedded for one node — shared by `semantic-index` (full pass) and
/// the post-index auto-refresh, so all vectors live in ONE embedding space.
/// Documents embed their chunk CONTENT (capped), not just the title.
pub fn embed_text_for(n: &Node) -> String {
    let mut t = format!("{} {:?} in {}", n.name, n.label, n.file_path);
    if let Some(text) = n.metadata.get("text").and_then(|v| v.as_str()) {
        let cap = text
            .char_indices()
            .nth(2000)
            .map(|(i, _)| i)
            .unwrap_or(text.len());
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
fn auto_embed_changed(store: &Store, root: &Path, changed: &[Node]) {
    // ponytail: inline ceiling — bigger batches (fresh index, git checkout)
    // should go through the explicit, progress-reporting `semantic-index`.
    const AUTO_EMBED_MAX: usize = 2000;
    let Ok(Some(stamped)) = store.meta_get("embed_model") else {
        return;
    };
    let items: Vec<(&Node, String)> = changed
        .iter()
        .filter(|n| n.label != NodeLabel::File)
        .map(|n| (n, embed_text_for(n)))
        .collect();
    if items.is_empty() {
        return;
    }
    if items.len() > AUTO_EMBED_MAX {
        // A full reparse (parser upgrade, tsconfig change, --full) deletes and
        // re-creates EVERY node — their vectors are gone with them. Silently
        // skipping here would leave semantic_search empty until a manual
        // semantic-index (an stderr warning the MCP client never sees). Since
        // the user has opted into semantic (embed_model stamped), self-heal by
        // running semantic-index in the BACKGROUND — same detached pattern as
        // auto_scip; the generation stamp prevents respawn loops.
        let generation = store
            .meta_get("generation")
            .ok()
            .flatten()
            .unwrap_or_default();
        let pending = store
            .meta_get("embed_pending")
            .ok()
            .flatten()
            .unwrap_or_default();
        if pending != generation {
            let spawned = std::env::current_exe().ok().and_then(|exe| {
                std::process::Command::new(exe)
                    .args(["semantic-index", "--path"])
                    .arg(root)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .stdin(std::process::Stdio::null())
                    .spawn()
                    .ok()
            });
            if spawned.is_some() {
                let _ = store.meta_set("embed_pending", &generation);
                eprintln!(
                    "codegraph: {} symbols need re-embedding (> inline ceiling {AUTO_EMBED_MAX}) — semantic-index running in the background",
                    items.len()
                );
                return;
            }
        }
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
            label => shape
                .other
                .push(format!("n\u{1}{}\u{1}{}\u{1}{:?}", n.id, n.name, label)),
        }
    }
    for i in &pf.inherits {
        shape.other.push(format!(
            "i\u{1}{}\u{1}{}\u{1}{:?}",
            i.impl_name, i.super_name, i.kind
        ));
    }
    for f in &pf.fields {
        shape.other.push(format!(
            "f\u{1}{}\u{1}{}\u{1}{}",
            f.class_id, f.field_name, f.type_name
        ));
    }
    shape.other.sort_unstable();
    shape
}

/// Names of Function/Method definitions present in exactly one of the two
/// shapes — the "dirty" names whose call sites (anywhere) must re-resolve.
fn fn_diff_names(
    old: &codegraph_store::FileShape,
    new: &codegraph_store::FileShape,
) -> Vec<String> {
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
    let Some(store_path) = find_index_store(root) else {
        return false;
    };
    let mtime = mtime_secs(&store_path);
    let stamped = db
        .meta_get("indexstore_mtime")
        .ok()
        .flatten()
        .unwrap_or_default();
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
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
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
        .args([
            "-C",
            &root.to_string_lossy(),
            "log",
            "--no-merges",
            "--name-only",
            "--pretty=format:%x00",
            "-n",
            COMMITS,
        ])
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
        let files: Vec<&str> = block
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        if files.len() < 2 || files.len() > MAX_FILES_PER_COMMIT {
            continue;
        }
        for i in 0..files.len() {
            for j in (i + 1)..files.len() {
                let (a, b) = if files[i] < files[j] {
                    (files[i], files[j])
                } else {
                    (files[j], files[i])
                };
                *counts.entry((a.to_string(), b.to_string())).or_insert(0) += 1;
            }
        }
    }
    let mut pairs: Vec<(String, String, u32)> = counts
        .into_iter()
        .filter(|(_, n)| *n >= MIN_PAIR_COUNT)
        .map(|((a, b), n)| (a, b, n))
        .collect();
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

/// File mtime as a seconds-since-epoch string ("" if unavailable) — the shared
/// freshness-stamp currency for the IndexStore/SCIP tiers.
fn mtime_secs(p: &Path) -> String {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default()
}

/// Splice `new` edges in, superseding any existing edge with the same
/// (src, dst, relation) key — compiler-grade edges outrank tree-sitter ones.
fn extend_superseding(edges: &mut Vec<Edge>, new: Vec<Edge>) {
    {
        let superseded: HashSet<(&str, &str, EdgeRelation)> = new
            .iter()
            .map(|e| (e.src.as_str(), e.dst.as_str(), e.relation))
            .collect();
        edges.retain(|e| !superseded.contains(&(e.src.as_str(), e.dst.as_str(), e.relation)));
    }
    edges.extend(new);
}

/// Replay previously persisted edges of one justification (e.g. "IndexStore"),
/// DROPPING any whose endpoint is gone from the current node set — a reparse can
/// rename/remove structural node ids, and replaying a stale edge verbatim would
/// violate the zero-phantom-edge invariant. Returns (reused, dropped).
///
/// ponytail: endpoint-existence is necessary, not sufficient — a retargeted
/// call whose OLD target still exists elsewhere replays a stale edge until the
/// next compiler run (IndexStore: next Xcode build; SCIP: next HEAD move).
/// Upgrade path if it bites: stamp per-file freshness on compiler edges and
/// drop those whose src file changed since the merge.
fn reuse_persisted_edges(
    db: &Store,
    justification: &str,
    nodes: &[Node],
    edges: &mut Vec<Edge>,
) -> (usize, usize) {
    let Ok(prev) = db.edges_by_justification(justification) else {
        return (0, 0);
    };
    if prev.is_empty() {
        return (0, 0);
    }
    let valid: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    let total = prev.len();
    let live: Vec<Edge> = prev
        .into_iter()
        .filter(|e| valid.contains(e.src.as_str()) && valid.contains(e.dst.as_str()))
        .collect();
    let reused = live.len();
    if reused > 0 {
        extend_superseding(edges, live);
    }
    (reused, total - reused)
}

/// Has the on-disk `.scip` changed since it was last merged (or was one passed
/// explicitly)? Gates the full-rebuild path: a `.scip` that merely EXISTS but
/// is already merged must not force a full rebuild on every index — only a new
/// or updated one does (that's how a background indexer run gets picked up).
fn scip_file_changed(db: &Store, root: &Path, explicit: Option<&Path>) -> bool {
    if explicit.is_some() {
        return true;
    }
    let Some(p) = scip_path(root, None) else {
        return false;
    };
    let mtime = mtime_secs(&p);
    db.meta_get("scip_file_mtime")
        .ok()
        .flatten()
        .unwrap_or_default()
        != mtime
}

/// Would `auto_scip` re-run the SCIP indexer on this run (opted in + HEAD
/// moved)? The partial edge rebuild must fall back to the full path then.
fn scip_wants_reacquire(db: &Store, root: &Path) -> bool {
    if db.meta_get("scip_auto").ok().flatten().as_deref() != Some("1")
        || std::env::var("CODEGRAPH_AUTO_SCIP").as_deref() == Ok("0")
    {
        return false;
    }
    let Some(head) = git_head(root) else {
        return false;
    };
    db.meta_get("scip_stamp").ok().flatten().as_deref() != Some(head.as_str())
}

/// SCIP tier lifecycle on the full-rebuild path. `fresh_scip` > 0 means
/// `merge_scip_edges` just consumed a `.scip` from disk: stamp it and opt in
/// to auto-reacquire. Otherwise: opted-in + HEAD moved → spawn the detected
/// indexer DETACHED (never block a query on a minutes-long compiler run; the
/// staleness probe picks the fresh `.scip` up when it lands) and meanwhile
/// replay the persisted Scip-justified edges (endpoint-filtered, zero-phantom)
/// so full rebuilds never silently lose the compiler tier.
fn auto_scip(
    db: &Store,
    root: &Path,
    nodes: &[Node],
    edges: &mut Vec<Edge>,
    fresh_scip: usize,
    scip_seen: Option<String>,
) {
    let head = git_head(root).unwrap_or_default();
    // Stamp the artifact as EXAMINED (pre-merge mtime) whether or not it
    // yielded edges — a zero-edge .scip (foreign/empty/still-being-written)
    // must not pin scip_file_changed→is_stale true and force a full rebuild
    // on every query forever. A later write moves the mtime → re-examined.
    if let Some(mtime) = &scip_seen {
        let _ = db.meta_set("scip_file_mtime", mtime);
    }
    if fresh_scip > 0 {
        let _ = db.meta_set("scip_auto", "1");
        let _ = db.meta_set("scip_pending", "");
        if !head.is_empty() {
            let _ = db.meta_set("scip_stamp", &head);
        }
        return;
    }
    if scip_wants_reacquire(db, root) {
        // one in-flight run at a time: `scip_pending` holds the HEAD being indexed
        let pending = db
            .meta_get("scip_pending")
            .ok()
            .flatten()
            .unwrap_or_default();
        if pending != head {
            if let Some(ix) = crate::scipcmd::detect(root) {
                if crate::scipcmd::on_path(ix.bin) {
                    let spawned = std::process::Command::new(ix.bin)
                        .args(ix.args)
                        .current_dir(root)
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .stdin(std::process::Stdio::null())
                        .spawn()
                        .is_ok();
                    if spawned {
                        let _ = db.meta_set("scip_pending", &head);
                        eprintln!(
                            "scip: HEAD moved — {} running in the background; edges refresh on the next index after it finishes",
                            ix.bin
                        );
                    }
                }
            }
        }
    }
    let (reused, dropped) = reuse_persisted_edges(db, "Scip", nodes, edges);
    if dropped > 0 {
        eprintln!("scip: reused {reused} compiler-grade edges, dropped {dropped} whose endpoints no longer exist");
    } else if reused > 0 {
        eprintln!("scip: reused {reused} compiler-grade edges");
    }
}

/// Drop any persisted edge with a missing endpoint and record the count.
/// Runs before every validate/commit — after it, a dangling edge reported by
/// `validate_graph` is a genuine bug, not reuse residue.
fn heal_dangling(store: &Store) {
    match store.drop_dangling_edges() {
        Ok(n) if n > 0 => {
            eprintln!("codegraph: auto-healed {n} dangling edge(s)");
            let _ = store.meta_set("healed_edges_last", &n.to_string());
        }
        _ => {}
    }
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
    let reuse = |edges: &mut Vec<Edge>| {
        let (reused, dropped) = reuse_persisted_edges(db, "IndexStore", nodes, edges);
        if dropped > 0 {
            eprintln!("indexstore: reused {reused} compiler-grade edges, dropped {dropped} whose endpoints no longer exist");
        } else if reused > 0 {
            eprintln!("indexstore: reused {reused} compiler-grade edges (no new Xcode build)");
        }
    };
    let Some(store) = find_index_store(root) else {
        reuse(edges);
        return;
    };
    let mtime = mtime_secs(&store);
    let stamped = db
        .meta_get("indexstore_mtime")
        .ok()
        .flatten()
        .unwrap_or_default();
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
        let Ok(m) = std::fs::metadata(&store).and_then(|md| md.modified()) else {
            continue;
        };
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
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".xcodeproj") || name.ends_with(".xcworkspace") {
                if let Some(stem) = name.rsplit_once('.').map(|(s, _)| s.to_string()) {
                    if !out.contains(&stem) {
                        out.push(stem);
                    }
                }
            } else if depth < 3
                && p.is_dir()
                && !name.starts_with('.')
                && name != "node_modules"
                && name != "Pods"
            {
                frontier.push((p, depth + 1));
            }
        }
    }
    out
}

#[cfg(not(feature = "indexstore"))]
#[allow(clippy::ptr_arg)] // signature must match the feature-on variant (which needs Vec)
fn auto_indexstore(
    _db: &Store,
    _root: &Path,
    _nodes: &[Node],
    _edges: &mut Vec<Edge>,
    force: bool,
) {
    if force {
        eprintln!(
            "indexstore: rebuild with `--features indexstore` (macOS + Xcode) to enable this tier"
        );
    }
}

fn merge_scip_edges(
    root: &Path,
    explicit: Option<&Path>,
    nodes: &[Node],
    edges: &mut Vec<Edge>,
) -> usize {
    let Some(path) = scip_path(root, explicit) else {
        return 0;
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return 0;
    };
    let Ok(scip) = codegraph_resolve::import_scip(&bytes, nodes) else {
        return 0;
    };
    if scip.is_empty() {
        return 0;
    }
    let n = scip.len();
    extend_superseding(edges, scip);
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
    meta.insert(
        "text".to_string(),
        serde_json::Value::String(ch.text.clone()),
    );
    meta.insert(
        "content_type".to_string(),
        serde_json::Value::String(ch.content_type.clone()),
    );
    Node {
        id: format!("doc.{safe}.{i}"),
        label: NodeLabel::Document,
        name: if title.trim().is_empty() {
            format!("{} #{i}", ch.source)
        } else {
            title
        },
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

    fn tnode(id: &str) -> Node {
        Node {
            id: id.into(),
            label: codegraph_core::NodeLabel::Function,
            name: id.rsplit('.').next().unwrap_or(id).into(),
            file_path: "t.py".into(),
            line_start: 1,
            line_end: 1,
            language: "python".into(),
            metadata: Metadata::new(),
            community: None,
            pagerank: 0.0,
            betweenness: 0.0,
        }
    }

    fn tedge(src: &str, dst: &str, justification: &str) -> Edge {
        let mut metadata = Metadata::new();
        metadata.insert(
            "justification".into(),
            serde_json::Value::String(justification.into()),
        );
        Edge {
            src: src.into(),
            dst: dst.into(),
            relation: EdgeRelation::Calls,
            tier: codegraph_core::ResolutionTier::Scip,
            confidence: codegraph_core::Confidence::Extracted,
            src_file: "t.py".into(),
            src_line: 1,
            metadata,
        }
    }

    /// Reused compiler-grade edges must be filtered against the CURRENT node
    /// set — a stale endpoint is dropped, never replayed (zero-phantom).
    #[test]
    fn reuse_filters_dangling_edges() {
        let tmp = std::env::temp_dir().join(format!("cg_reuse_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let store = Store::open(&tmp.join("g.db")).unwrap();
        let nodes = vec![tnode("p.a"), tnode("p.b")];
        store.bulk_upsert_nodes(&nodes).unwrap();
        store
            .bulk_upsert_edges(&[
                tedge("p.a", "p.b", "IndexStore"),
                tedge("p.a", "p.ghost", "IndexStore"),
            ])
            .unwrap();
        let mut edges = Vec::new();
        let (reused, dropped) = reuse_persisted_edges(&store, "IndexStore", &nodes, &mut edges);
        assert_eq!((reused, dropped), (1, 1));
        assert_eq!(edges.len(), 1);
        assert_eq!(
            (edges[0].src.as_str(), edges[0].dst.as_str()),
            ("p.a", "p.b")
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The partial (wave) path spares compiler-grade edges — if a reparse
    /// removed such an edge's endpoint, the pre-commit heal must drop it and
    /// the graph must converge to the from-scratch result.
    #[test]
    fn partial_rebuild_heals_spared_compiler_edge() {
        let tmp = std::env::temp_dir().join(format!("cg_heal_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("a.py"),
            "def helper():\n    return 1\n\ndef caller():\n    helper()\n",
        )
        .unwrap();
        let db = tmp.join("g.db");
        index_dir(&tmp, &db, false, None, false, None).unwrap();
        // Inject a stale compiler-grade edge: real src, endpoint that no longer exists.
        {
            let store = Store::open(&db).unwrap();
            let src = store
                .graph_nodes()
                .unwrap()
                .iter()
                .find(|n| n.name == "caller")
                .unwrap()
                .id
                .clone();
            store
                .bulk_upsert_edges(&[tedge(&src, "p.ghost", "IndexStore")])
                .unwrap();
            assert!(
                !store.validate_graph().unwrap().is_empty(),
                "injection produced a dangling edge"
            );
        }
        // Body-only edit → partial path (edges table is NOT cleared) → heal must fire.
        std::fs::write(
            tmp.join("a.py"),
            "def helper():\n    return 2\n\ndef caller():\n    helper()\n",
        )
        .unwrap();
        let s = index_dir(&tmp, &db, false, None, false, None).unwrap();
        assert!(s.partial, "body-only edit must take the partial path");
        let store = Store::open(&db).unwrap();
        assert!(
            store.validate_graph().unwrap().is_empty(),
            "dangling edge healed before commit"
        );
        // Converges to the from-scratch graph byte-for-byte.
        let db2 = tmp.join("g2.db");
        index_dir(&tmp, &db2, false, None, false, None).unwrap();
        assert_eq!(
            store.canonical_hash().unwrap(),
            Store::open(&db2).unwrap().canonical_hash().unwrap(),
            "healed incremental graph must equal a fresh full index"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The graph answers only for the repo it was built from: a stamped
    /// repo_root mismatch is a hard error; legacy stamps pass; non-codegraph
    /// files are rejected.
    #[test]
    fn identity_stamp_and_check() {
        let tmp = std::env::temp_dir().join(format!("cg_ident_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let repo = tmp.join("repo");
        let other = tmp.join("other");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(repo.join("a.py"), "def f():\n    return 1\n").unwrap();
        let db = tmp.join("g.db");
        index_dir(&repo, &db, false, None, false, None).unwrap();
        // stamped + matching root → ok
        assert_eq!(
            Store::open(&db)
                .unwrap()
                .meta_get("repo_root")
                .unwrap()
                .unwrap(),
            repo.canonicalize().unwrap().to_string_lossy()
        );
        assert!(check_identity(&repo, &db).is_ok());
        // different repo → hard error naming both paths
        let err = check_identity(&other, &db).unwrap_err().to_string();
        assert!(err.contains("was built for"), "unexpected error: {err}");
        // legacy graph (meta table, no stamp) → passes with no error
        let legacy = tmp.join("legacy.db");
        drop(Store::open(&legacy).unwrap());
        assert!(check_identity(&repo, &legacy).is_ok());
        // non-codegraph file → rejected, never served
        let foreign = tmp.join("foreign.db");
        std::fs::write(&foreign, b"not a database").unwrap();
        assert!(check_identity(&repo, &foreign).is_err());
        // missing db → fine (first index will create it)
        assert!(check_identity(&repo, &tmp.join("missing.db")).is_ok());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// SCIP lifecycle safety: a repo that never opted in must never spawn an
    /// indexer (no pending stamp), and `.scip` change detection must be
    /// mtime-stamped — presence alone must NOT force endless full rebuilds.
    #[test]
    fn scip_gating_and_file_stamp() {
        let tmp = std::env::temp_dir().join(format!("cg_scipgate_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("a.py"), "def f():\n    return 1\n").unwrap();
        let db = tmp.join("g.db");
        index_dir(&tmp, &db, false, None, false, None).unwrap();
        let store = Store::open(&db).unwrap();
        assert_eq!(
            store.meta_get("scip_auto").unwrap(),
            None,
            "no opt-in without a merged .scip"
        );
        assert!(
            store
                .meta_get("scip_pending")
                .unwrap()
                .unwrap_or_default()
                .is_empty(),
            "no background indexer without opt-in"
        );
        assert!(!scip_wants_reacquire(&store, &tmp));
        // .scip change detection: none → unchanged; new file → changed; stamp → unchanged
        assert!(!scip_file_changed(&store, &tmp, None));
        std::fs::write(tmp.join("index.scip"), b"not a real scip file").unwrap();
        assert!(
            scip_file_changed(&store, &tmp, None),
            "new .scip must be picked up"
        );
        // the unstamped .scip must make the WHOLE staleness probe fire, not
        // just the scip_file_changed sub-check asserted above
        assert!(
            is_stale(&tmp),
            "a new .scip on disk must flag the graph stale"
        );
        let mtime = std::fs::metadata(tmp.join("index.scip"))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs().to_string())
            .unwrap();
        store.meta_set("scip_file_mtime", &mtime).unwrap();
        assert!(
            !scip_file_changed(&store, &tmp, None),
            "merged .scip must not force full rebuilds"
        );
        assert!(
            scip_file_changed(&store, &tmp, Some(&tmp.join("index.scip"))),
            "explicit --scip always merges"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// tsconfig `paths` aliases: `@app/svc` binds to the aliased in-repo file
    /// (ImportNarrowed), and an alias edit invalidates the graph (hash gate).
    #[test]
    fn tsconfig_alias_resolves_and_invalidates() {
        let tmp = std::env::temp_dir().join(format!("cg_alias_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("web/src")).unwrap();
        std::fs::write(
            tmp.join("web/tsconfig.json"),
            r#"{"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/*"]}}}"#,
        )
        .unwrap();
        std::fs::write(
            tmp.join("web/src/svc.ts"),
            "export function doThing() { return 1; }\n",
        )
        .unwrap();
        std::fs::write(
            tmp.join("web/src/a.ts"),
            "import { doThing } from '@app/svc';\nexport function go() { doThing(); }\n",
        )
        .unwrap();
        // decoy with the same fn name elsewhere — kills GlobalUnique so only the
        // import evidence can resolve the call
        std::fs::write(
            tmp.join("web/src/other.ts"),
            "export function doThing() { return 2; }\n",
        )
        .unwrap();
        let db = tmp.join("g.db");
        index_dir(&tmp, &db, false, None, false, None).unwrap();
        {
            let store = Store::open(&db).unwrap();
            let callers: Vec<String> = store
                .callers_of("doThing")
                .unwrap()
                .into_iter()
                .map(|n| n.name)
                .collect();
            assert_eq!(
                callers,
                vec!["go"],
                "alias-narrowed import must bind the call"
            );
            let edges = store.edges_by_justification("ImportNarrowed").unwrap();
            assert!(
                edges
                    .iter()
                    .any(|e| e.src.ends_with("go") && e.dst.contains("svc")),
                "edge must carry ImportNarrowed and point at the ALIASED file: {edges:?}"
            );
        }
        // break the alias → the edge must disappear (tsconfig hash forces full)
        std::fs::write(
            tmp.join("web/tsconfig.json"),
            r#"{"compilerOptions":{"baseUrl":".","paths":{"@elsewhere/*":["src/*"]}}}"#,
        )
        .unwrap();
        assert!(is_stale(&tmp), "tsconfig edit must flag the graph stale");
        index_dir(&tmp, &db, false, None, false, None).unwrap();
        let store = Store::open(&db).unwrap();
        assert!(
            store.callers_of("doThing").unwrap().is_empty(),
            "broken alias must drop the edge"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

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
        std::fs::write(
            tmp.join("a.py"),
            "def helper():\n    return 1\n\ndef caller():\n    helper()\n",
        )
        .unwrap();
        std::fs::write(tmp.join("b.py"), "def other():\n    helper()\n").unwrap();
        let db1 = tmp.join("g1.db");
        let s1 = index_dir(&tmp, &db1, false, None, false, None).unwrap();
        assert!(!s1.partial, "first index is a full build");

        // BODY-only edit: same defs, caller() drops its call — the stale
        // caller->helper edge must disappear via the partial path.
        std::fs::write(
            tmp.join("a.py"),
            "def helper():\n    return 2\n\ndef caller():\n    return 3\n",
        )
        .unwrap();
        let s2 = index_dir(&tmp, &db1, false, None, false, None).unwrap();
        assert!(
            s2.partial,
            "stable interface signature must take the partial path"
        );

        // Fresh full index of the same tree → byte-identical canonical graph.
        let db2 = tmp.join("g2.db");
        let s3 = index_dir(&tmp, &db2, false, None, false, None).unwrap();
        assert!(!s3.partial);
        let h_incremental = Store::open(&db1).unwrap().canonical_hash().unwrap();
        let h_full = Store::open(&db2).unwrap().canonical_hash().unwrap();
        assert_eq!(
            h_incremental, h_full,
            "partial rebuild must equal a full rebuild byte-for-byte"
        );

        // The dropped call really is gone, the cross-file edge really remains.
        let store = Store::open(&db1).unwrap();
        let helper_callers: Vec<String> = store
            .callers_of("helper")
            .unwrap()
            .into_iter()
            .map(|n| n.name)
            .collect();
        assert_eq!(
            helper_callers,
            vec!["other"],
            "only b.other still calls helper"
        );
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
        std::fs::write(
            tmp.join("c.py"),
            "def lonely():\n    return 2\n\ndef c_caller():\n    lonely()\n",
        )
        .unwrap();
        let db = tmp.join("g.db");
        index_dir(&tmp, &db, false, None, false, None).unwrap();
        assert_eq!(
            Store::open(&db)
                .unwrap()
                .callers_of("helper")
                .unwrap()
                .len(),
            1
        );

        // rename helper -> freshname: b.py's call must stop resolving — the wave
        // reaches b.py through the dirty name, NOT through a full rebuild.
        std::fs::write(tmp.join("a.py"), "def freshname():\n    return 1\n").unwrap();
        let s = index_dir(&tmp, &db, false, None, false, None).unwrap();
        assert!(s.partial, "definition-only change must take the wave path");
        {
            let store = Store::open(&db).unwrap();
            assert!(
                store.callers_of("helper").unwrap().is_empty(),
                "stale edge to renamed def is gone"
            );
            assert_eq!(
                store.callers_of("lonely").unwrap().len(),
                1,
                "untouched file keeps its edges"
            );
        }
        // byte-identical to a fresh full index of the same tree
        let db2 = tmp.join("g2.db");
        index_dir(&tmp, &db2, false, None, false, None).unwrap();
        let h1 = Store::open(&db).unwrap().canonical_hash().unwrap();
        let h2 = Store::open(&db2).unwrap().canonical_hash().unwrap();
        assert_eq!(
            h1, h2,
            "wave rebuild must equal a full rebuild byte-for-byte"
        );
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
        assert!(
            store.validate_graph().unwrap().is_empty(),
            "no dangling edges after prune"
        );
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
        std::fs::write(
            tmp.join("a.py"),
            "def helper():\n    return 1\n\nclass NewThing:\n    pass\n",
        )
        .unwrap();
        let s = index_dir(&tmp, &db, false, None, false, None).unwrap();
        assert!(!s.partial, "class addition must force the full path");
        assert!(Store::open(&db)
            .unwrap()
            .validate_graph()
            .unwrap()
            .is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Serializes every test that sets CODEGRAPH_CACHE_DIR: env vars are
    /// process-global, and two parallel tests pointing the cache at their own
    /// temp dirs corrupt each other's `db_path` mid-flight (CI-caught race).
    /// unwrap_or_else(into_inner): a panicked holder must not poison the rest.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn ensure_fresh_detects_every_change_class() {
        let _env = env_lock();
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

    /// git-ls-files enumeration must agree with the walker's semantics:
    /// tracked + untracked-unignored in, gitignored + EXCLUDE_DIRS out.
    #[test]
    fn git_enumeration_matches_walker_semantics() {
        let tmp = std::env::temp_dir().join(format!("cg_git_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("node_modules")).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&tmp)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            return; // no git on this machine — the walker fallback is already covered
        }
        std::fs::write(tmp.join("tracked.py"), "def a():\n    pass\n").unwrap();
        std::fs::write(tmp.join("untracked.py"), "def b():\n    pass\n").unwrap();
        std::fs::write(tmp.join("ignored.py"), "def c():\n    pass\n").unwrap();
        std::fs::write(tmp.join(".gitignore"), "ignored.py\n").unwrap();
        std::fs::write(tmp.join("node_modules/dep.py"), "def d():\n    pass\n").unwrap();
        assert!(git(&["add", "tracked.py"]));
        let names: Vec<String> = git_ls_files(&tmp)
            .expect("fresh git repo must take the git path")
            .iter()
            .map(|p| rel_path(&tmp, p))
            .collect();
        assert!(names.contains(&"tracked.py".into()), "{names:?}");
        assert!(
            names.contains(&"untracked.py".into()),
            "untracked-unignored must be listed: {names:?}"
        );
        assert!(
            !names.contains(&"ignored.py".into()),
            "gitignored must be skipped: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.starts_with("node_modules/")),
            "EXCLUDE_DIRS must apply: {names:?}"
        );
        // nested plain repo (monorepo-of-repos, no .gitmodules): ls-files skips
        // its subtree — enumeration must recurse into it, not lose it
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        assert!(std::process::Command::new("git")
            .args(["-C", &tmp.join("sub").to_string_lossy(), "init", "-q"])
            .status()
            .unwrap()
            .success());
        std::fs::write(tmp.join("sub/inner.py"), "def nested():\n    pass\n").unwrap();
        let names: Vec<String> = git_ls_files(&tmp)
            .unwrap()
            .iter()
            .map(|p| rel_path(&tmp, p))
            .collect();
        assert!(
            names.contains(&"sub/inner.py".into()),
            "nested repo files must be enumerated: {names:?}"
        );
        // .codegraphignore is applied by US on the git listing (root cascade
        // covers nested-repo paths too) — no walker fallback needed
        std::fs::write(tmp.join(".codegraphignore"), "untracked.py\nsub/\n").unwrap();
        let names: Vec<String> = git_ls_files(&tmp)
            .unwrap()
            .iter()
            .map(|p| rel_path(&tmp, p))
            .collect();
        assert!(
            !names.contains(&"untracked.py".into()),
            ".codegraphignore must filter the git listing: {names:?}"
        );
        assert!(
            !names.contains(&"sub/inner.py".into()),
            "root .codegraphignore must cascade into nested repos: {names:?}"
        );
        assert!(names.contains(&"tracked.py".into()), "{names:?}");
        std::fs::remove_file(tmp.join(".codegraphignore")).unwrap();
        // walker-only features force the fallback
        std::fs::write(tmp.join(".gitmodules"), "").unwrap();
        assert!(
            git_ls_files(&tmp).is_none(),
            "submodules must fall back to the walker"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The flock must exclude a second acquirer while held and release on drop
    /// (kernel-released — a dead owner can never leave a stale lock).
    #[test]
    fn index_lock_excludes_while_held_releases_on_drop() {
        let tmp = std::env::temp_dir().join(format!("cg_lock_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let db = tmp.join("graph.db");
        let held = IndexLock::acquire(&db).expect("free lock must acquire");
        // an independent open-file-description must NOT get the lock while held
        // truncate(false): flock semantics — the lock file's content is never
        // meaningful and the path may be held by another descriptor; truncating
        // a live lock file would be exactly the race flock exists to prevent
        let probe = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(db.with_extension("lock"))
            .unwrap();
        assert!(
            probe.try_lock().is_err(),
            "second acquirer must be excluded"
        );
        drop(held);
        // parallel test threads can wedge a moment between close and retry —
        // poll briefly instead of asserting on the first attempt
        let released = (0..50).any(|_| {
            probe.try_lock().is_ok() || {
                std::thread::sleep(std::time::Duration::from_millis(20));
                false
            }
        });
        assert!(released, "drop must release the lock");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A NUL-carrying file must be manifested (stat-only staleness stays quiet)
    /// but contribute zero symbols.
    #[test]
    fn binary_sniff_skips_nul_files() {
        let tmp = std::env::temp_dir().join(format!("cg_nul_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("bin.js"), "function evil() {}\n\0\0generated blob").unwrap();
        std::fs::write(tmp.join("ok.py"), "def fine():\n    pass\n").unwrap();
        let db = tmp.join("g.db");
        index_dir(&tmp, &db, false, None, false, None).unwrap();
        let store = Store::open(&db).unwrap();
        let hits = store.search_smart("evil", 10).unwrap();
        assert!(
            hits.is_empty(),
            "NUL file must contribute no symbols: {hits:?}"
        );
        assert!(!store.search_smart("fine", 10).unwrap().is_empty());
        assert!(
            store
                .manifest_map()
                .unwrap()
                .iter()
                .any(|m| m.file_path == "bin.js"),
            "must stay manifested"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A different engine version (parse OR release) must flag every graph stale.
    #[test]
    fn engine_version_change_forces_rebuild() {
        let _env = env_lock();
        let tmp = std::env::temp_dir().join(format!("cg_engv_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("CODEGRAPH_CACHE_DIR", tmp.join("cache"));
        std::fs::write(tmp.join("a.py"), "def foo():\n    return 1\n").unwrap();
        ensure_fresh(&tmp).unwrap();
        assert!(!is_stale(&tmp));
        let store = Store::open(&db_path(&tmp)).unwrap();
        store.meta_set("parser_version", "0+0.0.0").unwrap(); // simulate an old binary's stamp
        drop(store);
        assert!(is_stale(&tmp), "an old engine stamp must read as stale");
        std::env::remove_var("CODEGRAPH_CACHE_DIR");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
