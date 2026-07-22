//! `codegraph config` — view and edit configuration. Precedence (low→high):
//! defaults < global (~/.config/codegraph/config.toml) < project (.codegraph.toml) < env.
//! Writes are comment-preserving (toml_edit) and backed up (.bak).

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use codegraph_core::{global_config_path, project_config_path, Config};

/// Editable keys: dotted path → type. The single source of truth for get/set/view.
const KEYS: &[(&str, &str)] = &[
    ("cache_dir", "string"),
    ("embed_model", "string"),
    ("llm.provider", "string"),
    ("llm.base_url", "string"),
    ("llm.model", "string"),
    ("llm.rerank", "bool"),
    ("llm.hyde", "bool"),
    ("llm.auto_install", "bool"),
    ("ingest.media", "bool"),
    ("ingest.images", "bool"),
    ("ingest.videos", "bool"),
];

fn resolved(cfg: &Config, key: &str) -> Option<String> {
    Some(match key {
        "cache_dir" => cfg
            .cache_dir
            .clone()
            .unwrap_or_else(|| "~/.cache/codegraph (default)".into()),
        "embed_model" => cfg.embed_model.clone().unwrap_or_else(|| "(none)".into()),
        "llm.provider" => cfg.llm.provider.clone(),
        "llm.base_url" => cfg
            .llm
            .base_url
            .clone()
            .unwrap_or_else(|| "(auto-detect)".into()),
        "llm.model" => cfg.llm.model.clone(),
        "llm.rerank" => cfg.llm.rerank.to_string(),
        "llm.hyde" => cfg.llm.hyde.to_string(),
        "llm.auto_install" => cfg.llm.auto_install.to_string(),
        "ingest.media" => cfg.ingest.media.to_string(),
        "ingest.images" => cfg.ingest.images.to_string(),
        "ingest.videos" => cfg.ingest.videos.to_string(),
        _ => return None,
    })
}

/// `codegraph config` — print the resolved config.
pub fn view(cwd: &Path) -> Result<()> {
    // RECOVERY-friendly: a malformed file must not lock the user out of the
    // very command that shows where the file is — report the parse error and
    // the resolved defaults instead of failing.
    let cfg = match Config::load(cwd) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warning: config is malformed and being ignored for this view — fix it (see `codegraph config path` / `config edit`):\n  {e}");
            Config::default()
        }
    };
    let w = KEYS.iter().map(|(k, _)| k.len()).max().unwrap_or(12);
    for (key, _) in KEYS {
        println!("{:<w$} = {}", key, resolved(&cfg, key).unwrap_or_default());
    }
    println!("\n(values resolved from: defaults < global < project < env. `codegraph config path` for files.)");
    Ok(())
}

/// `codegraph config path` — where config files live + which exist.
pub fn path() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let mark = |p: &Path| if p.exists() { "  (exists)" } else { "" };
    match global_config_path() {
        Some(g) => println!("global:  {}{}", g.display(), mark(&g)),
        None => println!("global:  (unavailable)"),
    }
    match project_config_path(&cwd) {
        Some(p) => println!("project: {}{}", p.display(), mark(&p)),
        None => println!(
            "project: {}  (none yet — `codegraph config set <k> <v> --local` creates it)",
            cwd.join(".codegraph.toml").display()
        ),
    }
    Ok(())
}

/// `codegraph config get <key>` — resolved value, scriptable.
pub fn get(cwd: &Path, key: &str) -> Result<()> {
    let cfg = Config::load(cwd)?;
    println!("{}", resolved(&cfg, key).ok_or_else(|| unknown(key))?);
    Ok(())
}

fn unknown(key: &str) -> anyhow::Error {
    anyhow!(
        "unknown key '{key}'. Known keys:\n  {}",
        KEYS.iter()
            .map(|(k, _)| *k)
            .collect::<Vec<_>>()
            .join("\n  ")
    )
}

fn target(local: bool, cwd: &Path) -> PathBuf {
    if local {
        cwd.join(".codegraph.toml")
    } else {
        global_config_path().unwrap_or_else(|| cwd.join(".codegraph.toml"))
    }
}

