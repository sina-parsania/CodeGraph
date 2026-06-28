# CodeGraph

**Give your AI agent a map of your codebase — so it stops grepping and reading whole files to answer simple questions.** One static binary, one command to set up, zero config, no API key. Works as an **MCP server** (Claude Code, Cursor, …) and a standalone **CLI**.

> **One command:** `codegraph init` — indexes your repo, wires the MCP into Claude Code, and nudges the agent to use it. That's it. Everything AI is optional; the core graph works fully offline.

---

## ⚡ Why it matters: 332× fewer tokens per code question

When an AI agent answers _"who calls this function?"_ without CodeGraph, it greps, gets ambiguous hits, then **reads whole files into its context** to disambiguate — burning thousands of tokens and many tool round-trips. With CodeGraph it gets one compact, resolved `file:line` answer.

Measured on **CodeGraph's own repo** (reproduce: `python3 scripts/benchmark.py`):

| Real navigation question        | grep + read files         | **CodeGraph**         |
| ------------------------------- | ------------------------- | --------------------- |
| Where is `index_dir` defined?   | 5,166 tok · 3 calls       | **18 tok · 1 call**   |
| Who calls `ensure_fresh`?       | 14,081 tok · 5 calls      | **22 tok · 1 call**   |
| What does `run_init` call?      | 3,957 tok · 3 calls       | **71 tok · 1 call**   |
| Where is `OpenAiCompatBackend`? | 15,900 tok · 8 calls      | **16 tok · 1 call**   |
| Who calls `db_path`?            | 19,547 tok · 7 calls      | **36 tok · 1 call**   |
| Where is `Store` defined?       | 8,154 tok · 3 calls       | **39 tok · 1 call**   |
| **Total**                       | **66,967 tok · 29 calls** | **202 tok · 6 calls** |

→ **332× fewer context tokens, ~5× fewer tool round-trips** — so the agent is **faster and cheaper** on every code-navigation step. And that's only the questions grep _can_ answer; **impact/blast-radius, shortest-path trace, importance (PageRank), and communities** grep can't answer at all without reading half the tree.

---

## Quickstart

```bash
# install
git clone git@github.com:sina-parsania/FlowCrafter.git codegraph && cd codegraph
cargo install --path crates/codegraph-cli         # one static binary, no native deps

# set up any repo in one command
cd ~/my-project && codegraph init                 # index + wire Claude Code MCP + agent nudge + .codegraph.toml
```

Then just ask Claude Code to _"use codegraph to find …"_ — its tools are live. Or use the CLI directly. Prebuilt binaries (macOS arm64/x64, Linux x64/arm64, Windows x64) ship on every `v*` tag.

## What you get

- **Self-setup, zero config** — `codegraph init` does everything; re-runnable, `--yes` for CI, `--uninstall` to undo. No model, key, or daemon required.
- **Always fresh, never wrong** — every query (CLI **and** MCP) runs a stat-only probe and **auto-reindexes before serving**, so edits, file add/delete, and `git checkout`/`switch` are reflected instantly. No stale results, no manual reindex.
- **13 languages** — Rust, Python, JS, TS, Go, Swift, Kotlin, Java, C, C++, Ruby, C#, Bash. One grammar-driven parser.
- **A real graph** — `Function/Method/Class/Enum/Interface/Type/Module/Route/Document` nodes joined by `DEFINES / CALLS / INHERITS / IMPLEMENTS` (+ IMPLEMENTS hyperedges). Honest cross-file resolution: ambiguous names stay unlinked (no phantom edges).
- **Compiler-grade precision (optional)** — merge a `.scip` (scip-typescript / rust-analyzer / scip-java / …) for **Tier-A edges** that resolve overloads, re-exports, and ambiguous names tree-sitter can't.
- **Graph intelligence grep can't do** — `impact` (blast-radius), `trace` (shortest path), `callers`/`callees`, `implementers`, `important` (PageRank), `communities` (Louvain), `routes`.
- **Search** — full-text (`--rerank`), **semantic** vector (`--hyde`), and `ask` (NL answer over real snippets). All optional; degrade gracefully with no model.
- **Any input** — `index` also ingests docs + localization (md/rst/txt/`.strings`/po/xliff/…); `ingest` adds PDFs, URLs, json/yaml/csv/log/…, and (with `--features media`) images via OCR. One graph = code + docs + config + localization.
- **Arbitrary analytics** — `query` runs read-only SQL over the graph.
- **Fast & lean** — respects `.gitignore` + `.codegraphignore`; parallel parsing; one SQLite file per project in a **central cache** (`~/.cache/codegraph/`) so repos stay pristine. Real-world repos index in **<1.4s**; the 23k-symbol Swift app in 1.3s. Deterministic builds + auto-TTL cleanup.

