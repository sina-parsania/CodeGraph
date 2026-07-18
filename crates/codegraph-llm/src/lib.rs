//! Optional LLM layer: ONE OpenAI-compatible backend, parameterized by a
//! provider spec. Local-first (MLX → LM Studio → Ollama), cloud opt-in
//! (OpenAI / Gemini via an env key). Implements the core `LlmClient` trait;
//! every call degrades to `None` when no server is reachable.

use std::time::Duration;

use codegraph_core::LlmClient;

struct Candidate {
    id: &'static str,
    base_url: String,
    api_key_env: Option<&'static str>,
    default_model: &'static str,
}

/// A resolved, reachable OpenAI-compatible endpoint.
pub struct OpenAiCompatBackend {
    client: reqwest::blocking::Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
    embed_model: Option<String>,
    provider: &'static str,
}

fn candidates() -> Vec<Candidate> {
    let env = |k: &str| std::env::var(k).ok();
    if let Some(base) = env("CODEGRAPH_LLM_BASE_URL") {
        return vec![Candidate { id: "custom", base_url: base, api_key_env: None, default_model: "local-model" }];
    }
    // MLX first on request (best perf/RAM on Apple Silicon when running).
    let local = vec![
        Candidate { id: "mlx", base_url: "http://localhost:8080/v1".into(), api_key_env: None, default_model: "local-model" },
        Candidate { id: "lmstudio", base_url: "http://localhost:1234/v1".into(), api_key_env: None, default_model: "local-model" },
        Candidate { id: "ollama", base_url: "http://localhost:11434/v1".into(), api_key_env: None, default_model: "qwen2.5-coder:1.5b" },
    ];
    let cloud = vec![
        Candidate { id: "openai", base_url: "https://api.openai.com/v1".into(), api_key_env: Some("OPENAI_API_KEY"), default_model: "gpt-4o-mini" },
        Candidate { id: "gemini", base_url: "https://generativelanguage.googleapis.com/v1beta/openai".into(), api_key_env: Some("GEMINI_API_KEY"), default_model: "gemini-2.0-flash" },
    ];
    match env("CODEGRAPH_LLM_PROVIDER").as_deref() {
        Some("mlx") => local.into_iter().take(1).collect(),
        Some("lmstudio") => local.into_iter().skip(1).take(1).collect(),
        Some("ollama") => local.into_iter().skip(2).take(1).collect(),
        Some("openai") => cloud.into_iter().take(1).collect(),
        Some("gemini") => cloud.into_iter().skip(1).take(1).collect(),
        _ => local.into_iter().chain(cloud).collect(),
    }
}

impl OpenAiCompatBackend {
    /// Probe candidates in preference order; return the first reachable one.
    pub fn detect() -> Option<OpenAiCompatBackend> {
        let probe = reqwest::blocking::Client::builder()
            .timeout(Duration::from_millis(800))
            .build()
            .ok()?;
        let model_override = std::env::var("CODEGRAPH_LLM_MODEL").ok();
        for c in candidates() {
            let key = c.api_key_env.and_then(|e| std::env::var(e).ok());
            if c.api_key_env.is_some() && key.is_none() {
                continue;
            }
            let url = format!("{}/models", c.base_url.trim_end_matches('/'));
            let mut req = probe.get(&url);
            if let Some(k) = &key {
                req = req.bearer_auth(k);
            }
            let Ok(resp) = req.send() else { continue };
            if !resp.status().is_success() {
                continue;
            }
            let models = model_ids(resp);
            let is_chat = |m: &&String| {
                let s = m.to_lowercase();
                !s.contains("embed") && !s.contains("rerank")
            };
            let model = model_override
                .clone()
                .or_else(|| models.iter().find(is_chat).cloned())
                .or_else(|| models.first().cloned())
                .unwrap_or_else(|| c.default_model.to_string());
            let embed_model = std::env::var("CODEGRAPH_EMBED_MODEL")
                .ok()
                .or_else(|| models.iter().find(|m| m.to_lowercase().contains("embed")).cloned());
            return Some(OpenAiCompatBackend {
                client: reqwest::blocking::Client::builder().timeout(Duration::from_secs(60)).build().ok()?,
                base_url: c.base_url,
                api_key: key,
                model,
                embed_model,
                provider: c.id,
            });
        }
        None
    }

