//! Core type system, config, and the declare-early LLM client traits for CodeGraph.

mod config;
mod llm;
mod types;

pub use config::{
    global_config_path, project_config_path, Config, ConfigError, LlmConfig, MediaGate,
};
pub use llm::{LlmClient, VisionLlmClient};
pub use types::{
    Confidence, Edge, EdgeRelation, Hyperedge, HyperedgeMember, HyperedgeRelation, InheritKind, Metadata, Node,
    NodeLabel, QualifiedName, RawCall, RawInherit, ResolutionTier,
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Cosine similarity of two vectors (shared by CLI + MCP semantic search).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for i in 0..n {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}
