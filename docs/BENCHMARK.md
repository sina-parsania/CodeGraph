# CodeGraph vs the field — benchmark & feature parity

Honest comparison of **CodeGraph** against (a) the **conceptual competitors** in the
same problem space — **Serena** (LSP-based coding-agent MCP) and **aider's repo-map**
(tree-sitter + PageRank context selection) — and (b) the tools it was built to
supersede: **codebase-memory-mcp**, **graphify**, **qmd**, **codebase-index**.

> Method: numbers for CodeGraph are measured on this machine (Apple Silicon, 8 cores)
> against real codebases. **Serena was run live** here (cold-index timing below);
> **aider's repo-map** is source-verified from `aider/repomap.py` (not executed — the
> source is the authoritative spec). `codebase-index` and `qmd` were run live; the
> rest are compared on documented capabilities. Gaps are called out, not hidden.

## Measured precision & who-references eval (v1.31, reproducible)

Everything in this section reproduces from `scripts/eval/` (pinned repo SHAs,
seeded sampling, SCIP ground truth). No other tool in this space publishes —
or can publish — a compiler-verified precision number for its own edges.

### `codegraph audit` — per-tier precision vs a compiler oracle

| repo (oracle) | ImportNarrowed | SameFileUnique | SelfThisMember | overall |
| --- | ---: | ---: | ---: | ---: |
| zod `v3.23.8` (scip-typescript) | **100%** (62/62) | 93.9% | **100%** (57/57) | **98.7%** |
| fastapi `0.115.0` (scip-python) | 91.1% | 97.0% | 82.4% | **93.8%** |
| private 55k-node polyglot monorepo (Xcode IndexStore) | — | 95.5% | **100%** | **95.0%** |

Numbers are LOWER BOUNDS (an oracle disagreement counts against us even when
the compiler bound a related-but-different target). The audit is also how we
find our own bugs: it measured the old unknown-receiver→global-unique fallback
at **27% precision** on fastapi — that tier was removed, not defended
(`qualified obj.method()` with an untypeable receiver now drops, and function
parameters shadow same-named free functions).

### who-references vs grep (68 questions, SCIP ground truth)

| tool | precision (answered) | recall | answer rate | avg bytes/answer |
|---|---:|---:|---:|---:|
| codegraph | **0.78** | 0.54 | 87% | **759** |
| grep -rn | 0.63 | **0.94** | 100% | 2,701 |

Read this honestly: the graph is the precise, 3.5×-cheaper first answer;
grep is the exhaustive fallback. That is exactly the contract the MCP server
teaches agents (`coverage.may_be_incomplete` + `_fallback`): use the graph,
corroborate with text search when coverage says the list is a lower bound.
The recall gap is dominated by call-returns (`z.string().email()`) — closable
per-repo by the auto-maintained SCIP tier, not by guessing.

## Verdict (v1.24, measured + source-audited)

Live 3-way runs on the same repos (binaries: codebase-memory-mcp v0.8.1, code-review-graph 2.3.6):

| | **CodeGraph** | codebase-memory-mcp | code-review-graph |
| --- | --- | --- | --- |
| cold index (14k-symbol backend) | **1.5–2.0 s** | 7.8 s (2.8 s small repo) | 11.1 s |
| binary / install | **~5 MB, single file** | 269 MB | pip + Python |
| ambiguous `callers create` | **44 pinnable candidates** + coverage | 79 callers silently merged across ALL same-name defs (`mcp.c:2888` unions by design) | common names blocklisted → 0 results |
| confidence honesty | justification tag on every edge + `may_be_incomplete` | hidden floor (0.006) | tiers are a dead schema column (never populated) |
| review/risk | multiplicative resolved-edge risk, `--md`, GH Action, TESTS edges, co-change | — | flat keyword bumps (+0.20 "validate") → gate unusable at defaults; flow recall 33 % (their FAQ) |
| cross-service | route hubs in ONE graph (334 HttpCalls edges, hubs across 3 projects) | multi-DB CROSS_* over guessed edges | — |
| Swift | compiler-grade via Xcode IndexStore (+171 %) | tree-sitter only | tree-sitter only |

Deliberately not chased: their 158 vendored grammars (we ship 13 + SCIP for the rest), 3-D visualization, audio/video ingest.

## 0. The real competitors — Serena & aider repo-map

These two share CodeGraph's space (give an LLM agent structural code understanding),
so position honestly: each does something CodeGraph does not, and CodeGraph does
things neither attempts.

### Serena (`oraios/serena`) — LSP-backed semantic edit/navigation

