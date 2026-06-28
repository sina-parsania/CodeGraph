//! CodeGraph CLI. `codegraph mcp` (M6) is one subcommand among many; the CLI is
//! a real standalone package.

mod index;
mod query;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use codegraph_core::{Config, LlmClient};

#[derive(Parser)]
#[command(name = "codegraph", version, about = "Project-agnostic code-intelligence graph + MCP server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print version, config defaults, and a readiness check.
    Status,
    /// Index a repository into a local graph (.codegraph/graph.db).
    Index {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Force a full re-index (ignore the sha256 manifest).
        #[arg(long)]
        full: bool,
    },
    /// Full-text search the indexed graph for a term.
    Search {
        term: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value_t = 20)]
        limit: usize,
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
    /// List the largest code communities (clusters) detected in the graph.
    Communities {
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value_t = 12)]
        limit: usize,
    },
    /// Most central symbols by PageRank.
    Important { #[arg(long, default_value = ".")] path: PathBuf, #[arg(long, default_value_t = 15)] limit: usize },
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
    },
    /// Health check: languages, schema, and local-LLM availability.
    Doctor,
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

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
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
        Command::Index { path, full } => {
            let db = index::db_path(&path);
            let stats = index::index_dir(&path, &db, full)?;
            println!(
                "indexed {} files ({} changed{}) → {} nodes, {} edges  ({})",
                stats.files,
                stats.changed,
                if stats.pruned > 0 { format!(", {} pruned", stats.pruned) } else { String::new() },
                stats.nodes,
                stats.edges,
                db.display()
            );
        }
        Command::Search { term, path, limit } => {
            let db = index::db_path(&path);
            let store = codegraph_store::Store::open(&db)?;
            let hits = store.search_fts(&term, limit)?;
            if hits.is_empty() {
                println!("no matches for {:?}", term);
            }
            for n in hits {
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
        Command::Semantic { query: q, path, limit } => {
            let store = codegraph_store::Store::open(&index::db_path(&path))?;
            let Some(b) = codegraph_llm::OpenAiCompatBackend::detect().filter(|b| b.embed_model().is_some()) else {
                println!("no embedding model available (load one in LM Studio / Ollama)");
                return Ok(());
            };
            let Some(qv) = b.embed(&q) else {
                println!("embedding request failed - is an embedding model LOADED? (LM Studio: lms load <embed-model>; only downloaded != loaded)");
                return Ok(());
            };
            let vectors = store.all_vectors()?;
            if vectors.is_empty() {
                println!("no vectors yet - run `codegraph semantic-index` first");
                return Ok(());
            }
            let mut scored: Vec<(f32, String)> =
                vectors.iter().map(|(id, v)| (query::cosine(&qv, v), id.clone())).collect();
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
            println!("languages:  rust, python, javascript, typescript, go");
            match codegraph_llm::OpenAiCompatBackend::detect() {
                Some(llm) => println!("local LLM:  available  ({} / {})", llm.provider(), llm.model()),
                None => println!("local LLM:  not detected  (search + graph work fully offline; LM Studio/Ollama enables `ask`)"),
            }
        }
        Command::Install { print, repo } => {
            let repo = repo.canonicalize().unwrap_or(repo);
            let entry = serde_json::json!({"command": "codegraph", "args": ["mcp", "--path", repo.to_string_lossy()]});
            let snippet = serde_json::to_string_pretty(&serde_json::json!({"mcpServers": {"codegraph": entry.clone()}}))?;
            if print {
                println!("Add to your agent's MCP config:\n{}", snippet);
                return Ok(());
            }
            let home = std::env::var("HOME").unwrap_or_default();
            let path = std::path::Path::new(&home).join(".claude.json");
            let mut root: serde_json::Value = if path.exists() {
                serde_json::from_str(&std::fs::read_to_string(&path)?).unwrap_or_else(|_| serde_json::json!({}))
            } else {
                serde_json::json!({})
            };
            if !root.is_object() {
                root = serde_json::json!({});
            }
            let obj = root.as_object_mut().unwrap();
            let servers = obj.entry("mcpServers").or_insert_with(|| serde_json::json!({}));
            if let Some(sm) = servers.as_object_mut() {
                sm.insert("codegraph".to_string(), entry);
            }
            if path.exists() {
                let _ = std::fs::copy(&path, path.with_extension("json.bak"));
            }
            std::fs::write(&path, serde_json::to_string_pretty(&root)?)?;
            println!("configured Claude Code MCP at {} (backup .bak written)", path.display());
            println!("for other agents, add:\n{}", snippet);
        }
        Command::Mcp { path } => {
            let db = index::db_path(&path);
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(codegraph_mcp::serve_stdio(db))?;
        }
    }
    Ok(())
}
