//! Core type system, config, and the declare-early LLM client traits for CodeGraph.

mod config;
mod llm;
mod types;

pub use config::{
    global_config_path, project_config_path, Config, ConfigError, LlmConfig, MediaGate,
};
pub use llm::{LlmClient, VisionLlmClient};
pub use types::{
    Confidence, Coverage, Edge, EdgeRelation, Hyperedge, HyperedgeMember, HyperedgeRelation, InheritKind, Metadata,
    Node, NodeLabel, QualifiedName, RawCall, RawField, RawImport, RawInherit, RawLocal, Receiver, ResolutionTier,
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

/// Is `path` a test file? Token-based, NOT substring-based: `latest_prices.rs`,
/// `contest.py`, `attestation.go` are NOT tests; `foo_test.go`, `FooTests.swift`,
/// `__tests__/x.ts`, `spec/y.rb` are. Splits path segments on `/ _ - .` and
/// camelCase boundaries, then matches whole tokens only. Shared by the graph
/// builder (TESTS edges) and the store (dead-code / test-coverage queries) so
/// the two views never disagree.
pub fn is_test_path(path: &str) -> bool {
    let mut token = String::new();
    let mut prev_lower = false;
    let check = |t: &mut String| {
        let hit = matches!(t.as_str(), "test" | "tests" | "spec" | "specs" | "testing");
        t.clear();
        hit
    };
    for c in path.chars() {
        if c.is_alphanumeric() {
            if c.is_uppercase() && prev_lower && !token.is_empty() && check(&mut token) {
                return true;
            }
            prev_lower = c.is_lowercase();
            token.push(c.to_ascii_lowercase());
        } else {
            prev_lower = false;
            if !token.is_empty() && check(&mut token) {
                return true;
            }
        }
    }
    !token.is_empty() && check(&mut token)
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
mod test_path_tests {
    use super::is_test_path;
    #[test]
    fn tokens_not_substrings() {
        for p in [
            "src/foo_test.go", "Tests/AuthTests.swift", "src/__tests__/x.ts",
            "spec/y_spec.rb", "tests/test_foo.py", "src/user.spec.ts", "FooTest.java",
        ] {
            assert!(is_test_path(p), "{p} IS a test path");
        }
        for p in [
            "src/latest_prices.rs", "src/contest.py", "pkg/attestation.go",
            "src/spectrum.ts", "app/protester.rb", "src/inspection.java",
        ] {
            assert!(!is_test_path(p), "{p} is NOT a test path");
        }
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