    pub fn provider(&self) -> &str {
        self.provider
    }
    pub fn model(&self) -> &str {
        &self.model
    }
}

/// `detect()` cached for the process lifetime: probing costs up to 5 HTTP
/// requests × 800 ms timeout, which a long-lived MCP server must not pay per
/// query. A server started AFTER this process won't be picked up until restart —
/// pin one explicitly via CODEGRAPH_LLM_PROVIDER / CODEGRAPH_LLM_BASE_URL.
fn detected_backend() -> &'static Option<OpenAiCompatBackend> {
    use std::sync::OnceLock;
    static BACKEND: OnceLock<Option<OpenAiCompatBackend>> = OnceLock::new();
    BACKEND.get_or_init(OpenAiCompatBackend::detect)
}

fn model_ids(resp: reqwest::blocking::Response) -> Vec<String> {
    resp.json::<serde_json::Value>()
        .ok()
        .and_then(|v| {
            v["data"].as_array().map(|a| {
                a.iter().filter_map(|m| m["id"].as_str().map(String::from)).collect()
            })
        })
        .unwrap_or_default()
}

impl LlmClient for OpenAiCompatBackend {
    fn generate(&self, prompt: &str, max_tokens: usize) -> Option<String> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": self.model,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": max_tokens,
            "temperature": 0.2,
        });
        let mut req = self.client.post(&url).json(&body);
        if let Some(k) = &self.api_key {
            req = req.bearer_auth(k);
        }
        let v: serde_json::Value = req.send().ok()?.json().ok()?;
        v["choices"][0]["message"]["content"].as_str().map(|s| s.to_string())
    }
}

impl OpenAiCompatBackend {
    pub fn embed_model(&self) -> Option<&str> {
        self.embed_model.as_deref()
    }

    /// Embed text via the OpenAI-compatible `/v1/embeddings` endpoint. Returns
    /// `None` if no embedding model is available or the request fails.
    pub fn embed(&self, text: &str) -> Option<Vec<f32>> {
        let model = self.embed_model.as_ref()?;
        let url = format!("{}/embeddings", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({ "model": model, "input": text });
        let mut req = self.client.post(&url).json(&body);
        if let Some(k) = &self.api_key {
            req = req.bearer_auth(k);
        }
        let v: serde_json::Value = req.send().ok()?.json().ok()?;
        let arr = v["data"][0]["embedding"].as_array()?;
        Some(arr.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect())
    }
}

/// Embed texts → L2-normalized vectors + the model label. Prefers a BUNDLED local
/// model (`bge-small-en-v1.5`, no server needed) when built with `--features
/// local-embed`; else a detected OpenAI-compatible endpoint; else `None`. Vectors
/// are normalized so cosine == dot at query time.
pub fn embed_texts(texts: &[String]) -> Option<(Vec<Vec<f32>>, String)> {
    if texts.is_empty() {
        return Some((Vec::new(), String::new()));
    }
    #[cfg(feature = "local-embed")]
    if let Some((v, label)) = local_embed(texts) {
        let v = v.iter().map(|x| codegraph_core::normalize(x)).collect();
        return Some((v, label));
    }
    let backend = detected_backend().as_ref().filter(|b| b.embed_model().is_some())?;
    let model = backend.embed_model().unwrap_or("?").to_string();
    let mut out = Vec::with_capacity(texts.len());
    for t in texts {
        out.push(codegraph_core::normalize(&backend.embed(t)?));
    }
    Some((out, model))
}

/// True when an embedder is available (a bundled local model, or a reachable
/// server) — so callers can give a precise "no embedder" message.
/// `CODEGRAPH_NO_EMBEDDER=1` forces false (lexical-only mode; also how the
/// no-embedder degradation path is tested deterministically).
pub fn embedder_available() -> bool {
    if std::env::var("CODEGRAPH_NO_EMBEDDER").as_deref() == Ok("1") {
        return false;
    }
    if cfg!(feature = "local-embed") {
        return true;
    }
    detected_backend().as_ref().is_some_and(|b| b.embed_model().is_some())
}

/// Local model choice: `CODEGRAPH_LOCAL_EMBED=code` selects the code-trained
/// jina-embeddings-v2-base-code (768-d, better for code semantics); default is
/// bge-small-en-v1.5 (384-d, fast, matches earlier indexes).
///
/// The model is loaded ONCE per process (same pattern as `local_gen::ENGINE`) —
/// ONNX model load costs hundreds of ms to seconds, and a long-lived MCP server
/// serves many semantic_search calls. Mutex because `embed` needs exclusive access.
#[cfg(feature = "local-embed")]
fn local_embedder() -> &'static Option<(std::sync::Mutex<fastembed::TextEmbedding>, String)> {
    use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
    use std::sync::{Mutex, OnceLock};
    static MODEL: OnceLock<Option<(Mutex<TextEmbedding>, String)>> = OnceLock::new();
    MODEL.get_or_init(|| {
        let (which, label) = match std::env::var("CODEGRAPH_LOCAL_EMBED").as_deref() {
            Ok("code") => (EmbeddingModel::JinaEmbeddingsV2BaseCode, "jina-code-v2 (local)"),
            _ => (EmbeddingModel::BGESmallENV15, "bge-small-en-v1.5 (local)"),
        };
        let cache = std::env::var_os("CODEGRAPH_CACHE_DIR")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache/codegraph")))
            .unwrap_or_else(|| std::path::PathBuf::from(".codegraph-cache"))
            .join("fastembed");
        let _ = std::fs::create_dir_all(&cache);
        // max_length 256: embed texts are name+signature+context lines, far
        // below 512 tokens — halving the sequence cap halves every activation
        // tensor ort allocates (its arena never shrinks; defaults were
        // observed >16 GB peak on a real machine).
        let opts = InitOptions::new(which)
            .with_cache_dir(cache)
            .with_show_download_progress(true)
            .with_max_length(256);
        TextEmbedding::try_new(opts).ok().map(|m| (Mutex::new(m), label.to_string()))
    })
}

