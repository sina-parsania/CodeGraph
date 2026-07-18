//! MCP server: exposes the code graph to AI agents over stdio (search, callers,
//! callees, trace_path, blast_radius, context, important, implementers, routes,
//! semantic_search, get_node, stats). The graph is cached + auto-reindexed.

use std::path::{Path, PathBuf};

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::io::stdio,
    ErrorData as McpError, ServerHandler, ServiceExt,
};
use serde::Deserialize;

pub fn mcp_ready() -> bool {
    true
}

/// The built call graph + node list + O(1) lookup maps + semantic vectors —
/// expensive to construct, so cached per index generation. The maps exist
/// because several tools used to do LINEAR scans over all nodes inside loops
/// (context did ~200 × N id comparisons per call).
pub struct GraphSnapshot {
    lg: codegraph_graph::LoadedGraph,
    nodes: Vec<codegraph_core::Node>,
    by_id: std::collections::HashMap<String, usize>,
    /// First definition wins — matches the previous `iter().find(...)` semantics.
    by_name: std::collections::HashMap<String, usize>,
}

impl GraphSnapshot {
    fn node_by_id(&self, id: &str) -> Option<&codegraph_core::Node> {
        self.by_id.get(id).map(|&i| &self.nodes[i])
    }
    fn node_by_name(&self, name: &str) -> Option<&codegraph_core::Node> {
        self.by_name.get(name).map(|&i| &self.nodes[i])
    }
}

/// Cache key: the store's monotonic index generation (bumped per committed
/// index) plus the DB mtime as a fallback for pre-generation DBs — mtime alone
/// has 1-second granularity on some filesystems.
type SnapKey = (u64, Option<std::time::SystemTime>);
type GraphCache = std::sync::Arc<std::sync::Mutex<Option<(SnapKey, std::sync::Arc<GraphSnapshot>)>>>;

/// Identity of the DB file backing a pooled connection: (dev, inode) on unix.
/// A replaced file (gc + reindex) gets a different inode → the pooled handle
/// is dropped instead of silently serving the deleted file's content.
type DbFileId = Option<(u64, u64)>;

#[cfg(unix)]
fn db_file_id(p: &Path) -> DbFileId {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).ok().map(|m| (m.dev(), m.ino()))
}
#[cfg(not(unix))]
fn db_file_id(_p: &Path) -> DbFileId {
    Some((0, 0)) // no cheap inode identity — always reuse (files aren't swapped under open handles on Windows)
}

type StoreSlot = std::sync::Arc<std::sync::Mutex<Option<(DbFileId, codegraph_store::Store)>>>;

/// A Store checked out of the single-connection pool; returned on drop. Reuse
/// keeps SQLite's page cache warm across a burst of tool calls instead of
/// re-opening (and re-checking the schema) per call. Concurrent calls that
/// find the pool empty just open a fresh connection; the last one back wins.
pub struct PooledStore {
    entry: Option<(DbFileId, codegraph_store::Store)>,
    slot: StoreSlot,
}

impl std::ops::Deref for PooledStore {
    type Target = codegraph_store::Store;
    fn deref(&self) -> &codegraph_store::Store {
        &self.entry.as_ref().expect("present until drop").1
    }
}

impl Drop for PooledStore {
    fn drop(&mut self) {
        if let (Some(e), Ok(mut slot)) = (self.entry.take(), self.slot.lock()) {
            *slot = Some(e);
        }
    }
}

