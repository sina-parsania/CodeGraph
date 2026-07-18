# Changelog

## 1.38.0 — external-review hardening: gates that can't lie, config that can't be clobbered

All seven findings of an external production-readiness review, verified and
fixed in small commits:

- **Clippy was failing on test targets** (`--all-targets`): an always-true
  assertion (`is_stale(...) || true`) — now asserts what it meant (a fresh
  `.scip` flags the probe stale) — and an explicit `truncate(false)` on the
  lock-file probe (flock semantics: never truncate a possibly-held path).
- **release-qa.sh**: every gate now fails on the tool's own exit code
  (fmt --check, clippy --all-targets -D warnings, full tests) — no grep
  pipelines or `|| true` deciding pass/fail; e2e smoke sets a local git
  identity so the gate runs without global config.
- **`codegraph init` can no longer clobber user JSON**: invalid JSON, null/
  array roots, and non-object `mcpServers`/`hooks` fields error with
  file+field+found-type instead of being silently replaced with `{}`;
  success prints only after the written file is re-read and verified;
  merges are idempotent. 7 regression tests.
- **release.yml**: matrix jobs only build + upload artifacts; a single
  publish job validates all 5 artifacts (name/count/non-empty), creates a
  DRAFT, attaches, then publishes — a failed platform means no release,
  never a partial one. Least-privilege permissions (contents:read; publish
  escalates to write).
- **ci.yml**: `cargo fmt --all -- --check` gate (repo reformatted in one
  isolated commit) before clippy `--workspace --all-targets -D warnings`.
- **`build_with` decomposed** into typed phases (ResolutionIndexes,
  resolve_receiver/resolve_bare/resolve_call with a `Resolution` enum —
  Resolved/Ambiguous/Dropped makes unique-or-drop structural, and gated
  resolutions provably never fall through to the ambiguous tier),
  emit_defines/calls/inheritance, materialize. Semantics proven identical:
  pre/post binaries produce byte-identical canonical graphs on the pinned
  zod corpus.
- **Docs honesty**: binary size ~39 MB lean / ~55 MB with the bundled
  embedder (the "5 MB" claim was several releases stale); tool count fixed
  to 18 everywhere and pinned by a test against the live ToolRouter.

## 1.37.1 — rerank reaches the MCP

`search` (MCP) gains an optional `rerank` parameter — same local-LLM
reordering the CLI's `--rerank` had (config `rerank = true` /
`CODEGRAPH_RERANK=1` for the CLI default). The implementation moved to
`codegraph-llm` so both surfaces share one function; the LLM call runs on
`spawn_blocking` (the embedder-probe wedge class stays impossible), and it
degrades to the original order when no local model answers.

## 1.37.0 — lean MCP payloads, semantic that can't dead-end, MCP↔CLI parity

Driven by the third field report (v1.36 head-to-head vs ripgrep on a 5-project
monorepo — structural queries won; three defects found, all fixed).

- **MCP `routes` was unusable by its own client (232 KB single-line JSON,
  rejected by Claude Code)**: each route serialized the FULL graph node.
  Now: lean rows ({method, path, handler, file, line}) + `limit`/`offset`
  pagination + `path_prefix`/`method` filters; 560 routes ≈ 6 KB. Same lean
  sweep applied to every list-shaped MCP response that still leaked full
  nodes: `implementers`, `dead_code`, pinned `callers`. One shared `lean`
  serializer; full node detail stays behind `get_node(id)`.
- **`semantic_search` can no longer dead-end**: with no embedder it degrades
  to lexical search and says so (`"degraded": "lexical fallback — no
  embedder…"`) instead of hard-failing a tool it advertised. `stats` reports
  `embedder_available` up front so agents route around it. `install.sh` now
  builds with `local-embed` (bundled bge-small) by default — and
  `indexstore` on macOS — so the stock install answers semantic queries out
  of the box. `CODEGRAPH_NO_EMBEDDER=1` forces lexical-only (also the
  deterministic test hook).
- **MCP↔CLI naming parity**: `semantic-search`/`semantic_search` →
  `semantic`, `blast-radius`/`blast_radius` → `impact`,
  `trace-path`/`trace_path` → `trace`, `graph-query`/`graph_query` →
  `cypher`, `dead_code` → `dead-code` (joins the earlier `stats` → `status`).
  Agents translate MCP names to CLI constantly; every advertised name now
  resolves.
