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

/// Merge the MCP server entry into `~/.claude.json` (idempotent), or print the
/// snippet when `print_only` (no `~/.claude.json`, or `--print`). Shared by
/// `init` and the back-compat `install` command.
pub fn wire_mcp(repo: &Path, print_only: bool) -> Result<()> {
    let repo = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let entry = serde_json::json!({"command": "codegraph", "args": ["mcp", "--path", repo.to_string_lossy()]});
    let snippet =
        serde_json::to_string_pretty(&serde_json::json!({"mcpServers": {"codegraph": entry.clone()}}))?;
    let home = std::env::var("HOME").unwrap_or_default();
    let path = Path::new(&home).join(".claude.json");
    if print_only || !path.exists() {
        println!("Add this to your agent's MCP config:\n{snippet}");
        return Ok(());
    }
    let mut root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path)?).unwrap_or_else(|_| serde_json::json!({}));
    if !root.is_object() {
        root = serde_json::json!({});
    }
    let obj = root.as_object_mut().unwrap();
    let servers = obj.entry("mcpServers").or_insert_with(|| serde_json::json!({}));
    if let Some(sm) = servers.as_object_mut() {
        sm.insert("codegraph".to_string(), entry);
    }
    let _ = std::fs::copy(&path, path.with_extension("json.bak"));
    std::fs::write(&path, serde_json::to_string_pretty(&root)?)?;
    println!("  → wired MCP into {} (backup .bak written)", path.display());
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
    let mut v: serde_json::Value = std::fs::read_to_string(&settings)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !v.is_object() {
        v = serde_json::json!({});
    }
    let hooks = v.as_object_mut().unwrap().entry("hooks").or_insert_with(|| serde_json::json!({}));
    if let Some(h) = hooks.as_object_mut() {
        h.insert(
            "SessionStart".to_string(),
            serde_json::json!([{"hooks": [{"type": "command", "command": "bash .claude/hooks/codegraph-nudge.sh"}]}]),
        );
    }
    std::fs::create_dir_all(repo.join(".claude"))?;
    std::fs::write(&settings, serde_json::to_string_pretty(&v)?)?;
    println!("  → agent nudge written (CLAUDE.md block + .claude SessionStart hook)");
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
        println!("  → indexed {} files → {} nodes, {} edges", stats.files, stats.nodes, stats.edges);
    }

    // Step 2: MCP wiring (global ~/.claude.json) + agent nudge (local repo files).
    // `--print` only affects the global wiring; the local nudge is always written.
    if !no_mcp && ask("Wire CodeGraph into Claude Code (MCP) + add the agent nudge?", true, interactive) {
        wire_mcp(repo, print)?;
        agent_nudge(repo, false)?;
    }

    // Step 3: optional AI features (report only — never install/block)
    if ask("Enable optional AI features (semantic search / ask / rerank)?", false, interactive) {
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
                println!("  Semantic search needs a separate EMBEDDING model. To enable, run one of:");
                println!("      ollama pull nomic-embed-text");
                println!("      lms get nomic-embed-text-v1.5  (then `lms server start`)");
                println!("  then:  codegraph semantic-index");
            } else {
                println!("  → embedding model ready ({}). Run: codegraph semantic-index", b.embed_model().unwrap_or("?"));
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
}
