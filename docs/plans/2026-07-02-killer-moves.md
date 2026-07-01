# CodeGraph Killer-Move List (ranked, synthesized from CBM source audit + CRG source audit + live benchmark)

**Strategic frame:** the live benchmark shows CodeGraph already wins on speed (1.5s vs 2.8s/11.1s cold index), footprint (small binary vs 269MB/Python), isolation, and — uniquely — coverage honesty. Neither rival can copy honesty quickly: CBM's architecture *requires* guessed edges (confidence floor 0.006, `bfs_union_same_name` lumping), and CRG's confidence tiers are a dead schema column (grep INFERRED → hits only README/FAQ). So the plan is: (a) weaponize honesty, (b) steal their two real user-facing wedges (PR review + search ergonomics), (c) publish the numbers.

---

## 1. `codegraph review` + GitHub Action (steal CRG's entire wedge, fix its flaw)
- **What:** `codegraph review --base <ref>` → JSON {per-node risk, review_priorities, test_gaps, affected entry points}; ship a composite GitHub Action: download single binary → restore cached index (cache key = lockfile hashes + schema version, their `action.yml:50-56` trick) → incremental update → sticky PR comment upserted via marker → optional risk gate.
- **Why it beats them:** CRG's action pays pip install + Python setup every run and their risk formula false-positives so hard the gate defaults OFF (flat +0.20 for any name containing "validate"/"request", +0.30 untested, overall = max → `handle_request` starts at ~0.55–0.75). CodeGraph: 5MB binary, 2s index, and risk computed over *resolved* callers instead of global-name-matched noise. Fix their formula: multiplicative terms, keyword hit counts only when combined with entry-point reachability; report the risk-vs-gap inconsistency they have (transitive tests in risk, direct-only in gaps, `changes.py:255` vs `:355`).
- **Rust:** `git2` for diff hunks → line-range overlap vs node spans (rusqlite query); risk = pure fn; markdown renderer ~200 LOC; action.yml + curl for the binary.
- **Effort:** M (~1 week incl. action).
- **Success:** action cold-run end-to-end < 20s on backend-app-scale repo (their FAQ: ~10s per 500 files build alone + pip); on a corpus of 20 real PRs, high-risk flag precision ≥ 70% (vs their gate being unusable at default thresholds).