- Regression tests: routes payload cap (<25 KB on 300 routes) + pagination +
  filters, alias resolution for every MCP name, forced no-embedder
  degradation with `embedder_available:false` in stats.

## 1.36.0 — speed where it's visible + every fix is now a release gate

Driven by the second field report ("we should be 10–20× faster than grep,
we're tied"): the tie was the freshness probe, not the answer.

- **Probe parallelized**: the two root `git ls-files` spawn concurrently,
  nested-repo enumerations fan out on rayon, and the 8k-file stat sweep is
  parallel. Default-path query on an 8.7k-file monorepo: 0.21 s → 0.10 s —
  vs rg's 0.21 s *without* any freshness guarantee. Warm path (`--no-autoheal`
  / MCP between heals): 0.00–0.02 s — 10–100× rg. The honest framing: rg
  pays full scan per query; codegraph pays a freshness proof once and ~0
  after.
- **Constant-path routes recovered**: `@Delete(SOME_CONST)` endpoints are
  real — they now emit with the constant as a «symbolic» segment +
  `path_unresolved: true` instead of being dropped (66 real endpoints on the
  field monorepo; route accounting now closes exactly: 524 decorators =
  458 literal/bare + 66 symbolic).
- **`--indexstore` early-return bug**: with zero file changes the flag never
  reached the merge path — a re-merge no-op'ed. Fixed; field monorepo now
  carries a 46,110-edge Swift compiler oracle and `codegraph audit` measures
  **96.1% precision** (SelfThisMember 100%, SameFileUnique 96.6%,
  FieldTypeMember 83.3% on a small sample) — served via MCP `stats`.
- **`codegraph stats`** aliases `status` (agents guess the MCP tool name).
- **Doc noise capped**: identifier search keeps docs to ≤5 rows (one per
  file) when code answered the query.
- **Release gate**: `scripts/release-qa.sh` runs clippy-clean, the full
  suite, crash-recovery + determinism e2e, and the eval receipts — every
  field-reported bug fixed in 1.34–1.36 has a pinned regression test
  (enumeration classes, flock exclusion, MCP empty-graph/dead-root/
  generation-bump, `--files` machine contract, NestJS route shapes, DI
  narrowing, search ranking).

## 1.35.0 — field-test fixes: the MCP can't lie, NestJS answers

Driven by a promom field-test report (11.6k files, TS+Swift+Python) that
found two red bugs and three weaknesses. All fixed, all regression-tested.

- **MCP empty-graph bug (red)**: `codegraph init` wired the USER-GLOBAL MCP
  registration with one repo's absolute `--path` — the last-initialized repo
  won globally, and when that repo moved every project got a confidently
  empty graph ("no callers", nodes: 0) while the CLI was healthy. Fixed
  three-deep: registration is now cwd-following (no `--path`); a dead root
  refuses to serve at startup; and an EMPTY graph refuses to answer at all —
  every tool returns a diagnosis instead of clean emptiness (`stats` stays
  reachable and reports `EMPTY_GRAPH`). Regression suite proves a generation
  bump under a running server is served fresh on the next call.
- **NestJS routes (red)**: decorator routes (`@Get(':id')`, bare `@Get()`)
  never matched the leading-`/` requirement, and `@Controller('prefix')` was
  never joined (including the `@Controller(…)\nexport class` shape where the
  decorator hangs off the export_statement). e2e/spec files polluted the
  answer with test traffic. Object-form `@Controller({ path: 'x', … })` is
  read too, and a constant path (`@Delete(SOME_CONST)`) is skipped instead of
  fabricating "/" — a made-up path is a wrong answer, not a route. promom:
  120 noisy routes → 494 fully-prefixed real ones, zero spec noise, zero
  fabricated paths.
- **TS DI caller resolution (was ~0%)**: `this.profileService.getUserProfile()`
  with `constructor(private readonly profileService: ProfileService)` never
  resolved when the class NAME exists in two apps of a monorepo (global
  unique-or-drop). The caller file's import now disambiguates the type —
  same evidence class as ImportNarrowed, unique-or-drop preserved. promom:
  0/23 → fully resolved caller list (+465 edges). Swift resolution untouched
  (its tests pin the behavior).
