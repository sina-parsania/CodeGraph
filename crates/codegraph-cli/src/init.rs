//! `codegraph init` — first-run setup. Index the repo, wire the MCP server into
//! Claude Code, drop an agent-nudge (so agents prefer the MCP over grep), and
//! write a commented `.codegraph.toml`. Everything AI is opt-in; core works with
//! no model. Non-interactive (`--yes` or non-TTY) accepts every default.

use std::io::{IsTerminal, Write};
use std::path::Path;

use anyhow::Result;

use crate::index;

const CONFIG_TEMPLATE: &str = r#"# CodeGraph configuration. Core (index/search/callers/impact/trace/query) works
# with NO model — everything below is OPTIONAL. Env vars (CODEGRAPH_*) override these.

# Where graphs are stored (default: ~/.cache/codegraph, keyed by project path).
# cache_dir = "/custom/path"

# Embedding model for semantic search (needs a running provider that serves it,
# or build with --features local-embed for in-process embeddings).
# embed_model = "nomic-embed-text"

[llm]
provider = "auto"                       # auto | lmstudio | mlx | ollama | openai | gemini
# base_url = "http://localhost:1234/v1"
model = "Qwen2.5-Coder-1.5B-Instruct"   # small chat model for `ask` / HyDE
rerank = false                          # rerank `search` results with the LLM
hyde = false                            # HyDE in semantic search by default
auto_install = false                    # never auto-download models

[ingest]
media = false                           # OCR images / transcribe video (opt-in; needs --features media)
prompted = true                         # init already asked about media; don't re-prompt
"#;

const CLAUDE_BEGIN: &str = "<!-- BEGIN codegraph -->";
const CLAUDE_END: &str = "<!-- END codegraph -->";

fn claude_block() -> String {
    format!(
        "{CLAUDE_BEGIN}\n\
## CodeGraph (code navigation)\n\n\
This repository is indexed by **CodeGraph** into a live code knowledge graph (auto-reindexed before each query). \
For code navigation, **prefer the `mcp__codegraph__*` tools over grep / reading files** — they return exact `file:line` and resolved call edges:\n\
- `search` — where a symbol is defined or used\n\
- `callers` / `callees` — trace call edges\n\
- `blast_radius` — what depends on a symbol (before a refactor)\n\
- `trace_path` — shortest path between two symbols\n\
- `important` — most central symbols (map an unfamiliar repo)\n\
- `semantic_search` — find code by meaning\n\
{CLAUDE_END}\n"
    )
}

const NUDGE_SCRIPT: &str = r#"#!/usr/bin/env bash
# CodeGraph agent nudge (SessionStart). Emits factual context so the agent uses the MCP.
cat <<'JSON'
{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"This repository is indexed by CodeGraph (live, auto-reindexed before each query). For code navigation prefer the mcp__codegraph__* tools (search, callers, callees, blast_radius, trace_path, important, semantic_search) over grep or reading files — they return exact file:line and resolved call edges."}}
JSON
"#;

/// JSON type name for error messages.
fn json_type(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// Parse a user config file that MUST be a JSON object. Invalid JSON or a
/// non-object root is the USER'S data in an unexpected state — return a
/// contextual error naming the file instead of silently replacing it with `{}`
/// (which would destroy every other setting in it). A missing file is fine:
/// start from an empty object.
fn load_json_object(path: &Path) -> Result<serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(serde_json::json!({}));
    };
    let root: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        anyhow::anyhow!(
            "{}: invalid JSON ({e}) — fix or remove the file and re-run; refusing to overwrite it",
            path.display()
        )
    })?;
    anyhow::ensure!(
        root.is_object(),
        "{}: top-level JSON is {} (expected an object) — fix the file and re-run; refusing to overwrite it",
        path.display(),
        json_type(&root)
    );
    Ok(root)
}

/// Return `root[key]` as a mutable object, creating `{}` when absent. When the
/// field EXISTS with a non-object type, error with file + field + found type —
/// never clobber, never report false success.
fn ensure_object_field<'a>(
    root: &'a mut serde_json::Value,
    key: &str,
    path: &Path,
) -> Result<&'a mut serde_json::Map<String, serde_json::Value>> {
    let obj = root
        .as_object_mut()
        .expect("load_json_object guarantees an object root");
    let slot = obj
        .entry(key.to_string())
        .or_insert_with(|| serde_json::json!({}));
    if !slot.is_object() {
        anyhow::bail!(
            "{}: `{key}` is {} (expected an object) — fix the file and re-run; refusing to overwrite it",
            path.display(),
            json_type(slot)
        );
    }
    Ok(slot.as_object_mut().expect("checked above"))
}

