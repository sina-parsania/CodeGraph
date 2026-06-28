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
    NodeLabel, QualifiedName, RawCall, RawInherit, Receiver, ResolutionTier,
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

/// Plain dot product. For L2-normalized vectors this equals cosine — cheaper, and
/// what semantic search scores with after `normalize`.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let mut acc = 0.0f32;
    for i in 0..n {
        acc += a[i] * b[i];
    }
    acc
}

/// Return an L2-normalized copy (unit length) so dot == cosine. Zero vectors pass through.
pub fn normalize(v: &[f32]) -> Vec<f32> {
    let mag = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if mag == 0.0 {
        v.to_vec()
    } else {
        v.iter().map(|x| x / mag).collect()
    }
}

#[cfg(test)]
mod vec_tests {
    use super::*;
    #[test]
    fn dot_of_normalized_equals_cosine() {
        let a = [1.0f32, 2.0, 3.0, 0.5];
        let b = [0.2f32, -1.0, 4.0, 2.0];
        let (na, nb) = (normalize(&a), normalize(&b));
        assert!((dot(&na, &nb) - cosine(&a, &b)).abs() < 1e-5, "normalized dot must equal cosine");
        // normalize is idempotent in magnitude
        assert!((normalize(&na).iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-5);
    }
}
