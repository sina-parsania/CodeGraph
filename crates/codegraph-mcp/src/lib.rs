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

    #[tool(description = "Full-text search the code graph for a symbol by name. Returns matching nodes with file:line.")]
    async fn search(&self, args: Parameters<SearchArgs>) -> Result<CallToolResult, McpError> {
        let store = self.open()?;
        let hits = store
            .search_fts(&args.0.query, args.0.limit.unwrap_or(20))
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::json(hits)?]))
    }

    #[tool(description = "Get a single node by its fully-qualified id.")]
    async fn get_node(&self, args: Parameters<IdArgs>) -> Result<CallToolResult, McpError> {
        let store = self.open()?;
        let node = store
            .get_node(&args.0.id)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::json(node)?]))
    }

    #[tool(description = "Find functions that call a given function name (reverse CALLS edges).")]
    async fn callers(&self, args: Parameters<NameArgs>) -> Result<CallToolResult, McpError> {
        let store = self.open()?;
        let callers = store
            .callers_of(&args.0.name)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::json(callers)?]))
    }

    fn load_graph(&self) -> Result<(codegraph_graph::LoadedGraph, Vec<codegraph_core::Node>), McpError> {
        let store = self.open()?;
        let nodes = store.all_nodes().map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let edges = store.all_edges().map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok((codegraph_graph::LoadedGraph::load(&nodes, &edges), nodes))
    }

    #[tool(description = "Semantic (vector) search over embedded symbols by meaning. Requires a local embedding model and a prior `codegraph semantic-index`.")]
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

    #[tool(description = "Shortest dependency path between two symbols (by name).")]
    async fn trace_path(&self, args: Parameters<TwoNamesArgs>) -> Result<CallToolResult, McpError> {
        let (lg, nodes) = self.load_graph()?;
        let find = |name: &str| nodes.iter().find(|n| n.name == name).map(|n| n.id.clone());
        let path = match (find(&args.0.from), find(&args.0.to)) {
            (Some(a), Some(b)) => lg.shortest_path(&a, &b).unwrap_or_default(),
            _ => Vec::new(),
        };
        Ok(CallToolResult::success(vec![Content::json(path)?]))
    }

    #[tool(description = "Impact / blast-radius: which symbols depend on the given symbol (reverse reachability).")]
    async fn blast_radius(&self, args: Parameters<NameArgs>) -> Result<CallToolResult, McpError> {
        let (lg, nodes) = self.load_graph()?;
        let affected = match nodes.iter().find(|n| n.name == args.0.name) {
            Some(n) => lg.blast_radius(&n.id, 5),
            None => Vec::new(),
        };
        Ok(CallToolResult::success(vec![Content::json(affected)?]))
    }

    #[tool(description = "Direct callees (outgoing CALLS) of a symbol.")]
    async fn callees(&self, args: Parameters<NameArgs>) -> Result<CallToolResult, McpError> {
        let (lg, nodes) = self.load_graph()?;
        let out = match nodes.iter().find(|n| n.name == args.0.name) {
            Some(n) => lg.callees(&n.id),
            None => Vec::new(),
        };
        Ok(CallToolResult::success(vec![Content::json(out)?]))
    }

    #[tool(description = "Most central symbols by PageRank (importance ranking).")]
    async fn important(&self, args: Parameters<LimitArgs>) -> Result<CallToolResult, McpError> {
        let (lg, _) = self.load_graph()?;
        let top = lg.pagerank_top(args.0.limit.unwrap_or(15));
        Ok(CallToolResult::success(vec![Content::json(top)?]))
    }

    #[tool(description = "Graph statistics (total node count).")]
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
    let Some(qv) = backend.embed(q) else { return Vec::new() };
    let Ok(vectors) = store.all_vectors() else { return Vec::new() };
    let mut scored: Vec<(f32, String)> =
        vectors.iter().map(|(id, v)| (codegraph_core::cosine(&qv, v), id.clone())).collect();
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
            "CodeGraph: a project-agnostic code knowledge graph. Use `search` to find symbols,              `get_node` for details, `callers` for reverse call edges, `stats` for counts."
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
