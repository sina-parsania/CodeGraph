//! Core type system, config, and the declare-early LLM client traits for CodeGraph.

mod config;
mod llm;
mod types;

pub use config::{Config, ConfigError, LlmConfig, MediaGate};
pub use llm::{LlmClient, VisionLlmClient};
pub use types::{
    Confidence, Edge, EdgeRelation, Hyperedge, HyperedgeMember, HyperedgeRelation, Metadata, Node,
    NodeLabel, QualifiedName, RawCall, ResolutionTier,
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