#[derive(Clone)]
pub struct CodeGraphServer {
    db_path: PathBuf,
    root: PathBuf,
    /// Injected freshness gate (CLI passes `index::ensure_fresh`) so live MCP
    /// queries never serve a graph that disagrees with the working tree.
    refresh: Option<fn(&Path) -> anyhow::Result<()>>,
    /// Debounce so a burst of tool calls in one agent turn re-checks at most once/sec.
    last_fresh: std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    /// Built-graph cache keyed by the DB's index generation — so a burst of graph
    /// queries in one agent turn builds the petgraph ONCE, not per call.
    graph_cache: GraphCache,
    /// Reusable read connection (see `PooledStore`).
    store_slot: StoreSlot,
    tool_router: ToolRouter<CodeGraphServer>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
    /// Symbol name or full-text query to search for.
    pub query: String,
    /// Maximum number of results (default 20).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Treat `query` as a REGEX over symbol names (middle fragments, alternations,
    /// anchors) instead of full-text search.
    #[serde(default)]
    pub regex: Option<bool>,
    /// Rerank hits with a local LLM by relevance to the query (slower; no-op
    /// when no local model is reachable).
    #[serde(default)]
    pub rerank: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ContextArgs {
    /// Natural-language description of the task/area to assemble context for.
    pub query: String,
    /// Approximate token budget for the returned context (default 1000).
    #[serde(default)]
    pub budget: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IdArgs {
    /// Fully-qualified node id (e.g. `proj.src.lib_rs.foo`).
    pub id: String,
    /// Include the symbol's SOURCE CODE (its exact line span, read from disk).
    #[serde(default)]
    pub snippet: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NameArgs {
    /// Function name.
    pub name: String,
    /// Pin ONE definition by node id (from a prior candidates list) — callers of
    /// exactly that definition, never a same-name union.
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TwoNamesArgs {
    /// Source symbol name.
    pub from: String,
    /// Target symbol name.
    pub to: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FileArgs {
    /// Repo-relative file path.
    pub file: String,
    /// Max results (default 10).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ChangesArgs {
    /// Git ref to diff against (default HEAD = uncommitted changes).
    #[serde(default)]
    pub base: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CypherArgs {
    /// Cypher-lite query: 1-2 hop MATCH with labels/relations, WHERE
    /// (=/CONTAINS/STARTS WITH/AND), RETURN var.prop..., LIMIT n.
    pub query: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LimitArgs {
    /// Max results (default 15).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RoutesArgs {
    /// Max routes returned (default 100 — keeps the payload well under client
    /// tool-result ceilings; paginate or filter for more).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Skip this many routes (pagination).
    #[serde(default)]
    pub offset: Option<usize>,
    /// Only routes whose path starts with this prefix (e.g. "/baby-tracker").
    #[serde(default)]
    pub path_prefix: Option<String>,
    /// Only this HTTP method (GET/POST/PUT/PATCH/DELETE).
    #[serde(default)]
    pub method: Option<String>,
}

/// LIST-shaped MCP responses use this LEAN row — full node JSON (metadata,
/// pagerank, community, …) measured 232 KB for one `routes` call and got the
/// result rejected by the very client the tool exists for. Full detail stays
/// behind `get_node(id)`.
fn lean(n: &codegraph_core::Node) -> serde_json::Value {
    serde_json::json!({"name": n.name, "kind": n.label, "file": n.file_path, "line": n.line_start})
}

#[tool_router]
impl CodeGraphServer {
    pub fn new(db_path: PathBuf) -> Self {
        Self::with_refresh(db_path.clone(), db_path, None)
    }

    pub fn with_refresh(
        root: PathBuf,
        db_path: PathBuf,
        refresh: Option<fn(&Path) -> anyhow::Result<()>>,
    ) -> Self {
        Self {
            db_path,
            root,
            refresh,
            last_fresh: std::sync::Arc::new(std::sync::Mutex::new(None)),
            graph_cache: std::sync::Arc::new(std::sync::Mutex::new(None)),
            store_slot: std::sync::Arc::new(std::sync::Mutex::new(None)),
            tool_router: Self::tool_router(),
        }
    }

    /// Reindex-before-serve, debounced to once per second. Best-effort — a failed
    /// refresh logs and serves the last snapshot rather than failing the query.
    fn maybe_refresh(&self) {
        let Some(f) = self.refresh else { return };
        if let Ok(mut last) = self.last_fresh.lock() {
            let due = last.map(|t| t.elapsed().as_millis() > 1000).unwrap_or(true);
            if due {
                if let Err(e) = f(&self.root) {
                    eprintln!("codegraph: auto-reindex failed ({e}); serving last snapshot");
                }
                *last = Some(std::time::Instant::now());
            }
        }
    }

    fn open(&self) -> Result<PooledStore, McpError> {
        let store = self.open_any()?;
        // ZERO-FALSE-NEGATIVE GUARD: an empty graph must never produce clean
        // empty answers ("no callers" from 0 nodes is a confident lie). The
        // classic cause is a server pointed at a moved repo / stale --path.
        // `stats` uses open_any() so the emptiness itself stays diagnosable.
        if store.node_count().unwrap_or(0) == 0 {
            return Err(McpError::internal_error(
                format!(
                    "graph is EMPTY for root {} — likely a stale/wrong server root (moved repo? stale --path in the MCP registration?). \
                     Fix the registration (register `codegraph mcp` WITHOUT --path so it follows the project directory) or run `codegraph index` in the intended repo. \
                     Do NOT conclude anything about the code from this answer.",
                    self.root.display()
                ),
                None,
            ));
        }
        Ok(store)
    }

    fn open_any(&self) -> Result<PooledStore, McpError> {
        self.maybe_refresh();
        let id = db_file_id(&self.db_path);
        if let Ok(mut slot) = self.store_slot.lock() {
            if let Some((cached, store)) = slot.take() {
                if id.is_some() && cached == id {
                    return Ok(PooledStore { entry: Some((id, store)), slot: self.store_slot.clone() });
                }
            }
        }
        let store = codegraph_store::Store::open(&self.db_path)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(PooledStore { entry: Some((id, store)), slot: self.store_slot.clone() })
    }

    #[tool(description = "Locate a symbol by NAME (exact/subword/regex). Use ONLY when you know (part of) the identifier. NOT for: conceptual or docs/wiki questions (use semantic_search), who-calls (callers), task context (context), API surface (routes). Returns exact file:line + node kind; beats grep (no comment/string hits).")]
    async fn search(&self, args: Parameters<SearchArgs>) -> Result<CallToolResult, McpError> {
        let store = self.open()?;
        let limit = args.0.limit.unwrap_or(20);
        let mut hits = if args.0.regex.unwrap_or(false) {
            store.search_regex(&args.0.query, limit)
        } else {
            store.search_smart(&args.0.query, limit)
        }
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        if args.0.rerank.unwrap_or(false) {
            // blocking LLM call — never on the async runtime (the embedder
            // probe wedge taught this lesson); best-effort, degrades to the
            // original order when no local model answers
            let q = args.0.query.clone();
            hits = tokio::task::spawn_blocking(move || codegraph_llm::rerank(&q, hits))
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        }
        // variables/properties aren't nodes — surface their declarations too
        let fields = store.field_matches(&args.0.query).unwrap_or_default();
        // COMPACT rows: full node JSON dragged metadata (incl. a Document's
        // ENTIRE text) into every answer — measured 23 KB for one query.
        // get_node(id) is the drill-down; search is the map.
        let hits: Vec<serde_json::Value> = hits
            .iter()
            .map(|n| {
                let mut row = serde_json::json!({
                    "name": n.name, "kind": n.label, "file": n.file_path, "line": n.line_start, "id": n.id,
                });
                if n.label == codegraph_core::NodeLabel::Document {
                    // one-line preview instead of the whole chunk
                    if let Some(t) = n.metadata.get("text").and_then(|v| v.as_str()) {
                        let preview: String = t.lines().find(|l| !l.trim().is_empty()).unwrap_or("").chars().take(120).collect();
                        row["preview"] = serde_json::json!(preview);
                    }
                }
                row
            })
            .collect();
        let mut out = serde_json::json!({ "hits": hits });
        if !concise() {
            out["_hints"] = serde_json::json!(["get_node(id, snippet=true) for source/full text", "callers(name) to trace usage", "context(query) to assemble task context"]);
        }
        if !fields.is_empty() {
            let rows: Vec<serde_json::Value> = fields
                .iter()
                .map(|(f, ty, file)| serde_json::json!({"field": f, "type": ty, "file": file}))
                .collect();
            out["field_declarations"] = serde_json::json!(rows);
        }
        Ok(CallToolResult::success(vec![Content::json(out)?]))
    }

    #[tool(description = "Get full details of one symbol by its fully-qualified id (from a prior search/callers result): kind, file:line, language, metadata. Pass snippet=true to ALSO get its exact source code — cheaper than reading the whole file.")]
    async fn get_node(&self, args: Parameters<IdArgs>) -> Result<CallToolResult, McpError> {
        let store = self.open()?;
        let node = store
            .get_node(&args.0.id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        if args.0.snippet.unwrap_or(false) {
            if let Some(n) = &node {
                let mut out = serde_json::to_value(n)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                // exact span from disk — never a whole-file dump
                if let Ok(text) = std::fs::read_to_string(self.root.join(&n.file_path)) {
                    let s = (n.line_start.max(1) as usize) - 1;
                    let e = (n.line_end as usize).min(s + 400).max(s + 1);
                    let lines: Vec<&str> = text.lines().collect();
                    if s < lines.len() {
                        out["snippet"] =
                            serde_json::json!(lines[s..e.min(lines.len())].join("\n"));
                    }
                }
                return Ok(CallToolResult::success(vec![Content::json(out)?]));
            }
        }
        Ok(CallToolResult::success(vec![Content::json(node)?]))
    }

    #[tool(description = "Who calls X — ALWAYS use this (not search/grep) for usage/caller questions. Resolved call edges; ambiguous names return pinnable per-definition CANDIDATES (re-call with id=<id>). Includes `coverage`: if may_be_incomplete, the list is a precise LOWER BOUND — corroborate with text search before concluding nothing else calls it.")]
    async fn callers(&self, args: Parameters<NameArgs>) -> Result<CallToolResult, McpError> {
        let store = self.open()?;
        let err = |e: codegraph_store::StoreError| McpError::internal_error(e.to_string(), None);
        if let Some(pin) = &args.0.id {
            let callers: Vec<serde_json::Value> =
                store.callers_of_id(pin).map_err(err)?.iter().map(lean).collect();
            return Ok(CallToolResult::success(vec![Content::json(
                serde_json::json!({"pinned": pin, "callers": callers}),
            )?]));
        }
        let mut defs = store.definitions_of(&args.0.name).map_err(err)?;
        if defs.len() > 1 {
            // Ambiguous: return pinnable candidates instead of silently merging
            // callers of different same-name definitions (what rivals do).
            // Rank: strongest resolved evidence first; cross-language ties
            // broken toward the language family the textual call-site evidence
            // lives in (ranking only — never changes which edges exist).
            let ev_files = store.unresolved_call_site_files(&args.0.name, None).unwrap_or_default();
            let fam_votes = |file: &str| {
                ev_files.iter().filter(|f| lang_family(f) == lang_family(file)).count()
            };
            defs.sort_by(|(a, na), (b, nb)| {
                nb.cmp(na)
                    .then_with(|| fam_votes(&b.file_path).cmp(&fam_votes(&a.file_path)))
                    .then_with(|| a.file_path.cmp(&b.file_path))
            });
            let candidates: Vec<serde_json::Value> = defs
                .iter()
                .map(|(d, nc)| serde_json::json!({
                    "id": d.id, "file": d.file_path, "line": d.line_start, "resolved_callers": nc,
                }))
                .collect();
            let coverage = store.coverage_for_callers(&args.0.name).map_err(err)?;
            return Ok(CallToolResult::success(vec![Content::json(serde_json::json!({
                "ambiguous": true,
                "note": format!("'{}' has {} definitions; callers differ per definition. Re-call with id=<id> to pin one.", args.0.name, defs.len()),
                "candidates": candidates,
                "coverage": coverage,
            }))?]));
        }
        let callers = store.callers_of(&args.0.name).map_err(err)?;
        let coverage = store.coverage_for_callers(&args.0.name).map_err(err)?;
        // Compact rows (name/file/line) — the full Node JSON doubled the token
        // cost of every answer, and the ~80-byte qualified id per resolved row
        // added 40% more (measured 17 KB for a 62-caller method). Ids remain
        // where they're actionable: ambiguous CANDIDATES (pinning) and search
        // hits (get_node drill-down).
        let caller_files: std::collections::HashSet<&str> =
            callers.iter().map(|n| n.file_path.as_str()).collect();
        let rows: Vec<serde_json::Value> = callers
            .iter()
            .map(|n| serde_json::json!({"name": n.name, "file": n.file_path, "line": n.line_start}))
            .collect();
        // TEXTUAL layer: files whose parser-verified CALL SITES name it but did
        // not resolve into an edge — the recall the resolved list can't give,
        // clearly separated so it never masquerades as a resolved edge.
        let mut referencing_files: Vec<String> = store
            .unresolved_call_site_files(&args.0.name, None)
            .map_err(err)?
            .into_iter()
            .filter(|f| !caller_files.contains(f.as_str()))
            .collect();
        referencing_files.sort();
        let mut out = serde_json::json!({
            "callers": rows,
            "coverage": coverage,
        });
        if !concise() {
            out["_hints"] = serde_json::json!(["blast_radius(name) before changing it", "co_changes(file) for what usually changes too"]);
        }
        if !referencing_files.is_empty() {
            out["unresolved_call_site_files"] = serde_json::json!(referencing_files);
            if !concise() {
                out["_note"] = serde_json::json!(
                    "unresolved_call_site_files = parser-verified call tokens naming it that did NOT resolve to an edge (textual evidence, not resolved callers)"
                );
            }
        }
        // classes/interfaces are USED via injection/type annotations, not call
        // sites — surface that evidence so "no callers" never reads as "unused"
        let usages = store.type_usages(&args.0.name).map_err(err)?;
        if !usages.is_empty() {
            let rows: Vec<serde_json::Value> = usages
                .iter()
                .take(50)
                .map(|(f, ev)| serde_json::json!({"file": f, "evidence": ev}))
                .collect();
            out["type_usages"] = serde_json::json!(rows);
            if !concise() {
                out["_type_note"] = serde_json::json!(
                    "type_usages = files USING this name as a TYPE (DI fields, typed locals, imports, subtypes) — the caller equivalent for classes/interfaces"
                );
            }
        }
        if let Some(fb) = fallback_hint(&coverage, &args.0.name) {
            out["_fallback"] = fb;
        }
        Ok(CallToolResult::success(vec![Content::json(out)?]))
    }

    /// Build (or reuse) the call graph. Cached by the index generation (+ mtime):
    /// a burst of graph queries in one agent turn builds the snapshot once; a
    /// reindex bumps the generation and invalidates it. Cheap to clone (Arc).
    fn load_graph(&self) -> Result<std::sync::Arc<GraphSnapshot>, McpError> {
        self.maybe_refresh();
        let key: SnapKey = (
            codegraph_store::generation(&self.db_path),
            std::fs::metadata(&self.db_path).and_then(|m| m.modified()).ok(),
        );
        if let Ok(cache) = self.graph_cache.lock() {
            if let Some((cached_key, snap)) = cache.as_ref() {
                if *cached_key == key {
                    return Ok(snap.clone());
                }
            }
        }
        let err = |e: codegraph_store::StoreError| McpError::internal_error(e.to_string(), None);
        let store = self.open()?;
        // Light loaders: no Document chunk text, no per-edge JSON parse.
        let nodes = store.graph_nodes().map_err(err)?;
        let edges = store.graph_edges().map_err(err)?;
        let lg = codegraph_graph::LoadedGraph::load(&nodes, &edges);
        let mut by_id = std::collections::HashMap::with_capacity(nodes.len());
        let mut by_name = std::collections::HashMap::with_capacity(nodes.len());
        for (i, n) in nodes.iter().enumerate() {
            by_id.insert(n.id.clone(), i);
            by_name.entry(n.name.clone()).or_insert(i);
        }
        let snap = std::sync::Arc::new(GraphSnapshot { lg, nodes, by_id, by_name });
        if let Ok(mut cache) = self.graph_cache.lock() {
            *cache = Some((key, snap.clone()));
        }
        Ok(snap)
    }

    #[tool(description = "Find code AND documentation by MEANING (vector search over all symbols + docs/wiki Document nodes). USE THIS for: conceptual questions ('code that retries with backoff'), docs/wiki lookups ('what does the wiki say about X' — do NOT grep/Read doc files first), and any query where you don't know the identifier. Bundled local embedder, no server. If empty, fall back to search.")]
    async fn semantic_search(&self, args: Parameters<SearchArgs>) -> Result<CallToolResult, McpError> {
        let snap = self.load_graph()?; // refreshes; nodes for hit hydration
        let store = self.open()?; // pooled connection, moved into the blocking task
        let q = args.0.query.clone();
        let limit = args.0.limit.unwrap_or(15);
        // NO EMBEDDER must never be a dead end (field-measured: the tool was
        // advertised, then hard-failed at runtime): degrade to lexical search
        // and SAY SO — the agent can still act, and knows why.
        // spawn_blocking: the probe uses blocking reqwest — calling it on the
        // async runtime panics ("runtime within a runtime") and wedges the
        // server (caught by the freshness regression suite).
        if !embedder_available_async().await? {
            let hits: Vec<serde_json::Value> =
                store.search_smart(&q, limit).unwrap_or_default().iter().map(lean).collect();
            return Ok(CallToolResult::success(vec![Content::json(serde_json::json!({
                "degraded": "lexical fallback — no embedder (rebuild with --features local-embed, or load an embedding model in LM Studio / Ollama)",
                "hits": hits,
            }))?]));
        }
        let results = tokio::task::spawn_blocking(move || semantic_blocking(&store, &snap, &q, limit))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::json(results)?]))
    }

    #[tool(description = "Shortest dependency/call path between two symbols by name: how A reaches B through the call graph.")]
    async fn trace_path(&self, args: Parameters<TwoNamesArgs>) -> Result<CallToolResult, McpError> {
        let g = self.load_graph()?;
        let find = |name: &str| g.node_by_name(name).map(|n| n.id.clone());
        let path = match (find(&args.0.from), find(&args.0.to)) {
            (Some(a), Some(b)) => g.lg.shortest_path(&a, &b).unwrap_or_default(),
            _ => Vec::new(),
        };
        Ok(CallToolResult::success(vec![Content::json(path)?]))
    }

    #[tool(description = "Impact / blast-radius: every symbol that (transitively) depends on the given one. Use BEFORE changing or renaming a symbol to see what could break. Includes a `coverage` object — if `may_be_incomplete` is true the radius may miss callers whose calls were dropped; corroborate with text search.")]
    async fn blast_radius(&self, args: Parameters<NameArgs>) -> Result<CallToolResult, McpError> {
        let g = self.load_graph()?;
        let store = self.open()?;
        let (affected, coverage) = match g.node_by_name(&args.0.name) {
            Some(n) => (
                g.lg.blast_radius(&n.id, 5),
                store
                    .coverage_for_callers(&n.name)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?,
            ),
            None => (Vec::new(), codegraph_core::Coverage::callers(&args.0.name, 0, 0)),
        };
        // compact rows, capped — a hub can reach hundreds of symbols and the
        // agent needs name/file:line, not full node JSON for each
        let total = affected.len();
        let rows: Vec<serde_json::Value> = affected
            .iter()
            .take(200)
            .map(|id| match g.node_by_id(id) {
                Some(n) => serde_json::json!({"name": n.name, "file": n.file_path, "line": n.line_start}),
                None => serde_json::json!({"id": id}),
            })
            .collect();
        let mut out = serde_json::json!({
            "affected": rows,
            "total_affected": total,
            "coverage": coverage,
        });
        if total > 200 {
            out["_note"] = serde_json::json!("truncated to 200 rows — total_affected is the real count");
        }
        if let Some(fb) = fallback_hint(&coverage, &args.0.name) {
            out["_fallback"] = fb;
        }
        Ok(CallToolResult::success(vec![Content::json(out)?]))
    }

    #[tool(description = "List the functions a given function CALLS (outgoing call edges). PREFER over reading the body to enumerate its calls. Layered: resolved callees + `unresolved_calls` (in-repo-plausible call names the resolver dropped). `coverage.dropped` counts what's absent.")]
    async fn callees(&self, args: Parameters<NameArgs>) -> Result<CallToolResult, McpError> {
        let g = self.load_graph()?;
        let store = self.open()?;
        let err = |e: codegraph_store::StoreError| McpError::internal_error(e.to_string(), None);
        let (rows, coverage, unresolved) = match g.node_by_name(&args.0.name) {
            Some(n) => {
                let rows: Vec<serde_json::Value> = g
                    .lg
                    .callees(&n.id)
                    .iter()
                    .filter_map(|id| g.node_by_id(id))
                    .map(|c| serde_json::json!({"name": c.name, "file": c.file_path, "line": c.line_start}))
                    .collect();
                let mut unresolved = store.unresolved_callee_names(&n.id).map_err(err)?;
                unresolved.truncate(30);
                (rows, store.coverage_for_callees(&n.id).map_err(err)?, unresolved)
            }
            None => (Vec::new(), codegraph_core::Coverage::callees(0, 0), Vec::new()),
        };
        let mut body = serde_json::json!({
            "callees": rows,
            "coverage": coverage,
        });
        if !unresolved.is_empty() {
            body["unresolved_calls"] = serde_json::json!(unresolved);
            body["_note"] = serde_json::json!(
                "unresolved_calls = call names in the body the resolver DROPPED (in-repo candidates exist) — textual evidence, not resolved edges"
            );
        }
        if let Some(fb) = fallback_hint(&coverage, &args.0.name) {
            body["_fallback"] = fb;
        }
        Ok(CallToolResult::success(vec![Content::json(body)?]))
    }

    #[tool(description = "START HERE when beginning a task/bug/feature: assembles the most relevant symbols (personalized PageRank over resolved call edges, token-budgeted) — the structural neighborhood a plain search misses. Cheaper and more complete than reading files to orient yourself.")]
    async fn context(&self, args: Parameters<ContextArgs>) -> Result<CallToolResult, McpError> {
        let store = self.open()?;
        let budget = args.0.budget.unwrap_or(1000);
        let fts = args
            .0
            .query
            .split_whitespace()
            .map(|w| w.chars().filter(|c| c.is_alphanumeric()).collect::<String>())
            .filter(|w| w.len() > 1)
            .map(|w| format!("{w}*"))
            .collect::<Vec<_>>()
            .join(" OR ");
        let fts = if fts.is_empty() { args.0.query.clone() } else { fts };
        let seeds: Vec<String> =
            store.search_fts(&fts, 12).unwrap_or_default().into_iter().map(|n| n.id).collect();
        let g = self.load_graph()?;
        let ranked = g.lg.personalized_pagerank_top(&seeds, 200);
        let mut used = 0usize;
        let mut out = Vec::new();
        // Signature line per symbol (cached per file) — one tool call gives the
        // agent orientation without a follow-up Read per hit.
        let mut file_cache: std::collections::HashMap<String, Option<Vec<String>>> =
            std::collections::HashMap::new();
        for (id, score) in ranked {
            let Some(n) = g.node_by_id(&id) else { continue };
            if n.label == codegraph_core::NodeLabel::File {
                continue;
            }
            let snippet = file_cache
                .entry(n.file_path.clone())
                .or_insert_with(|| {
                    std::fs::read_to_string(self.root.join(&n.file_path))
                        .ok()
                        .map(|s| s.lines().map(str::to_string).collect())
                })
                .as_ref()
                .and_then(|lines| lines.get(n.line_start.saturating_sub(1) as usize))
                .map(|l| l.trim().chars().take(120).collect::<String>())
                .unwrap_or_default();
            let cost = (n.name.len() + n.file_path.len() + snippet.len()) / 4 + 4;
            if used + cost > budget {
                break;
            }
            used += cost;
            out.push(serde_json::json!({
                "name": n.name, "label": format!("{:?}", n.label),
                "file": n.file_path, "line": n.line_start, "score": score,
                "snippet": snippet,
            }));
        }
        Ok(CallToolResult::success(vec![Content::json(serde_json::json!({
            "query": args.0.query, "context": out, "tokens": used,
        }))?]))
    }

    #[tool(description = "The most central/important symbols by PageRank (real code symbols only, utility-sink damped): a fast way to map the core of an unfamiliar codebase.")]
    async fn important(&self, args: Parameters<LimitArgs>) -> Result<CallToolResult, McpError> {
        let g = self.load_graph()?;
        let top: Vec<serde_json::Value> = g
            .lg
            .important(args.0.limit.unwrap_or(15), &g.nodes)
            .into_iter()
            .map(|(id, score)| match g.node_by_id(&id) {
                Some(n) => serde_json::json!({
                    "label": codegraph_core::display_label(n), "id": id, "kind": n.label, "score": score,
                }),
                None => serde_json::json!({ "id": id, "score": score }),
            })
            .collect();
        Ok(CallToolResult::success(vec![Content::json(top)?]))
    }

    #[tool(description = "ARCHITECTURE MAP in one call: node/edge counts, languages, resolution quality, measured precision, top communities (dominant directory + key symbols by hub score), and route count. Use to orient in an unfamiliar repo before drilling down with important/flows/callers.")]
    async fn architecture(&self) -> Result<CallToolResult, McpError> {
        let g = self.load_graph()?;
        let store = self.open()?;
        let err = |e: codegraph_store::StoreError| McpError::internal_error(e.to_string(), None);
        let mut by_label: std::collections::BTreeMap<String, usize> = Default::default();
        let mut by_lang: std::collections::BTreeMap<String, usize> = Default::default();
        let mut comms: std::collections::HashMap<u32, Vec<&codegraph_core::Node>> = Default::default();
        for n in &g.nodes {
            *by_label.entry(format!("{:?}", n.label)).or_default() += 1;
            if !matches!(n.label, codegraph_core::NodeLabel::File | codegraph_core::NodeLabel::Document) {
                *by_lang.entry(n.language.clone()).or_default() += 1;
                if let Some(c) = n.community {
                    comms.entry(c).or_default().push(n);
                }
            }
        }
        let hub = |n: &codegraph_core::Node| {
            let fi = n.metadata.get("fan_in").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let fo = n.metadata.get("fan_out").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            codegraph_graph::hub_score(n.pagerank, fi, fo)
        };
        let mut comms: Vec<(u32, Vec<&codegraph_core::Node>)> = comms.into_iter().collect();
        comms.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
        let communities: Vec<serde_json::Value> = comms
            .iter()
            .take(8)
            .map(|(_, members)| {
                // dominant 2-level dir prefix = the community's human name
                let mut prefixes: std::collections::BTreeMap<String, usize> = Default::default();
                for n in members {
                    let mut it = n.file_path.split('/');
                    if let (Some(a), Some(b)) = (it.next(), it.next()) {
                        *prefixes.entry(format!("{a}/{b}")).or_default() += 1;
                    }
                }
                let dir = prefixes.iter().max_by_key(|(_, c)| **c).map(|(p, _)| p.clone()).unwrap_or_default();
                // key symbols must be CONNECTED ones — a cluster of zero-degree
                // DTOs would otherwise pick alphabetical noise via the tiebreak
                let mut top: Vec<&&codegraph_core::Node> = members
                    .iter()
                    .filter(|n| {
                        n.metadata.get("fan_in").and_then(|v| v.as_u64()).unwrap_or(0)
                            + n.metadata.get("fan_out").and_then(|v| v.as_u64()).unwrap_or(0)
                            > 0
                    })
                    .collect();
                if top.is_empty() {
                    top = members.iter().collect();
                }
                top.sort_by(|a, b| hub(b).partial_cmp(&hub(a)).unwrap_or(std::cmp::Ordering::Equal).then(a.id.cmp(&b.id)));
                let key: Vec<&str> = top.iter().take(3).map(|n| n.name.as_str()).collect();
                serde_json::json!({"dir": dir, "symbols": members.len(), "key_symbols": key})
            })
            .collect();
        let mut out = serde_json::json!({
            "nodes_by_kind": by_label,
            "languages": by_lang,
            "communities": communities,
            "routes": g.nodes.iter().filter(|n| n.label == codegraph_core::NodeLabel::Route).count(),
        });
        if let Ok(Some(raw)) = store.meta_get("audit_result") {
            if let Ok(audit) = serde_json::from_str::<serde_json::Value>(&raw) {
                out["measured_precision"] = audit;
            }
        }
        let resolvable = store.external_bound_call_sites().map_err(err)?;
        out["_note"] = serde_json::json!(format!(
            "next: important (core symbols) · flows (entry-point chains) · routes (API surface); {} call sites are unresolvable in-repo and excluded from recall accounting",
            resolvable
        ));
        Ok(CallToolResult::success(vec![Content::json(out)?]))
    }

    #[tool(description = "Graph size + trust card: node count and, when `codegraph audit` has run, the MEASURED per-tier precision of this repo's resolved edges vs a compiler oracle.")]
    async fn stats(&self) -> Result<CallToolResult, McpError> {
        // open_any: stats is the DIAGNOSTIC tool — it must stay reachable on an
        // empty graph precisely so the emptiness itself can be reported loudly.
        let store = self.open_any()?;
        let err = |e: codegraph_store::StoreError| McpError::internal_error(e.to_string(), None);
        let n = store.node_count().map_err(err)?;
        // embedder availability up front, so agents route around a degraded
        // semantic_search instead of discovering it mid-task
        let mut out = serde_json::json!({
            "nodes": n,
            "embedder_available": embedder_available_async().await?,
        });
        if n == 0 {
            out["EMPTY_GRAPH"] = serde_json::json!(format!(
                "0 nodes for root {} — likely a stale/wrong server root (moved repo? stale --path?). All other tools will refuse to answer until this is fixed.",
                self.root.display()
            ));
        }
        if let Ok(Some(raw)) = store.meta_get("audit_result") {
            if let Ok(audit) = serde_json::from_str::<serde_json::Value>(&raw) {
                let current = codegraph_store::generation(&self.db_path);
                let audited = audit.get("generation").and_then(|g| g.as_u64()).unwrap_or(0);
                if audited < current {
                    // STALE audit: never serve old precision numbers as if
                    // current — the stale payload moves under an explicit key.
                    out["stale_audit_not_current"] = audit;
                    out["measured_precision_note"] = serde_json::json!(
                        "audit predates the current index generation — numbers are for an OLDER graph; re-run `codegraph audit` to refresh"
                    );
                } else {
                    out["measured_precision"] = audit;
                }
            }
        }
        Ok(CallToolResult::success(vec![Content::json(out)?]))
    }

    #[tool(description = "Dead-code CANDIDATES: functions/methods that no call site in the repo even names (entry points, route handlers, and test files excluded). Static view — dynamic dispatch/exports/reflection are invisible, so treat as candidates to verify, not verdicts.")]
    async fn dead_code(&self, args: Parameters<LimitArgs>) -> Result<CallToolResult, McpError> {
        let store = self.open()?;
        let dead: Vec<serde_json::Value> = store
            .dead_code_candidates(args.0.limit.unwrap_or(50))
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .iter()
            .map(lean)
            .collect();
        Ok(CallToolResult::success(vec![Content::json(dead)?]))
    }

    #[tool(description = "Files that historically CHANGE TOGETHER with the given file (mined from git history). Use before a change to see what usually needs touching alongside it.")]
    async fn co_changes(&self, args: Parameters<FileArgs>) -> Result<CallToolResult, McpError> {
        let store = self.open()?;
        let pairs = store
            .cochanges_for(&args.0.file, args.0.limit.unwrap_or(10))
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let out: Vec<serde_json::Value> =
            pairs.into_iter().map(|(f, n)| serde_json::json!({"file": f, "co_changed": n})).collect();
        Ok(CallToolResult::success(vec![Content::json(out)?]))
    }

    #[tool(description = "Change-aware review: map the git diff (vs a base ref, default HEAD = uncommitted) to affected symbols with fan-in, test-gap flags, a risk tier, and co-change hints (files that usually change with this diff but aren't in it). Use to review a change's blast radius before committing/merging.")]
    async fn changes(&self, args: Parameters<ChangesArgs>) -> Result<CallToolResult, McpError> {
        let store = self.open()?;
        let base = args.0.base.unwrap_or_else(|| "HEAD".to_string());
        let out = std::process::Command::new("git")
            .args(["-C", &self.root.to_string_lossy(), "diff", "--name-only", &base])
            .output()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let changed: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .filter(|f| !f.is_empty())
            .collect();
        let mut symbols = Vec::new();
        let mut hints: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
        for f in &changed {
            for sym in store.symbols_in_file(f).map_err(|e| McpError::internal_error(e.to_string(), None))? {
                let fan_in = store.call_site_count(&sym.name).unwrap_or(0);
                let tested = store.has_test_reference(&sym.name).unwrap_or(false);
                let risk = match (fan_in, tested) {
                    (f, false) if f >= 10 => "HIGH",
                    (f, _) if f >= 10 => "MED",
                    (f, false) if f >= 3 => "MED",
                    _ => "low",
                };
                symbols.push(serde_json::json!({
                    "name": sym.name, "file": sym.file_path, "line": sym.line_start,
                    "fan_in": fan_in, "tested": tested, "risk": risk,
                }));
            }
            for (other, n) in store.cochanges_for(f, 5).unwrap_or_default() {
                if n >= 3 && !changed.contains(&other) {
                    let e = hints.entry(other).or_insert(0);
                    *e = (*e).max(n);
                }
            }
        }
        symbols.sort_by_key(|s| {
            std::cmp::Reverse(
                s["fan_in"].as_u64().unwrap_or(0) * if s["tested"].as_bool().unwrap_or(false) { 1 } else { 3 },
            )
        });
        symbols.truncate(40);
        let co_change_hints: Vec<serde_json::Value> =
            hints.into_iter().map(|(f, n)| serde_json::json!({"file": f, "co_changed": n})).collect();
        Ok(CallToolResult::success(vec![Content::json(serde_json::json!({
            "base": base, "changed_files": changed, "affected_symbols": symbols,
            "co_change_hints": co_change_hints,
        }))?]))
    }

    #[tool(description = "Graph query in Cypher-lite (read-only openCypher subset): 1-2 hop patterns like MATCH (a:Method)-[:Calls]->(b) WHERE b.name = 'save' RETURN a.name, a.file LIMIT 10. Relations: Calls, Tests, Inherits, Implements, HttpCalls, Defines. Props: name/file/line/label/language/id/pagerank. ALSO the tool for EXHAUSTIVE listings (all files/symbols matching a filter): MATCH (n) WHERE n.file CONTAINS 'x' RETURN n.file LIMIT 1000 — a complete filter, unlike search's ranked top-N. Unsupported syntax errors clearly — never a wrong answer.")]
    async fn graph_query(&self, args: Parameters<CypherArgs>) -> Result<CallToolResult, McpError> {
        self.maybe_refresh();
        let sql = codegraph_store::cypher::to_sql(&args.0.query)
            .map_err(|e| McpError::invalid_params(format!("cypher-lite: {e}"), None))?;
        let (cols, rows) = codegraph_store::query_readonly(&self.db_path, &sql, 500)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::json(serde_json::json!({
            "columns": cols, "rows": rows,
        }))?]))
    }

    #[tool(description = "Execution FLOWS: call chains from entry points (route handlers, main, zero-fan-in tasks) ranked by criticality (reach × centrality). Use to map what a service actually DOES, find the most critical paths, or see which flows a change touches.")]
    async fn flows(&self, args: Parameters<LimitArgs>) -> Result<CallToolResult, McpError> {
        let g = self.load_graph()?;
        let entries = codegraph_graph::detect_entry_points(&g.nodes);
        let mut flows: Vec<serde_json::Value> = entries
            .iter()
            .filter_map(|(n, kind)| {
                let body = g.lg.flow_from(&n.id, 6);
                if body.is_empty() {
                    return None;
                }
                let crit: f64 = body
                    .iter()
                    .filter_map(|id| g.node_by_id(id))
                    .map(|x| x.pagerank)
                    .sum::<f64>()
                    * (1.0 + body.len() as f64).ln();
                Some(serde_json::json!({
                    "entry": n.name, "label": codegraph_core::display_label(n), "id": n.id,
                    "kind": kind, "file": n.file_path, "line": n.line_start,
                    "reach": body.len(), "criticality": crit,
                }))
            })
            .collect();
        flows.sort_by(|a, b| {
            b["criticality"].as_f64().partial_cmp(&a["criticality"].as_f64()).unwrap_or(std::cmp::Ordering::Equal)
        });
        flows.truncate(args.0.limit.unwrap_or(10));
        Ok(CallToolResult::success(vec![Content::json(flows)?]))
    }

    #[tool(description = "List the types that IMPLEMENT or EXTEND a given interface/class/protocol (by name). Use to find every concrete implementation of an abstraction before changing it.")]
    async fn implementers(&self, args: Parameters<NameArgs>) -> Result<CallToolResult, McpError> {
        let store = self.open()?;
        let impls: Vec<serde_json::Value> = store
            .implementers_of(&args.0.name)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?
            .iter()
            .map(lean)
            .collect();
        Ok(CallToolResult::success(vec![Content::json(impls)?]))
    }

    #[tool(description = "List the HTTP routes/endpoints detected in the repo (NestJS/Express/Flask/Spring/etc.), each with method + path + handler. Filter with path_prefix/method, paginate with limit/offset. Use to map a backend's API surface.")]
    async fn routes(&self, args: Parameters<RoutesArgs>) -> Result<CallToolResult, McpError> {
        let store = self.open()?;
        let mut routes = store
            .nodes_by_label("Route")
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        routes.sort_by(|a, b| a.name.cmp(&b.name));
        let meta = |n: &codegraph_core::Node, k: &str| {
            n.metadata.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string()
        };
        let want_method = args.0.method.as_deref().map(str::to_ascii_uppercase);
        let filtered: Vec<&codegraph_core::Node> = routes
            .iter()
            .filter(|n| {
                args.0.path_prefix.as_deref().is_none_or(|p| meta(n, "path").starts_with(p))
                    && want_method.as_deref().is_none_or(|m| meta(n, "method") == m)
            })
            .collect();
        let total = filtered.len();
        let offset = args.0.offset.unwrap_or(0);
        let limit = args.0.limit.unwrap_or(100);
        let rows: Vec<serde_json::Value> = filtered
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|n| {
                serde_json::json!({
                    "method": meta(n, "method"), "path": meta(n, "path"),
                    "handler": meta(n, "handler"), "file": n.file_path, "line": n.line_start,
                })
            })
            .collect();
        let mut out = serde_json::json!({ "total": total, "routes": rows });
        if offset + limit < total {
            out["_note"] = serde_json::json!(format!(
                "showing {}..{} of {total} — re-call with offset={} (or filter with path_prefix/method)",
                offset,
                offset + limit,
                offset + limit
            ));
        }
        Ok(CallToolResult::success(vec![Content::json(out)?]))
    }
}

/// `codegraph_llm::embedder_available` probes local LLM servers with BLOCKING
/// reqwest — it must never run on the async runtime (panic → wedged server).
/// Result is OnceLock-cached inside the llm crate, so this is one hop once.
async fn embedder_available_async() -> Result<bool, McpError> {
    tokio::task::spawn_blocking(codegraph_llm::embedder_available)
        .await
        .map_err(|e| McpError::internal_error(e.to_string(), None))
}

fn semantic_blocking(
    store: &codegraph_store::Store,
    snap: &GraphSnapshot,
    q: &str,
    limit: usize,
) -> Vec<serde_json::Value> {
    let Some((qvs, _)) = codegraph_llm::embed_texts(&[q.to_string()]) else { return Vec::new() };
    let Some(qv) = qvs.into_iter().next() else { return Vec::new() };
    // Indexed KNN via sqlite-vec — no full vector scan, no blob reload per query.
    store
        .knn(&qv, limit)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(id, score)| {
            snap.node_by_id(&id).map(|n| {
                serde_json::json!({"name": n.name, "label": format!("{:?}", n.label), "file": n.file_path, "line": n.line_start, "score": score})
            })
        })
        .collect()
}