Serena drives **real language servers** (Pyright, rust-analyzer, gopls, tsserver, …
via its `solidlsp` client) so its find-references and definitions are **compiler-accurate**
(scope-, type-, overload-aware), and it can **edit** code (`rename`, `replace_symbol_body`,
`safe_delete`). That is genuinely beyond CodeGraph.

|                      | Serena                                                                                                                               | CodeGraph                                                                               |
| -------------------- | ------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------- |
| Reference resolution | compiler-grade via LSP (high recall + precision)                                                                                     | tree-sitter CHA, **precision-first, lower recall** (unique-or-drop); SCIP tier optional |
| Edits code           | ✅ rename/replace/move/safe-delete                                                                                                   | ⚠ coverage-gated `rename-symbol` (refuses when not provably safe)                                                                   |
| Per-language setup   | one language server **per language** (some need the toolchain on PATH)                                                               | **one static binary**, 13 languages, nothing else installed                             |
| Cold start           | **194.5 s measured live** to index a 1-file Python project (uv install + Pyright auto-download dominate); recurs per language server | a 2,189-file Swift project in **~1.3 s**, queries answer cold                           |
| Graph analytics      | ❌ none                                                                                                                              | ✅ PageRank · Louvain communities · betweenness · blast-radius · trace · routes         |
| Doc/PDF/web ingest   | ❌                                                                                                                                   | ✅                                                                                      |

**Where each wins:** Serena for _editing_ and _high-recall compiler-accurate references_ on a
language whose server is installed; CodeGraph for _zero-setup polyglot indexing_, _sub-second cold
queries_, and _whole-graph analytics_ (centrality, communities, impact) Serena has no concept of.
CodeGraph also ships coverage-gated `rename-symbol` (refuses rather than corrupts); Serena's LSP renames remain broader.

### aider repo-map — name-match PageRank for context selection

aider's repo-map (source: `aider/repomap.py`) parses defs/refs with tree-sitter `tags.scm`, builds an
**in-memory `nx.MultiDiGraph`** where edges link a referencing file to **every** file defining a
matching name (pure name-match, **no call resolution** — collisions are damped by a `×0.1`-if-defined-in-
\>5-files heuristic, not resolved), runs a **conversation-personalized PageRank**, and trims the top
symbols to fit `--map-tokens`. Its purpose is **dynamic, prompt-aware context selection**, not navigation.

|                     | aider repo-map                                              | CodeGraph                                                                                |
| ------------------- | ----------------------------------------------------------- | ---------------------------------------------------------------------------------------- |
| Edges               | name-match file→file (every definer), approximate by design | **resolved** CALLS edges (unique-or-drop; audited 99.4–99.9% call-present, zero phantom) |
| Ranking             | **PageRank personalized to the live prompt**, budget-fitted | **`context`**: personalized PageRank over RESOLVED edges, token-budgeted (query-seeded)         |
| Queryable graph     | ❌ transient, discarded after building the map              | ✅ callers/callees/impact/trace/implementers + SQL, persisted in SQLite                  |
| Compiler-grade tier | ❌                                                          | ✅ optional SCIP merge                                                                   |

