# CodeGraph

**The code-intelligence engine for AI agents — one 5 MB binary, zero config, zero phantom edges.**

CodeGraph indexes any repository into a **resolved code knowledge graph** (SQLite) and serves it to AI agents over **MCP** (Claude Code, Cursor, Zed, …) and a full **CLI**. Agents stop grepping and reading whole files — they ask the graph and get exact `file:line` answers with resolved call edges.

[![release](https://img.shields.io/github/v/release/sina-parsania/CodeGraph?label=release)](../../releases)
![rust](https://img.shields.io/badge/rust-single%20static%20binary-orange)
![languages](https://img.shields.io/badge/languages-13-blue)
![license](https://img.shields.io/badge/license-MIT%2FApache--2.0-green)

```bash
cargo install --path crates/codegraph-cli   # one binary, no deps
codegraph init                              # index + wire MCP + done
```

---

## Why teams pick CodeGraph

|                      | CodeGraph                                                                                                                                                                                      | typical graph tools                                                                   |
| -------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------- |
| **Answer trust**     | **Zero phantom edges** — ambiguous calls are _dropped, never guessed_; every edge carries a `justification` tag; results include a **coverage signal** (`may_be_incomplete`)                   | silently merge same-name symbols, or emit guessed edges with hidden confidence floors |
| **Ambiguity**        | `callers create` → **44 pinnable candidates** grouped per definition                                                                                                                           | one merged (wrong) list, or a blocklist refusing the query                            |
| **Speed** (measured) | cold index **1.5 s**, queries **~13 ms**, binary **~5 MB**; **incremental reindex is O(impact)** — an edit re-resolves only the files whose call sites name a changed definition, not the repo | 2.8 s–11 s cold, 269 MB binaries, pip/npm setup tax                                   |
| **Determinism**      | same commit → **byte-identical graph** — safe to commit & share (`export`/`import`, 88 % smaller)                                                                                              | machine-dependent results                                                             |
| **Reach**            | **cross-service route hubs**: a backend route and its frontend caller collapse onto one node — `blast_radius` crosses service boundaries                                                       | per-repo silos                                                                        |

## ⚡ ~20–100× fewer agent tokens per code question

A competent agent answering _"who calls this?"_ with grep must read context around every hit. With CodeGraph it gets one resolved answer. Measured across four repos (bounded, task-appropriate baselines — not whole-file strawmen): **86× (Rust) · 21× (Python) · 29× (Go) · 100× (TypeScript)**. Reproduce: `python3 scripts/benchmark.py --repo <path>`.

## What you get

### 🧠 A graph agents can trust

- **13 languages**, one grammar-driven tree-sitter parser — Rust, Python, JS/TS, Go, Swift, Kotlin, Java, C, C++, Ruby, C#, Bash. Arrow-function/`const` components are first-class nodes; every language ships a **golden fixture suite** asserting exactly which edges must (and must NOT) exist.
- **Tiered, evidence-based resolution** (unique-or-drop at every tier): same-file → `self`/`this` CHA (now incl. Swift/Kotlin inheritance) → field-type (DI, incl. TS constructor properties & Kotlin constructor `val`s) → local-var type (incl. `new Foo()` initializers, generic/`| null` unwrap) → **import-narrowed** (the import _is_ the evidence — **tsconfig `paths` aliases resolved**) → Go package scope → global-unique. Details: [docs/RESOLUTION.md](docs/RESOLUTION.md).
- **`codegraph audit` — MEASURED precision, not claimed**: samples tree-sitter edges, verifies them against the compiler oracle merged into your graph (SCIP/IndexStore), and stores per-tier precision that MCP `stats` and `report` quote back to the agent. No hidden confidence floors — evidence classes with measured numbers on YOUR repo.
- **Self-healing invariants**: dangling edges are auto-healed before every commit (reused compiler edges are endpoint-filtered), and the graph carries an **identity stamp** — codegraph refuses to answer from a graph built for a different repo or written by another tool.
- **Compiler-grade tiers (optional, sticky):** `codegraph scip` merges any SCIP index — and once you've run it, the tier is **auto-maintained**: full rebuilds reuse the merged edges, and a moved HEAD re-runs the detected indexer automatically (`CODEGRAPH_AUTO_SCIP=0` opts out). `--features indexstore` reads **Xcode's IndexStore** for Swift (+171 % resolved calls on a real iOS app) — merged at index time, queries stay milliseconds.
- **Per-node metrics**: cyclomatic complexity, fan-in/fan-out (resolved-only — honest degrees), PageRank, betweenness, Louvain community.
- **Incremental & live**: an edit re-resolves only the files whose call sites name a changed definition (wave-propagation, O(impact) not O(repo)) — proven byte-identical to a from-scratch index. The MCP server watches the repo and heals in the background, so queries never wait on a reindex. Details: [docs/INCREMENTAL.md](docs/INCREMENTAL.md).

### 🔎 Search that actually finds things

- **Subword FTS**: `Cook` finds `OrderCheckoutSessionViewController` (camelCase/snake split at index time).
- `--regex` for anchors/middle fragments; multi-word OR; **semantic search by meaning** via a **bundled local embedder** — _no server, no API key_ (`bge-small` default, `CODEGRAPH_LOCAL_EMBED=code` for the 768-d code-trained model; the model is stamped into the index, mismatches refused).
- **Indexed vector search** — embeddings live in a `sqlite-vec` (`vec0`) KNN table, not a brute-force scan, so semantic search scales past the old ~10k-vector ceiling. Vectors auto-refresh for the symbols an incremental index touched — no manual `semantic-index` after every edit.

### 🧭 Graph intelligence

`callers` / `callees` (with coverage + pinnable candidates) · `impact` (blast radius) · `trace` · `implementers` · `important` · `communities` · `routes` · **`context`** (task-relevant symbols by personalized PageRank within a token budget) · **`flows`** (entry-point call chains ranked by criticality) · **`cypher`** (openCypher-subset graph queries) · raw SQL.

### 🛡️ Change-aware review, built in

- **`codegraph review --base origin/main --md`** — risk-scored affected symbols (multiplicative: reach × complexity × untested), test gaps via first-class **TESTS edges**, **co-change hints** mined from git history.
- **GitHub Action** ([`action/action.yml`](action/action.yml)): single-binary install, sticky PR comment, optional `fail-on-high-risk` gate. No pip/npm.
- `dead-code` — candidates that _no call site in the repo even names_ (raw-call-site evidence, entry points/routes/tests excluded).

### ✏️ Safe semantic edits

`rename-symbol old new [--write]` rewrites a symbol + all resolved references — or **refuses** when any occurrence isn't provably accounted for. It never corrupts code to complete an edit.

### 📊 See the graph

- **`codegraph report`** — deterministic Markdown snapshot (no LLM): overview, call-resolution quality by tier, central symbols, strongest co-changes. Reproducible for CI diffs.
- **`codegraph html [--open]`** — one **self-contained** interactive HTML file (force layout, no CDN, no server): pan/zoom the resolved graph offline. Written next to the cached graph so the repo stays pristine.

### 📦 Team-ready

- Central cache (`~/.cache/codegraph`) — repos stay pristine. Auto-TTL cleanup.
- **`export` / `import`**: commit a zstd graph artifact; teammates skip the full reindex.
- **Always fresh**: stat-only staleness probe + auto-reindex before every query.
- Docs/PDF/URL/localization ingest — code + docs in one graph.

## Quickstart

```bash
codegraph init                    # one-time: index + MCP wiring + agent nudge
codegraph search OrderCheckout    # subword-tolerant symbol search
codegraph callers create          # 44 definitions? → pinnable candidates, not a merged lie
codegraph review --base develop   # risk + test gaps + co-change hints for your diff
codegraph flows                   # entry-point call chains by criticality
codegraph context "auth jwt" --budget 1000        # LLM-ready task context
codegraph cypher "MATCH (a)-[:Calls]->(b) WHERE b.name = 'save' RETURN a.name LIMIT 10"
codegraph semantic "retry with backoff"           # meaning search, serverless
codegraph export                  # commit .codegraph/graph.db.zst for your team
```

**MCP (17 tools):** `search`, `callers`, `callees`, `blast_radius`, `trace_path`, `context`, `changes`, `dead_code`, `co_changes`, `implementers`, `routes`, `important`, `semantic_search`, `flows`, `graph_query`, `get_node`, `stats` — each with agent-guidance descriptions, coverage signals, and `_hints`.

**Navigation Protocol (known unknowns, made actionable):** every edge names its evidence class; `stats` quotes the audit-measured per-tier precision; and whenever a precise answer may be incomplete the response carries a ready-made **`_fallback` grep pattern**, so the agent verifies instead of concluding absence. Coverage denominators exclude call sites **bound to external-package imports** (evidence, not heuristics) — `may_be_incomplete` means something.

## Configuration (all optional)

Everything works with **no model, no key, no daemon**. `codegraph init` writes a commented `.codegraph.toml`; env vars (`CODEGRAPH_*`) override. `codegraph doctor` shows what's ready.

**LLM features (`ask`, `--rerank`, `--hyde`) — layered, all optional:**

1. A running OpenAI-compatible server is used first (MLX → LM Studio → Ollama → OpenAI/Gemini via API key).
2. No server? A build with `--features local-llm` bundles a **pure-Rust in-process engine** (mistral.rs, CPU — macOS/Linux/Windows). Default model: Qwen2.5-Coder-0.5B GGUF (~400 MB, ~600 MB RAM), loaded lazily and only when actually used; auto-downloaded once. Override via `CODEGRAPH_LOCAL_LLM_REPO`/`CODEGRAPH_LOCAL_LLM_FILE` (e.g. the 1.5B for higher quality). Release binaries ship with it.
3. Macs with the Xcode Metal Toolchain can build `--features local-llm-metal` for GPU inference.

Semantic search is the same story: `--features local-embed` bundles the embedder (release binaries include it) — no server needed.

## How it compares

Full measured head-to-heads (live runs + source-level audits of competing tools): **[docs/BENCHMARK.md](docs/BENCHMARK.md)** · comparative roadmap: [docs/plans/](docs/plans/).

## Architecture

```
crates/
  codegraph-core       types, config, deterministic ids
  codegraph-parse      tree-sitter → nodes, calls, imports, fields, locals, metrics
  codegraph-graph      tiered resolution (unique-or-drop), PageRank/Louvain/betweenness, flows
  codegraph-resolve    SCIP merge (compiler-grade, optional)
  codegraph-indexstore Xcode IndexStore merge (Swift compiler-grade, optional)
  codegraph-store      SQLite: nodes/edges/calls/sqlite-vec KNN/FTS5(external-content)/cochanges/meta
  codegraph-llm        OpenAI-compat client + optional bundled embedder (fastembed) & chat engine (mistral.rs)
  codegraph-mcp        MCP server (17 tools, generation-keyed graph cache, fs-watcher, coverage signals)
  codegraph-cli        the `codegraph` binary
```

**Design invariants:** single static binary · deterministic builds · precision-sacred resolution · heavy deps feature-gated (`indexstore`, `local-embed`, `local-llm`, `media`).

## License

MIT OR Apache-2.0.

---

⭐ **If CodeGraph saves your agent's tokens (it will), star the repo** — it helps others find it.
