# Changelog

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
