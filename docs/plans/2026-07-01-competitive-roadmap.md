# CodeGraph — Competitive Roadmap (close the gaps, keep the brand)

**Sources studied:** `DeusData/codebase-memory-mcp` (pure-C, Hybrid-LSP, rich edges, Cypher,
nomic code-embeddings, 158 langs, scale) and `tirth8205/code-review-graph` (Python, change-aware
review, risk scoring, PR/CI integration, confidence-tiered edges, flow/knowledge-gap analytics),
plus our own audit.

**Brand invariants (do NOT break):** one static binary · deterministic (byte-identical graph) ·
**precision-sacred** (unique-or-drop is the DEFAULT; any looser edge is opt-in + clearly tagged, never
silent) · no mandatory server · zero-config core. Heavy deps stay feature-gated (like `indexstore`,
`local-embed`).

**Where we honestly stand:** on most axes both rivals are more feature-rich. Our real edge is
precision-honesty (`coverage` signal), Swift compiler-grade (IndexStore), safe editing, determinism,
simplicity. This roadmap closes the gaps that matter without abandoning the brand.

---

## W1 — Search quality (quick wins, ship first)

| #   | Item                                                                                                                                                                              | Why (who)                | Effort  | Notes / precision                                                                                                                                         |
| --- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------ | ------- | --------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1.1 | **camelCase/snake FTS5 tokenizer** — split `MealSenseCookSession`→`meal sense cook session` at INDEX time (custom FTS5 tokenizer or `trigram`), retire the query-side prefix hack | both (`cbm_camel_split`) | **S**   | subword/substring search works natively; keep `--regex` as the exact-pattern escape hatch. No brand impact.                                               |
| 1.2 | **Code-specific embeddings option** — offer `nomic-embed-code` (768-d, code-trained) alongside `bge-small`                                                                        | codebase-memory          | **S–M** | verify fastembed ships a nomic-code ONNX; else `nomic-embed-text`. Stamp `embed_model+dim` in the index → refuse/auto-reindex on mismatch. Feature-gated. |
| 1.3 | **Hybrid search ranking** — blend BM25 + vector + graph-proximity + git-recency into one score for `search`/`semantic`                                                            | both (11-signal / MRR)   | **M**   | reuse the `context` blend; deterministic weights.                                                                                                         |

## W2 — Richer edge model (medium, high agent value)

| #   | Item                                                                                                                                    | Why (who)                                        | Effort  | Notes / precision                                                                                                                                                  |
| --- | --------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------ | ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| 2.1 | **New precise edges**: `IMPORTS`, `USES_TYPE`, `MEMBER_OF`, `USAGE` (non-call references), `WRITES` (var writes), `TESTS` (test→symbol) | codebase-memory + code-review-graph              | **M**   | all unique-or-drop; extends our `justification`-tagged model. Big win: agents get "what imports X", "tests for X".                                                 |
| 2.2 | **Opt-in AMBIGUOUS tier** — keep dropped ambiguous edges, tag `confidence=Ambiguous`, off by default, filterable                        | code-review-graph (EXTRACTED/INFERRED/AMBIGUOUS) | **M**   | preserves "zero phantom by DEFAULT"; `--include-ambiguous` raises recall honestly (clearly labeled). We already have a `Confidence` enum + tier field — expose it. |
| 2.3 | **git co-change edges** `CHANGES_WITH` — from `git log` (files that change together), weighted by frequency                             | codebase-memory (`FILE_CHANGES_WITH`)            | **S–M** | cheap; great for impact/"what usually changes with this". Determinism: pin to a commit range in the graph meta.                                                    |

## W3 — Change-aware review (BOTH rivals have it; we don't — build it)

| #   | Item                                                                                                                                                                          | Why (who)                             | Effort  | Notes                                                             |
| --- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------- | ------- | ----------------------------------------------------------------- |
| 3.1 | **`changes` command** — `git diff` → affected symbols (via `impact`) + **test gaps** (changed symbols with no `TESTS` edge) + **risk score** (fan-in × centrality × no-tests) | both (`detect_changes`)               | **M**   | reuses blast-radius + W2.1 TESTS edges. Read-only, deterministic. |
| 3.2 | **PR/CI integration** — a `codegraph review --base <ref>` that emits a markdown risk report; a GitHub Action that posts a sticky PR comment with `--fail-on-risk` gate        | code-review-graph (GH Action)         | **M–L** | optional; ships as a separate action, not the core binary.        |
| 3.3 | **Token-savings metadata** — each MCP tool response reports approx tokens saved vs reading files                                                                              | code-review-graph (`context_savings`) | **S**   | UX/trust; cheap.                                                  |

## W4 — Recall: type-aware resolution (the BIGGEST lever)

