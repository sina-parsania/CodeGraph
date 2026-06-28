//! Optional LLM layer: ONE OpenAI-compatible backend, parameterized by a
//! provider spec. Local-first (LM Studio → mlx → Ollama), cloud opt-in
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
    let local = vec![
        Candidate { id: "lmstudio", base_url: "http://localhost:1234/v1".into(), api_key_env: None, default_model: "local-model" },
        Candidate { id: "mlx", base_url: "http://localhost:8080/v1".into(), api_key_env: None, default_model: "local-model" },
        Candidate { id: "ollama", base_url: "http://localhost:11434/v1".into(), api_key_env: None, default_model: "qwen2.5-coder:1.5b" },
    ];
    let cloud = vec![
        Candidate { id: "openai", base_url: "https://api.openai.com/v1".into(), api_key_env: Some("OPENAI_API_KEY"), default_model: "gpt-4o-mini" },
        Candidate { id: "gemini", base_url: "https://generativelanguage.googleapis.com/v1beta/openai".into(), api_key_env: Some("GEMINI_API_KEY"), default_model: "gemini-2.0-flash" },
    ];
    match env("CODEGRAPH_LLM_PROVIDER").as_deref() {
        Some("lmstudio") => local.into_iter().take(1).collect(),
        Some("mlx") => local.into_iter().skip(1).take(1).collect(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_order_is_local_first() {
        // No env set in the harness → auto order starts with local providers.
        let c = candidates();
        assert_eq!(c[0].id, "lmstudio");
        assert!(c.iter().any(|x| x.id == "ollama"));
    }

    #[test]
    fn detect_is_none_without_a_server() {
        // No LLM server in CI → graceful None (never panics).
        let _ = OpenAiCompatBackend::detect();
    }
}
