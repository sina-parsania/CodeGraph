//! Opt-in Swift precision tier — read Apple's **IndexStore** (the compiler-built
//! index in `DerivedData/<proj>/Index.noindex/DataStore`, populated by normal Xcode
//! builds) and merge compiler-grade CALL edges into the graph. macOS-only; gated
//! behind the CLI `indexstore` feature so the default static binary never links it.
//!
//! It enriches at INDEX time (read once, merge); queries are still served from the
//! fast SQLite graph — so the agent never pays a per-query LSP cost (unlike Serena).

use std::collections::HashMap;
use std::path::Path;

use codegraph_core::{Confidence, Edge, EdgeRelation, Metadata, Node, NodeLabel, ResolutionTier};

/// One symbol occurrence read from the index store.
pub struct Occ {
    /// Unified Symbol Resolution id (stable identity across files).
    pub usr: String,
    /// Bitset of `indexstore_symbol_role_t` roles for this occurrence.
    pub roles: u64,
    /// File the occurrence is in (repo-relative when possible).
    pub file: String,
    /// 1-based line.
    pub line: u32,
    /// For a CALL occurrence: the USR of the containing/calling symbol
    /// (from the CALLEDBY / CONTAINEDBY relation).
    pub caller_usr: Option<String>,
}

// `indexstore_symbol_role_t` bits (confirmed against indexstore.h).
pub const ROLE_DEFINITION: u64 = 1 << 1;
pub const ROLE_CALL: u64 = 1 << 5;
pub const ROLE_REL_CALLEDBY: u64 = 1 << 13;

/// Map the index store's call occurrences onto the existing tree-sitter graph,
/// producing compiler-grade CALL edges (tier=Lsp, justification=IndexStore).
/// Mirrors `codegraph-resolve::import_scip`: USR→def-node, then call→edge.
pub fn import_indexstore(store: &Path, nodes: &[Node], repo_root: &Path) -> anyhow::Result<Vec<Edge>> {
    let occs = read_occurrences(store, repo_root)?;
    if occs.is_empty() {
        return Ok(Vec::new());
    }
    let mut by_file: HashMap<&str, Vec<&Node>> = HashMap::new();
    for n in nodes {
        by_file.entry(n.file_path.as_str()).or_default().push(n);
    }
    // Pass 1: USR → its defining node (by enclosing span at the def's line).
    let mut sym_def: HashMap<&str, &Node> = HashMap::new();
    for o in &occs {
        if o.roles & ROLE_DEFINITION == 0 {
            continue;
        }
        if let Some(file_nodes) = by_file.get(o.file.as_str()) {
            if let Some(n) = best_def_node(file_nodes, o.line) {
                sym_def.entry(o.usr.as_str()).or_insert(n);
            }
        }
    }
    // Pass 2: each CALL occurrence → edge caller-def-node → callee-def-node.
    let mut edges = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for o in &occs {
        if o.roles & ROLE_CALL == 0 {
            continue;
        }
        let Some(caller_usr) = o.caller_usr.as_deref() else { continue };
        let (Some(&src), Some(&dst)) = (sym_def.get(caller_usr), sym_def.get(o.usr.as_str())) else { continue };
        if src.id == dst.id || !seen.insert((src.id.clone(), dst.id.clone())) {
            continue;
        }
        let mut metadata = Metadata::new();
        metadata.insert("justification".to_string(), serde_json::json!("IndexStore"));
        edges.push(Edge {
            src: src.id.clone(),
            dst: dst.id.clone(),
            relation: EdgeRelation::Calls,
            tier: ResolutionTier::Scip, // compiler-grade tier (supersedes tree-sitter)
            confidence: Confidence::Extracted,
            src_file: o.file.clone(),
            src_line: o.line,
            metadata,
        });
    }
    Ok(edges)
}

/// Smallest definition node whose span contains `line` (the enclosing callable/type).
fn best_def_node<'a>(file_nodes: &[&'a Node], line: u32) -> Option<&'a Node> {
    file_nodes
        .iter()
        .filter(|n| {
            matches!(n.label, NodeLabel::Function | NodeLabel::Method | NodeLabel::Class | NodeLabel::Interface)
                && n.line_start <= line
                && line <= n.line_end
        })
        .min_by_key(|n| n.line_end - n.line_start)
        .copied()
}

#[cfg(not(all(target_os = "macos", feature = "link")))]
fn read_occurrences(_store: &Path, _root: &Path) -> anyhow::Result<Vec<Occ>> {
    anyhow::bail!("IndexStore reading needs macOS + `--features indexstore` (links libIndexStore)")
}

#[cfg(all(target_os = "macos", feature = "link"))]
mod macos;
#[cfg(all(target_os = "macos", feature = "link"))]
fn read_occurrences(store: &Path, root: &Path) -> anyhow::Result<Vec<Occ>> {
    macos::read_occurrences(store, root)
}