**Where each wins:** aider for _prompt-aware, token-budgeted context ranking_ inside an edit loop (its
core competency — CodeGraph's `important` is static and not budget-fitted); CodeGraph for _precise,
queryable, persistent structural navigation_ (resolved callers/callees/impact/trace) aider's
ranking-only map never attempts. The honest gap aider exposes: CodeGraph's `context` now covers the personalized, budget-fitted selection (resolved-edge-based).

## 1. Live head-to-heads (same machine, same corpus)

### a. Symbol definition lookup — `AuthService` in a NestJS backend

| Tool           | Result                                                        | How                                                                               |
| -------------- | ------------------------------------------------------------- | --------------------------------------------------------------------------------- |
| **CodeGraph**  | `AuthService  Class  src/auth/auth.service.ts:66`             | AST parse — knows it's a **Class**, graph-connected (callers/impact one hop away) |
| codebase-index | 2 hits: `auth.service.ts:66` **+ a README.md false-positive** | ripgrep for a "definition keyword" — can't tell a class from a markdown snippet   |

CodeGraph returns the real definition with its **kind**, no documentation noise, and the
node is already wired into the call graph. Grep-based tools return text matches.

### b. Ambiguous cross-file resolution — the SCIP advantage

Two files each define `helper()`; `b.ts` imports the one from `a.ts` and calls it.

| Mode                         | `callees run`     | Verdict                                                     |
| ---------------------------- | ----------------- | ----------------------------------------------------------- |
| CodeGraph (tree-sitter only) | _(empty)_         | Honest — ambiguous name, refuses to guess (no phantom edge) |
| **CodeGraph + SCIP**         | `helper @ a.ts:1` | Compiler-grade — resolves to the **right** file, not c.ts   |

Proven against a **real `scip-typescript` index**. No other tool here imports SCIP.

### c. Search corpus

| Tool          | Corpus                   | Search modes                                      |
| ------------- | ------------------------ | ------------------------------------------------- |
| **CodeGraph** | **code + ingested docs** | lex (FTS5) · vec (embeddings) · HyDE · LLM rerank |
| qmd           | markdown only (70 docs)  | lex (BM25) · vec · HyDE · rerank                  |

CodeGraph carries qmd's entire hybrid-search arsenal **and** applies it to code plus a code graph.

## 2. Performance (measured, real-world repos)

### Index build (full, cold)

| Codebase           | Files | Symbols | CodeGraph |
| ------------------ | ----- | ------- | --------- |
| Python service     | 152   | 893     | **0.9s**  |
| TypeScript web app | 1,718 | 4,168   | **0.2s**  |
| Kotlin app         | 613   | 4,425   | **0.2s**  |
| NestJS backend     | 2,797 | 13,640  | **0.8s**  |
| Swift iOS app      | 2,189 | 23,492  | **1.3s**  |

Single static binary → SQLite. No server, no daemon. A Neo4j-backed graph
(codebase-memory) pays network + server round-trips on every ingest and query;
a ripgrep tool (codebase-index) skips the build but re-scans the tree on every call.

### Query latency (NestJS backend, 13.6k nodes, cold process each call)

| Query                                                                          | Latency     |
| ------------------------------------------------------------------------------ | ----------- |
| `search` / `callers` / `impact` / `implementers` / `important` / `communities` | **< 10 ms** |
| `routes` (full label scan)                                                     | ~100 ms     |

Every query opens the DB fresh and still returns in well under a tenth of a second.

## 2b. Token economy — honest, cross-language (`scripts/benchmark.py`)

How many **context tokens** an agent ingests to answer a navigation question, with
CodeGraph vs a **competent agent using ripgrep + bounded reads**. The baseline is
modelled per task kind (read the **one definition region** for "where defined"; read
**±5 lines around each hit** for "who calls") — **not** whole files. The whole-file
number is shown as a labelled naive upper bound only; the headline uses the bounded
baseline. Reproduce on any repo: `python3 scripts/benchmark.py --repo <path>`.

| Repo (lang)             | grep + bounded reads | CodeGraph | **headline** | whole-file upper bound |
| ----------------------- | -------------------: | --------: | -----------: | ---------------------: |
| CodeGraph (Rust, self)  |           24,822 tok |   290 tok |      **86×** |                   480× |
| `psf/requests` (Python) |           58,127 tok | 2,824 tok |      **21×** |                    96× |
| `spf13/cobra` (Go)      |          121,453 tok | 4,135 tok |      **29×** |                   109× |
| `colinhacks/zod` (TS)   |          707,399 tok | 7,075 tok |     **100×** |                   293× |

**Honest range: ~20–100× fewer context tokens** — lowest for plain definition lookups,
highest for who-calls / call-graph questions on large repos (where grep must read context
around many hits). We retired the old "332×" headline: it ran only on the small self-repo
and charged the grep baseline for reading **whole files**. The numbers above use bounded,
task-appropriate reads, the same model on every repo. And this only counts questions grep
_can_ answer — **impact/blast-radius, shortest-path trace, PageRank importance, and
communities** grep cannot answer at all without reading much of the tree.

> Coverage caveat (see Issue 1 / `RESOLUTION.md`): `callers`/`callees`/`impact` are precise
> but not exhaustive — each result now carries a `coverage` signal (`may_be_incomplete` +
> dropped count) so an agent treats a sparse list as a lower bound and falls back to search.

## 3. Feature parity matrix

✅ first-class · ➖ partial/indirect · ❌ absent

> **Live vs documented:** `qmd` and `codebase-index` were run **live, head-to-head** in the same session
> (Section 1 shows their real output). `codebase-memory` and `graphify` rows below are from their
> **documented capabilities**, not a live run — treat them as claims to verify, not measurements.

| Capability                            |   CodeGraph    | codebase-memory **(documented)** | graphify **(documented)** | qmd **(live)** | codebase-index **(live)** |
| ------------------------------------- | :------------: | :------------------------------: | :-----------------------: | :------------: | :-----------------------: |
| Multi-language code parsing           |   ✅ **13**    |                ✅                |            ➖             |       ❌       |       ➖ (3, grep)        |
| AST-precise symbol defs               |       ✅       |                ✅                |            ❌             |       ❌       |         ❌ (grep)         |
| Compiler-grade SCIP resolution        |       ✅       |                ❌                |            ❌             |       ❌       |            ❌             |
| Call graph (callers/callees)          |       ✅       |                ✅                |            ❌             |       ❌       |      ➖ (grep refs)       |
| Blast radius / impact                 |       ✅       |                ➖                |            ❌             |       ❌       |            ❌             |
| Shortest-path trace                   |       ✅       |                ✅                |            ❌             |       ❌       |            ❌             |
| Community detection (Louvain)         |       ✅       |                ❌                |            ❌             |       ❌       |            ❌             |
| Centrality (PageRank + betweenness)   |       ✅       |                ❌                |            ❌             |       ❌       |            ❌             |
| Inheritance / implements + hyperedges |       ✅       |                ➖                |            ❌             |       ❌       |            ❌             |
| HTTP route extraction                 |       ✅       |                ❌                |            ❌             |       ❌       |            ✅             |
| Arbitrary query language              |    ✅ (SQL)    |           ✅ (Cypher)            |            ❌             |       ❌       |            ❌             |
| Full-text search                      |   ✅ (FTS5)    |                ➖                |            ➖             |   ✅ (BM25)    |       ✅ (ripgrep)        |
| Semantic / vector search              |       ✅       |                ➖                |            ✅             |       ✅       |            ❌             |
| HyDE search                           |       ✅       |                ❌                |            ➖             |       ✅       |            ❌             |
| LLM rerank                            |       ✅       |                ❌                |            ➖             |       ✅       |            ❌             |
| NL Q&A over source                    |       ✅       |                ❌                |            ➖             |       ❌       |            ❌             |
| Doc ingest (PDF / web / text)         |       ✅       |                ❌                |            ✅             |    ➖ (md)     |            ❌             |
| Image OCR ingest                      |       ✅       |                ❌                |            ✅             |       ❌       |            ❌             |
| **Audio / video media ingest**        | ❌ _(roadmap)_ |                ❌                |            ✅             |       ❌       |            ❌             |
| Optional local LLM (no key)           |       ✅       |                ❌                |            ✅             |       ➖       |            ❌             |
| Incremental indexing (sha256)         |       ✅       |                ➖                |            ➖             |       ✅       |            n/a            |
| Single static binary (no server)      |       ✅       |            ❌ (Neo4j)            |            ❌             |       ❌       |        ❌ (Python)        |
| Standalone CLI **and** MCP            |       ✅       |             ➖ (MCP)             |            ➖             |       ✅       |         ➖ (MCP)          |
| Project-agnostic                      |       ✅       |                ✅                |            ✅             |       ✅       |    ❌ (repo-specific)     |

## 4. Where CodeGraph is #1

- **Languages** — 13, the widest set here.
- **Precision** — the only tool that does both AST parsing **and** compiler-grade SCIP import; the only one that _refuses_ to emit a guess rather than a phantom edge.
- **Graph analytics** — the only tool with community detection **and** PageRank **and** betweenness centrality.
- **Speed & footprint** — a single static binary, no server: 23k symbols in 1.3s, queries < 10 ms. Neo4j- and Python-backed competitors can't match the cold-start or the zero-dependency deploy.
- **Search breadth** — matches qmd's full lex + vec + HyDE + rerank stack and applies it to code, not just markdown.
- **Packaging** — the only one that is simultaneously a real installable CLI, an MCP server, and dependency-free.

## 5. Honest gaps (and how they close)

- **Audio/video media ingest** (graphify has it) — CodeGraph ships **image OCR** today; audio (whisper) + video (ffmpeg keyframes) are the gated `media` feature's next expansion. The seam exists.
- **Dedicated data-flow / cross-service _call_ tracing** (codebase-memory advertises modes for these) — CodeGraph offers `routes` + arbitrary SQL + shortest-path tracing, which cover the practical questions, but not a purpose-built data-flow analyzer yet.
- **Repo-specific helpers** (codebase-index: `find_migration_for_column`, multi-repo `monorepo_overview`) — deliberately out of scope; CodeGraph is project-agnostic. The same answers come from `query` + per-repo indexing.

**Bottom line:** CodeGraph is a strict superset of qmd and codebase-index, and beats
codebase-memory on languages, precision (SCIP), analytics, speed, and deployment. The
single capability another tool has that CodeGraph does not is graphify's audio/video
media ingest — already scoped as the next `media` expansion.