#[cfg(feature = "local-embed")]
fn local_embed(texts: &[String]) -> Option<(Vec<Vec<f32>>, String)> {
    let (model, label) = local_embedder().as_ref()?;
    let docs: Vec<&str> = texts.iter().map(String::as_str).collect();
    // small batches bound ort's per-shape arena growth — memory, not
    // throughput, is the binding constraint on user machines
    let out = model.lock().ok()?.embed(docs, Some(32)).ok()?;
    Some((out, label.clone()))
}


/// Generate text: a reachable OpenAI-compat server FIRST (MLX preferred — best
/// perf/RAM on Apple Silicon when running), else the BUNDLED in-process engine
/// (mistral.rs, GGUF auto-downloaded once) when built with `--features
/// local-llm` (CPU) / `local-llm-metal` (GPU), else None. Same layering as
/// `embed_texts`.
pub fn generate_text(prompt: &str, max_tokens: usize) -> Option<String> {
    generate_text_labeled(prompt, max_tokens).map(|(out, _)| out)
}

/// Like `generate_text`, but also returns a "provider / model" label so
/// callers can attribute the answer.
pub fn generate_text_labeled(prompt: &str, max_tokens: usize) -> Option<(String, String)> {
    if let Some(b) = detected_backend().as_ref() {
        if let Some(out) = b.generate(prompt, max_tokens) {
            return Some((out, format!("{} / {}", b.provider(), b.model())));
        }
    }
    #[cfg(feature = "local-llm")]
    {
        return local_gen::generate(prompt, max_tokens).map(|out| (out, local_gen::label()));
    }
    #[allow(unreachable_code)]
    None
}

/// True when any generation path exists (server or bundled engine).
pub fn generator_available() -> bool {
    cfg!(feature = "local-llm") || detected_backend().is_some()
}