/// Coarse language family from a path's extension — a ranking tie-breaker for
/// cross-language name collisions (never affects which edges exist).
fn lang_family(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" => "js",
        "py" | "pyi" => "py",
        "rs" => "rs",
        "go" => "go",
        "swift" => "swift",
        "kt" | "kts" | "java" => "jvm",
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => "c",
        "rb" => "rb",
        "cs" => "cs",
        _ => "other",
    }
}

/// Token-diet dialect (CODEGRAPH_MCP_CONCISE=1): drop the per-response coaching
/// fields (_hints, explainer _notes) for agents that already know the tools.
/// Data and safety fields (coverage, _fallback, truncation notes) always stay.
fn concise() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("CODEGRAPH_MCP_CONCISE").as_deref() == Ok("1"))
}

/// Actionable "known unknowns" hint: when a precise answer may be incomplete,
/// hand the agent a ready-made lexical pattern so it verifies instead of
/// concluding absence. Evidence-gated — only fires when coverage says so.
fn fallback_hint(coverage: &codegraph_core::Coverage, name: &str) -> Option<serde_json::Value> {
    if !coverage.may_be_incomplete {
        return None;
    }
    Some(serde_json::json!({
        "why": format!(
            "{} in-repo call site(s) naming '{name}' did not resolve — the precise list is a LOWER BOUND",
            coverage.dropped
        ),
        "run": format!("grep -rn \"{name}\\s*(\" --include=\"*.ts\" --include=\"*.tsx\" --include=\"*.js\" --include=\"*.py\" --include=\"*.swift\" --include=\"*.kt\" --include=\"*.java\" --include=\"*.go\" --include=\"*.rb\" --include=\"*.cs\" --include=\"*.rs\""),
    }))
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for CodeGraphServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        info.instructions = Some(
            "CodeGraph = a resolved code+docs knowledge graph for THIS repo (auto-fresh). ROUTE BY INTENT — do not send everything to `search`: \
know the identifier → search · conceptual/'how does X work'/docs+wiki → semantic_search · EXHAUSTIVE listing ('ALL files/symbols matching …') → graph_query with a high LIMIT (complete filter, not ranked/truncated — e.g. MATCH (n) WHERE n.file CONTAINS 'mealsense' RETURN n.file LIMIT 1000) ·who-calls → callers (ambiguous ⇒ pinnable candidates, re-call with id) · what-does-it-call → callees · what-breaks → blast_radius · path A→B → trace_path · starting a task → context · repo orientation → architecture · diff review/risk/test-gaps → changes · unused code → dead_code · co-edited files → co_changes · API surface → routes · interface impls → implementers · repo map → important. \
NAVIGATION PROTOCOL (evidence classes, not made-up confidence numbers): every edge names WHY it exists. Compiler-grade tiers (justification `Scip` / `IndexStore`) are extracted by a compiler — navigate them freely. Tree-sitter tiers (`SelfThisMember`, `FieldTypeMember`, `LocalVarType`, `ImportNarrowed`, `SameFileUnique`, `GlobalUnique`) are unique-or-drop: never guessed, but not exhaustive. `stats` returns this repo's MEASURED per-tier precision when `codegraph audit` has run — quote it instead of assuming. \
KNOWN UNKNOWNS: the graph is precise, NOT exhaustive. A missing edge is not evidence of absence. When `coverage.may_be_incomplete` is true or a result is empty, the response carries a `_fallback` lexical pattern — run it (grep/text search) before concluding nobody calls X. Never invent connections the graph did not return. Docs/wiki pages are indexed as Document nodes — query them here instead of reading files."
                .to_string(),
        );
        info
    }
}