/// Parse the target for a MUTATION. A missing file starts empty; a read error
/// or malformed TOML is fatal WITH context — `set`/`unset` must never
/// clobber a file they could not faithfully parse (that would silently
/// destroy the user's comments and unparsed keys).
fn load_doc(path: &Path) -> Result<toml_edit::DocumentMut> {
    let s = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(toml_edit::DocumentMut::default())
        }
        Err(e) => return Err(anyhow!("cannot read {}: {e}", path.display())),
    };
    s.parse().map_err(|e| {
        anyhow!(
            "{} is not valid TOML ({e}) — refusing to modify it. The file is unchanged; repair it with `codegraph config edit`",
            path.display()
        )
    })
}

fn write_doc(path: &Path, doc: &toml_edit::DocumentMut) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Backup is part of the destructive-write contract: if it cannot be
    // taken, the write does not happen.
    if path.exists() {
        let bak = path.with_extension("toml.bak");
        std::fs::copy(path, &bak).map_err(|e| {
            anyhow!(
                "cannot back up {} to {}: {e}",
                path.display(),
                bak.display()
            )
        })?;
    }
    // Same-directory tempfile + atomic replace: a failed/interrupted write
    // can never leave a truncated config behind.
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let staging = tempfile::Builder::new()
        .prefix(".codegraph.toml.")
        .suffix(".tmp")
        .tempfile_in(dir.unwrap_or_else(|| Path::new(".")))
        .map_err(|e| anyhow!("cannot stage config write next to {}: {e}", path.display()))?;
    std::fs::write(staging.path(), doc.to_string())
        .map_err(|e| anyhow!("cannot write staged config: {e}"))?;
    staging
        .persist(path)
        .map_err(|e| anyhow!("cannot replace {}: {e}", path.display()))?;
    Ok(())
}

/// `codegraph config set <key> <value> [--local]`.
pub fn set(cwd: &Path, key: &str, value: &str, local: bool) -> Result<()> {
    let (_, ty) = KEYS
        .iter()
        .find(|(k, _)| *k == key)
        .ok_or_else(|| unknown(key))?;
    let path = target(local, cwd);
    let mut doc = load_doc(&path)?;
    let item = match *ty {
        "bool" => toml_edit::value(matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )),
        _ => toml_edit::value(value),
    };
    let parts: Vec<&str> = key.split('.').collect();
    let mut tbl = doc.as_table_mut();
    for p in &parts[..parts.len() - 1] {
        tbl = tbl
            .entry(p)
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_mut()
            .ok_or_else(|| anyhow!("'{p}' is not a table"))?;
    }
    tbl[parts[parts.len() - 1]] = item;
    write_doc(&path, &doc)?;
    println!("set {key} = {value}  → {}", path.display());
    Ok(())
}

/// `codegraph config unset <key> [--local]`.
pub fn unset(cwd: &Path, key: &str, local: bool) -> Result<()> {
    KEYS.iter()
        .find(|(k, _)| *k == key)
        .ok_or_else(|| unknown(key))?;
    let path = target(local, cwd);
    let mut doc = load_doc(&path)?;
    let parts: Vec<&str> = key.split('.').collect();
    let mut tbl = doc.as_table_mut();
    for p in &parts[..parts.len() - 1] {
        match tbl.get_mut(p).and_then(|i| i.as_table_mut()) {
            Some(t) => tbl = t,
            None => {
                println!("{key} not set in {}", path.display());
                return Ok(());
            }
        }
    }
    tbl.remove(parts[parts.len() - 1]);
    write_doc(&path, &doc)?;
    println!("unset {key}  → {}", path.display());
    Ok(())
}

/// `codegraph config edit [--local]` — open the file in $VISUAL/$EDITOR.
pub fn edit(cwd: &Path, local: bool) -> Result<()> {
    let path = target(local, cwd);
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            &path,
            "# CodeGraph config — see `codegraph config` for keys.\n",
        )?;
    }
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new(&editor).arg(&path).status()?;
    if !status.success() {
        return Err(anyhow!("editor '{editor}' exited with an error"));
    }
    // Re-parse to surface syntax errors immediately.
    let s = std::fs::read_to_string(&path)?;
    s.parse::<toml_edit::DocumentMut>()
        .map_err(|e| anyhow!("{} has a TOML error: {e}", path.display()))?;
    println!("saved {}", path.display());
    Ok(())
}
