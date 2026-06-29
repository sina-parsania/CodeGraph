# CodeGraph — Revised Implementation Plan (v3)

> All 6 blocking issues from the adversarial review are addressed. Each fix is annotated with the issue ID it resolves.

---

## Brand Invariants (Non-Negotiable)

1. **Precision sacred** — zero phantom edges. An edge in the graph means "we have a mechanistic justification." AMBIGUOUS resolution → edge dropped, never guessed. There is no opt-in imprecise tier; advisory edges that can be wrong violate unique-or-drop and are not permitted.
2. **Deterministic** — same commit → byte-identical canonical structural artifact across machines. Analytics (pagerank, betweenness) are same-machine deterministic (tested) and excluded from the cross-machine canonical artifact because they are derived/recomputable, not structural source-of-truth.
3. **Single static binary** — one `codegraph` binary: MCP server + CLI + indexer. No runtime deps, no language servers.

---

## Honest Callouts

- **Dynamic dispatch ceiling (all CHA tiers T0–T6)**: CHA records the *static/declared* type. A `Foo`-typed variable or field whose runtime value is a `Bar` subclass will record an edge to `Foo.method`. This is not a phantom edge — the declared-type call is real code — but it is a documented approximation. T5 (LocalInferredType) and T3 (FieldTypeMember) share this same ceiling.
- **Import narrowing (T6 / ImportNarrowed)**: a call whose callee name is unique within the file's explicit imports can be pinned to one target per import resolution. Falls back to AMBIGUOUS if the import is a wildcard or re-export.
- **RTA removed**: Rapid Type Analysis (WS5 in prior drafts) is eliminated. Whole-program RTA is not feasible in a single-pass tree-sitter context, and file-local instantiation promotion can emit phantom edges when the receiver's value originates outside the analyzed file. There is no brand precedent for an opt-in imprecise tier. WS5 is dropped entirely. *(Fixes blocking issue 1.)*

---

## Terminology

| Term | Definition |
|---|---|
| **justification tag** | `Edge.metadata["justification"]` string. Every CALLS edge must carry one (RESOLUTION.md:59 invariant). Current values: `SameFileUnique`, `SelfThisMember`, `StaticTypeMember`, `FieldTypeMember`, `GlobalUnique`. New: `LocalInferredType`, `ImportNarrowed`. |
| **ResolutionTier** | `{Scip, TreeSitter, Llm, Ingest}` — the SOURCE axis at `codegraph-core/src/types.rs:33`. Unchanged. Never extended with CHA variants. |
| **T0–T6** | Exposition labels for CHA resolution tiers used in this document. Implemented as justification tags, not enum variants. |
| **canonical structural artifact** | BTree-sorted serialization of `(nodes, edges, tier, justification, provenance)`. Excludes `pagerank`, `betweenness`, `community` (analytics columns — derived/recomputable, excluded from the cross-machine canonical artifact). |
| **same-machine analytics guarantee** | `analyze_is_deterministic` test at `graph/src/lib.rs:832` asserts `.to_bits()` equality for pagerank AND betweenness on the same machine. Must not regress. |
| **B1/B2/B3** | Foundational bugs listed in `docs/RESOLUTION.md:45`. B1 = class-qualified Method node IDs. B2 = MemberOf edges. B3 = Swift extension methods. Must ship in Phase 0 before WS1. *(Fixes blocking issue 2.)* |

---

## Current State (Shipped)

**CHA tiers live in `codegraph-graph/src/lib.rs build()` (lines 59–265).** `codegraph-resolve` is SCIP-import-only (192 lines total). All WS work touches `codegraph-graph`, not `codegraph-resolve`.

Shipped tiers and their justification tags:

| Tier | Tag | Description |
|---|---|---|
| T0 | `SameFileUnique` | Callee name unique in file |
| T1 | `SelfThisMember` | `self`/`this` → enclosing class member |
| T2 | `StaticTypeMember` | Named receiver whose type is statically declared |
| T3 | `FieldTypeMember` | `this.field.method()` via field→type map (`parse/src/lib.rs:318` TS gate) |
| T4 | `GlobalUnique` | Callee name unique across entire project |
| — | AMBIGUOUS | >1 candidate → drop |

