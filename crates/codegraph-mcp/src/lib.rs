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

/// The built call graph + its node list — expensive to construct, so cached.
type GraphSnapshot = (codegraph_graph::LoadedGraph, Vec<codegraph_core::Node>);
/// Mtime-keyed cache of the built graph, shared across cloned server handles.
type GraphCache = std::sync::Arc<std::sync::Mutex<Option<(std::time::SystemTime, std::sync::Arc<GraphSnapshot>)>>>;

#[derive(Clone)]
pub struct CodeGraphServer {
    db_path: PathBuf,
    root: PathBuf,
    /// Injected freshness gate (CLI passes `index::ensure_fresh`) so live MCP
    /// queries never serve a graph that disagrees with the working tree.
    refresh: Option<fn(&Path) -> anyhow::Result<()>>,
    /// Debounce so a burst of tool calls in one agent turn re-checks at most once/sec.
    last_fresh: std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    /// Built-graph cache keyed by the DB's mtime — so a burst of graph queries in
    /// one agent turn builds the petgraph ONCE, not per call. Invalidates on reindex.
    graph_cache: GraphCache,
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
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NameArgs {
    /// Function name.
    pub name: String,
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
pub struct LimitArgs {
    /// Max results (default 15).
    #[serde(default)]
    pub limit: Option<usize>,
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

    fn open(&self) -> Result<codegraph_store::Store, McpError> {
        self.maybe_refresh();
        codegraph_store::Store::open(&self.db_path)
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(description = "Find where a symbol (function/class/type) is defined or referenced, by name. PREFER over Grep/ripgrep for code navigation: indexed, returns exact file:line + the node kind, no matches inside comments/strings. Use before reading files.")]
    async fn search(&self, args: Parameters<SearchArgs>) -> Result<CallToolResult, McpError> {
        let store = self.open()?;
        let limit = args.0.limit.unwrap_or(20);
        let hits = if args.0.regex.unwrap_or(false) {
            store.search_regex(&args.0.query, limit)
        } else {
            store.search_smart(&args.0.query, limit)
        }
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::json(hits)?]))
    }

    #[tool(description = "Get full details of one symbol by its fully-qualified id (from a prior search/callers result): kind, file:line, language, metadata.")]
    async fn get_node(&self, args: Parameters<IdArgs>) -> Result<CallToolResult, McpError> {
        let store = self.open()?;
        let node = store
            .get_node(&args.0.id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::json(node)?]))
    }

    #[tool(description = "List the functions that CALL a given function (reverse call edges). PREFER over grepping the name: resolved and exact, no false hits in comments/strings. The result includes a `coverage` object — if `coverage.may_be_incomplete` is true, some calls to this name were dropped (ambiguous/external); fall back to text search to be sure.")]
    async fn callers(&self, args: Parameters<NameArgs>) -> Result<CallToolResult, McpError> {
        let store = self.open()?;
        let callers = store
            .callers_of(&args.0.name)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let coverage = store
            .coverage_for_callers(&args.0.name)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::json(serde_json::json!({
            "callers": callers,
            "coverage": coverage,
        }))?]))
    }

    /// Build (or reuse) the call graph. Cached by the DB mtime: a burst of graph
    /// queries in one agent turn rebuilds the petgraph once; a reindex bumps the
    /// mtime and invalidates it. Returns a shared snapshot (cheap to clone).
    fn load_graph(&self) -> Result<std::sync::Arc<GraphSnapshot>, McpError> {
        self.maybe_refresh();
        let mtime = std::fs::metadata(&self.db_path).and_then(|m| m.modified()).ok();
        if let (Some(mt), Ok(cache)) = (mtime, self.graph_cache.lock()) {
            if let Some((cached_mt, snap)) = cache.as_ref() {
                if *cached_mt == mt {
                    return Ok(snap.clone());
                }
            }
        }
        let store = codegraph_store::Store::open(&self.db_path)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let nodes = store.all_nodes().map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let edges = store.all_edges().map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let snap = std::sync::Arc::new((codegraph_graph::LoadedGraph::load(&nodes, &edges), nodes));
        if let (Some(mt), Ok(mut cache)) = (mtime, self.graph_cache.lock()) {
            *cache = Some((mt, snap.clone()));
        }
        Ok(snap)
    }

    #[tool(description = "Find symbols by MEANING rather than exact name (vector search). Use when you do not know the symbol name. Uses a BUNDLED local embedder (bge-small, no server) when built with --features local-embed, else a local LLM endpoint; needs a prior `codegraph semantic-index`. Degrades gracefully if unavailable.")]
    async fn semantic_search(&self, args: Parameters<SearchArgs>) -> Result<CallToolResult, McpError> {
        self.maybe_refresh();
        let db = self.db_path.clone();
        let q = args.0.query.clone();
        let limit = args.0.limit.unwrap_or(15);
        let results = tokio::task::spawn_blocking(move || semantic_blocking(&db, &q, limit))
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::json(results)?]))
    }

    #[tool(description = "Shortest dependency/call path between two symbols by name: how A reaches B through the call graph.")]
    async fn trace_path(&self, args: Parameters<TwoNamesArgs>) -> Result<CallToolResult, McpError> {
        let g = self.load_graph()?;
        let (lg, nodes) = (&g.0, &g.1);
        let find = |name: &str| nodes.iter().find(|n| n.name == name).map(|n| n.id.clone());
        let path = match (find(&args.0.from), find(&args.0.to)) {
            (Some(a), Some(b)) => lg.shortest_path(&a, &b).unwrap_or_default(),
            _ => Vec::new(),
        };
        Ok(CallToolResult::success(vec![Content::json(path)?]))
    }

    #[tool(description = "Impact / blast-radius: every symbol that (transitively) depends on the given one. Use BEFORE changing or renaming a symbol to see what could break. Includes a `coverage` object — if `may_be_incomplete` is true the radius may miss callers whose calls were dropped; corroborate with text search.")]
    async fn blast_radius(&self, args: Parameters<NameArgs>) -> Result<CallToolResult, McpError> {
        let g = self.load_graph()?;
        let (lg, nodes) = (&g.0, &g.1);
        let store = self.open()?;
        let (affected, coverage) = match nodes.iter().find(|n| n.name == args.0.name) {
            Some(n) => (
                lg.blast_radius(&n.id, 5),
                store
                    .coverage_for_callers(&n.name)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?,
            ),
            None => (Vec::new(), codegraph_core::Coverage::callers(&args.0.name, 0, 0)),
        };
        Ok(CallToolResult::success(vec![Content::json(serde_json::json!({
            "affected": affected,
            "coverage": coverage,
        }))?]))
    }

    #[tool(description = "List the functions a given function CALLS (outgoing call edges). PREFER over reading the body to enumerate its calls. Includes a `coverage` object — `dropped` counts external/unresolved calls absent from the list.")]
    async fn callees(&self, args: Parameters<NameArgs>) -> Result<CallToolResult, McpError> {
        let g = self.load_graph()?;
        let (lg, nodes) = (&g.0, &g.1);
        let store = self.open()?;
        let (out, coverage) = match nodes.iter().find(|n| n.name == args.0.name) {
            Some(n) => (
                lg.callees(&n.id),
                store
                    .coverage_for_callees(&n.id)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?,
            ),
            None => (Vec::new(), codegraph_core::Coverage::callees(0, 0)),
        };
        Ok(CallToolResult::success(vec![Content::json(serde_json::json!({
            "callees": out,
            "coverage": coverage,
        }))?]))
    }

    #[tool(description = "Assemble the most relevant symbols for a task/query within a token budget, ranked by personalized PageRank over the RESOLVED call graph. PREFER for 'what code is relevant to X' — it returns symbols structurally related to the query INCLUDING their call-graph dependencies, not just name/text matches. Each result has name/file/line/label/score.")]
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
        let (lg, nodes) = (&g.0, &g.1);
        let ranked = lg.personalized_pagerank_top(&seeds, 200);
        let mut used = 0usize;
        let mut out = Vec::new();
        for (id, score) in ranked {
            let Some(n) = nodes.iter().find(|n| n.id == id) else { continue };
            if n.label == codegraph_core::NodeLabel::File {
                continue;
            }
            let cost = (n.name.len() + n.file_path.len()) / 4 + 4;
            if used + cost > budget {
                break;
            }
            used += cost;
            out.push(serde_json::json!({
                "name": n.name, "label": format!("{:?}", n.label),
                "file": n.file_path, "line": n.line_start, "score": score,
            }));
        }
        Ok(CallToolResult::success(vec![Content::json(serde_json::json!({
            "query": args.0.query, "context": out, "tokens": used,
        }))?]))
    }

    #[tool(description = "The most central/important symbols by PageRank: a fast way to map the core of an unfamiliar codebase.")]
    async fn important(&self, args: Parameters<LimitArgs>) -> Result<CallToolResult, McpError> {
        let g = self.load_graph()?;
        let lg = &g.0;
        let top = lg.pagerank_top(args.0.limit.unwrap_or(15));
        Ok(CallToolResult::success(vec![Content::json(top)?]))
    }

    #[tool(description = "Graph size (node/edge counts): a quick check that the repository is indexed and how big it is.")]
    async fn stats(&self) -> Result<CallToolResult, McpError> {
        let store = self.open()?;
        let n = store
            .node_count()
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::json(serde_json::json!({ "nodes": n }))?]))
    }

    #[tool(description = "Dead-code CANDIDATES: functions/methods that no call site in the repo even names (entry points, route handlers, and test files excluded). Static view — dynamic dispatch/exports/reflection are invisible, so treat as candidates to verify, not verdicts.")]
    async fn dead_code(&self, args: Parameters<LimitArgs>) -> Result<CallToolResult, McpError> {
        let store = self.open()?;
        let dead = store
            .dead_code_candidates(args.0.limit.unwrap_or(50))
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
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

    #[tool(description = "List the types that IMPLEMENT or EXTEND a given interface/class/protocol (by name). Use to find every concrete implementation of an abstraction before changing it.")]
    async fn implementers(&self, args: Parameters<NameArgs>) -> Result<CallToolResult, McpError> {
        let store = self.open()?;
        let impls = store
            .implementers_of(&args.0.name)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::json(impls)?]))
    }

    #[tool(description = "List the HTTP routes/endpoints detected in the repo (NestJS/Express/Flask/Spring/etc.), each with method + path + handler. Use to map a backend's API surface.")]
    async fn routes(&self) -> Result<CallToolResult, McpError> {
        let store = self.open()?;
        let mut routes = store
            .nodes_by_label("Route")
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        routes.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(CallToolResult::success(vec![Content::json(routes)?]))
    }
}