- **search noise**: identifier search ranks ANY code symbol above Document
  fragments, and collapses doc hits to one row per file.
- **stale audit**: `stats` no longer serves an outdated audit under
  `measured_precision` — stale numbers move to `stale_audit_not_current`.
- **index lock → OS flock** (std `File::try_lock`, Rust 1.89 — zero deps):
  the kernel releases the lock the instant the owner dies, so the PID-stamp /
  dead-owner-steal machinery from 1.34 is deleted outright. Measured: kill -9
  mid-index → next query self-heals and answers in 69 ms.
- **callers --files**: unpinned name-level questions return the UNION across
  same-name definitions again (dominant-definition narrowing was a measured
  recall loss), definition files stay as labeled evidence, human notes moved
  to stderr. Eval receipts (SCIP ground truth, 68 questions): **P 0.66 /
  R 0.98 / answer-rate 100% / 319 B** vs grep 0.63 / 0.94 / 100% / 2,701 B —
  ahead of grep on every measured dimension. (The 1.33 "R 0.87" receipt was
  stale: the harness predated the v1.33 `--files` format.)

## 1.34.0 — zero-false-negative enumeration + ops hardening

Enumeration moved from directory-walking to `git ls-files` (tracked +
untracked-unignored), with the walker as fallback. This is not just faster —
it is MORE COMPLETE: git knows which files are tracked, and tracked beats
gitignore. Measured on a live monorepo: 26 real docs (SRS, bug
investigations) lived under a `docs/*` ignore pattern but were committed
anyway — the walker dropped them forever, the git tier indexes them.
Staleness probe on a 3.6k-file repo: no-op index 0.19s.

- **Nested plain repos** (monorepo of independent .git checkouts, no
  .gitmodules): ls-files silently skips their subtrees — untracked dirs
  carrying a `.git` are enumerated recursively; any level that can't take
  the git path falls the whole enumeration back to the walker.
- **`.codegraphignore` applied on the git listing** (root-level, cascades
  into nested repos) — no more walker fallback just because the file exists.
  Submodules (`.gitmodules`) still force the walker: dropping submodule
  symbols would be a false negative.
- **Walker parity**: `hidden(false)` — dot-dirs carry real content
  (`.claude/` agent docs, `.github/`), and git enumerates them; junk
  dot-dirs stay excluded via EXCLUDE_DIRS.
- **Engine-version gate**: the rebuild stamp now includes the release
  version alongside PARSER_VERSION — every upgraded binary rebuilds
  automatically; a resolver change nobody remembered to stamp can no longer
  serve a stale-engine graph.
- **Cross-process index lock**: MCP server, its watcher thread, and parallel
  CLI runs serialize on a PID-stamped lock file; dead owners are stolen
  instantly (`kill -0` liveness + stale-age window), so a `kill -9` mid-index
  never bricks the next query — verified live: the orphaned lock was stolen
  by the very next search, which self-healed and answered.
- **Binary sniff**: a NUL byte in the first 8KB (generated blobs with code
  extensions — valid UTF-8) keeps the file manifested but contributes zero
  symbols.
- **Ambiguous-candidate ranking** (MCP callers): strongest resolved evidence
  first, cross-language ties broken toward the language family the textual
  evidence lives in. Ranking only — never changes which edges exist.
- **`CODEGRAPH_MCP_CONCISE=1`**: drops per-response coaching fields
  (`_hints`, explainer notes); coverage/`_fallback`/truncation notes always
  stay. Measured 265→166 B on a callers answer.

Receipts: branch-switch storm (150 files) heals partially in 0.12s with
exactly reversible node counts; git-vs-walker parity proven on an identical
tree (1200 == 1200 nodes); `verify-determinism` byte-identical on a 3.6k-file
repo; zero phantom edges on 71k- and 25k-edge live graphs.

## 1.33.1 — token diet

