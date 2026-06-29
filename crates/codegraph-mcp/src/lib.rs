//! MCP server (M6): exposes the code graph to AI agents over stdio. Tools:
//! `search`, `get_node`, `callers`, `stats`. The whole CLI is the standalone
//! package; `codegraph mcp` runs this server as one subcommand.

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

#[derive(Clone)]
pub struct CodeGraphServer {
    db_path: PathBuf,
    root: PathBuf,
    /// Injected freshness gate (CLI passes `index::ensure_fresh`) so live MCP
    /// queries never serve a graph that disagrees with the working tree.
    refresh: Option<fn(&Path) -> anyhow::Result<()>>,
    /// Debounce so a burst of tool calls in one agent turn re-checks at most once/sec.
    last_fresh: std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    tool_router: ToolRouter<CodeGraphServer>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
    /// Symbol name or full-text query to search for.
    pub query: String,
    /// Maximum number of results (default 20).
    #[serde(default)]
    pub limit: Option<usize>,
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
        let hits = store
            .search_fts(&args.0.query, args.0.limit.unwrap_or(20))
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

    fn load_graph(&self) -> Result<(codegraph_graph::LoadedGraph, Vec<codegraph_core::Node>), McpError> {
        let store = self.open()?;
        let nodes = store.all_nodes().map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let edges = store.all_edges().map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok((codegraph_graph::LoadedGraph::load(&nodes, &edges), nodes))
    }

    #[tool(description = "Find symbols by MEANING rather than exact name (vector search). Use when you do not know the symbol name. Needs a local embedding model + a prior `codegraph semantic-index`; degrades gracefully if unavailable.")]
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
        let (lg, nodes) = self.load_graph()?;
        let find = |name: &str| nodes.iter().find(|n| n.name == name).map(|n| n.id.clone());
        let path = match (find(&args.0.from), find(&args.0.to)) {
            (Some(a), Some(b)) => lg.shortest_path(&a, &b).unwrap_or_default(),
            _ => Vec::new(),
        };
        Ok(CallToolResult::success(vec![Content::json(path)?]))
    }

    #[tool(description = "Impact / blast-radius: every symbol that (transitively) depends on the given one. Use BEFORE changing or renaming a symbol to see what could break. Includes a `coverage` object — if `may_be_incomplete` is true the radius may miss callers whose calls were dropped; corroborate with text search.")]
    async fn blast_radius(&self, args: Parameters<NameArgs>) -> Result<CallToolResult, McpError> {
        let (lg, nodes) = self.load_graph()?;
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
        let (lg, nodes) = self.load_graph()?;
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
        let (lg, nodes) = self.load_graph()?;
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
        let (lg, _) = self.load_graph()?;
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
}

fn semantic_blocking(db: &std::path::Path, q: &str, limit: usize) -> Vec<serde_json::Value> {
    let Ok(store) = codegraph_store::Store::open(db) else { return Vec::new() };
    let Some(backend) = codegraph_llm::OpenAiCompatBackend::detect().filter(|b| b.embed_model().is_some()) else {
        return Vec::new();
    };
    let Some(qv) = backend.embed(q).map(|v| codegraph_core::normalize(&v)) else { return Vec::new() };
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
            "CodeGraph indexes this repository into a live code knowledge graph (auto-reindexed before each query). Its tools return exact file:line and resolved call edges, so they beat text search for code navigation: `search` to locate a symbol, `callers`/`callees` to trace call edges, `blast_radius` before a refactor, `trace_path` between two symbols, `important` to map an unfamiliar repo, `context` to assemble the symbols relevant to a task (personalized PageRank over the resolved graph, within a token budget — better than reading files), `semantic_search` to find code by meaning, `get_node` for one symbol's details, `stats` for counts. \
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
