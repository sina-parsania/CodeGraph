//! CodeGraph CLI. `codegraph mcp` (M6) is one subcommand among many; the CLI is
//! a real standalone package.

mod configcmd;
mod index;
mod init;
mod query;
mod registry;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use codegraph_core::{Config, LlmClient};

#[derive(Parser)]
#[command(name = "codegraph", version, about = "Project-agnostic code-intelligence graph + MCP server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Don't auto-reindex before a query (serve the current snapshot as-is).
    #[arg(long, global = true)]
    no_autoheal: bool,
}

#[derive(Subcommand)]
enum Command {
    /// First-run setup: index, wire the MCP into Claude Code, add an agent nudge,
    /// and write a commented .codegraph.toml. AI is opt-in; core needs no model.
    Init {
        #[arg(default_value = ".")]
        repo: PathBuf,
        /// Accept every default, no prompts (CI-friendly).
        #[arg(long, short = 'y')]
        yes: bool,
        /// Skip indexing.
        #[arg(long)]
        no_index: bool,
        /// Skip MCP wiring + agent nudge.
        #[arg(long)]
        no_mcp: bool,
        /// Overwrite an existing .codegraph.toml.
        #[arg(long)]
        force: bool,
        /// Print the MCP snippet instead of writing ~/.claude.json.
        #[arg(long)]
        print: bool,
        /// Remove the agent nudge (CLAUDE.md block + SessionStart hook).
        #[arg(long)]
        uninstall: bool,
    },
    /// View or edit configuration (global ~/.config/codegraph/config.toml + project .codegraph.toml).
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
    /// Print version, config defaults, and a readiness check.
    Status,
    /// Index a repository into a local graph (.codegraph/graph.db).
    Index {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Force a full re-index (ignore the sha256 manifest).
        #[arg(long)]
        full: bool,
        /// Merge a compiler-grade SCIP index for Tier-A precise edges.
        /// Defaults to `index.scip` (or any `*.scip`) found at the repo root.
        #[arg(long)]
        scip: Option<PathBuf>,
    },
    /// Full-text search the indexed graph for a term.
    Search {
        term: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Rerank results with a local LLM (if one is running).
        #[arg(long)]
        rerank: bool,
    },
    /// Shortest dependency path between two symbols (by name).
    Trace { from: String, to: String, #[arg(long, default_value = ".")] path: PathBuf },
    /// Impact / blast-radius: what depends on a symbol (reverse reachability).
    Impact {
        name: String,
        #[arg(long, default_value = ".")] path: PathBuf,
        #[arg(long, default_value_t = 5)] depth: usize,
    },
    /// Direct callees (outgoing CALLS) of a symbol.
    Callees { name: String, #[arg(long, default_value = ".")] path: PathBuf },
    /// List detected HTTP routes (NestJS/Express/Flask/Spring patterns).
    Routes {
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    /// List indexed projects + their cache sizes (graphs live in the central cache).
    Projects,
    /// Reclaim disk: delete graphs of projects idle past the TTL
    /// (CODEGRAPH_TTL_DAYS, default 30). Runs opportunistically on every command;
    /// this forces it now.
    Gc {
        /// Idle days before a graph is reclaimed (overrides CODEGRAPH_TTL_DAYS).
        #[arg(long)]
        ttl_days: Option<u64>,
        /// Remove ALL registered graphs regardless of age.
        #[arg(long)]
        all: bool,
        /// Show what would be removed without deleting.
        #[arg(long)]
        dry_run: bool,
    },
    /// Run a READ-ONLY SQL query against the graph (arbitrary analytics).
    Query {
        sql: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// List the largest code communities (clusters) detected in the graph.
    Communities {
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value_t = 12)]
        limit: usize,
    },
    /// Most central symbols by PageRank.
    Important { #[arg(long, default_value = ".")] path: PathBuf, #[arg(long, default_value_t = 15)] limit: usize },
    /// Find types that implement or extend a given interface/class.
    Implementers { name: String, #[arg(long, default_value = ".")] path: PathBuf },
    /// Find functions that call a given function name (reverse CALLS edges).
    Callers {
        name: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    /// Ask a natural-language question; answered by a local LLM over the graph (if one is running).
    Ask {
        question: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    /// Embed all symbols (uses a local embedding model) for semantic search.
    SemanticIndex {
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    /// Semantic (vector) search over embedded symbols.
    Semantic {
        query: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value_t = 15)]
        limit: usize,
        /// HyDE: have the LLM write a hypothetical answer, then embed THAT for search.
        #[arg(long)]
        hyde: bool,
    },
    /// Health check: languages, schema, and local-LLM availability.
    Doctor,
    /// Ingest a PDF, text/markdown file, or web URL as searchable Document nodes.
    Ingest {
        input: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    /// Configure this tool as an MCP server for Claude Code (and print config for others).
    Install {
        /// Only print the config; do not write any files.
        #[arg(long)]
        print: bool,
        /// Repo path the MCP server should index.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Run the MCP server over stdio (for AI agents like Claude Code).
    Mcp {
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Show where config files live (global + project) and which exist.
    Path,
    /// Print a resolved value (e.g. `config get llm.model`).
    Get { key: String },
    /// Set a value; global by default, `--local` writes ./.codegraph.toml.
    Set {
        key: String,
        value: String,
        #[arg(long)]
        local: bool,
    },
    /// Remove a value.
    Unset {
        key: String,
        #[arg(long)]
        local: bool,
    },
    /// Open the config file in $VISUAL/$EDITOR.
    Edit {
        #[arg(long)]
        local: bool,
    },
}

/// Promote resolved config values to the env vars the downstream readers already
/// use (cache_root, detect, ...), so editing config actually takes effect. The
/// user's env is already folded into the resolved Config (env wins), so this is
/// idempotent and preserves precedence.
fn apply_config_env(cfg: &codegraph_core::Config) {
    if let Some(c) = &cfg.cache_dir {
        std::env::set_var("CODEGRAPH_CACHE_DIR", c);
    }
    if let Some(e) = &cfg.embed_model {
        std::env::set_var("CODEGRAPH_EMBED_MODEL", e);
    }
    std::env::set_var("CODEGRAPH_LLM_PROVIDER", &cfg.llm.provider);
    if let Some(u) = &cfg.llm.base_url {
        std::env::set_var("CODEGRAPH_LLM_URL", u);
    }
    std::env::set_var("CODEGRAPH_LLM_MODEL", &cfg.llm.model);
}

/// The project root a command operates on (for TTL bookkeeping), if any.
fn project_path(cmd: &Command) -> Option<PathBuf> {
    use Command::*;
    match cmd {
        Index { path, .. } | Search { path, .. } | Trace { path, .. } | Impact { path, .. }
        | Callees { path, .. } | Routes { path, .. } | Query { path, .. } | Communities { path, .. }
        | Important { path, .. } | Implementers { path, .. } | Callers { path, .. } | Ask { path, .. }
        | SemanticIndex { path, .. } | Semantic { path, .. } | Ingest { path, .. } | Mcp { path, .. } => {
            Some(path.clone())
        }
        Init { repo, .. } => Some(repo.clone()),
        Install { .. } | Status | Doctor | Gc { .. } | Projects | Config { .. } => None,
    }
}

/// Read-only query commands that must see a fresh graph (auto-heal before serving).
fn needs_fresh(cmd: &Command) -> bool {
    use Command::*;
    matches!(
        cmd,
        Search { .. } | Callers { .. } | Callees { .. } | Impact { .. } | Trace { .. }
            | Important { .. } | Communities { .. } | Routes { .. } | Query { .. }
            | Implementers { .. } | Ask { .. } | Semantic { .. }
    )
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cmd = cli.command;
    // Resolve config (defaults < global < project < env) and promote it to the
    // env vars downstream readers use, so config edits actually take effect.
    let cfg = codegraph_core::Config::load(&std::env::current_dir().unwrap_or_default()).unwrap_or_default();
    apply_config_env(&cfg);
    // Opportunistic TTL housekeeping: stamp this project as used + reclaim graphs
    // of projects untouched within CODEGRAPH_TTL_DAYS. Best-effort, never blocks.
    let root = project_path(&cmd);
    let db = root.as_ref().map(|p| index::db_path(p));
    registry::housekeeping(
        root.as_deref()
            .zip(db.as_deref())
            .map(|(r, d)| (r, d, matches!(cmd, Command::Index { .. }))),
    );

    // Freshness gate: reindex before serving so a query never returns a result
    // that disagrees with the working tree (edits / add / delete / git checkout).
    if !cli.no_autoheal && needs_fresh(&cmd) {
        if let Some(r) = &root {
            if let Err(e) = index::ensure_fresh(r) {
                eprintln!("warning: auto-reindex failed ({e}); serving last snapshot");
            }
        }
    }

    match cmd {
        Command::Status => {
            let cfg = Config::load(&std::env::current_dir()?)?;
            let store = codegraph_store::Store::open_in_memory()?;
            println!(
                "codegraph {}  (mcp_ready={}, schema=v{}, media={}, llm_model={})",
                codegraph_core::VERSION,
                codegraph_mcp::mcp_ready(),
                store.schema_version()?,
                cfg.ingest.media_enabled(),
                cfg.llm.model,
            );
        }
        Command::Index { path, full, scip } => {
            let db = index::db_path(&path);
            let stats = index::index_dir(&path, &db, full, scip.as_deref())?;
            println!(
                "indexed {} files ({} changed{}) → {} nodes, {} edges{}  ({})",
                stats.files,
                stats.changed,
                if stats.pruned > 0 { format!(", {} pruned", stats.pruned) } else { String::new() },
                stats.nodes,
                stats.edges,
                if stats.scip_edges > 0 { format!(" (+{} SCIP tier-A)", stats.scip_edges) } else { String::new() },
                db.display()
            );
        }
        Command::Search { term, path, limit, rerank } => {
            let db = index::db_path(&path);
            let store = codegraph_store::Store::open(&db)?;
            let mut hits = store.search_fts(&term, limit)?;
            if rerank || cfg.llm.rerank {
                if let Some(llm) = codegraph_llm::OpenAiCompatBackend::detect() {
                    hits = query::rerank(&term, hits, &llm);
                }
            }
            if hits.is_empty() {
                println!("no matches for {:?}", term);
            }
            for n in hits {
                println!("{:<24} {:?}  {}:{}", n.name, n.label, n.file_path, n.line_start);
            }
        }
        Command::Implementers { name, path } => {
            let store = codegraph_store::Store::open(&index::db_path(&path))?;
            let impls = store.implementers_of(&name)?;
            if impls.is_empty() {
                println!("no implementers/subtypes of {:?}", name);
            }
            for n in impls {
                println!("{:<24} {:?}  {}:{}", n.name, n.label, n.file_path, n.line_start);
            }
        }
        Command::Callers { name, path } => {
            let store = codegraph_store::Store::open(&index::db_path(&path))?;
            let callers = store.callers_of(&name)?;
            if callers.is_empty() {
                println!("no callers of {:?}", name);
            }
            for n in callers {
                println!("{:<24} {:?}  {}:{}", n.name, n.label, n.file_path, n.line_start);
            }
        }
        Command::Trace { from, to, path } => {
            let l = query::Loaded::open(&index::db_path(&path))?;
            match (l.resolve(&from), l.resolve(&to)) {
                (Some(a), Some(b)) => match l.lg.shortest_path(&a.id, &b.id) {
                    Some(p) => {
                        for id in p {
                            println!("{}", l.fmt(&id));
                        }
                    }
                    None => println!("no path from {:?} to {:?}", from, to),
                },
                _ => println!("symbol not found"),
            }
        }
        Command::Impact { name, path, depth } => {
            let l = query::Loaded::open(&index::db_path(&path))?;
            match l.resolve(&name) {
                Some(n) => {
                    let affected = l.lg.blast_radius(&n.id, depth);
                    if affected.is_empty() {
                        println!("nothing depends on {:?}", name);
                    }
                    for id in affected {
                        println!("{}", l.fmt(&id));
                    }
                }
                None => println!("symbol {:?} not found", name),
            }
        }
        Command::Callees { name, path } => {
            let l = query::Loaded::open(&index::db_path(&path))?;
            match l.resolve(&name) {
                Some(n) => {
                    for id in l.lg.callees(&n.id) {
                        println!("{}", l.fmt(&id));
                    }
                }
                None => println!("symbol {:?} not found", name),
            }
        }
        Command::Routes { path } => {
            let store = codegraph_store::Store::open(&index::db_path(&path))?;
            let mut routes = store.nodes_by_label("Route")?;
            routes.sort_by(|a, b| a.name.cmp(&b.name));
            if routes.is_empty() {
                println!("no routes detected");
            }
            for n in routes {
                println!("{:<28} {}:{}", n.name, n.file_path, n.line_start);
            }
        }
        Command::Config { action } => {
            let cwd = std::env::current_dir()?;
            match action {
                None => configcmd::view(&cwd)?,
                Some(ConfigAction::Path) => configcmd::path()?,
                Some(ConfigAction::Get { key }) => configcmd::get(&cwd, &key)?,
                Some(ConfigAction::Set { key, value, local }) => configcmd::set(&cwd, &key, &value, local)?,
                Some(ConfigAction::Unset { key, local }) => configcmd::unset(&cwd, &key, local)?,
                Some(ConfigAction::Edit { local }) => configcmd::edit(&cwd, local)?,
            }
        }
        Command::Projects => {
            let projects = registry::list_projects();
            if projects.is_empty() {
                println!("no indexed projects yet — run `codegraph index <dir>`");
            }
            for p in projects {
                let age = if p.idle_secs < 3600 {
                    format!("{}m", p.idle_secs / 60)
                } else if p.idle_secs < 86_400 {
                    format!("{}h", p.idle_secs / 3600)
                } else {
                    format!("{}d", p.idle_secs / 86_400)
                };
                let size = if p.exists { registry::human_bytes(p.bytes) } else { "(missing)".to_string() };
                println!("{:>10}  idle {:>4}  {}", size, age, p.root);
            }
        }
        Command::Gc { ttl_days, all, dry_run } => {
            let ttl = ttl_days.map(|d| d.saturating_mul(86_400));
            let report = registry::run_gc(ttl, all, dry_run);
            if report.removed.is_empty() {
                println!("nothing to reclaim — all indexed graphs are within the TTL");
            } else {
                let verb = if dry_run { "would free" } else { "freed" };
                println!(
                    "{} {} graph(s), {}{}",
                    verb,
                    report.removed.len(),
                    registry::human_bytes(report.freed_bytes),
                    if dry_run { " (dry-run)" } else { "" }
                );
                for (root, bytes) in &report.removed {
                    println!("  {}  ({})", root, registry::human_bytes(*bytes));
                }
            }
        }
        Command::Query { sql, path, limit } => {
            let db = index::db_path(&path);
            let (cols, rows) = codegraph_store::query_readonly(&db, &sql, limit)?;
            println!("{}", cols.join(" | "));
            for row in rows {
                println!("{}", row.join(" | "));
            }
        }
        Command::Communities { path, limit } => {
            use std::collections::BTreeMap;
            let store = codegraph_store::Store::open(&index::db_path(&path))?;
            let nodes = store.all_nodes()?;
            let mut by: BTreeMap<u32, Vec<&codegraph_core::Node>> = BTreeMap::new();
            for n in &nodes {
                if n.label == codegraph_core::NodeLabel::File {
                    continue;
                }
                if let Some(c) = n.community {
                    by.entry(c).or_default().push(n);
                }
            }
            let mut comms: Vec<_> = by.into_iter().collect();
            comms.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
            for (c, members) in comms.into_iter().take(limit) {
                let mut names: Vec<&str> = members.iter().map(|n| n.name.as_str()).collect();
                names.sort();
                names.dedup();
                let sample: Vec<&str> = names.into_iter().take(8).collect();
                println!("community {:<3} ({} symbols): {}", c, members.len(), sample.join(", "));
            }
        }
        Command::Important { path, limit } => {
            let l = query::Loaded::open(&index::db_path(&path))?;
            for (id, score) in l.lg.pagerank_top(limit) {
                println!("{:.4}  {}", score, l.fmt(&id));
            }
        }
        Command::Ask { question, path } => {
            let store = codegraph_store::Store::open(&index::db_path(&path))?;
            let fq = query::fts_query_from(&question);
            let hits = if fq.is_empty() { Vec::new() } else { store.search_fts(&fq, 8)? };
            let mut context = String::new();
            for n in hits.iter().take(6) {
                context.push_str(&format!("### {} ({:?}) - {}:{}\n", n.name, n.label, n.file_path, n.line_start));
                if let Some(snip) = query::read_snippet(&path, &n.file_path, n.line_start, n.line_end) {
                    context.push_str(&format!("```\n{}\n```\n", snip));
                }
            }
            match codegraph_llm::OpenAiCompatBackend::detect() {
                Some(llm) => {
                    let prompt = format!(
                        "You are a code assistant answering questions about a codebase using its symbol graph. \
                         Use ONLY the context below; if it is insufficient, say so. Be concise.\n\n\
                         Context (relevant symbols):\n{}\n\nQuestion: {}\n\nAnswer:",
                        context, question
                    );
                    match llm.generate(&prompt, 600) {
                        Some(ans) => println!("{}\n\n[{} / {}]", ans.trim(), llm.provider(), llm.model()),
                        None => println!("LLM request failed. Relevant symbols:\n{}", context),
                    }
                }
                None => println!(
                    "No local LLM detected (start LM Studio or Ollama, or set CODEGRAPH_LLM_BASE_URL).\n\nRelevant symbols:\n{}",
                    context
                ),
            }
        }
        Command::SemanticIndex { path } => {
            let store = codegraph_store::Store::open(&index::db_path(&path))?;
            match codegraph_llm::OpenAiCompatBackend::detect().filter(|b| b.embed_model().is_some()) {
                Some(b) => {
                    let nodes = store.all_nodes()?;
                    let mut n = 0usize;
                    for node in &nodes {
                        if node.label == codegraph_core::NodeLabel::File {
                            continue;
                        }
                        let text = format!("{} {:?} in {}", node.name, node.label, node.file_path);
                        if let Some(v) = b.embed(&text) {
                            store.upsert_vector(&node.id, &v)?;
                            n += 1;
                        }
                    }
                    println!("embedded {} symbols using {}", n, b.embed_model().unwrap_or("?"));
                }
                None => println!("no embedding model loaded - load one (LM Studio: `lms load <embed-model>`; Ollama: `ollama pull nomic-embed-text`)"),
            }
        }
        Command::Semantic { query: q, path, limit, hyde } => {
            let store = codegraph_store::Store::open(&index::db_path(&path))?;
            let Some(b) = codegraph_llm::OpenAiCompatBackend::detect().filter(|b| b.embed_model().is_some()) else {
                println!("no embedding model available (load one in LM Studio / Ollama)");
                return Ok(());
            };
            let query_text = if hyde || cfg.llm.hyde {
                b.generate(&format!("Write a short code documentation snippet that would answer this query (no preamble): {}", q), 200)
                    .unwrap_or_else(|| q.clone())
            } else {
                q.clone()
            };
            let Some(qv) = b.embed(&query_text) else {
                println!("embedding request failed - is an embedding model LOADED? (LM Studio: lms load <embed-model>; only downloaded != loaded)");
                return Ok(());
            };
            let qv = codegraph_core::normalize(&qv);
            let vectors = store.all_vectors()?;
            if vectors.is_empty() {
                println!("no vectors yet - run `codegraph semantic-index` first");
                return Ok(());
            }
            // Stored vectors are L2-normalized, so dot == cosine (cheaper).
            let mut scored: Vec<(f32, String)> =
                vectors.iter().map(|(id, v)| (codegraph_core::dot(&qv, v), id.clone())).collect();
            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            scored.truncate(limit);
            for (score, id) in scored {
                if let Some(n) = store.get_node(&id)? {
                    println!("{:.3}  {:<22} {:?}  {}:{}", score, n.name, n.label, n.file_path, n.line_start);
                }
            }
        }
        Command::Doctor => {
            println!("codegraph {}", codegraph_core::VERSION);
            println!("languages:  13 (rust, python, js, ts, go, swift, kotlin, java, c, c++, ruby, c#, bash)");
            println!("core graph + search:  ✓ always available offline (no model needed)");
            let backend = codegraph_llm::OpenAiCompatBackend::detect();
            match &backend {
                Some(llm) => {
                    println!("chat model (ask/rerank/HyDE):  ✓ {} / {}", llm.provider(), llm.model());
                    match llm.embed_model() {
                        Some(m) => println!("embedding model (semantic):    ✓ {m}  — run `codegraph semantic-index`"),
                        None => {
                            println!("embedding model (semantic):    ✗ none — `ollama pull nomic-embed-text` (or `lms get`), then `codegraph semantic-index`");
                        }
                    }
                }
                None => {
                    println!("chat model (ask/rerank/HyDE):  ✗ no local provider (start LM Studio/Ollama, or set an API key)");
                    println!("embedding model (semantic):    ✗ none");
                }
            }
            #[cfg(feature = "local-embed")]
            println!("local embeddings:  ✓ compiled in (--features local-embed)");
            println!("\nsetup:  codegraph init   |   config: .codegraph.toml (env CODEGRAPH_* overrides)");
        }
        Command::Ingest { input, path } => {
            let chunks = codegraph_ingest::ingest(&input).map_err(anyhow::Error::msg)?;
            let store = codegraph_store::Store::open(&index::db_path(&path))?;
            for (i, ch) in chunks.iter().enumerate() {
                store.upsert_node(&index::document_node_from_chunk(ch, i))?;
            }
            store.rebuild_fts()?;
            println!("ingested {} chunk(s) from {} as Document nodes (searchable by title; semantic over content)", chunks.len(), input);
        }
        Command::Init { repo, yes, no_index, no_mcp, force, print, uninstall } => {
            init::run_init(&repo, yes, no_index, no_mcp, force, print, uninstall)?;
        }
        Command::Install { print, repo } => {
            // Back-compat thin alias: just the MCP wiring (init does the full setup).
            init::wire_mcp(&repo, print)?;
            println!("(tip: `codegraph init` also indexes + adds an agent nudge.)");
        }
        Command::Mcp { path } => {
            let db = index::db_path(&path);
            let refresh = if cli.no_autoheal { None } else { Some(index::ensure_fresh as fn(&std::path::Path) -> anyhow::Result<()>) };
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(codegraph_mcp::serve_stdio(path, db, refresh))?;
        }
    }
    Ok(())
}