Measured on a live monorepo session: `search` returned FULL node JSON — for a
Document hit that meant its entire text (23.5 KB for one query); resolved
caller rows each carried an ~80-byte qualified id nobody acts on. Now: search
returns compact rows (Documents get a one-line preview; `get_node(id,
snippet=true)` is the drill-down), resolved caller/callee rows drop the id
(kept where actionable: ambiguous candidates for pinning, search hits).
search 23.5→3.9 KB (6×), 62-caller answer 17.3→9.1 KB.

## 1.33.0 — every name answers

Driven by a 63-name grep-vs-graph differential harness on a private polyglot
monorepo: for every REAL usage pattern found only by grep, either the graph
now answers or the gap is a verified non-usage (string/docstring mentions).

- **type_refs**: every type NAME a file references (generic args, array/
  optional elements, return types, annotations) and Capitalized member-access
  bases (`ERROR_CODE.X`, `Foo.shared`) are recorded per file — evidence for
  `type_usages`, never resolution input.
- **type_usages** (callers, CLI + MCP): definition · DI/typed field · typed
  local · import · subtype · type reference · static member access · doc
  mention — a DI'd NestJS service or a Kotlin object no longer reads as
  "unused". `--files` keeps the machine contract (usage sites only).
- **baseUrl-style TS imports** (`from 'src/models/x'`) resolve root-relative,
  gated on real top-level directories so packages stay external; **barrel
  re-exports** (`export { X } from './y'`) bind like imports; **Kotlin
  imports** extracted.
- Document CONTENT is FTS-indexed (schema v7): localization keys
  (`.strings`), wiki/docs text — searchable with bm25 column weights.
- Eval receipts unchanged: P 0.73 / R 0.87 / answer-rate 99% / 247 B.

## 1.32.0 — every name answers: types, variables, doc keys, files

Driven by a 300-check stress harness over a real polyglot monorepo (functions,
classes, files, folders, variables, localization keys × swift/ts/kotlin/python)
— every failure class it found is fixed:

- **Type-usage layer**: `callers` on a class/interface (NestJS DI services,
  python DTOs) now surfaces WHERE the type is used (DI fields, typed locals,
  imports incl. python dotted modules, subtypes) — previously answered
  "no call sites reference this name ✓" for services injected everywhere.
- **Document content is searchable** (schema v6): FTS gains a `doc_text`
  column — Swift `.strings` localization keys, wiki/docs text. bm25 column
  weights keep definitions above docs. Existing graphs migrate on open.
- **Search answers dotted/hyphenated names**: AND-of-prefixes fallback before
  OR, plus a verbatim re-rank (exact name / filename stem / doc containing the
  raw query) — `feature-discovery.controller` and
  `gem.preparation.failed_to_load` now hit their targets first.
- **Variables answer**: `search` returns field/property declarations
  (name: Type file) from the fields/locals tables — variables aren't graph
  nodes, but "where is X declared" deserves a real answer.
- semantic-index: chunked embedding (flat memory, progress, resumable) and a
  12× embedder memory fix (16.8 GB → 1.4 GB peak, 4× faster).
- MCP: exhaustive-listing affordance advertised (graph_query high-LIMIT filter).

## 1.31.1 — release-pipeline fixes

- macOS arm64 binary carried ONLY the CI runner's Xcode rpath and crashed with
  "Library not loaded: libIndexStore.dylib" on user machines — the standard
  /Applications/Xcode.app toolchain rpath is now added post-build (re-signed).
- The release workflow creates the GitHub release if a bare tag push has none
  ("release not found" broke every v1.31.0 asset upload).

## 1.31.0 (continued) — the layered-answer contract

Driven by a reproducible eval (68 who-references questions, SCIP ground
truth) that exposed resolved-only answers as recall-starved (0.54): agents
need both truth AND coverage. Every graph answer now follows one contract —
**precise layer + labeled textual layer + coverage + `_fallback`**:

- `callers`: compact resolved rows + `unresolved_call_site_files` — parser-
  verified call tokens, evidence-filtered (local-def shadowing, external-
  import binding, nearest-definition attribution for same-name defs). CLI
  `--files` prints the dominant definition's layered file list.
  Eval: recall 0.54→0.87, answer rate 87→99%, bytes/answer 759→227.
- `callees`: compact rows + `unresolved_calls` (in-repo-plausible dropped
  names); `blast_radius`: compact rows capped at 200 + `total_affected`.
