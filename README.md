# CodeGraph

A single static binary that turns **any codebase** into a queryable code-knowledge graph for AI agents — over **MCP** (Claude Code, Cursor, …) and from a **standalone CLI**. Project-agnostic, local-only, no API key required. An optional local LLM (LM Studio / MLX / Ollama, or a cloud key) adds natural-language Q&A and semantic search — everything degrades gracefully with no model running.

> **v1.3 — production-grade.** 13 languages, cross-file resolution, **compiler-grade SCIP import**, incremental indexing, Louvain communities + betweenness, inheritance/hyperedges, HTTP route extraction, full-text + semantic search, doc/image ingestion, an MCP server with 9 tools. Indexes real large repos fast — the 23k-symbol, 2,189-file iOS app in **1.3s**. 38 tests, zero clippy warnings.

## Features

- **13 languages** — Rust, Python, JavaScript, TypeScript, Go, **Swift**, **Kotlin**, Java, C, C++, Ruby, C#, Bash. One grammar-driven parser.
- **The graph** — `File / Function / Method / Class / Enum / Interface / Type / Module / Route / Document` nodes joined by `DEFINES`, `CALLS`, `INHERITS`, `IMPLEMENTS` edges, plus **IMPLEMENTS hyperedges**.
- **Honest cross-file resolution** — calls resolve in-file first, then to a project-wide unique name; ambiguous names stay unlinked (no phantom edges, no cross-language calls).
- **Compiler-grade SCIP import** — merge a `.scip` (scip-typescript, rust-analyzer, scip-java, scip-python, …) for **Tier-A precise edges** that resolve what heuristics can't — overloads, re-exports, ambiguous names. Auto-detected at the repo root; supersedes the tree-sitter edge for the same pair.
- **Fast & incremental** — respects `.gitignore` + a custom `.codegraphignore`; prunes dependency/build dirs; sha-256 manifest skips unchanged files; parallel parsing; single-transaction prepared bulk writes; O(V+E) PageRank. Real ProMom projects index in <1.4s.
- **Graph intelligence** — `trace`, `impact` (blast-radius), `callers` / `callees`, `implementers`, `important` (PageRank), `communities` (Louvain), `routes`.
- **Search** — full-text (`search`, optional `--rerank`), **semantic** vector search (`semantic`, `--hyde`), and `ask` (NL answer over real source snippets).
- **Ingest** — `ingest` pulls PDFs, text/markdown, web pages, and (with `--features media`) **images via OCR**.
- **MCP server** — `search, semantic_search, get_node, callers, callees, trace_path, blast_radius, important, stats` over stdio.
- **Arbitrary analytics** — `query` runs read-only SQL over the graph (a universal alternative to a graph query language).

See **[docs/BENCHMARK.md](docs/BENCHMARK.md)** for a measured head-to-head vs qmd, graphify, codebase-memory, and codebase-index — feature parity matrix + perf + honest gaps.

## Install

Rust 1.89+:

```bash
git clone git@github.com:sina-parsania/FlowCrafter.git codegraph && cd codegraph
./install.sh                                   # release build → ~/.local/bin + Claude Code MCP hint
cargo install --path crates/codegraph-cli      # or via cargo
cargo install --path crates/codegraph-cli --features media   # + image OCR (needs tesseract)
```

Prebuilt binaries (macOS arm64/x64, Linux x64/arm64, Windows x64) are built by CI on every `v*` tag.

## Usage

```bash
codegraph index .                    # incremental index → .codegraph/graph.db  (--full to force)
codegraph index . --scip index.scip  # + merge compiler-grade SCIP edges (Tier-A; auto-detected if present)
codegraph search UserService --rerank
codegraph semantic "retry with backoff" --hyde
codegraph ask "how does auth work?"
codegraph important --limit 15        # most central symbols (PageRank)
codegraph communities                 # detected code clusters (Louvain)
codegraph routes                      # detected HTTP routes (NestJS/Express/Flask/Spring)
codegraph query "SELECT label, COUNT(*) FROM nodes GROUP BY label"   # arbitrary read-only SQL analytics
codegraph impact processPayment       # blast-radius
codegraph callers handleLogin   /   codegraph callees parseFile
codegraph implementers Repository     # who implements/extends it
codegraph trace router handler        # shortest dependency path
codegraph ingest ./docs/guide.pdf     # ingest a PDF/markdown/URL/image as Document nodes
codegraph doctor      /   codegraph install   /   codegraph mcp
```

### Custom ignore file

Beyond `.gitignore`, drop a **`.codegraphignore`** (same gitignore syntax) in a repo to exclude extra paths from indexing — e.g. `generated/`, `*.pb.go`, `fixtures/`.

### Use from Claude Code

`codegraph install` writes the MCP server into `~/.claude.json`, or add manually:

```json
{
  "mcpServers": {
    "codegraph": {
      "command": "codegraph",
      "args": ["mcp", "--path", "/path/to/repo"]
    }
  }
}
```

## Optional LLM (local-first, no key)

Auto-detected, first reachable wins: **LM Studio** (`:1234`, MLX) → **mlx-lm/mlx-vlm** (`:8080`) → **Ollama** (`:11434`) → **OpenAI/Gemini** (opt-in key). One `LlmClient` over a unified OpenAI-compatible backend; adding a provider is one config entry. Override with `CODEGRAPH_LLM_PROVIDER` / `_BASE_URL` / `_MODEL` / `CODEGRAPH_EMBED_MODEL`.

## Architecture

Cargo workspace: `codegraph-core` · `codegraph-parse` (tree-sitter, 13 langs, routes) · `codegraph-graph` (cross-file resolution, Louvain, betweenness, PageRank, hyperedges) · `codegraph-resolve` (SCIP import → Tier-A edges) · `codegraph-store` (SQLite + FTS5 + vectors + zst) · `codegraph-llm` (provider registry) · `codegraph-ingest` (PDF/web/text/image) · `codegraph-mcp` · `codegraph-cli`.

## Roadmap (gated; the core never depends on these)

- **Audio/video media ingest** (whisper transcription, ffmpeg keyframes) — the `media` feature's next expansion; image OCR ships today.

## License

Dual-licensed under MIT or Apache-2.0.
