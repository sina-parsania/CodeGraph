//! MCP server (M6): exposes the code graph to AI agents over stdio. Tools:
//! `search`, `get_node`, `callers`, `stats`. The whole CLI is the standalone
//! package; `codegraph mcp` runs this server as one subcommand.

use std::path::PathBuf;

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
        Self { db_path, tool_router: Self::tool_router() }
    }

    fn open(&self) -> Result<codegraph_store::Store, McpError> {
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

/// Run the MCP server over stdio until the client disconnects.
pub async fn serve_stdio(db_path: PathBuf) -> anyhow::Result<()> {
    let service = CodeGraphServer::new(db_path).serve(stdio()).await?;
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