/// Merge the codegraph MCP entry into the JSON config at `path`, preserving
/// everything else in the file. Verifies the POSTCONDITION by re-reading what
/// was written — success is only reported for a config that actually carries
/// the entry.
fn merge_mcp_entry(path: &Path) -> Result<()> {
    let entry = serde_json::json!({"command": "codegraph", "args": ["mcp"]});
    let mut root = load_json_object(path)?;
    let servers = ensure_object_field(&mut root, "mcpServers", path)?;
    servers.insert("codegraph".to_string(), entry.clone());
    if path.exists() {
        let _ = std::fs::copy(path, path.with_extension("json.bak"));
    }
    std::fs::write(path, serde_json::to_string_pretty(&root)?)?;
    // postcondition: the file on disk parses and carries the entry
    let check: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    anyhow::ensure!(
        check.get("mcpServers").and_then(|s| s.get("codegraph")) == Some(&entry),
        "{}: postcondition failed — written config does not carry the codegraph entry",
        path.display()
    );
    Ok(())
}

/// Merge the MCP server entry into `~/.claude.json` (idempotent), or print the
/// snippet when `print_only` (no `~/.claude.json`, or `--print`). Shared by
/// `init` and the back-compat `install` command.
pub fn wire_mcp(_repo: &Path, print_only: bool) -> Result<()> {
    // NO --path: `~/.claude.json` mcpServers is USER-GLOBAL, and agents launch
    // MCP servers with cwd = the active project. Pinning one repo's absolute
    // path here made the LAST-initialized repo win globally — and when that
    // repo moved, EVERY project got a confidently-empty graph (measured in the
    // field). cwd-following serves each project its own graph.
    let snippet = serde_json::to_string_pretty(&serde_json::json!({
        "mcpServers": {"codegraph": {"command": "codegraph", "args": ["mcp"]}}
    }))?;
    let home = std::env::var("HOME").unwrap_or_default();
    let path = Path::new(&home).join(".claude.json");
    if print_only || !path.exists() {
        println!("Add this to your agent's MCP config:\n{snippet}");
        return Ok(());
    }
    merge_mcp_entry(&path)?;
    println!(
        "  → wired MCP into {} (backup .bak written)",
        path.display()
    );
    Ok(())
}

/// Drop the agent nudge: a sentinel block in CLAUDE.md + a SessionStart hook that
/// emits factual context. Both idempotent. `remove` strips them.
fn agent_nudge(repo: &Path, remove: bool) -> Result<()> {
    // 1. CLAUDE.md sentinel block
    let claude_md = repo.join("CLAUDE.md");
    let existing = std::fs::read_to_string(&claude_md).unwrap_or_default();
    let stripped = strip_block(&existing);
    let next = if remove {
        stripped
    } else if stripped.trim().is_empty() {
        claude_block()
    } else {
        format!("{}\n\n{}", stripped.trim_end(), claude_block())
    };
    if next != existing {
        if next.trim().is_empty() {
            let _ = std::fs::remove_file(&claude_md);
        } else {
            std::fs::write(&claude_md, next)?;
        }
    }

    // 2. SessionStart hook script + settings.local.json registration
    let hooks_dir = repo.join(".claude").join("hooks");
    let script = hooks_dir.join("codegraph-nudge.sh");
    let settings = repo.join(".claude").join("settings.local.json");
    if remove {
        let _ = std::fs::remove_file(&script);
        if let Ok(s) = std::fs::read_to_string(&settings) {
            if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&s) {
                if let Some(h) = v.get_mut("hooks").and_then(|h| h.as_object_mut()) {
                    h.remove("SessionStart");
                }
                let _ = std::fs::write(&settings, serde_json::to_string_pretty(&v)?);
            }
        }
        return Ok(());
    }
    std::fs::create_dir_all(&hooks_dir)?;
    std::fs::write(&script, NUDGE_SCRIPT)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755));
    }
    merge_hook_settings(&settings)?;
    println!("  → agent nudge written (CLAUDE.md block + .claude SessionStart hook)");
    Ok(())
}