**Content-hashing already shipped**: `index.rs:210` uses `sha256(&source)` as the staleness key. `index.rs:205` uses mtime only as a cheap stat pre-filter. Cross-machine determinism for node IDs is already satisfied: `project_name()` at `index.rs:392` returns the directory basename (not an abs-path hash), and node IDs are built via `QualifiedName::build(project, path_parts, name)` at `parse/lib.rs:254` using repo-relative path components throughout.

**Known foundational bugs (RESOLUTION.md:45)**:

| Bug | Description | Status |
|---|---|---|
| B1 | Method node IDs lack class qualification — `project.<dir>.<file>.<method>` — so two methods with the same name in two classes in the same file get identical IDs, causing merge/misattribution in the member-set lookup used by T1/T2/T3 | Pending — Phase 0 |
| B2 | MemberOf edges not emitted; class→method membership reconstructed heuristically from file co-location | Pending — Phase 0 |
| B3 | Swift extension methods not associated with the extended type's member set | Pending — Phase 0 |

**Measured corpus results (shipped T3)**:
- NestJS: 8751 → 10602 edges (+21%)
- iOS Swift: 9711 → 17872 edges (+84%)
- Android Kotlin: 3199 → 4104 edges (+28%)

---

## Sequencing

```
Phase 0: B1 + B2 + B3 fixes  (prerequisite — must ship before WS1)
WS9:  benchmark harness        (must precede WS1 to capture pre-WS1 baseline)
WS0:  canonical artifact + verify-determinism
WS1:  T5 LocalInferredType
WS2:  T6 ImportNarrowed
WS3:  TypeScript field gate completeness
WS4:  Kotlin/Swift field & local type coverage
WS6:  Personalized PageRank (query-time only)
WS7:  Coverage reporting enhancements
WS8:  CI golden-file test
```

**Why WS9 precedes WS1**: CP2's gate is "zero new false positives vs. pre-WS1 baseline." The harness that captures that baseline is WS9. If WS9 ships after WS1, there is no baseline to gate against. *(Fixes blocking issue 5.)*

---

## Phase 0 — Foundational Bug Fixes (Prerequisite)

*(Fixes blocking issue 2.)*

**B1 — Class-qualified Method node IDs**

**Problem**: `parse/lib.rs:252–253,272,293` builds Method node IDs as `project.<dir>.<file>.<method>`. Two methods with the same name in two classes in the same file get identical IDs. The member-set lookup in T1 (`SelfThisMember`), T2 (`StaticTypeMember`), T3 (`FieldTypeMember`), and the forthcoming T5 (`LocalInferredType`) all resolve methods against a class's member set; colliding IDs cause merge/misattribution — a precision violation.

**Fix**: change the ID construction path at `parse/lib.rs:252–253,272,293` to `project.<dir>.<file>.<ClassName>.<method>`. Requires:
- Tracking the enclosing class name at the point of Method node emission.
- Updating `QualifiedName::build()` call sites to pass the enclosing class segment.
- Migrating existing DB records (version bump on the DB schema; old DBs are rebuilt on next index).

**Limitation scope**: until B1 ships, WS1 and any other tier that resolves via class member sets must be scoped to the **one-class-per-file** case, where the interim containment map (file co-location heuristic) is exact. This limitation is stated in the WS1 section below.

**B2 — MemberOf edges**

**Problem**: class→method membership is reconstructed from file co-location heuristic, not explicit edges. When B1 ships and IDs are class-qualified, MemberOf edges can be emitted precisely.

**Fix**: emit `MemberOf` edges in `codegraph-parse` at Method node creation, using the enclosing class node ID as the target. These edges are structural (included in the canonical artifact), not analytics.

**B3 — Swift extension methods**

**Problem**: a Swift `extension Foo { func bar() }` in a separate file does not associate `bar` with `Foo`'s member set. T1/T2/T3 miss these calls.

**Fix**: in `codegraph-parse`'s Swift extractor, emit a `MemberOf` edge from the extension method to the extended type's node. Requires resolving the extended type name to its canonical node ID — use the same `GlobalUnique`-style lookup already available in `build()` but applied at parse time.

**Effort**: M (3–4 days total for B1+B2+B3).

---

## WS9 — Benchmark Harness (SCIP Oracle) — Ships Before WS1

