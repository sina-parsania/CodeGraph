use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Ordered map so JSON serialization is canonical (sorted keys) — required for
/// the byte-identical-graph determinism guarantee. `BTreeMap` is a drop-in for
/// the `new`/`insert`/`get` surface used across the codebase.
pub type Metadata = BTreeMap<String, serde_json::Value>;

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

/// Human/agent-facing label that DISAMBIGUATES same-named symbols: prefixes the
/// enclosing container (from the B1-nested id: `…file.class.method`) when it
/// adds information, and always appends `(file:line)`. Six `viewDidLoad` entry
/// points must never render identically.
pub fn display_label(n: &Node) -> String {
    let segs: Vec<&str> = n.id.split('.').collect();
    let container = if segs.len() >= 2 { segs[segs.len() - 2] } else { "" };
    let file_name = n.file_path.rsplit('/').next().unwrap_or(&n.file_path);
    let file_stem = file_name.split('.').next().unwrap_or(file_name);
    let prefix = if !container.is_empty()
        && container != normalize(&n.name)
        && container != normalize(file_stem)
    {
        format!("{container}.")
    } else {
        String::new()
    };
    format!("{prefix}{} ({file_name}:{})", n.name, n.line_start)
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

/// What a call is invoked on. Drives tiered Class-Hierarchy-Analysis resolution
/// (see docs/RESOLUTION.md). `enclosing_class` says which class node `self`/`this`
/// is statically bound to at the call site (None if rebound by a nested function).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Receiver {
    #[default]
    Bare,
    SelfThis,
    Super,
    Named(String),
    /// `this.field.method()` — `field` is looked up in the enclosing class's
    /// field→type map, and the method resolved on that type (T3 / DI).
    Field(String),
}

/// A typed field/property of a class: `field_name: TypeName`. Powers T3 resolution
/// of `this.field.method()` without type inference (the type is a literal token).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawField {
    pub class_id: String,
    pub field_name: String,
    pub type_name: String,
}

/// An import binding: `name` is usable in `file_path` and comes from `module`
/// (language-specific spec: TS relative path, Python dotted module). The resolver
/// uses it as EVIDENCE to bind an otherwise-ambiguous bare call to one module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawImport {
    pub file_path: String,
    pub name: String,
    pub module: String,
}

/// A local variable whose static type the parser inferred within a function body
/// (declared-type locals + typed params; flow-insensitive single-assignment —
/// conflicting/ambiguous declarations are dropped, never guessed). Lets the
/// resolver turn `x.method()` into a precise edge via the variable's type (T5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawLocal {
    pub caller_id: String,
    pub var_name: String,
    pub type_name: String,
}

/// An unresolved call reference captured by the parser: the enclosing caller's
/// node id, the called name, the line, plus the receiver kind + the node id of
/// the class `self`/`this` is bound to (for receiver-aware resolution).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawCall {
    pub caller_id: String,
    pub callee_name: String,
    pub line: u32,
    #[serde(default)]
    pub receiver: Receiver,
    #[serde(default)]
    pub enclosing_class: Option<String>,
}

/// A coverage signal attached to call-graph results (callers/callees/impact).
/// Derived from REAL counters: raw call sites in the `calls` table vs resolved
/// `Calls` edges. Tells an agent when a precise-but-sparse result may be missing
/// entries, so it falls back to text search instead of trusting it as complete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    /// Call sites that resolved into the graph.
    pub resolved: usize,
    /// Total raw textual call sites considered for this query.
    pub total_call_sites: usize,
    /// Call sites that did NOT resolve (ambiguous / external / unresolved).
    pub dropped: usize,
    /// True when `dropped > 0` — the precise result may be incomplete.
    pub may_be_incomplete: bool,
    /// One-line, agent-facing explanation + fallback instruction.
    pub note: String,
}

impl Coverage {
    pub fn callers(name: &str, resolved: usize, total: usize) -> Self {
        let dropped = total.saturating_sub(resolved);
        let note = if total == 0 {
            "No call sites reference this name in the indexed sources.".to_string()
        } else if dropped > 0 {
            format!(
                "Coverage: {resolved}/{total} call sites naming '{name}' resolved into the graph; \
                 {dropped} were dropped (ambiguous, external, or unresolved). This callers list may be \
                 INCOMPLETE — fall back to text search for '{name}(' to be sure."
            )
        } else {
            format!("Coverage: all {total} call sites naming '{name}' resolved — this callers list is complete.")
        };
        Coverage { resolved, total_call_sites: total, dropped, may_be_incomplete: dropped > 0, note }
    }

    pub fn callees(resolved: usize, total: usize) -> Self {
        let dropped = total.saturating_sub(resolved);
        let note = if total == 0 {
            "This symbol makes no calls in the indexed sources.".to_string()
        } else if dropped > 0 {
            format!(
                "Coverage: {resolved}/{total} outbound call sites resolved to internal definitions; \
                 {dropped} are external (library) or unresolved and are NOT in this list — read the body or grep for those."
            )
        } else {
            format!("Coverage: all {total} outbound call sites resolved — this callees list is complete.")
        };
        Coverage { resolved, total_call_sites: total, dropped, may_be_incomplete: dropped > 0, note }
    }
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

    #[test]
    fn coverage_incomplete_flag() {
        let c = Coverage::callers("foo", 3, 10);
        assert_eq!(c.dropped, 7);
        assert!(c.may_be_incomplete && c.note.contains("INCOMPLETE"));
        assert!(!Coverage::callers("foo", 5, 5).may_be_incomplete);
        assert!(!Coverage::callees(0, 0).may_be_incomplete);
    }
}
