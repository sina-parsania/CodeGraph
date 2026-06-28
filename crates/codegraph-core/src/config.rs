use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml parse error: {0}")]
    Toml(#[from] toml::de::Error),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub cache_dir: Option<String>,
    #[serde(default)]
    pub embed_model: Option<String>,
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub ingest: MediaGate,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmConfig {
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_vision_model")]
    pub vision_model: String,
    #[serde(default)]
    pub auto_install: bool,
    #[serde(default)]
    pub lockfile: Option<String>,
    #[serde(default)]
    pub rerank: bool,
    #[serde(default)]
    pub hyde: bool,
    #[serde(default = "default_whisper")]
    pub whisper_model: String,
}

fn default_provider() -> String { "auto".to_string() }
fn default_model() -> String { "Qwen2.5-Coder-1.5B-Instruct".to_string() }
fn default_vision_model() -> String { "SmolVLM2-500M-Instruct".to_string() }
fn default_whisper() -> String { "base".to_string() }

impl Default for LlmConfig {
    fn default() -> Self {
        LlmConfig {
            provider: default_provider(),
            base_url: None,
            model: default_model(),
            vision_model: default_vision_model(),
            auto_install: false,
            lockfile: None,
            rerank: false,
            hyde: false,
            whisper_model: default_whisper(),
        }
    }
}

/// Media (image/video) ingestion gate. Default ALL-OFF: media is opt-in and
/// prompted. One source of truth read by the ingest dispatch, the vision-model
/// probe, the CLI prompt, `index_status`, and `doctor`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MediaGate {
    #[serde(default)]
    pub media: bool,
    #[serde(default)]
    pub images: bool,
    #[serde(default)]
    pub videos: bool,
    #[serde(default)]
    pub prompted: bool,
}

impl MediaGate {
    pub fn images_enabled(&self) -> bool { self.media || self.images }
    pub fn videos_enabled(&self) -> bool { self.media || self.videos }
    pub fn media_enabled(&self) -> bool { self.media || self.images || self.videos }
}

impl Config {
    /// Walk up from `start` to find `.codegraph.toml`, parse it, then layer env
    /// overrides on top.
    pub fn load(start: &Path) -> Result<Config, ConfigError> {
        let mut cfg = match find_config(start)? {
            Some(path) => toml::from_str(&std::fs::read_to_string(path)?)?,
            None => Config::default(),
        };
        cfg.apply_env_from(|k| std::env::var(k).ok());
        Ok(cfg)
    }

    /// Env-override layer, parameterized by a getter for testability (no process
    /// env mutation in tests).
    pub fn apply_env_from<F: Fn(&str) -> Option<String>>(&mut self, get: F) {
        if let Some(v) = get("CODEGRAPH_CACHE_DIR") { self.cache_dir = Some(v); }
        if let Some(v) = get("CODEGRAPH_EMBED_MODEL") { self.embed_model = Some(v); }
        if let Some(v) = get("CODEGRAPH_LLM_PROVIDER") { self.llm.provider = v; }
        if let Some(v) = get("CODEGRAPH_LLM_URL") { self.llm.base_url = Some(v); }
        if let Some(v) = get("CODEGRAPH_LLM_MODEL") { self.llm.model = v; }
        if let Some(v) = get("CODEGRAPH_LLM_VISION_MODEL") { self.llm.vision_model = v; }
        if let Some(v) = get("CODEGRAPH_INGEST_WHISPER_MODEL") { self.llm.whisper_model = v; }
        if let Some(v) = get("CODEGRAPH_LLM_LOCKFILE") { self.llm.lockfile = Some(v); }
        if let Some(b) = parse_bool(get("CODEGRAPH_LLM_AUTO_INSTALL")) { self.llm.auto_install = b; }
        if let Some(b) = parse_bool(get("CODEGRAPH_RERANK")) { self.llm.rerank = b; }
        if let Some(b) = parse_bool(get("CODEGRAPH_HYDE")) { self.llm.hyde = b; }
        if let Some(b) = parse_bool(get("CODEGRAPH_MEDIA")) { self.ingest.media = b; }
        if let Some(b) = parse_bool(get("CODEGRAPH_IMAGES")) { self.ingest.images = b; }
        if let Some(b) = parse_bool(get("CODEGRAPH_VIDEOS")) { self.ingest.videos = b; }
    }
}

fn parse_bool(v: Option<String>) -> Option<bool> {
    v.map(|s| matches!(s.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

fn find_config(start: &Path) -> std::io::Result<Option<PathBuf>> {
    let mut dir = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    loop {
        let candidate = dir.join(".codegraph.toml");
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
        match dir.parent() {
            Some(p) => dir = p.to_path_buf(),
            None => return Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn defaults_are_media_off_and_small_model() {
        let c = Config::default();
        assert!(!c.ingest.media_enabled());
        assert_eq!(c.llm.model, "Qwen2.5-Coder-1.5B-Instruct");
        assert_eq!(c.llm.provider, "auto");
        assert!(!c.llm.auto_install);
    }

    #[test]
    fn media_gate_resolution() {
        let g = MediaGate { media: false, images: true, videos: false, prompted: true };
        assert!(g.images_enabled());
        assert!(!g.videos_enabled());
        assert!(g.media_enabled());
        let all = MediaGate { media: true, ..Default::default() };
        assert!(all.images_enabled() && all.videos_enabled());
    }

    #[test]
    fn env_overrides_apply() {
        let mut c = Config::default();
        let mut env = HashMap::new();
        env.insert("CODEGRAPH_LLM_MODEL".to_string(), "custom-7b".to_string());
        env.insert("CODEGRAPH_MEDIA".to_string(), "true".to_string());
        env.insert("CODEGRAPH_LLM_AUTO_INSTALL".to_string(), "1".to_string());
        c.apply_env_from(|k| env.get(k).cloned());
        assert_eq!(c.llm.model, "custom-7b");
        assert!(c.ingest.media_enabled());
        assert!(c.llm.auto_install);
    }

    #[test]
    fn parse_error_surfaces() {
        let dir = tempdir_with(".codegraph.toml", "this is = = not valid toml [[[");
        let err = Config::load(&dir);
        assert!(matches!(err, Err(ConfigError::Toml(_))));
    }

    #[test]
    fn missing_file_uses_defaults() {
        let dir = std::env::temp_dir().join(format!("cg_missing_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let c = Config::load(&dir).unwrap();
        assert_eq!(c, Config::default());
        std::fs::remove_dir_all(&dir).ok();
    }

    fn tempdir_with(name: &str, contents: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cg_cfg_{}_{}", std::process::id(), name.len()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), contents).unwrap();
        dir
    }
}
