# CodeGraph

A single static binary that turns **any codebase** into a queryable code-knowledge graph for AI agents — over **MCP** (Claude Code, Cursor, …) and from a **standalone CLI**. Project-agnostic, local-only, no API key required. An optional local LLM (LM Studio / MLX / Ollama, or a cloud key) adds natural-language Q&A and semantic search — and everything degrades gracefully when no model is running.

> **v1.0** — multi-language indexing, a persistent graph with full-text + semantic search, graph traversal/ranking (trace, impact, importance), reverse-call lookup, an MCP server with 8 tools, and an optional LLM layer. 29 tests, zero clippy warnings.

## Highlights

- **Index any repo** — tree-sitter parses **Rust, Python, JavaScript, TypeScript, Go** into a graph of `File / Function / Method / Class / Enum / Interface / Type / Module` nodes joined by `DEFINES` and intra-file `CALLS` edges; persisted to `.codegraph/graph.db` (SQLite + FTS5).
- **Search** — full-text (`search`) and **semantic** vector search (`semantic`, via a local embedding model).
- **Graph intelligence** — `trace` (shortest dependency path), `impact` (blast-radius / what depends on X), `callers` / `callees`, `important` (PageRank ranking of the most central symbols).
- **Ask** — natural-language questions answered by a local LLM over real source snippets (`ask`), with a graceful "here are the relevant symbols" fallback when no LLM is running.
- **MCP server** — exposes `search, get_node, callers, callees, trace_path, blast_radius, important, stats` so an agent queries the graph directly instead of grepping.
- **Honest edges** — call edges are intra-language and intra-file; a name match in another file is **not** a phantom edge. No phantom cross-language calls.
- **Local-first** — works fully offline with zero deps; the LLM/embedding layers are optional enrichment, never required.

## Install

Rust 1.89+:

```bash
git clone git@github.com:sina-parsania/FlowCrafter.git codegraph && cd codegraph
./install.sh                                   # release build → ~/.local/bin + Claude Code MCP hint
# or:
cargo install --path crates/codegraph-cli
```

## Usage

```bash
codegraph index .                         # index this repo → .codegraph/graph.db
codegraph search UserService              # full-text symbol search → file:line
codegraph important --limit 15            # most central symbols (PageRank)
codegraph impact processPayment           # what breaks if this changes (blast-radius)
codegraph callers handleLogin             # who calls it
codegraph callees parse_file              # what it calls
codegraph trace router handleRequest      # shortest dependency path between two symbols
codegraph ask "how does auth work?"       # NL answer via local LLM over source snippets
codegraph semantic-index                  # embed symbols (needs a local embedding model)
codegraph semantic "retry with backoff"   # vector search by meaning
codegraph doctor                          # languages, schema, local-LLM availability
codegraph mcp                             # run the MCP server over stdio (for agents)
```

### Use from Claude Code (MCP)

`install.sh` prints the snippet; or add to `~/.claude.json`:

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

Then: _"use codegraph to find what depends on `processPayment`"_ → the agent calls `blast_radius`.

## Optional LLM (local-first, no key)

CodeGraph auto-detects an OpenAI-compatible endpoint in this order, and uses the first that answers:

1. **LM Studio** (`:1234`) — MLX-native on Apple Silicon (preferred)
2. **mlx-lm / mlx-vlm** (`:8080`)
3. **Ollama** (`:11434`)
4. **Cloud** — OpenAI / Gemini, opt-in via `OPENAI_API_KEY` / `GEMINI_API_KEY`

Override with `CODEGRAPH_LLM_PROVIDER`, `CODEGRAPH_LLM_BASE_URL`, `CODEGRAPH_LLM_MODEL`, `CODEGRAPH_EMBED_MODEL`. Adding a provider is one config entry — the call sites never change (one `LlmClient` over a unified OpenAI-compatible backend). With no model running, `search` / graph / MCP all still work; only `ask` / `semantic` need one.

> **Semantic search** uses `/v1/embeddings`. It works with any compliant endpoint (e.g. Ollama `nomic-embed-text`). LM Studio note: an embedding model must be _loaded_ and its API embeddings server enabled — `/v1/models` lists _downloaded_ models, not loaded ones.

## Languages

| Language   | Extensions           | Definitions                        |
| ---------- | -------------------- | ---------------------------------- |
| Rust       | `.rs`                | fn, struct, enum, trait, type, mod |
| Python     | `.py .pyi`           | def, class                         |
| JavaScript | `.js .jsx .mjs .cjs` | function, class, method            |
| TypeScript | `.ts .tsx .mts .cts` | + interface, type alias, enum      |
| Go         | `.go`                | func, method, type                 |

## Architecture

A Cargo workspace of focused crates: `codegraph-core` (types, config, LLM traits) · `codegraph-parse` (grammar-driven tree-sitter) · `codegraph-graph` (edge build + traversal/PageRank) · `codegraph-store` (SQLite + FTS5 + vectors + zst artifact) · `codegraph-llm` (OpenAI-compatible provider registry) · `codegraph-mcp` (rmcp server) · `codegraph-cli` (the `codegraph` binary).

Pipeline: **walk → tree-sitter parse → build graph → persist (SQLite) → search / traverse / serve (MCP) → optional LLM enrichment.**

## Roadmap (gated; the core never depends on these)

- Compiler-grade resolution (SCIP) where a build env exists; tree-sitter scoped symbol-table fallback otherwise
- Cross-service link detection (HTTP/gRPC) and hyperedges
- Incremental indexing (sha-256 manifest + git diff)
- Opt-in ingestion (PDF/audio/web; image/video via a vision model) — off by default
- More languages; prebuilt static binaries + Homebrew tap + multi-agent auto-config installer

## License

Dual-licensed under MIT or Apache-2.0.
