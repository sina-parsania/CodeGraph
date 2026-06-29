# CodeGraph

**Give your AI agent a map of your codebase — so it stops grepping and reading whole files to answer simple questions.** One static binary, one command to set up, zero config, no API key. Works as an **MCP server** (Claude Code, Cursor, …) and a standalone **CLI**.

> **One command:** `codegraph init` — indexes your repo, wires the MCP into Claude Code, and nudges the agent to use it. That's it. Everything AI is optional; the core graph works fully offline.

---

## ⚡ Why it matters: ~20–100× fewer tokens per code question

When an AI agent answers _"who calls this function?"_ without CodeGraph, it greps the name and **reads context around every hit** to disambiguate real call sites from comments/strings/defs — burning tokens and tool round-trips. With CodeGraph it gets one compact, resolved `file:line` answer.

The baseline here is a **competent agent**: grep, then a **bounded read** sized to the task (read the one definition region for "where defined"; read ±5 lines around each hit for "who calls"). It is **not** reading whole files — that naive number is ~5× larger and we don't headline it.

Measured on **CodeGraph's own repo** (reproduce: `python3 scripts/benchmark.py`):

| Real navigation question        | task | grep + bounded reads | **CodeGraph** |
| ------------------------------- | ---- | -------------------- | ------------- |
| Where is `index_dir` defined?   | def  | 1,654 tok            | **18 tok**    |
| Who calls `ensure_fresh`?       | refs | 3,116 tok            | **45 tok**    |
| What does `run_init` call?      | body | 847 tok              | **114 tok**   |
| Where is `OpenAiCompatBackend`? | refs | 5,550 tok            | **16 tok**    |
| Who calls `db_path`?            | refs | 10,318 tok           | **58 tok**    |
| Where is `Store` defined?       | def  | 3,337 tok            | **39 tok**    |
| **Total**                       |      | **24,822 tok**       | **290 tok**   |

→ **86× fewer context tokens** on this repo. Across external repos with the same honest methodology: **21× (Python · requests), 29× (Go · cobra), 100× (TypeScript · zod)** — so a realistic range of **~20–100×**, lowest for simple definition lookups, highest for call-graph / who-calls questions on large repos. And that's only the questions grep _can_ answer; **impact/blast-radius, shortest-path trace, importance (PageRank), and communities** grep can't answer at all without reading half the tree.

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
- **A real graph** — `Function/Method/Class/Enum/Interface/Type/Module/Route/Document` nodes joined by `DEFINES / CALLS / INHERITS / IMPLEMENTS` (+ IMPLEMENTS hyperedges). Honest, **language-agnostic** receiver-aware resolution (same-file → Class-Hierarchy-Analysis for `self`/`this` and `this.field.method()` DI → unique name) — one resolver fires across TS, Swift, Kotlin, Python, Java, … A qualified call on a named variable never guesses a same-file member; ambiguous names stay unlinked, no phantom edges. Precision is sacred — see [docs/RESOLUTION.md](docs/RESOLUTION.md).
- **Compiler-grade precision (optional, one command)** — `codegraph scip` detects your language, runs the matching SCIP indexer (scip-typescript / rust-analyzer / scip-java / …) if installed, and merges **Tier-A edges** that resolve overloads, re-exports, and ambiguous names tree-sitter can't. _Zero-config means the tree-sitter core_ (which needs nothing); SCIP is an opt-in precision upgrade.
- **Graph intelligence grep can't do** — `impact` (blast-radius), `trace` (shortest path), `callers`/`callees`, `implementers`, `important` (PageRank), `communities` (Louvain), `routes`, and `context` (assemble task-relevant symbols by **personalized PageRank over the resolved graph**, within a token budget — surfaces a query's call-graph dependencies, not just name matches).
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
codegraph context "auth login jwt" --budget 1000   # assemble task-relevant symbols (graph-ranked, budgeted)
codegraph communities  /  routes      # clusters; detected HTTP routes
codegraph semantic "retry with backoff" --hyde     # search by meaning (needs an embed model)
codegraph ask "how does auth work?"                # NL answer over real source
codegraph query "SELECT label, COUNT(*) FROM nodes GROUP BY label"   # arbitrary SQL
codegraph scip                        # one-command compiler-grade precision (runs the SCIP indexer + merges)
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
