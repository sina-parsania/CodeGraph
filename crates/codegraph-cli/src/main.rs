//! CodeGraph CLI. `codegraph mcp` (M6) is one subcommand among many; the CLI is
//! a real standalone package.

mod audit;
mod configcmd;
mod index;
mod init;
mod query;
mod registry;
mod tsconfig;
mod viz;
mod scipcmd;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use codegraph_core::Config;

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
    /// One-command compiler-grade precision: run the project's SCIP indexer (if installed) and merge.
    Scip {
        #[arg(default_value = ".")]
        path: PathBuf,
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
        /// Merge Apple's IndexStore (Swift compiler-grade calls) from the most
        /// recently built DerivedData. Needs `--features indexstore` (macOS + Xcode).
        #[arg(long)]
        indexstore: bool,
        /// ALSO emit edges for unresolved calls with 2-5 same-name candidates,
        /// tagged Confidence::Ambiguous (honest opt-in recall; sticky until
        /// --no-include-ambiguous). Coverage and the default graph stay precise.
        #[arg(long)]
        include_ambiguous: bool,
        /// Turn the sticky ambiguous tier back off.
        #[arg(long, conflicts_with = "include_ambiguous")]
        no_include_ambiguous: bool,
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
        /// Treat the term as a REGEX matched against symbol names (middle
        /// fragments, alternations, anchors) instead of full-text search.
        #[arg(long)]
        regex: bool,
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
    /// Cypher-lite graph query (read-only openCypher subset): 1-2 hop MATCH with
    /// labels/relations, WHERE (=/CONTAINS/STARTS WITH/AND), RETURN props, LIMIT.
    Cypher {
        query: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
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
    /// MEASURED per-tier precision: verify sampled tree-sitter CALLS edges
    /// against the compiler-grade oracle merged into this graph (SCIP/IndexStore).
    Audit {
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Number of tree-sitter edges to sample.
        #[arg(long, default_value_t = 200)]
        sample: usize,
        /// Sampling seed (same seed = same sample, reproducible).
        #[arg(long, default_value_t = 0)]
        seed: u64,
        /// Emit the JSON stored in meta instead of the table.
        #[arg(long)]
        json: bool,
    },
    /// Deterministic Markdown report: overview, resolution quality, central symbols,
    /// hotspots + test gaps, communities, flows, API surface, dead code, health.
    Report {
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Write to a file instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Self-contained interactive HTML visualization of the graph (force layout,
    /// search, communities, table view). No external requests — works offline.
    Html {
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Output file (default: graph.html next to the cached graph.db, so the repo stays pristine).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Max symbols shown (top by PageRank).
        #[arg(long, default_value_t = 300)]
        limit: usize,
        /// Open in the default browser after writing.
        #[arg(long)]
        open: bool,
    },
    /// Select the most relevant symbols for a query, ranked by personalized
    /// PageRank over the RESOLVED graph, within a token budget (for LLM context).
    Context {
        query: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value_t = 1000)]
        budget: usize,
    },
    /// Dead-code CANDIDATES: functions/methods no call site in the repo even names
    /// (excludes entry points, route handlers, test files). Candidates, not verdicts.
    DeadCode {
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Change-aware review: map the git diff against a base to affected symbols,
    /// with fan-in, complexity, test-gap flags, a risk tier, and co-change hints.
    #[command(visible_alias = "review")]
    Changes {
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Base to diff against (any git ref).
        #[arg(long, default_value = "HEAD")]
        base: String,
        /// Emit a markdown report (for PR comments / CI).
        #[arg(long)]
        md: bool,
    },
    /// Execution flows: call chains from ENTRY POINTS (route handlers, main,
    /// zero-fan-in hubs), ranked by criticality (reach × centrality).
    Flows {
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Prove the determinism guarantee: index the repo twice into fresh temp
    /// caches and compare canonical graph hashes (must be byte-identical).
    VerifyDeterminism {
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    /// Export the graph as a shareable zstd artifact (.codegraph/graph.db.zst).
    /// Deterministic graphs make this safe to commit: teammates `import` and skip
    /// the full reindex (incremental heals any drift).
    Export {
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Output file (default <repo>/.codegraph/graph.db.zst).
        #[arg(long, short = 'o')]
        out: Option<PathBuf>,
    },
    /// Import a graph artifact produced by `export` into the local cache.
    Import {
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Artifact file (default <repo>/.codegraph/graph.db.zst).
        file: Option<PathBuf>,
    },
    /// Rename a symbol + all its RESOLVED references. Safe: refuses unless every
    /// occurrence of the name in each affected file is accounted for by a resolved
    /// reference (else it could corrupt code). Dry-run diff by default; --write applies.
    RenameSymbol {
        name: String,
        new_name: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        write: bool,
    },
    /// Find types that implement or extend a given interface/class.
    Implementers { name: String, #[arg(long, default_value = ".")] path: PathBuf },
    /// Find functions that call a given function name (reverse CALLS edges).
    /// Ambiguous name (several definitions) → lists pinnable candidates instead
    /// of silently merging their callers.
    Callers {
        name: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Pin ONE definition by its node id (from the candidate list).
        #[arg(long)]
        id: Option<String>,
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
/// A planned per-file rename: (relative path, current source, identifier spans
/// `(byte_start, byte_end, line)` to rewrite).
type RenameFilePlan = (String, String, Vec<(usize, usize, u32)>);

/// Print a one-line coverage signal under a call-graph result so the agent (or
/// human) knows when the precise list may be incomplete and should grep instead.
fn print_coverage(c: &codegraph_core::Coverage) {
    let mark = if c.may_be_incomplete { "⚠" } else { "✓" };
    println!("{mark} {}", c.note);
}

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
        | SemanticIndex { path, .. } | Semantic { path, .. } | Ingest { path, .. } | Mcp { path, .. }
        | Context { path, .. } | RenameSymbol { path, .. } | DeadCode { path, .. }
        | Changes { path, .. } | Export { path, .. } | Import { path, .. } | Flows { path, .. }
        | Cypher { path, .. } | VerifyDeterminism { path, .. }
        | Report { path, .. } | Html { path, .. } | Audit { path, .. } => Some(path.clone()),
        Init { repo, .. } | Scip { path: repo } => Some(repo.clone()),
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
            | Implementers { .. } | Ask { .. } | Semantic { .. } | Context { .. } | RenameSymbol { .. }
            | DeadCode { .. } | Changes { .. } | Flows { .. } | Cypher { .. }
            | Report { .. } | Html { .. } | Audit { .. }
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

    // Identity gate: never serve (or index over) a graph that belongs to a
    // different repo or a foreign tool. One choke point for every command.
    if let (Some(r), Some(d)) = (&root, &db) {
        index::check_identity(r, d)?;
    }

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
        Command::Index { path, full, scip, indexstore, include_ambiguous, no_include_ambiguous } => {
            let db = index::db_path(&path);
            let ambiguous = if include_ambiguous {
                Some(true)
            } else if no_include_ambiguous {
                Some(false)
            } else {
                None
            };
            let stats = index::index_dir(&path, &db, full, scip.as_deref(), indexstore, ambiguous)?;
            println!(
                "indexed {} files ({} changed{}{}) → {} nodes, {} edges{}  ({})",
                stats.files,
                stats.changed,
                if stats.pruned > 0 { format!(", {} pruned", stats.pruned) } else { String::new() },
                if stats.partial { ", partial edge rebuild" } else { "" },
                stats.nodes,
                stats.edges,
                if stats.scip_edges > 0 { format!(" (+{} SCIP tier-A)", stats.scip_edges) } else { String::new() },
                db.display()
            );
        }
        Command::Report { path, out } => {
            let md = viz::report(&path, &index::db_path(&path))?;
            match out {
                Some(f) => {
                    std::fs::write(&f, md)?;
                    println!("report written to {}", f.display());
                }
                None => print!("{md}"),
            }
        }
        Command::Html { path, out, limit, open } => {
            let db = index::db_path(&path);
            let html = viz::html(&path, &db, limit)?;
            // Default next to the cached graph.db — the source repo stays pristine.
            let out = out.unwrap_or_else(|| db.with_file_name("graph.html"));
            std::fs::write(&out, html)?;
            println!("interactive graph written to {}", out.display());
            if open {
                #[cfg(target_os = "macos")]
                let opener = "open";
                #[cfg(all(unix, not(target_os = "macos")))]
                let opener = "xdg-open";
                #[cfg(windows)]
                let opener = "explorer";
                let _ = std::process::Command::new(opener).arg(&out).spawn();
            }
        }
        Command::Search { term, path, limit, rerank, regex } => {
            let db = index::db_path(&path);
            let store = codegraph_store::Store::open(&db)?;
            let mut hits =
                if regex { store.search_regex(&term, limit)? } else { store.search_smart(&term, limit)? };
            if rerank || cfg.llm.rerank {
                hits = query::rerank(&term, hits);
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
        Command::Callers { name, path, id } => {
            let store = codegraph_store::Store::open(&index::db_path(&path))?;
            if let Some(pin) = id {
                // Pinned: callers of exactly ONE definition, never a same-name union.
                for n in store.callers_of_id(&pin)? {
                    println!("{:<24} {:?}  {}:{}", n.name, n.label, n.file_path, n.line_start);
                }
                return Ok(());
            }
            let defs = store.definitions_of(&name)?;
            if defs.len() > 1 {
                println!("⚠ {:?} has {} definitions — callers are grouped per definition (rivals silently merge these). Pin one with --id <id>:
", name, defs.len());
                for (d, nc) in defs.iter().take(15) {
                    println!("  [{nc:>3} callers]  {}:{}  id={}", d.file_path, d.line_start, d.id);
                }
                print_coverage(&store.coverage_for_callers(&name)?);
                return Ok(());
            }
            let callers = store.callers_of(&name)?;
            if callers.is_empty() {
                println!("no callers of {:?}", name);
            }
            for n in callers {
                println!("{:<24} {:?}  {}:{}", n.name, n.label, n.file_path, n.line_start);
            }
            print_coverage(&store.coverage_for_callers(&name)?);
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
                    // Impact is built from inbound Calls edges, so it inherits the
                    // incompleteness of the direct callers (transitively more so).
                    let store = codegraph_store::Store::open(&index::db_path(&path))?;
                    print_coverage(&store.coverage_for_callers(&n.name)?);
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
                    let store = codegraph_store::Store::open(&index::db_path(&path))?;
                    print_coverage(&store.coverage_for_callees(&n.id)?);
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
        Command::Scip { path } => {
            scipcmd::run(&path)?;
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
        Command::Cypher { query: cq, path } => {
            let sql = match codegraph_store::cypher::to_sql(&cq) {
                Ok(s) => s,
                Err(e) => {
                    println!("cypher-lite: {e}");
                    return Ok(());
                }
            };
            let db = index::db_path(&path);
            let (cols, rows) = codegraph_store::query_readonly(&db, &sql, 500)?;
            println!("{}", cols.join(" | "));
            for row in rows {
                println!("{}", row.join(" | "));
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
        Command::Context { query, path, budget } => {
            let db = index::db_path(&path);
            let store = codegraph_store::Store::open(&db)?;
            // Seed with the query's lexical hits (canonical OR-of-tokens FTS query),
            // then rank the whole resolved graph by personalized PageRank restarted
            // at those seeds.
            use codegraph_core::NodeLabel::*;
            let is_code = |l: codegraph_core::NodeLabel| matches!(l, Function | Method | Class | Interface | Enum | Type);
            let seeds: Vec<String> = store
                .search_fts(&query::fts_query_from(&query), 40)
                .unwrap_or_default()
                .into_iter()
                .filter(|n| is_code(n.label))
                .map(|n| n.id)
                .take(12)
                .collect();
            let l = query::Loaded::open(&db)?;
            let ranked = l.lg.personalized_pagerank_top(&seeds, 200);
            // Emit DIRECT seed matches first (so context never loses name-match
            // recall), then the top graph-expanded neighbors → context = the
            // name-matched symbols ∪ their call-graph neighborhood. Budget in ≈chars/4.
            let scored: std::collections::HashMap<&str, f64> =
                ranked.iter().map(|(id, s)| (id.as_str(), *s)).collect();
            let seen_seed: std::collections::HashSet<&str> = seeds.iter().map(String::as_str).collect();
            let order = seeds
                .iter()
                .map(|id| (id.clone(), *scored.get(id.as_str()).unwrap_or(&0.0), true))
                .chain(ranked.iter().filter(|(id, _)| !seen_seed.contains(id.as_str())).map(|(id, s)| (id.clone(), *s, false)));
            let mut used = 0usize;
            let mut shown = 0usize;
            println!("# context for {:?} (budget {} tok)", query, budget);
            for (id, score, is_seed) in order {
                let label = l.nodes.iter().find(|n| n.id == id).map(|n| n.label);
                if !matches!(label, Some(lbl) if is_code(lbl)) {
                    continue; // code symbols only — not File/Document/Route nodes
                }
                let line = l.fmt(&id);
                let cost = line.len() / 4 + 1;
                if used + cost > budget {
                    break;
                }
                used += cost;
                shown += 1;
                println!("{} {:.4}  {}", if is_seed { "*" } else { " " }, score, line);
            }
            println!("# {} symbols, ~{} tokens", shown, used);
        }
        Command::DeadCode { path, limit } => {
            let store = codegraph_store::Store::open(&index::db_path(&path))?;
            let dead = store.dead_code_candidates(limit)?;
            if dead.is_empty() {
                println!("no dead-code candidates found");
            } else {
                println!("# {} candidate(s) — no call site in the repo even NAMES these (static view; dynamic dispatch/exports/reflection excluded):", dead.len());
                for n in dead {
                    println!("{:<28} {:?}  {}:{}", n.name, n.label, n.file_path, n.line_start);
                }
            }
        }
        Command::Changes { path, base, md } => {
            let store = codegraph_store::Store::open(&index::db_path(&path))?;
            let out = std::process::Command::new("git")
                .args(["-C", &path.to_string_lossy(), "diff", "--name-only", &base])
                .output()?;
            let changed: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::to_string)
                .filter(|f| !f.is_empty())
                .collect();
            if changed.is_empty() {
                println!("no changes vs {base}");
                return Ok(());
            }
            let mut rows: Vec<(f64, usize, u64, bool, codegraph_core::Node)> = Vec::new();
            for f in &changed {
                for sym in store.symbols_in_file(f)? {
                    let fan_in = store.call_site_count(&sym.name)?;
                    let tested = store.has_test_reference(&sym.name)?;
                    let cx = sym.metadata.get("complexity").and_then(|v| v.as_u64()).unwrap_or(1);
                    // Multiplicative risk (no crg-style flat keyword bumps): reach ×
                    // intrinsic complexity × untested penalty. Resolved-data only.
                    let risk = (1.0 + fan_in as f64).ln() * (1.0 + cx as f64 / 10.0) * if tested { 1.0 } else { 2.0 };
                    rows.push((risk, fan_in, cx, tested, sym));
                }
            }
            rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            let tier = |r: f64| if r >= 6.0 { "HIGH" } else if r >= 2.5 { "MED " } else { "low " };
            if md {
                println!("<!-- codegraph-review -->");
                println!("## CodeGraph review — {} file(s) vs `{base}`\n", changed.len());
                println!("| risk | symbol | fan-in | cx | tests | location |");
                println!("|------|--------|-------:|---:|-------|----------|");
                for (risk, fan_in, cx, tested, sym) in rows.iter().take(30) {
                    println!(
                        "| {} {:.1} | `{}` | {} | {} | {} | `{}:{}` |",
                        tier(*risk).trim(), risk, sym.name, fan_in, cx,
                        if *tested { "✓" } else { "**none**" }, sym.file_path, sym.line_start
                    );
                }
            } else {
                println!("# changes vs {base}: {} file(s)\n", changed.len());
                for (risk, fan_in, cx, tested, sym) in rows.iter().take(40) {
                    println!(
                        "{} {:>5.1}  {:<26} fan-in={:<4} cx={:<3} {}  {}:{}",
                        tier(*risk), risk, sym.name, fan_in, cx,
                        if *tested { "tested" } else { "NO-TESTS" }, sym.file_path, sym.line_start
                    );
                }
            }
            // co-change hints: files that usually change with these but aren't in the diff
            let mut hints: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
            for f in &changed {
                for (other, n) in store.cochanges_for(f, 5)? {
                    if n >= 3 && !changed.contains(&other) {
                        let e = hints.entry(other).or_insert(0);
                        *e = (*e).max(n);
                    }
                }
            }
            if !hints.is_empty() {
                println!("\n# usually change together with this diff (not in it):");
                let mut hv: Vec<_> = hints.into_iter().collect();
                hv.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
                for (f, n) in hv.into_iter().take(8) {
                    println!("  {f}  (co-changed {n}×)");
                }
            }
        }
        Command::Flows { path, limit } => {
            let l = query::Loaded::open(&index::db_path(&path))?;
            let entries = codegraph_graph::detect_entry_points(&l.nodes);
            let mut flows: Vec<(f64, &codegraph_core::Node, Vec<String>, &str)> = entries
                .iter()
                .map(|(n, kind)| {
                    let body = l.lg.flow_from(&n.id, 6);
                    let crit: f64 = body
                        .iter()
                        .filter_map(|id| l.nodes.iter().find(|x| x.id == *id))
                        .map(|x| x.pagerank)
                        .sum::<f64>()
                        * (1.0 + body.len() as f64).ln();
                    (crit, *n, body, *kind)
                })
                .filter(|(_, _, body, _)| !body.is_empty())
                .collect();
            flows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            println!("# {} entry points, top {} flows by criticality:\n", entries.len(), limit);
            for (crit, entry, body, kind) in flows.iter().take(limit) {
                println!(
                    "[{kind:<6}] {:<44} reach={:<4} crit={:.4}",
                    codegraph_core::display_label(entry), body.len(), crit
                );
                for id in body.iter().take(5) {
                    if let Some(x) = l.nodes.iter().find(|n| n.id == *id) {
                        println!("    → {}", codegraph_core::display_label(x));
                    }
                }
            }
        }
        Command::VerifyDeterminism { path } => {
            let tmp = std::env::temp_dir().join(format!("cg-verify-{}", std::process::id()));
            let mut hashes = Vec::new();
            for run in 1..=2u8 {
                let db = tmp.join(format!("run{run}")).join("graph.db");
                index::index_dir(&path, &db, true, None, false, Some(false))?;
                let store = codegraph_store::Store::open(&db)?;
                hashes.push(store.canonical_hash()?);
            }
            let _ = std::fs::remove_dir_all(&tmp);
            if hashes[0] == hashes[1] {
                println!("✓ deterministic: two full indexes produced byte-identical graphs ({})", &hashes[0][..16]);
            } else {
                println!("✗ NON-DETERMINISTIC: {} vs {}", &hashes[0][..16], &hashes[1][..16]);
                std::process::exit(1);
            }
        }
        Command::Export { path, out } => {
            let db = index::db_path(&path);
            let bytes = std::fs::read(&db)?;
            let compressed = zstd::encode_all(&bytes[..], 9)?;
            let dest = out.unwrap_or_else(|| path.join(".codegraph/graph.db.zst"));
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, &compressed)?;
            println!(
                "exported {} -> {} ({:.1} MB -> {:.1} MB, {:.0}% smaller)",
                db.display(),
                dest.display(),
                bytes.len() as f64 / 1e6,
                compressed.len() as f64 / 1e6,
                100.0 * (1.0 - compressed.len() as f64 / bytes.len() as f64)
            );
        }
        Command::Import { path, file } => {
            let src = file.unwrap_or_else(|| path.join(".codegraph/graph.db.zst"));
            let compressed = std::fs::read(&src)?;
            let bytes = zstd::decode_all(&compressed[..])?;
            let db = index::db_path(&path);
            if let Some(parent) = db.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&db, &bytes)?;
            // Sanity-open (runs migrations) then heal any drift incrementally.
            let _ = codegraph_store::Store::open(&db)?;
            index::ensure_fresh(&path)?;
            println!("imported {} -> {} (graph live; drift healed incrementally)", src.display(), db.display());
        }
        Command::RenameSymbol { name, new_name, path, write } => {
            use codegraph_core::NodeLabel::*;
            let store = codegraph_store::Store::open(&index::db_path(&path))?;
            // Gate 1 — the name must denote exactly ONE code definition.
            let defs: Vec<_> = store
                .find_by_name(&name)?
                .into_iter()
                .filter(|n| matches!(n.label, Function | Method | Class | Interface | Enum | Type))
                .collect();
            if defs.len() != 1 {
                println!("✗ refused: {:?} names {} code definitions — ambiguous, cannot rename safely.", name, defs.len());
                return Ok(());
            }
            // Gate 2 — every call to the name must already resolve (no dropped sites).
            let cov = store.coverage_for_callers(&name)?;
            if cov.may_be_incomplete {
                println!("✗ refused: {} call site(s) naming {:?} are unresolved — a rename could miss them and break code.\n  {}", cov.dropped, name, cov.note);
                return Ok(());
            }
            // Gate 3 (occurrence-completeness) — scan EVERY indexed file (not just
            // graph-known callers, so a call form the parser MISSED can't slip
            // through): each identifier token named `name` in a file must be
            // accounted for by the def + that file's resolved call sites, else REFUSE.
            let def = &defs[0];
            let call_counts = store.call_sites_by_file(&name)?;
            let mut plans: Vec<RenameFilePlan> = Vec::new();
            let mut unaccounted: Vec<String> = Vec::new();
            for f in store.indexed_files()? {
                let Ok(src) = std::fs::read_to_string(path.join(&f)) else { continue };
                if !src.contains(name.as_str()) {
                    continue;
                }
                let spans = codegraph_parse::identifier_spans(&f, &src, &name);
                if spans.is_empty() {
                    continue;
                }
                let expected = usize::from(def.file_path == f) + call_counts.get(&f).copied().unwrap_or(0);
                if spans.len() == expected {
                    plans.push((f.clone(), src, spans));
                } else {
                    unaccounted.push(format!("{f}: {} occurrences vs {expected} resolved references", spans.len()));
                }
            }
            if !unaccounted.is_empty() {
                println!("✗ refused: some occurrences of {:?} are NOT accounted for by resolved references", name);
                println!("  (could be a local/shadow/type-use of the same name — renaming would risk corruption):");
                for u in &unaccounted {
                    println!("    {u}");
                }
                return Ok(());
            }
            // Apply (byte ranges, right-to-left) — dry-run diff unless --write.
            let total: usize = plans.iter().map(|(_, _, s)| s.len()).sum();
            println!("{} rename {:?} → {:?}: {} occurrence(s) across {} file(s)",
                if write { "✓ APPLIED" } else { "DRY-RUN" }, name, new_name, total, plans.len());
            for (f, src, spans) in &plans {
                let mut new_src = src.clone();
                for &(s, e, _) in spans.iter().rev() {
                    new_src.replace_range(s..e, &new_name);
                }
                let lines: Vec<u32> = spans.iter().map(|&(_, _, l)| l).collect();
                println!("  {f}  (lines {:?})", lines);
                if write {
                    std::fs::write(path.join(f), new_src)?;
                }
            }
            if !write {
                println!("  re-run with --write to apply.");
            }
        }
        Command::Important { path, limit } => {
            let l = query::Loaded::open(&index::db_path(&path))?;
            for (id, score) in l.lg.important(limit, &l.nodes) {
                println!("{:.4}  {}", score, l.fmt(&id));
            }
        }
        Command::Audit { path, sample, seed, json } => {
            audit::run(&path, sample, seed, json)?;
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
            let prompt = format!(
                "You are a code assistant answering questions about a codebase using its symbol graph. \
                 Use ONLY the context below; if it is insufficient, say so. Be concise.\n\n\
                 Context (relevant symbols):\n{}\n\nQuestion: {}\n\nAnswer:",
                context, question
            );
            match codegraph_llm::generate_text_labeled(&prompt, 600) {
                Some((ans, label)) => println!("{}\n\n[{}]", ans.trim(), label),
                None => println!(
                    "No LLM available (start MLX/LM Studio/Ollama, set an API key, or install a build with --features local-llm).\n\nRelevant symbols:\n{}",
                    context
                ),
            }
        }
        Command::SemanticIndex { path } => {
            let store = codegraph_store::Store::open(&index::db_path(&path))?;
            let nodes = store.all_nodes()?;
            let items: Vec<(&codegraph_core::Node, String)> = nodes
                .iter()
                .filter(|n| n.label != codegraph_core::NodeLabel::File)
                .map(|n| (n, index::embed_text_for(n)))
                .collect();
            let texts: Vec<String> = items.iter().map(|(_, t)| t.clone()).collect();
            match codegraph_llm::embed_texts(&texts) {
                Some((vecs, model)) if !vecs.is_empty() => {
                    let rows: Vec<(String, Vec<f32>)> =
                        items.iter().zip(vecs).map(|((node, _), v)| (node.id.clone(), v)).collect();
                    store.upsert_vectors(&rows)?;
                    store.meta_set("embed_model", &model)?;
                    println!("embedded {} symbols using {}", rows.len(), model);
                }
                _ => println!(
                    "no embedder available — rebuild with `--features local-embed` for a bundled model (bge-small, no server), or load one in LM Studio / Ollama"
                ),
            }
        }
        Command::Semantic { query: q, path, limit, hyde } => {
            let store = codegraph_store::Store::open(&index::db_path(&path))?;
            // HyDE rewrites the query via a chat model (server or bundled); skip if none.
            let query_text = if hyde || cfg.llm.hyde {
                codegraph_llm::generate_text(
                    &format!("Write a short code documentation snippet that would answer this query (no preamble): {q}"),
                    200,
                )
                .unwrap_or_else(|| q.clone())
            } else {
                q.clone()
            };
            let Some((qvs, qmodel)) = codegraph_llm::embed_texts(&[query_text]) else {
                println!(
                    "no embedder available — rebuild with `--features local-embed` (bundled bge-small, no server) or load a model in LM Studio / Ollama"
                );
                return Ok(());
            };
            let Some(qv) = qvs.into_iter().next() else { return Ok(()) };
            if let Some(stamped) = store.meta_get("embed_model")? {
                if stamped != qmodel {
                    println!("index was embedded with {stamped:?} but the query used {qmodel:?} — cosine would be garbage.\nRe-run `codegraph semantic-index` (same CODEGRAPH_LOCAL_EMBED setting) first.");
                    return Ok(());
                }
            }
            // Indexed KNN via sqlite-vec (stored L2-normalized → score is cosine).
            let hits = store.knn(&qv, limit)?;
            if hits.is_empty() {
                println!("no vectors yet - run `codegraph semantic-index` first");
                return Ok(());
            }
            for (id, score) in hits {
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
                    if cfg!(feature = "local-llm") {
                        println!("chat model (ask/rerank/HyDE):  ✓ bundled mistral.rs engine (no server needed)");
                    } else {
                        println!("chat model (ask/rerank/HyDE):  ✗ no local provider (start MLX/LM Studio/Ollama, or set an API key)");
                    }
                    println!("embedding model (semantic):    ✗ none");
                }
            }
            #[cfg(feature = "local-embed")]
            println!("local embeddings:  ✓ compiled in (--features local-embed)");
            #[cfg(feature = "local-llm")]
            println!("local chat engine: ✓ compiled in (--features local-llm, mistral.rs)");
            println!("\nsetup:  codegraph init   |   config: .codegraph.toml (env CODEGRAPH_* overrides)");
        }
        Command::Ingest { input, path } => {
            let chunks = codegraph_ingest::ingest(&input).map_err(anyhow::Error::msg)?;
            let store = codegraph_store::Store::open(&index::db_path(&path))?;
            for (i, ch) in chunks.iter().enumerate() {
                store.upsert_node(&index::document_node_from_chunk(ch, i))?;
            }
            // FTS stays in sync via the nodes_fts triggers — no rebuild needed.
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
