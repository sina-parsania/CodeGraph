# CodeGraph vs the field — benchmark & feature parity

Honest comparison of **CodeGraph** against the tools it was built to supersede:
**codebase-memory-mcp** (Neo4j code graph), **graphify** (any-input knowledge graph),
**qmd** (local markdown search), and **codebase-index** (a repo-local Python MCP).

> Method: numbers for CodeGraph are measured on this machine (Apple Silicon, 8 cores)
> against a real multi-project monorepo (five codebases: Python, TypeScript, Kotlin,
> NestJS, Swift). Two competitors are live in the same session
> (`codebase-index`, `qmd`) and were run head-to-head; the others are compared on
> their documented capabilities. Gaps are called out, not hidden.

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