*(Fixes blocking issue 4 and blocking issue 5.)*

**Goal**: reproducible precision/recall measurement for CodeGraph's call edges using compiler-grade SCIP indexes as an imperfect oracle.

**SCIP-as-imperfect-oracle methodology** *(fixes blocking issue 4)*:

SCIP indexes (`scip-typescript`, `sourcekit-lsp`) have real recall gaps — they miss some valid edges. Treating "CodeGraph edge not in SCIP" as a false positive/phantom would count SCIP's misses as CodeGraph's phantoms, producing an indefensible precision number.

The harness uses **sample-and-adjudicate**:
1. Compute the diff: edges in CodeGraph only (call these "CodeGraph-only" edges), edges in SCIP only, edges in both.
2. For the CodeGraph-only set, draw a random sample (n=50 per corpus, or full set if smaller).
3. A human adjudicator (or a secondary static analysis pass) classifies each CodeGraph-only edge as: `true_phantom` (CodeGraph guessed wrong), `scip_miss` (CodeGraph is correct, SCIP missed it), or `uncertain`.
4. Report:
   - **Precision lower bound**: TP / (TP + `true_phantom` count). This is the conservative bound.
   - **Precision upper bound**: (TP + `uncertain`) / (TP + `true_phantom` + `uncertain`). This is the optimistic bound.
   - **Recall** = TP / (TP + FN) where FN = SCIP-only edges. Recall is reported as-is because SCIP is assumed to be a recall ceiling, not a floor.
5. The harness emits a JSON report: `{ true_positive, false_positive_sample_adjudicated, scip_miss_sample_adjudicated, uncertain_sample, precision_lower, precision_upper, recall, scip_oracle_caveat }`. The `scip_oracle_caveat` field is always populated with a human-readable note that SCIP is an imperfect oracle.

**Corpus**: NestJS (TypeScript, SCIP via `scip-typescript`), iOS (Swift, SCIP via `sourcekit-lsp`).

**Pre-WS1 baseline**: run the harness after Phase 0 and before WS1. Store the baseline JSON in `benches/baseline/`. CP2 compares the post-WS1 harness output against this stored baseline and fails if `true_phantom` count increases.

**Effort**: M (3–4 days tooling + corpus setup + adjudication pass for baseline).

---

## WS0 — Canonical Structural Artifact + `verify-determinism` Subcommand

**Goal**: make determinism testable by external CI.

**What changed from prior draft**: Removed the false "SIMD/FMA variance" claim for analytics exclusion. *(Fixes blocking issue 6.)* The honest reason analytics are excluded from the cross-machine canonical artifact is that they are **derived/recomputable** from the structural graph — they are not source-of-truth structural data. The same-machine determinism of `page_rank()` (scalar f64, fixed 50 iterations, deterministic Vec-indexed order, no fast-math, no mul_add at `graph/src/lib.rs:603–655`) is preserved and tested.

**One genuine exception**: the betweenness approximation path at `graph/src/lib.rs:704–706` (pivot approximation, activated for graphs >1500 nodes) is not exercised by the current small test fixture and has not been verified for cross-machine bit-identical output. The `analyze_is_deterministic` test must be extended with a >1500-node synthetic fixture to cover this path. Until that test passes, betweenness on large graphs carries an untested cross-machine determinism claim and is excluded from the canonical artifact on that specific basis.

**Canonical artifact definition**:
- Include: all nodes (sorted by `id`), all edges (sorted by `src,dst,relation`), `tier` field, `metadata["justification"]`, `metadata["provenance"]`.
- Exclude: `pagerank`, `betweenness`, `community` (analytics — derived/recomputable; betweenness additionally has an untested large-graph code path).
- Serialize to canonical JSON (sorted keys), hash with SHA-256 → the artifact hash.

**Implementation** (`codegraph-cli/src/main.rs` + new `verify.rs`):
```
codegraph verify-determinism [--project <path>] [--out <hash-file>]
```
- Builds/loads the graph.
- Emits `structural_hash: <hex>` to stdout (or writes to `--out` for CI comparison).
- Exit 0 if hash matches a previously stored baseline; exit 1 if not (CI mode).