/// Merge the SessionStart nudge hook into `settings`, preserving everything
/// else. Same contract as `merge_mcp_entry`: invalid JSON or a non-object
/// `hooks` field errors with context instead of being clobbered; success is
/// postcondition-checked.
fn merge_hook_settings(settings: &Path) -> Result<()> {
    let hook = serde_json::json!([{"hooks": [{"type": "command", "command": "bash .claude/hooks/codegraph-nudge.sh"}]}]);
    let mut v = load_json_object(settings)?;
    let hooks = ensure_object_field(&mut v, "hooks", settings)?;
    hooks.insert("SessionStart".to_string(), hook.clone());
    if let Some(dir) = settings.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(settings, serde_json::to_string_pretty(&v)?)?;
    let check: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(settings)?)?;
    anyhow::ensure!(
        check.get("hooks").and_then(|h| h.get("SessionStart")) == Some(&hook),
        "{}: postcondition failed — written settings do not carry the SessionStart hook",
        settings.display()
    );
    Ok(())
}

fn strip_block(s: &str) -> String {
    let (Some(b), Some(e)) = (s.find(CLAUDE_BEGIN), s.find(CLAUDE_END)) else {
        return s.to_string();
    };
    let end = e + CLAUDE_END.len();
    let mut out = String::new();
    out.push_str(&s[..b]);
    out.push_str(&s[end..]);
    out.trim().to_string()
}