## 2. Disambiguated ambiguity — candidate list + pinning ("we ask, they guess/refuse")
- **What:** on ambiguous name in `callers`/`trace_path`, return structured candidates (`file:line`, signature, span, per-candidate direct-caller count) and accept `qualified_name`/node-id to pin. Never silently union, never refuse.
- **Why:** the live benchmark caught all three philosophies on `create`: CBM silently traced ONE of dozens of definitions (`mcp.c:2888` deliberately discards the picked node and BFS-unions all same-name nodes — wrong merged answers by design, #546); CRG hard-blocklists common names → 0 results, no override. CodeGraph already warns (⚠ 184/323 resolved); adding pinnable candidates makes it the only tool that's both safe AND useful. This is the head-to-head demo.
- **Rust:** group query result by definition node before traversal; serde struct for candidate list; accept id param. Mostly MCP-layer.
- **Effort:** S (~2 days).
- **Success:** `callers create` returns per-definition candidates; pinned query returns exactly that definition's callers; zero silent unions in test corpus.

## 3. Per-node complexity metrics as columns, surfaced in MCP output
- **What:** port CBM's single-walk metrics (`helpers.c:586-660`): cyclomatic, cognitive (Campbell), loop_count/depth, access depth — as real columns (not JSON). Tier B: memoized DFS over CALLS → `transitive_loop_depth` + `recursive` flag. Surface in `get_node`/`important`/`context` with one-line explanations ("loop_depth 2, calls O(n) helper → O(n³) candidate").
- **Why:** CBM's best genuinely-clever feature, but buried in stringly-typed JSON their own passes parse with `strstr`. CodeGraph's transitive signal is *more trustworthy* because it propagates over resolved edges only — a guessed CALLS edge poisons their transitive loop-depth. Unlocks "find bottleneck/god-function candidates" queries neither rival exposes well.
- **Rust:** ~100 LOC in the existing tree-sitter walk (branch-node-type list per language); Tier B = one memoized DFS over the edge table at index time.
- **Effort:** S (Tier A) + S (Tier B), ~3 days total.
- **Success:** index-time overhead < 5% on backend-app; `important` output includes metrics; a "top 20 complexity hotspots" query returns in < 50ms.

## 4. Search: symmetric camel-split + two-step BM25 early-exit + label-tier boost
- **What:** (a) index the space-split form of identifiers alongside subword FTS (CBM's `cbm_camel_split`, `store.c:388-463`); (b) **also split the query** — CBM doesn't (`mcp.c:1527` — their asymmetry means `UpdateCloud` misses `updateCloudClient`); (c) their two-step trick: inner bare-FTS5 subquery `ORDER BY bm25() LIMIT ~2000` (WAND early-exit) then join/boost only those rows; (d) label boosts (Function +10, Route +8, Class +5).
- **Why:** flat worst-case query latency on huge repos + strictly better recall than CBM (symmetric split) + better ranking than CRG. Benchmark also showed CBM ranks `.cursor/*.md` docs first on generic terms — label-tier boost fixes exactly that class of noise.
- **Rust:** rusqlite FTS5; camel-split fn shared between insert and query paths; two-step is a SQL rewrite.
- **Effort:** S (~2-3 days).
- **Success:** `UpdateCloud` finds `updateCloudClient`; p99 search < 50ms on a 1M-node index; generic-term query ranks Functions above doc files.

## 5. Generalized entry-point/flow detection (attack CRG's 33% recall)
- **What:** entries = resolved route handlers (already have `routes`) ∪ zero-in-degree public symbols ∪ per-language tree-sitter registry queries (`http.HandleFunc`, NestJS decorators, Bull processors, `@app.get`, exported-uncalled in libs). BFS reachable sets over resolved edges + criticality score (start from their weights, `flows.py:308`). Feed into #1's risk ("this change touches 2 payment flows").
- **Why:** flows power CRG's risk + review UX and are their weakest tech — regex-only entry detection, 33% JS/Go recall by their own FAQ (`docs/FAQ.md:143`), benchmarked only against repos they curated. CodeGraph's resolved edges give deeper flows with zero phantom hops.
- **Rust:** per-language `.scm` query files + BFS over the edge table; criticality = pure fn.
- **Effort:** M (~1 week).
- **Success:** ≥ 80% entry-point recall on a published JS/Go/Python test set (vs their 33%); `review` output names affected flows.

## 6. `_hints` next-tool suggestions in MCP responses
- **What:** append `_hints` to every tool response: ring buffer of recent tool calls → intent classification (reviewing/debugging/refactoring/exploring) → suggest next tool from a static workflow-adjacency map (CRG's `hints.py`). Optionally include their `context_savings` tokens-saved number.
- **Why:** CRG's genuinely good ergonomic idea; agents actually follow these, lifting the perceived intelligence of a *small* 12-tool surface — exactly the discoverability crutch CRG built for its bloated 28-tool surface, but applied to a lean one.
- **Rust:** static `HashMap`, `VecDeque` per session, one serde field. 
- **Effort:** S (~1 day).
- **Success:** in agent-session replays, ≥ 30% of multi-tool chains follow a hint; zero latency impact.

## 7. Cross-project route/topic linking (`CROSS_*` edges)
- **What:** canonicalize HTTP client call paths (`/api/orders/{id}`) and async topics; match against Route/Channel nodes in other indexed project DBs; write bidirectional cross-edges (CBM's `pass_cross_repo.c` concept). `blast_radius` crosses service boundaries.
- **Why:** killer for exactly the monorepo user (an API service calling an internal ML service). CBM's version exists but sits on guessed call edges; ours sits on resolved routes — the answer is trustable.
- **Rust:** canonical-path table per project DB; join at index or query time; path templating normalizer.
- **Effort:** M (~1 week).
- **Success:** `blast_radius` on a client method lists the downstream service handler; zero false cross-edges on a real multi-service corpus.

## 8. zstd index artifact export/import
- **What:** `codegraph export` = `VACUUM INTO` temp + zstd → `graph.db.zst`; `codegraph import`. Wire into #1's action cache.
- **Why:** CBM's `artifact.c` recipe, cheap; enables "clone repo, download index, query in 2s" onboarding and CI cache. CRG dumps 224MB *inside the working tree*; CodeGraph's central relocatable cache + a portable artifact is the cleanest story of the three.
- **Rust:** `zstd` crate + rusqlite `VACUUM INTO`.
- **Effort:** S (~1 day).
- **Success:** backend-app artifact < 25MB; import→first query < 3s.

## 9. Publish the honesty + speed benchmark harness
- **What:** reproducible public harness: (a) phantom-edge count — sample N resolved `callers` claims per tool, verify against source; quote CBM's confidence-0.006 `suffix_match` single-winner edges and CRG's global-unique name matching with a permanently-DEFAULT confidence column (`graph.py:481-557`); (b) speed table from the live benchmark (cold 1.5s vs 2.8s/11.1s; warm 61ms; CRG re-parses files on no-change runs and full-FTS-rebuilds — so does CBM incrementally, `pipeline_incremental.c:658`); (c) the `create` ambiguity demo (item 2). Also fix/verify our own incremental FTS is delta-only before publishing.
- **Why:** converts architecture into a marketable number ("0 phantom edges vs N% guessed edges; 7× faster cold index than CRG"). Their weaknesses are verifiable in their own source — cite file:line.
- **Rust/infra:** scripts + markdown; mostly measurement.
- **Effort:** S (~2 days).
- **Success:** published repo anyone can re-run; three headline numbers (phantom %, cold index, warm reindex) with receipts.

## 10. Opt-in `INFERRED` edge tier (recall lever, honesty preserved)
- **What:** optional import-narrowed unique-name matching (what CRG does *always*, CBM does with guessing) producing edges tagged `INFERRED`, **excluded from `callers`/`blast_radius`/`review` by default**, includable via flag. Justification tags stay on every edge.
- **Why:** closes the recall gap on dynamic code without giving up the zero-phantom default; marketing: "every edge tells you WHY it exists — CRG ships the same three-tier story as a schema column no code ever sets."
- **Rust:** one more resolution pass reusing existing import maps; a tier column + query filters.
- **Effort:** S–M (~3 days).
- **Success:** with `--include-inferred`, `create`-class coverage rises measurably (e.g. 184/323 → >280/323 resolved-or-inferred) while default outputs remain phantom-free.

---

## Do NOT chase (hype, verified against their source)
- **Cypher-subset query engine** — 4.5K LOC in CBM; agents barely use it. Keep the small tool surface.
- **Aho-Corasick + LZ4 scanning** — dead code even in CBM's own tree (zero production callers; the "LZ4 HC compress" comment is stale).
- **Runtime trace ingestion** — CBM's is a stub returning "not yet implemented" (`mcp.c:4963`).
- **28-tool MCP surface / wiki generation / refactor-apply / suggested-questions tools** — CRG's long tail; their own hints system exists to paper over the bloat. Fold gap/surprise analysis into at most one `audit` tool later, using PageRank (already have) and real language fields instead of degree counts and file-extension "cross-language".
- **`strstr`-based test detection** — CBM's is buggy ("latest"/"attest" match "test"); use path/glob rules.
- **Static int8 token-embedding table** — CodeGraph already ships real bge-small; only borrow the int8 quantization idea if semantic-scan memory bandwidth ever measures as a bottleneck. Don't add 30MB to the binary on spec.
- **Leiden/igraph communities** — CRG couldn't even load igraph in the benchmark run (fell back to file-prefix grouping); low user-felt value per unit effort.

**Sequencing note:** items 2, 3, 4, 6, 8 are all S-effort (~1 sprint combined) and each is independently demoable; item 1 is the distribution play and should start in parallel; 9 lands the moment 1–2 are done to maximize the contrast.