**New test**: `analyze_is_deterministic` extended to run on a synthetic fixture with >1500 nodes to exercise the betweenness pivot-approximation path at `graph/src/lib.rs:704–706`. Assert `.to_bits()` equality for betweenness output on that fixture (same-machine). If this test fails, the honest position is that large-graph betweenness is not deterministic and must remain excluded.

**Effort**: S (2–3 days). No parser or resolver changes.

---

## WS1 — T5: Local-Inferred-Type Resolution

*(Fixes blocking issue 3.)*

**Prerequisite**: Phase 0 (B1+B2+B3) must ship first. Until B1 ships, WS1 is scoped to the one-class-per-file case; a file containing two classes with a method of the same name falls through to AMBIGUOUS (not promoted). This limitation is documented in the coverage report.

**Goal**: resolve calls on local variables whose type can be inferred from their initialization expression without a compiler.

**Justification tag**: `LocalInferredType`.

**ResolutionTier**: unchanged (`TreeSitter`).

**Scope disambiguation rule — drop-not-guess** *(fixes blocking issue 3)*:

The parser captures `RawLocalBinding { var_name, inferred_type_name, scope_span }`. At call-site resolution in `build()`, the resolver applies:

1. Collect all `RawLocalBinding` entries for `var_name` within the enclosing function whose `scope_span` contains the call site's position.
2. **If the count is 0**: no local binding found — fall through to next tier.
3. **If the count is 1**: exactly one binding live at the call site — proceed with `inferred_type_name`.
4. **If the count is ≥ 2**: `var_name` has multiple in-scope bindings at the call site (shadowing, multi-branch, or re-assignment to a different type) — **DROP to AMBIGUOUS**. Never guess which binding is active.

This rule is the explicit "callee receiver name with >1 in-scope binding live at the call site → DROP" requirement. A negative test must accompany it (see WS8).

**Crate split**:
- **codegraph-parse** (`parse/src/lib.rs`): capture `RawLocalBinding { var_name, inferred_type_name, scope_span }` structs from constructor calls and type-annotated declarations. TypeScript, Kotlin, Swift, Python. Java deferred (more complex flow analysis required).
- **codegraph-graph** (`graph/src/lib.rs build()` lines 59–265): consume `RawLocalBinding` slice. For each `RawCall` whose receiver is `Named(var)`, apply the scope disambiguation rule above. If exactly one binding live at call site, resolve method against that class's member set. Tag `LocalInferredType`.
- `build()` signature grows: `build(nodes, calls, inherits, fields, local_bindings)`.

**Precision claim**: same static-type ceiling as shipped T3 (`FieldTypeMember`). A `Foo`-typed local whose runtime value is a `Bar` subclass records `Foo.method`. This is documented, not hidden.

**Effort**: M (3–5 days parser + 2 days resolver in `build()`).

---

## WS2 — T6: Import-Narrowed Resolution

**Goal**: a bare call to `foo()` where `foo` is imported from exactly one in-repo module can be pinned to that target.

**Justification tag**: `ImportNarrowed`.

**ResolutionTier**: unchanged (`TreeSitter`).

**Crate split**:
- **codegraph-parse**: capture `RawImport { alias_or_name, source_module }` per file for all 13 languages (partially present; extend).
- **codegraph-graph** (`build()` resolution loop): before falling through to GlobalUnique or AMBIGUOUS, check if the callee name appears in exactly one import and that import's source module is within the project. Resolve to that module's exported symbol. Tag `ImportNarrowed`.

**Wildcard/re-export guard**: `import * from '...'` or unresolvable barrel re-export chains → AMBIGUOUS. Never guess.

**Effort**: M (2 days parser + 2 days `build()`).

---

## WS3 — TypeScript Field Gate Completeness

**Existing gate**: `parse/src/lib.rs:318`:
```rust
if ctx.spec.name == "typescript" {
    ts_extract_fields(node, src, cls_id, fields);
}
```

**Goal**: extend field extraction to cover:
1. Constructor-assigned fields (`this.x = new Foo()` in constructor body).
2. Class field declarations with initializers (`x: Foo = new Foo()`).
3. Getter return types (`get x(): Foo`).

**Implementation**: extend `ts_extract_fields()` in `codegraph-parse`. No changes to `build()` — the existing T3 resolver already consumes `RawField` and applies `FieldTypeMember` justification.