/// Directories whose churn never affects the graph (build output, deps, VCS) —
/// filtered at the watcher so `cargo build`/`npm install` don't cause wakeups.
/// The indexer's own walker excludes them too, so a missed filter here is only
/// a wasted (cheap, stat-only) staleness probe — never a wrong graph.
const WATCH_SKIP_DIRS: &[&str] = &[
    ".git", "target", "node_modules", "build", "dist", "out", ".venv", "venv",
    "__pycache__", "DerivedData", "Pods", ".gradle", ".next", ".cache", "vendor",
    // Our own graph DB: when it falls back under the repo root (no HOME /
    // CODEGRAPH_CACHE_DIR), a reindex write must NOT retrigger the watcher.
    ".codegraph", ".codegraph-cache",
];

/// Keep the index WARM: watch the repo and heal on quiet (debounced), so by the
/// time the agent's next tool call arrives the graph is already fresh and the
/// per-call `maybe_refresh` is a no-op. Best-effort — if the watcher can't
/// start, queries still self-heal exactly as before. Returns the watcher (must
/// stay alive for the server's lifetime).
fn spawn_fs_watcher(
    root: PathBuf,
    refresh: fn(&Path) -> anyhow::Result<()>,
) -> Option<notify::RecommendedWatcher> {
    use notify::Watcher;
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
        let Ok(ev) = res else { return };
        if matches!(ev.kind, notify::EventKind::Access(_)) {
            return;
        }
        let relevant = ev.paths.iter().any(|p| {
            !p.components().any(|c| {
                c.as_os_str().to_str().is_some_and(|s| WATCH_SKIP_DIRS.contains(&s))
            })
        });
        if relevant {
            let _ = tx.send(());
        }
    })
    .ok()?;
    watcher.watch(&root, notify::RecursiveMode::Recursive).ok()?;
    std::thread::spawn(move || {
        while rx.recv().is_ok() {
            // Debounce: drain events until 400ms of quiet (editors and git
            // checkouts write in bursts), then heal once.
            while rx.recv_timeout(std::time::Duration::from_millis(400)).is_ok() {}
            if let Err(e) = refresh(&root) {
                eprintln!("codegraph: watcher reindex failed ({e}); queries will self-heal");
            }
        }
    });
    Some(watcher)
}

/// Run the MCP server over stdio until the client disconnects. `refresh` is the
/// freshness gate (the CLI passes `index::ensure_fresh`); pass `None` to disable.
/// When enabled it ALSO drives a filesystem watcher so the index heals in the
/// background between tool calls instead of on the first query after an edit.
pub async fn serve_stdio(
    root: PathBuf,
    db_path: PathBuf,
    refresh: Option<fn(&Path) -> anyhow::Result<()>>,
) -> anyhow::Result<()> {
    let _watcher = refresh.and_then(|f| spawn_fs_watcher(root.clone(), f));
    let service = CodeGraphServer::with_refresh(root, db_path, refresh).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_constructs() {
        let s = CodeGraphServer::new(PathBuf::from("/tmp/none.db"));
        assert!(s.get_info().capabilities.tools.is_some());
    }
}
