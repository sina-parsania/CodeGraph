use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml parse error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("{0}")]
    Msg(String),
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

fn default_provider() -> String {
    "auto".to_string()
}
fn default_model() -> String {
    "Qwen2.5-Coder-1.5B-Instruct".to_string()
}
fn default_vision_model() -> String {
    "SmolVLM2-500M-Instruct".to_string()
}
fn default_whisper() -> String {
    "base".to_string()
}

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
    pub fn images_enabled(&self) -> bool {
        self.media || self.images
    }
    pub fn videos_enabled(&self) -> bool {
        self.media || self.videos
    }
    pub fn media_enabled(&self) -> bool {
        self.media || self.images || self.videos
    }
}

/// Global per-user config: `$XDG_CONFIG_HOME/codegraph/config.toml`, else
/// `~/.config/codegraph/config.toml`. Lowest precedence above built-in defaults.
pub fn global_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("codegraph").join("config.toml"))
}

/// Nearest project `.codegraph.toml` walking up from `start` (if any).
pub fn project_config_path(start: &Path) -> Option<PathBuf> {
    find_config(start).ok().flatten()
}

fn merge_tables(base: &mut toml::Table, over: toml::Table) {
    for (k, v) in over {
        match (base.get_mut(&k), v) {
            (Some(toml::Value::Table(bt)), toml::Value::Table(ot)) => merge_tables(bt, ot),
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}

impl Config {
    /// Resolve config with precedence (low→high): defaults < global < project < env.
    pub fn load(start: &Path) -> Result<Config, ConfigError> {
        let mut table = toml::Table::new();
        for path in [global_config_path(), project_config_path(start)]
            .into_iter()
            .flatten()
        {
            // ONLY a missing file is skippable. PermissionDenied and every
            // other read error must surface with the path — silently ignoring
            // an unreadable config makes the user's settings vanish.
            let s = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(ConfigError::Msg(format!(
                        "cannot read config {}: {e}",
                        path.display()
                    )))
                }
            };
            // Each LAYER parses and type-checks separately so an error names
            // the offending FILE and (via toml's key-path) the FIELD. A wrong
            // type in one field previously silently defaulted the ENTIRE
            // config (unwrap_or_default over the merged table).
            let t = toml::from_str::<toml::Table>(&s).map_err(|e| {
                ConfigError::Msg(format!("{}: TOML syntax error: {e}", path.display()))
            })?;
            t.clone().try_into::<Config>().map_err(|e| {
                ConfigError::Msg(format!("{}: invalid config: {e}", path.display()))
            })?;
            merge_tables(&mut table, t);
        }
        let mut cfg: Config = table
            .try_into()
            .map_err(|e| ConfigError::Msg(format!("merged config invalid: {e}")))?;
        cfg.apply_env_from(|k| std::env::var(k).ok());
        Ok(cfg)
    }

    /// Env-override layer, parameterized by a getter for testability (no process
    /// env mutation in tests).
    pub fn apply_env_from<F: Fn(&str) -> Option<String>>(&mut self, get: F) {
        if let Some(v) = get("CODEGRAPH_CACHE_DIR") {
            self.cache_dir = Some(v);
        }
        if let Some(v) = get("CODEGRAPH_EMBED_MODEL") {
            self.embed_model = Some(v);
        }
        if let Some(v) = get("CODEGRAPH_LLM_PROVIDER") {
            self.llm.provider = v;
        }
        if let Some(v) = get("CODEGRAPH_LLM_URL") {
            self.llm.base_url = Some(v);
        }
        if let Some(v) = get("CODEGRAPH_LLM_MODEL") {
            self.llm.model = v;
        }
        if let Some(v) = get("CODEGRAPH_LLM_VISION_MODEL") {
            self.llm.vision_model = v;
        }
        if let Some(v) = get("CODEGRAPH_INGEST_WHISPER_MODEL") {
            self.llm.whisper_model = v;
        }
        if let Some(v) = get("CODEGRAPH_LLM_LOCKFILE") {
            self.llm.lockfile = Some(v);
        }
        if let Some(b) = parse_bool(get("CODEGRAPH_LLM_AUTO_INSTALL")) {
            self.llm.auto_install = b;
        }
        if let Some(b) = parse_bool(get("CODEGRAPH_RERANK")) {
            self.llm.rerank = b;
        }
        if let Some(b) = parse_bool(get("CODEGRAPH_HYDE")) {
            self.llm.hyde = b;
        }
        if let Some(b) = parse_bool(get("CODEGRAPH_MEDIA")) {
            self.ingest.media = b;
        }
        if let Some(b) = parse_bool(get("CODEGRAPH_IMAGES")) {
            self.ingest.images = b;
        }
        if let Some(b) = parse_bool(get("CODEGRAPH_VIDEOS")) {
            self.ingest.videos = b;
        }
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
        let g = MediaGate {
            media: false,
            images: true,
            videos: false,
            prompted: true,
        };
        assert!(g.images_enabled());
        assert!(!g.videos_enabled());
        assert!(g.media_enabled());
        let all = MediaGate {
            media: true,
            ..Default::default()
        };
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

    /// Only NotFound is skippable — an unreadable config must error with its
    /// path, never silently vanish from the resolution chain.
    #[test]
    #[cfg(unix)]
    fn permission_denied_config_propagates_with_path() {
        use std::os::unix::fs::PermissionsExt;
        // OWN directory — tempdir_with keys on name length and would collide
        // with (and chmod-race) the malformed-config test's directory
        let dir = std::env::temp_dir().join(format!("cg_cfg_denied_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".codegraph.toml");
        std::fs::write(&path, "[llm]\nrerank = true\n").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&path, perms).unwrap();
        if std::fs::read(&path).is_ok() {
            return; // running as root / permission-less fs — scenario can't exist here
        }
        let err = Config::load(&dir).unwrap_err().to_string();
        let mut restore = std::fs::metadata(&path).unwrap().permissions();
        restore.set_mode(0o644);
        std::fs::set_permissions(&path, restore).unwrap();
        assert!(
            err.contains(".codegraph.toml"),
            "read error must name the file: {err}"
        );
    }

    #[test]
    fn malformed_config_errors_with_file_context() {
        // Contract flipped by external review: a malformed config must FAIL
        // LOUD naming the file — silently falling back to defaults made the
        // user's settings vanish without a trace.
        let dir = tempdir_with(".codegraph.toml", "this is = = not valid toml [[[");
        let err = Config::load(&dir).unwrap_err().to_string();
        assert!(
            err.contains(".codegraph.toml"),
            "error must name the file: {err}"
        );

        // and a WRONG TYPE in one field errors with the field path, never
        // silently defaulting the whole config
        let dir2 = tempdir_with(".codegraph.toml", "[llm]\nrerank = \"yes\"\n");
        let err2 = Config::load(&dir2).unwrap_err().to_string();
        assert!(err2.contains("rerank"), "error must name the field: {err2}");
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
        let dir =
            std::env::temp_dir().join(format!("cg_cfg_{}_{}", std::process::id(), name.len()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), contents).unwrap();
        dir
    }
}

#[cfg(test)]
mod layered_tests {
    use super::*;
    #[test]
    fn merge_tables_deep_overrides() {
        let mut base: toml::Table = toml::from_str("[llm]\nmodel='a'\nrerank=false\n").unwrap();
        let over: toml::Table = toml::from_str("[llm]\nmodel='b'\n").unwrap();
        merge_tables(&mut base, over);
        // project overrides model, inherits rerank from global
        assert_eq!(base["llm"]["model"].as_str(), Some("b"));
        assert_eq!(base["llm"]["rerank"].as_bool(), Some(false));
    }
}