**Effort**: S (2–3 days, parser-only).

---

## WS4 — Kotlin / Swift Field & Local Type Coverage

**Goal**: close the gap between TypeScript field coverage and the other top-corpus languages.

**Kotlin**: data class properties, `val`/`var` with explicit types, primary constructor parameters.

**Swift**: `var x: Foo`, stored properties, `@Published var x: Foo`. Extend `swift_extract_fields`.

**Justification tags**: existing `FieldTypeMember` for field-based resolution; `LocalInferredType` for `val x = Foo()` patterns (WS1).

**Effort**: S (2–3 days).

---

## WS6 — Personalized PageRank (Query-Time Only)

**Goal**: for `find_callers`/`impact_analysis` queries, bias PageRank toward the query seed node.

**Constraint**: must not regress `analyze_is_deterministic` at `graph/src/lib.rs:832`. WS6's personalized PageRank is computed at **query time** on the loaded graph — separate from index-time `LoadedGraph::analyze()`. Index-time analytics unchanged. Query-time result not written to DB, not part of canonical artifact.

**Implementation**: `personalized_page_rank(graph, seed_node_id, iterations: usize) -> HashMap<NodeId, f64>` in `graph/src/lib.rs`. Fixed iteration count for determinism. Called from MCP query handlers with the query's target node as seed.

**Effort**: S (2–3 days).

---

## WS7 — Coverage Reporting Enhancements

**Goal**: surface per-justification-tag coverage counts in the `Coverage` struct.

**Addition**: `coverage_by_justification: HashMap<String, usize>` — count of resolved edges per justification tag.

**Effort**: XS (1 day).

---

## WS8 — `verify-determinism` CI Integration + Golden-File Test

**Goal**: automated proof that the structural artifact hash is stable.

**Test harness** (`graph/tests/determinism.rs`):
- Index a fixed synthetic fixture (committed to repo).
- Run `verify-determinism` twice in the same process.
- Assert byte-identical structural hash.
- Assert `analyze_is_deterministic` does not regress (already existing).

**Mandatory negative tests** *(fixes blocking issue 3 — RESOLUTION.md proof-obligation 3)*:

Per `docs/RESOLUTION.md`'s proof-obligation 3, every new tier requires a drop-rule negative test. For WS1:
- **Shadowing negative test**: fixture with a function that has two `let x` bindings in two sub-scopes (one `x: Foo`, one `x: Bar`) and a call `x.method()` at a site where both are in scope. Assert the call is NOT resolved (no edge emitted with `LocalInferredType` justification for `x`).
- **Multi-branch negative test**: fixture with `if (cond) { let x = new Foo(); } else { let x = new Bar(); }` and a call `x.method()` after the branch. Assert no `LocalInferredType` edge for `x`.

**Additional invariants**:
- For each edge in the fixture, assert `metadata["justification"]` is present.
- Assert no edge with `confidence: Ambiguous` survives into the graph.
- Assert B1 coverage: two classes in the same file with a method of the same name produce two distinct node IDs.

**Effort**: S (1–2 days).

---

## Checkpoint Gates

| Checkpoint | After | Gate |
|---|---|---|
| CP0 | Phase 0 (B1+B2+B3) | Two classes in the same file with a method of the same name produce two distinct node IDs. MemberOf edges emitted. Swift extension methods associated with extended type. |
| CP1 | WS9 (baseline) | Pre-WS1 harness baseline JSON stored in `benches/baseline/`. Adjudication pass complete. |
| CP2 | WS0 | `verify-determinism` emits stable hash on two consecutive runs of the same fixture. `analyze_is_deterministic` passes. >1500-node fixture added and betweenness determinism tested on same machine. |
| CP3 | WS1+WS2+WS3+WS4 | WS9 harness: `true_phantom` count in post-WS1 adjudicated sample does not exceed pre-WS1 baseline. Shadowing negative test passes. Multi-branch negative test passes. |
| CP4 | WS6 | `cargo test analyze_is_deterministic` passes. Personalized PageRank not in structural artifact. |
| CP5 | WS8 | Golden-file hash matches on CI across 3 OS/arch combinations. All negative tests pass. |

---

## Scorecard