| #   | Item                                                                                                         | Why (who)                    | Effort | Notes / precision                                                                                                                                             |
| --- | ------------------------------------------------------------------------------------------------------------ | ---------------------------- | ------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 4.1 | **Finish T5 local-var type inference** (started) + **T6 import-narrowed** (design done) across the top langs | codebase-memory (Hybrid LSP) | **L**  | resolve-or-drop — still zero phantom. Closes the "resolve `user.profile.name()` cross-module" gap that both rivals brag about, WITHOUT their false-edge risk. |
| 4.2 | **Per-language import maps** → cross-module unique resolution (TS/Py/Go/Java/Kotlin)                         | codebase-memory              | **L**  | the import IS the evidence → precise.                                                                                                                         |
| 4.3 | **Measure honestly** vs a SCIP/LSP oracle before/after; report recall per tier                               | our discipline               | **M**  | prove the gain; keep precision ~100%.                                                                                                                         |

## W5 — Analytics depth (cheap, differentiating)

| #   | Item                                                                                                         | Why (who)         | Effort  |
| --- | ------------------------------------------------------------------------------------------------------------ | ----------------- | ------- |
| 5.1 | **Dead-code detection** — functions with 0 resolved callers, minus entry points/exports/routes               | both              | **S**   |
| 5.2 | **Flow detection** — call chains from entry points (routes/main) ranked by criticality (betweenness × depth) | code-review-graph | **M**   |
| 5.3 | **Knowledge gaps** — isolated nodes, untested hotspots (high fan-in + no TESTS), thin communities            | code-review-graph | **S–M** |
| 5.4 | **Surprise/coupling** — cross-community & cross-language edges = architectural smells                        | code-review-graph | **S**   |

## W6 — Scale & team sharing

| #   | Item                                                                                                                                                                     | Why (who)                           | Effort | Notes                                                          |
| --- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------- | ------ | -------------------------------------------------------------- |
| 6.1 | **Streaming graph ops** — don't load the whole petgraph per query; page/stream callers/impact so we scale past ~100k symbols                                             | codebase-memory (28M LOC)           | **L**  | our current ceiling. Keep the MCP graph cache (already added). |
| 6.2 | **Team-shared graph artifact** — export/import a `zstd` SQLite snapshot (`.codegraph/graph.db.zst`) committed to the repo; clone → import → incremental, no full reindex | codebase-memory + code-review-graph | **M**  | determinism makes ours trustworthy to share.                   |

## W7 — Query & extensibility

| #   | Item                                                                                                                                  | Why (who)                       | Effort  |
| --- | ------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------- | ------- |
| 7.1 | **Cypher-lite** read layer over SQLite (`MATCH (a)-[:CALLS]->(b) WHERE …`) — more expressive than raw SQL for agents                  | codebase-memory (`query_graph`) | **L**   |
| 7.2 | **Custom languages via `languages.toml`** (extension→grammar+node-kinds) — add a language with no rebuild; built-ins never overridden | code-review-graph               | **M**   |
| 7.3 | **Multi-platform MCP auto-config** — `init` also wires Cursor/Windsurf/Zed/Continue/Codex, not just Claude Code                       | code-review-graph               | **S–M** |

## W8 — Media / breadth (low priority)

- 8.1 IaC nodes (Dockerfile/K8s) + notebook (`.ipynb`) parsing — codebase-memory / code-review-graph. **M**, niche.

---

## Sequencing (recommended)

1. **Quick wins first:** W1.1 (camelCase tokenizer) · W2.3 (git co-change) · W5.1 (dead-code) · W1.2 (nomic-code embed) · W3.3 (token metadata). Cheap, each closes a visible gap.
2. **Edge model + change-aware:** W2.1 (new edges incl. TESTS) → W3.1 (`changes`/risk) → W3.2 (PR action).
3. **The big recall lever:** W4 (type-aware resolve-or-drop) with W4.3 oracle measurement.
4. **Analytics depth:** W5.2–5.4.
5. **Scale/sharing/query:** W6, W7 as needed for large-monorepo users.

## Definition of done (per item)

- precision unchanged (unique-or-drop default; ambiguous only opt-in + tagged) · deterministic ·
  default binary links no new native dep (heavy stuff feature-gated) · measured before/after ·
  tests + clippy green · README/docs updated.

## Honest note

Even fully executed, this doesn't make us "decisively superior" on every axis — codebase-memory's
breadth (158 langs, cross-service, Cypher, 28M-LOC scale) and code-review-graph's review/CI polish are
real. It DOES close the gaps that matter for our niche (precision-honest code intelligence, Swift
compiler-grade, safe editing, serverless) and adds their best cheap ideas on top.