fn semantic_blocking(db: &std::path::Path, q: &str, limit: usize) -> Vec<serde_json::Value> {
    let Ok(store) = codegraph_store::Store::open(db) else { return Vec::new() };
    let Some((qvs, _)) = codegraph_llm::embed_texts(&[q.to_string()]) else { return Vec::new() };
    let Some(qv) = qvs.into_iter().next() else { return Vec::new() };
    let Ok(vectors) = store.all_vectors() else { return Vec::new() };
    let mut scored: Vec<(f32, String)> =
        vectors.iter().map(|(id, v)| (codegraph_core::dot(&qv, v), id.clone())).collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);
    scored
        .into_iter()
        .filter_map(|(score, id)| {
            store.get_node(&id).ok().flatten().map(|n| {
                serde_json::json!({"name": n.name, "label": format!("{:?}", n.label), "file": n.file_path, "line": n.line_start, "score": score})
            })
        })
        .collect()
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for CodeGraphServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        info.instructions = Some(
            "CodeGraph indexes this repository into a live code knowledge graph (auto-reindexed before each query). Its tools return exact file:line and resolved call edges, so they beat text search for code navigation: `search` to locate a symbol, `callers`/`callees` to trace call edges, `blast_radius` before a refactor, `trace_path` between two symbols, `important` to map an unfamiliar repo, `context` to assemble the symbols relevant to a task (personalized PageRank over the resolved graph, within a token budget — better than reading files), `implementers` for the concrete types behind an interface, `routes` for the HTTP API surface, `changes` to review a git diff (affected symbols + risk + test gaps + co-change hints), `dead_code` for unused-function candidates, `co_changes` for files that historically change together, `semantic_search` to find code by meaning, `get_node` for one symbol's details, `stats` for counts. \
             IMPORTANT — coverage: `callers`/`callees`/`blast_radius` resolve calls precisely but NOT exhaustively (ambiguous or external calls are dropped, never guessed). Each result carries a `coverage` object with `resolved`/`dropped`/`may_be_incomplete`. When `may_be_incomplete` is true, treat the list as a precise LOWER BOUND, not the complete set — fall back to text search (grep the name) before concluding 'nothing else calls this'."
                .to_string(),
        );
        info
    }
}

/// Run the MCP server over stdio until the client disconnects. `refresh` is the
/// freshness gate (the CLI passes `index::ensure_fresh`); pass `None` to disable.
pub async fn serve_stdio(
    root: PathBuf,
    db_path: PathBuf,
    refresh: Option<fn(&Path) -> anyhow::Result<()>>,
) -> anyhow::Result<()> {
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
