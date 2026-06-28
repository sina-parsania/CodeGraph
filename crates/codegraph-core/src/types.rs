use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub type Metadata = HashMap<String, serde_json::Value>;

/// What a graph node represents. `Concept` is LLM-only (never produced on the
/// `--no-llm` path); `Image`/`Figure` are NOT LLM-only (they always carry at
/// least OCR/EXIF/filename text on the degraded path, per the N7 contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeLabel {
    Project, Package, Folder, File, Module, Class, Function, Method, Interface,
    Enum, Type, Route, Resource, Document, Image, Figure, Topic,
    /// Emitted ONLY when an LLM is available. Never emitted on `--no-llm`.
    Concept,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EdgeRelation {
    Calls, AsyncCalls, UsesType, Implements, Inherits, Defines, MemberOf, Contains,
    ContainsFile, ContainsFolder, ContainsPackage, Override, HttpCalls, Emits,
    ListensOn, PublishesTo, SubscribesTo, Configures, Tests, FileChangesWith,
    Similar, SemanticallySimilar, DataFlows, ConceptuallyRelated, RationaleFor,
    ParticipateIn, Implement, Form, MemberOfFlow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HyperedgeRelation { ParticipateIn, Implement, Form, MemberOfFlow }

/// Which mechanism produced an edge, most-precise first. Tagged on every edge so
/// a consumer can trust-rank (SCIP-verified vs name-matched vs LLM-guessed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ResolutionTier { Scip, TreeSitter, Llm, Ingest }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Confidence { Extracted, Inferred, Ambiguous }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub label: NodeLabel,
    pub name: String,
    pub file_path: String,
    pub line_start: u32,
    pub line_end: u32,
    pub language: String,
    #[serde(default)]
    pub metadata: Metadata,
    #[serde(default)]
    pub community: Option<u32>,
    #[serde(default)]
    pub pagerank: f64,
    #[serde(default)]
    pub betweenness: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub src: String,
    pub dst: String,
    pub relation: EdgeRelation,
    pub tier: ResolutionTier,
    pub confidence: Confidence,
    pub src_file: String,
    pub src_line: u32,
    #[serde(default)]
    pub metadata: Metadata,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hyperedge {
    pub id: String,
    pub relation: HyperedgeRelation,
    pub label: String,
    pub confidence: Confidence,
    pub tier: ResolutionTier,
    #[serde(default)]
    pub metadata: Metadata,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HyperedgeMember {
    pub hyperedge_id: String,
    pub node_id: String,
    #[serde(default)]
    pub role: Option<String>,
}

/// Builds deterministic qualified names: `<project>.<path>.<name>`, each segment
/// normalized to `[a-z0-9_]` so the same entity always yields the same id.
pub struct QualifiedName;

impl QualifiedName {
    pub fn build(project: &str, path_parts: &[&str], name: &str) -> String {
        let mut parts = Vec::with_capacity(path_parts.len() + 2);
        parts.push(normalize(project));
        parts.extend(path_parts.iter().map(|p| normalize(p)));
        parts.push(normalize(name));
        parts.into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(".")
    }
}

fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_us = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_us = false;
        } else if !prev_us {
            out.push('_');
            prev_us = true;
        }
    }
    out.trim_matches('_').to_string()
}

/// A class/type → supertype reference captured by the parser, resolved into an
/// INHERITS (extends) or IMPLEMENTS edge by the graph builder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InheritKind {
    Extends,
    Implements,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawInherit {
    pub impl_name: String,
    pub super_name: String,
    pub kind: InheritKind,
}

/// An unresolved call reference captured by the parser: the enclosing caller's
/// node id, the called name, and the line. The graph builder resolves
/// `callee_name` to a node id (intra-language, intra-file) into a CALLS edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawCall {
    pub caller_id: String,
    pub callee_name: String,
    pub line: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_name_normalizes() {
        assert_eq!(QualifiedName::build("MyApp", &["src", "auth"], "getProfile"), "myapp.src.auth.getprofile");
        assert_eq!(QualifiedName::build("a", &["b/c", "d.e"], "F-G"), "a.b_c.d_e.f_g");
        assert_eq!(QualifiedName::build("p", &[], "name"), "p.name");
        assert_eq!(QualifiedName::build("p", &["", "  "], "n"), "p.n");
    }

    #[test]
    fn enums_roundtrip() {
        let n = Node {
            id: "p.f".into(), label: NodeLabel::Function, name: "f".into(),
            file_path: "f.rs".into(), line_start: 1, line_end: 9, language: "rust".into(),
            metadata: Metadata::new(), community: Some(3), pagerank: 0.5, betweenness: 0.1,
        };
        let j = serde_json::to_string(&n).unwrap();
        let back: Node = serde_json::from_str(&j).unwrap();
        assert_eq!(n, back);
    }
}