## Usage

```bash
codegraph init                        # one-time setup (index + MCP + nudge + config)
codegraph search UserService          # find a symbol  (PREFER over grep)
codegraph callers handleLogin         # who calls it (resolved, exact)
codegraph callees parseFile           # what it calls
codegraph impact processPayment       # blast-radius: what breaks if I change it
codegraph trace router handler        # shortest dependency path between two symbols
codegraph important                   # most central symbols (map an unfamiliar repo)
codegraph communities  /  routes      # clusters; detected HTTP routes
codegraph semantic "retry with backoff" --hyde     # search by meaning (needs an embed model)
codegraph ask "how does auth work?"                # NL answer over real source
codegraph query "SELECT label, COUNT(*) FROM nodes GROUP BY label"   # arbitrary SQL
codegraph config                      # view resolved config; `config set llm.model …` / `config edit`
codegraph projects  /  gc             # list indexed projects; reclaim idle graphs
codegraph doctor                      # what's available + how to enable AI features
```

## Configuration (all optional)

`codegraph init` writes a commented **`.codegraph.toml`** (walked up from cwd). Precedence: built-in defaults < global `~/.config/codegraph/config.toml` < project `.codegraph.toml` < **`CODEGRAPH_*`** env. View/edit it with **`codegraph config`** (`config set llm.model <x>`, `config set <k> <v> --local`, `config edit`, `config get <k>`, `config path`). Core works with **no model**.

| Setting          | `.codegraph.toml`            | Env                                      | Default              |
| ---------------- | ---------------------------- | ---------------------------------------- | -------------------- |
| graph cache dir  | `cache_dir`                  | `CODEGRAPH_CACHE_DIR` / `XDG_CACHE_HOME` | `~/.cache/codegraph` |
| auto-reclaim TTL | —                            | `CODEGRAPH_TTL_DAYS` (`0`=off)           | 30 days              |
| LLM provider     | `llm.provider`               | `CODEGRAPH_LLM_PROVIDER`                 | `auto`               |
| LLM url / model  | `llm.base_url` / `llm.model` | `CODEGRAPH_LLM_URL` / `_MODEL`           | Qwen2.5-Coder-1.5B   |
| embedding model  | `embed_model`                | `CODEGRAPH_EMBED_MODEL`                  | —                    |
| rerank / HyDE    | `llm.rerank` / `llm.hyde`    | `CODEGRAPH_RERANK` / `_HYDE`             | off                  |
| media ingest     | `ingest.media`               | `CODEGRAPH_MEDIA`                        | off                  |

**Optional local LLM**, auto-detected (first reachable wins): LM Studio (`:1234`) → MLX (`:8080`) → Ollama (`:11434`) → OpenAI/Gemini (key). `codegraph doctor` shows what's ready and the exact command to enable semantic search.

## How it compares

|                                 | grep / ripgrep | LSP | a graph DB (Neo4j) | **CodeGraph** |
| ------------------------------- | :------------: | :-: | :----------------: | :-----------: |
| Agent-friendly (MCP)            |       ➖       | ❌  |         ➖         |      ✅       |
| Resolved call graph             |       ❌       | ✅  |         ✅         |      ✅       |
| Blast-radius / trace / PageRank |       ❌       | ➖  |         ✅         |      ✅       |
| One static binary, no server    |       ✅       | ❌  |         ❌         |      ✅       |
| Always fresh (auto-reindex)     |       ✅       | ✅  |         ❌         |      ✅       |
| Tokens per agent question       |      huge      | n/a |       medium       |   **tiny**    |

Comparison vs qmd / graphify / codebase-memory / codebase-index — **qmd + codebase-index were run live**, the other two from their documented capabilities (each row is labelled): **[docs/BENCHMARK.md](docs/BENCHMARK.md)**. Storage + freshness design: **[docs/STORAGE.md](docs/STORAGE.md)**.

> The token benchmark above runs on CodeGraph's own repo by default; run it on **any** repo with `python3 scripts/benchmark.py --repo /path/to/repo` to verify on your own code.

## Architecture

Cargo workspace: `codegraph-core` · `codegraph-parse` (tree-sitter, 13 langs) · `codegraph-graph` (resolution, Louvain, PageRank, betweenness, hyperedges) · `codegraph-resolve` (SCIP) · `codegraph-store` (SQLite + FTS5 + vectors) · `codegraph-llm` (provider registry) · `codegraph-ingest` · `codegraph-mcp` · `codegraph-cli`.

## License

Dual-licensed under MIT or Apache-2.0.