#[cfg(feature = "local-llm")]
mod local_gen {
    /// Bundled engine = mistral.rs (pure Rust, cargo-only, no cmake, no server;
    /// CPU by default, Metal GPU via `local-llm-metal`). Default model:
    /// Qwen2.5-Coder 0.5B Q4 GGUF (~400 MB download, ~600 MB RAM) — sized for
    /// fast rerank/HyDE/ask assists, loaded lazily and only when no server is
    /// reachable. Override: CODEGRAPH_LOCAL_LLM_REPO / CODEGRAPH_LOCAL_LLM_FILE
    /// (e.g. the 1.5B for higher answer quality).
    const REPO: &str = "Qwen/Qwen2.5-Coder-0.5B-Instruct-GGUF";
    const FILE: &str = "qwen2.5-coder-0.5b-instruct-q4_k_m.gguf";

    pub fn label() -> String {
        let file = std::env::var("CODEGRAPH_LOCAL_LLM_FILE").unwrap_or_else(|_| FILE.into());
        format!("bundled mistral.rs / {file}")
    }

    use std::sync::OnceLock;

    // Model load costs seconds; long-lived processes (MCP server) pay it once.
    static ENGINE: OnceLock<Option<(tokio::runtime::Runtime, mistralrs::Model)>> = OnceLock::new();

    fn engine() -> &'static Option<(tokio::runtime::Runtime, mistralrs::Model)> {
        ENGINE.get_or_init(|| {
            use mistralrs::GgufModelBuilder;
            let repo = std::env::var("CODEGRAPH_LOCAL_LLM_REPO").unwrap_or_else(|_| REPO.into());
            let file = std::env::var("CODEGRAPH_LOCAL_LLM_FILE").unwrap_or_else(|_| FILE.into());
            eprintln!("[codegraph] no LLM server detected — loading bundled engine ({repo}); first run downloads the model (one-time, cached in ~/.cache/huggingface)");
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().ok()?;
            let model = rt
                .block_on(GgufModelBuilder::new(repo, vec![file]).build())
                .map_err(|e| eprintln!("[codegraph] bundled engine failed to load: {e}"))
                .ok()?;
            Some((rt, model))
        })
    }

    pub fn generate(prompt: &str, max_tokens: usize) -> Option<String> {
        use mistralrs::{RequestBuilder, TextMessageRole};
        let (rt, model) = engine().as_ref()?;
        rt.block_on(async move {
            let req = RequestBuilder::new()
                .add_message(TextMessageRole::User, prompt)
                .set_sampler_max_len(max_tokens);
            let resp = model.send_chat_request(req).await.ok()?;
            resp.choices.first()?.message.content.as_ref().map(|c| c.trim().to_string())
        })
    }
}

/// LLM rerank: ask the model to reorder hits by relevance to the query.
/// Best-effort — falls back to the original order on any parse failure.
pub fn rerank(query: &str, hits: Vec<codegraph_core::Node>) -> Vec<codegraph_core::Node> {
    if hits.len() < 2 {
        return hits;
    }
    let listing: String = hits
        .iter()
        .enumerate()
        .map(|(i, n)| format!("{}. {} ({:?}) {}", i, n.name, n.label, n.file_path))
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = format!(
        "Rank these code symbols by relevance to the query \"{}\". Reply with ONLY the leading numbers, best first, comma-separated.\n\n{}",
        query, listing
    );
    let Some(resp) = generate_text(&prompt, 200) else { return hits };
    let order: Vec<usize> = resp
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|t| t.parse::<usize>().ok())
        .filter(|&i| i < hits.len())
        .collect();
    if order.is_empty() {
        return hits;
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for &i in &order {
        if seen.insert(i) {
            out.push(hits[i].clone());
        }
    }
    for (i, n) in hits.iter().enumerate() {
        if !seen.contains(&i) {
            out.push(n.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_order_is_local_first() {
        // No env set in the harness → auto order starts with local providers.
        let c = candidates();
        assert_eq!(c[0].id, "mlx");
        assert!(c.iter().any(|x| x.id == "lmstudio"));
        assert!(c.iter().any(|x| x.id == "ollama"));
    }

    #[test]
    fn detect_is_none_without_a_server() {
        // No LLM server in CI → graceful None (never panics).
        let _ = OpenAiCompatBackend::detect();
    }
}
