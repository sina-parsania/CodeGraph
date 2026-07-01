# CodeGraph

**The code-intelligence engine for AI agents — one 5 MB binary, zero config, zero phantom edges.**

CodeGraph indexes any repository into a **resolved code knowledge graph** (SQLite) and serves it to AI agents over **MCP** (Claude Code, Cursor, Zed, …) and a full **CLI**. Agents stop grepping and reading whole files — they ask the graph and get exact `file:line` answers with resolved call edges.

[![release](https://img.shields.io/github/v/release/sina-parsania/FlowCrafter?label=release)](../../releases)
![rust](https://img.shields.io/badge/rust-single%20static%20binary-orange)
![languages](https://img.shields.io/badge/languages-13-blue)
![license](https://img.shields.io/badge/license-MIT%2FApache--2.0-green)

```bash
cargo install --path crates/codegraph-cli   # one binary, no deps
codegraph init                              # index + wire MCP + done
```

---

## Why teams pick CodeGraph

|                      | CodeGraph                                                                                                                                                                    | typical graph tools                                                                   |
| -------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------- |
| **Answer trust**     | **Zero phantom edges** — ambiguous calls are _dropped, never guessed_; every edge carries a `justification` tag; results include a **coverage signal** (`may_be_incomplete`) | silently merge same-name symbols, or emit guessed edges with hidden confidence floors |
| **Ambiguity**        | `callers create` → **44 pinnable candidates** grouped per definition                                                                                                         | one merged (wrong) list, or a blocklist refusing the query                            |
| **Speed** (measured) | cold index **1.5 s**, queries **~13 ms**, binary **~5 MB**                                                                                                                   | 2.8 s–11 s cold, 269 MB binaries, pip/npm setup tax                                   |
| **Determinism**      | same commit → **byte-identical graph** — safe to commit & share (`export`/`import`, 88 % smaller)                                                                            | machine-dependent results                                                             |
| **Reach**            | **cross-service route hubs**: a backend route and its frontend caller collapse onto one node — `blast_radius` crosses service boundaries                                     | per-repo silos                                                                        |

## ⚡ ~20–100× fewer agent tokens per code question

A competent agent answering _"who calls this?"_ with grep must read context around every hit. With CodeGraph it gets one resolved answer. Measured across four repos (bounded, task-appropriate baselines — not whole-file strawmen): **86× (Rust) · 21× (Python) · 29× (Go) · 100× (TypeScript)**. Reproduce: `python3 scripts/benchmark.py --repo <path>`.

## What you get

### 🧠 A graph agents can trust

- **13 languages**, one grammar-driven tree-sitter parser — Rust, Python, JS/TS, Go, Swift, Kotlin, Java, C, C++, Ruby, C#, Bash.
- **Tiered, evidence-based resolution** (unique-or-drop at every tier): same-file → `self`/`this` CHA → field-type (DI) → local-var type → **import-narrowed** (the import _is_ the evidence) → Go package scope → global-unique. Details: [docs/RESOLUTION.md](docs/RESOLUTION.md).
- **Compiler-grade tiers (optional):** `codegraph scip` merges any SCIP index; `--features indexstore` reads **Xcode's IndexStore** for Swift (+171 % resolved calls on a real iOS app) — merged at index time, queries stay milliseconds.
- **Per-node metrics**: cyclomatic complexity, fan-in/fan-out (resolved-only — honest degrees), PageRank, betweenness, Louvain community.

### 🔎 Search that actually finds things

- **Subword FTS**: `Cook` finds `OrderCheckoutSessionViewController` (camelCase/snake split at index time).
- `--regex` for anchors/middle fragments; multi-word OR; **semantic search by meaning** via a **bundled local embedder** — _no server, no API key_ (`bge-small` default, `CODEGRAPH_LOCAL_EMBED=code` for the 768-d code-trained model; the model is stamped into the index, mismatches refused).

### 🧭 Graph intelligence

`callers` / `callees` (with coverage + pinnable candidates) · `impact` (blast radius) · `trace` · `implementers` · `important` · `communities` · `routes` · **`context`** (task-relevant symbols by personalized PageRank within a token budget) · **`flows`** (entry-point call chains ranked by criticality) · **`cypher`** (openCypher-subset graph queries) · raw SQL.

### 🛡️ Change-aware review, built in

- **`codegraph review --base origin/main --md`** — risk-scored affected symbols (multiplicative: reach × complexity × untested), test gaps via first-class **TESTS edges**, **co-change hints** mined from git history.
- **GitHub Action** ([`action/action.yml`](action/action.yml)): single-binary install, sticky PR comment, optional `fail-on-high-risk` gate. No pip/npm.
- `dead-code` — candidates that _no call site in the repo even names_ (raw-call-site evidence, entry points/routes/tests excluded).

### ✏️ Safe semantic edits

`rename-symbol old new [--write]` rewrites a symbol + all resolved references — or **refuses** when any occurrence isn't provably accounted for. It never corrupts code to complete an edit.

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

**MCP (15 tools):** `search`, `callers`, `callees`, `blast_radius`, `trace_path`, `context`, `changes`, `dead_code`, `co_changes`, `implementers`, `routes`, `important`, `semantic_search`, `get_node`, `stats` — each with agent-guidance descriptions, coverage signals, and `_hints`.

## Configuration (all optional)

Everything works with **no model, no key, no daemon**. `codegraph init` writes a commented `.codegraph.toml`; env vars (`CODEGRAPH_*`) override. Optional local LLM (LM Studio → MLX → Ollama → OpenAI/Gemini) adds `ask`, `--rerank`, `--hyde`. `codegraph doctor` shows what's ready.

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
  codegraph-store      SQLite: nodes/edges/calls/vectors/FTS5(subword)/cochanges/meta
  codegraph-llm        bundled fastembed embedder (optional) + OpenAI-compat client
  codegraph-mcp        MCP server (15 tools, graph cache, coverage signals)
  codegraph-cli        the `codegraph` binary
```

**Design invariants:** single static binary · deterministic builds · precision-sacred resolution · heavy deps feature-gated (`indexstore`, `local-embed`, `media`).

## License

MIT OR Apache-2.0.

---

⭐ **If CodeGraph saves your agent's tokens (it will), star the repo** — it helps others find it.