> **WS9 measures CodeGraph only.** Competitor cells are competitor self-reported claims, unverified by our harness. WS9 reports a precision bound range (lower/upper) reflecting SCIP's imperfect oracle status, not a single number.

| Dimension | Before WS1–4 | After WS1–4 | Notes |
|---|---|---|---|
| CodeGraph call-edge precision (NestJS) | WS9 baseline (lower bound, upper bound) | WS9 post (lower bound, upper bound) | Sample-adjudicated; SCIP-only edges classified as `scip_miss` or `true_phantom` |
| CodeGraph call-edge recall (NestJS) | WS9 baseline | WS9 post | +T5/T6 coverage expected |
| codebase-memory precision | — | — | Competitor self-report, unverified |
| graphify precision | — | — | Competitor self-report, unverified |
| qmd call-edge precision | — | — | Competitor self-report, unverified |
| `verify-determinism` hash stability | not shipped | WS0 ships it | Same commit → byte-identical across machines |
| Analytics same-machine determinism | tested (small fixture) | WS0 adds >1500-node fixture | Betweenness pivot path now exercised |
| Drop-not-guess invariant | T0–T4 | T0–T6 + shadowing-drop | Every CALLS edge has justification tag; shadowing → AMBIGUOUS |
| RTA advisory tier | n/a | eliminated | File-local promotion can emit phantoms; no opt-in imprecise tier permitted |

---

## File:Line Anchor Reference

| Fact | Location |
|---|---|
| `ResolutionTier` definition | `codegraph-core/src/types.rs:33` |
| `Edge` struct with `metadata: Metadata` | `codegraph-core/src/types.rs:57–68` |
| Justification axis definition | `docs/RESOLUTION.md:54` |
| Justification invariant | `docs/RESOLUTION.md:59` |
| Foundational bugs B1/B2/B3 | `docs/RESOLUTION.md:45` |
| CHA tiered resolver (`build()`) | `codegraph-graph/src/lib.rs:59–265` |
| `resolve_member()` helper | `codegraph-graph/src/lib.rs:15–43` |
| `page_rank()` — scalar, fixed 50 iter, no fast-math | `codegraph-graph/src/lib.rs:603–655` |
| Betweenness pivot-approximation path (>1500 nodes, untested cross-machine) | `codegraph-graph/src/lib.rs:704–706` |
| `analyze_is_deterministic` test | `codegraph-graph/src/lib.rs:832` |
| `.to_bits()` assertions | `codegraph-graph/src/lib.rs:842–843` |
| `LoadedGraph::analyze()` | `codegraph-graph/src/lib.rs:584` |
| TS field gate | `codegraph-parse/src/lib.rs:318` |
| Method node ID construction (B1 fix target) | `codegraph-parse/src/lib.rs:252–253,272,293` |
| `QualifiedName::build` | `codegraph-parse/src/lib.rs:254` |
| Content-hash staleness key | `codegraph-cli/src/index.rs:210` |
| mtime stat pre-filter | `codegraph-cli/src/index.rs:205` |
| Same-content refresh | `codegraph-cli/src/index.rs:212–214` |
| `project_name()` — basename, not abs-path hash | `codegraph-cli/src/index.rs:392` |
| DB path keyed by abs-path hash | `codegraph-cli/src/index.rs:75–78` |
| `codegraph-resolve` SCIP-import-only | `codegraph-resolve/src/lib.rs:1–192` |

---

## What This Plan Does NOT Do

- Does not extend `ResolutionTier`.
- Does not add typed enums for justification values — they remain string metadata keys.
- Does not claim any WS pierces dynamic dispatch — the static-type ceiling is documented across all CHA tiers.
- Does not ship an opt-in imprecise advisory tier — RTA is eliminated.
- Does not claim competitor precision/recall numbers — those cells are marked "competitor self-report, unverified."
- Does not modify `codegraph-resolve`.
- Does not move analytics out of the same-machine determinism guarantee — `analyze_is_deterministic` is preserved and extended.
- Does not include personalized PageRank in the canonical structural artifact.
- Does not claim false SIMD/FMA non-determinism — the honest exclusion reason for analytics is "derived/recomputable"; the one genuine exception (betweenness >1500-node path) is called out specifically and gated on a new test.