- **Definition-first search**: `search_fts` was UNRANKED (rowid + LIMIT could
  drop the actual definition below tests that mention it) — now bm25 +
  definition-label boost + exact-name boost + test-path tiebreak penalty.
- New MCP `architecture` tool (one-call repo orientation: counts, languages,
  communities with connected key symbols, routes, measured precision) and
  `get_node(snippet=true)` (the symbol's exact source span, never a whole
  file). MCP tool count: 17 → 18.
- Resolver precision (audit-driven): unknown-receiver calls never bind by
  repo-wide name uniqueness (measured 27% precision — removed); function
  parameters shadow same-named free functions in all 13 languages.

## 1.31.0 — the trust & recall leap

Motivated by a real-world audit on a private polyglot monorepo (~8.6k files,
4 languages) that exposed dangling edges, a 10% TypeScript resolution rate,
noisy rankings, and unproven precision claims. Everything below is measured,
not asserted.

### Correctness (zero-phantom, now enforced)
- **Fixed**: reused compiler-grade edges (IndexStore/SCIP) could reference
  nodes removed by a reparse → dangling edges committed to the graph. Reuse is
  now endpoint-filtered, and `drop_dangling_edges` auto-heals before every
  commit (15 healed → 0 on the discovery repo).
- **Graph identity stamp**: every graph records its `repo_root`; every command
  refuses to answer from a graph built for a different repo or written by a
  foreign tool into the cache slot.
- **`PARSER_VERSION` gate**: a binary with different parse behavior forces a
  full reparse — one graph never mixes two parser generations.

### Recall (evidence-based, still unique-or-drop)
- **tsconfig `paths` aliases** resolved (extends chains incl. node_modules
  packages, JSONC, baseUrl, Angular-style variant configs, nearest-ancestor
  scopes, content-hash invalidation). ImportNarrowed edges 3.8× on the
  discovery repo.
- **TS**: arrow-function/`const` components are first-class Function nodes;
  locals typed from `new Foo()` initializers; generic (`Foo<T>`) and
  `| null`/`| undefined` annotations unwrap to their base.
- **Kotlin**: primary-constructor `val`/`var` properties feed the DI tier;
  local inference (`val x = Foo()`, lambda-transparent scoping); inheritance
  extraction → CHA. **Swift**: inheritance extraction → CHA (edge kind
  corrected against the resolved target's label).
- Result on the discovery repo: TS +25%, Kotlin +27%, inherits edges 570 → 1531.

### Honesty (measured, not claimed)
- **`codegraph audit`**: samples tree-sitter edges (rebuilt in memory, so the
  compiler-merge can't bias the sample) and verifies them against the SCIP /
  IndexStore oracle in the graph; per-tier precision stored in meta, quoted in
  MCP `stats` and `report`, labeled with oracle languages and lower-bound
  semantics.
- **Honest coverage denominators**: call sites bound to external-package
  imports (incl. namespace imports) or naming no in-repo definition are
  excluded — `may_be_incomplete` now means something.
- **Navigation Protocol** in the MCP server: evidence classes instead of
  numeric confidence; `_fallback` grep patterns whenever a precise answer may
  be incomplete.

### Signal
- `important` now ranks by **hub score** (`ln(1+fan_in)×ln(1+fan_out)` +
  PageRank tiebreak) over real code symbols — utility sinks and their
  mass-inheriting helpers no longer crowd out the actual core.
- Flows/report labels disambiguate same-named entries with container +
  `file:line`; `$200`-style generated names are dropped at parse.

### SCIP lifecycle
- The SCIP tier is **sticky**: full rebuilds replay persisted compiler edges
  (endpoint-filtered); once opted in (`codegraph scip`), a moved HEAD re-runs
  the detected indexer **in the background** (never blocks a query) and the
  staleness probe merges the fresh index when it lands. `CODEGRAPH_AUTO_SCIP=0`
  opts out. A merely-present `index.scip` no longer forces full rebuilds.

### Tooling
- **Golden fixtures**: per-language integration suites for all 13 languages
  asserting required edges (with tiers) and forbidden edges.
- **`scripts/eval/`**: reproducible who-calls benchmark against SCIP ground
  truth (pinned OSS repos), scoring codegraph vs a grep baseline.