fn ask(question: &str, default_yes: bool, interactive: bool) -> bool {
    if !interactive {
        return default_yes;
    }
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    print!("? {question} {hint} ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return default_yes;
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run_init(
    repo: &Path,
    yes: bool,
    no_index: bool,
    no_mcp: bool,
    force: bool,
    print: bool,
    uninstall: bool,
) -> Result<()> {
    if uninstall {
        agent_nudge(repo, true)?;
        println!("removed CodeGraph agent nudge (CLAUDE.md block + SessionStart hook). MCP entry left intact.");
        return Ok(());
    }
    let interactive = !yes && std::io::stdin().is_terminal();
    println!("codegraph {} — setup", codegraph_core::VERSION);
    println!("  Core (index/search/callers/impact/trace/query) works with NO model. AI features are optional.\n");

    // Step 1: index
    if !no_index && ask("Index this repo now?", true, interactive) {
        let db = index::db_path(repo);
        let stats = index::index_dir(repo, &db, false, None, false, None)?;
        println!(
            "  → indexed {} files → {} nodes, {} edges",
            stats.files, stats.nodes, stats.edges
        );
    }

    // Step 2: MCP wiring (global ~/.claude.json) + agent nudge (local repo files).
    // `--print` only affects the global wiring; the local nudge is always written.
    if !no_mcp
        && ask(
            "Wire CodeGraph into Claude Code (MCP) + add the agent nudge?",
            true,
            interactive,
        )
    {
        wire_mcp(repo, print)?;
        agent_nudge(repo, false)?;
    }

    // Step 3: optional AI features (report only — never install/block)
    if ask(
        "Enable optional AI features (semantic search / ask / rerank)?",
        false,
        interactive,
    ) {
        report_ai();
    } else {
        println!("  → AI features off. Core works fully offline; run `codegraph doctor` to check models later.");
    }

    // Step 4: write the commented config
    let cfg_path = repo.join(".codegraph.toml");
    if cfg_path.exists() && !force {
        println!("\n.codegraph.toml already exists — not overwriting (use --force to regenerate).");
    } else {
        std::fs::write(&cfg_path, CONFIG_TEMPLATE)?;
        println!("\n  → wrote {}", cfg_path.display());
    }
    println!("\n✓ Setup complete. Try:  codegraph status  |  codegraph search <term>  |  codegraph doctor");
    Ok(())
}

fn report_ai() {
    match codegraph_llm::OpenAiCompatBackend::detect() {
        Some(b) => {
            println!("  → provider: {} (chat model: {})", b.provider(), b.model());
            if b.embed_model().is_none() {
                println!(
                    "  Semantic search needs a separate EMBEDDING model. To enable, run one of:"
                );
                println!("      ollama pull nomic-embed-text");
                println!("      lms get nomic-embed-text-v1.5  (then `lms server start`)");
                println!("  then:  codegraph semantic-index");
            } else {
                println!(
                    "  → embedding model ready ({}). Run: codegraph semantic-index",
                    b.embed_model().unwrap_or("?")
                );
            }
        }
        None => {
            println!("  No local LLM provider running. Start LM Studio or Ollama (or set an API key), then re-run.");
            println!("  Core features need none of this — they work offline.");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_block_is_idempotent() {
        // Inserting the block twice (strip+append) must yield exactly one block.
        let base = "# My Project\n\nSome notes.";
        let once = format!("{}\n\n{}", base, claude_block());
        let stripped = strip_block(&once);
        let twice = format!("{}\n\n{}", stripped.trim_end(), claude_block());
        assert_eq!(twice.matches(CLAUDE_BEGIN).count(), 1, "no duplicate block");
        assert!(twice.contains("# My Project"), "user content preserved");
        // Stripping returns to the original (minus trailing ws).
        assert_eq!(strip_block(&twice).trim(), base);
    }

    #[test]
    fn config_template_is_valid_toml() {
        let cfg: toml::Value = toml::from_str(CONFIG_TEMPLATE).expect("template parses");
        assert_eq!(cfg["llm"]["provider"].as_str(), Some("auto"));
        assert_eq!(cfg["ingest"]["prompted"].as_bool(), Some(true));
    }

    fn tmp_file(name: &str, content: Option<&str>) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cg_init_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.json");
        if let Some(c) = content {
            std::fs::write(&p, c).unwrap();
        }
        p
    }

    /// Invalid JSON must produce a contextual error naming the file — never a
    /// silent `{}` overwrite that destroys the user's config.
    #[test]
    fn invalid_json_errors_and_preserves_file() {
        let p = tmp_file("invalid", Some("{not json"));
        let err = merge_mcp_entry(&p).unwrap_err().to_string();
        assert!(
            err.contains("invalid JSON") && err.contains("config.json"),
            "{err}"
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "{not json",
            "file must be untouched"
        );
    }

    #[test]
    fn null_root_errors() {
        let p = tmp_file("null", Some("null"));
        let err = merge_mcp_entry(&p).unwrap_err().to_string();
        assert!(
            err.contains("null") && err.contains("expected an object"),
            "{err}"
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "null");
    }

    #[test]
    fn array_root_errors() {
        let p = tmp_file("arr", Some("[1,2]"));
        let err = merge_mcp_entry(&p).unwrap_err().to_string();
        assert!(
            err.contains("an array") && err.contains("expected an object"),
            "{err}"
        );
    }

    #[test]
    fn non_object_mcp_servers_errors() {
        let p = tmp_file("badservers", Some(r#"{"mcpServers":[]}"#));
        let err = merge_mcp_entry(&p).unwrap_err().to_string();
        assert!(
            err.contains("`mcpServers`") && err.contains("an array"),
            "{err}"
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), r#"{"mcpServers":[]}"#);
    }

    #[test]
    fn non_object_hooks_errors() {
        let p = tmp_file("badhooks", Some(r#"{"hooks":[]}"#));
        let err = merge_hook_settings(&p).unwrap_err().to_string();
        assert!(err.contains("`hooks`") && err.contains("an array"), "{err}");
    }

    /// A valid config keeps every unrelated field the user had.
    #[test]
    fn valid_config_preserves_unrelated_fields() {
        let p = tmp_file(
            "valid",
            Some(r#"{"theme":"dark","mcpServers":{"other":{"command":"x"}},"numStartups":7}"#),
        );
        merge_mcp_entry(&p).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["numStartups"], 7);
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(v["mcpServers"]["codegraph"]["args"][0], "mcp");
    }

    /// Running twice is byte-identical (idempotent), and a missing file starts
    /// from `{}`.
    #[test]
    fn merge_is_idempotent_and_creates_missing() {
        let p = tmp_file("idem", None);
        merge_mcp_entry(&p).unwrap();
        let first = std::fs::read_to_string(&p).unwrap();
        merge_mcp_entry(&p).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            first,
            "second run must be a no-op"
        );
        let ph = tmp_file("idem_hooks", None);
        merge_hook_settings(&ph).unwrap();
        let h1 = std::fs::read_to_string(&ph).unwrap();
        merge_hook_settings(&ph).unwrap();
        assert_eq!(std::fs::read_to_string(&ph).unwrap(), h1);
    }
}
