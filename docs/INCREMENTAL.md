# Incremental indexing — wave-propagation invalidation (schema v5)

How an edit reindexes in O(impact) instead of O(repo), without weakening the
determinism guarantee (`verify-determinism` still byte-identical).

## The invariant

A call site's resolution depends on exactly two things:

1. **Global tables** built from the whole repo: name-uniqueness sets
   (`fn_by_name`, `class_by_name`, `node_by_name`), the class hierarchy
   (inherits), class membership (node ids/labels), and typed fields — all
   **keyed by name**.
2. **File-local inputs** of the *call's own file*: its imports (T6) and its
   typed locals (T5).

Because every global table is name-keyed, a change to a definition can only
affect calls **naming** that definition. That is the wave.

## File shapes and the change classifier

`Store::file_shape(file)` reads the file's observable contribution from the
live tables *before* its rows are deleted; `parsed_shape(ParsedFile)`
(crates/codegraph-cli/src/index.rs) builds the same structure from the fresh
parse. Each changed (or pruned) file classifies as:

- **body-only** — shapes equal. Contributes nothing; only its own edges
  re-resolve (line numbers, bodies, calls, locals, imports are not part of the
  shape by construction).
- **definition change** — only the Function/Method id→name map differs
  (add / remove / rename / move-between-classes; ids encode class nesting).
  The differing names become **dirty names**.
- **beyond functions** — classes, interfaces, enums, routes, doc titles,
  inherits, or typed fields differ → **full rebuild** (these feed CHA tables
  whose effects aren't name-local).

## The wave

wave = changed files ∪ { files with a call site naming a dirty name }

found via the indexed raw-calls table (`idx_calls_callee`) — a BFS over the
impact radius, never a repo scan. `codegraph_graph::resolve_files` then runs
the **same** `build_with` code path (byte-identical edges by construction)
with the full node/inherit/field/local/import set but only the wave files'
calls, filtered to edges originating in wave files. The indexer:

1. `DELETE FROM edges WHERE src_file = ? AND tier = 'TreeSitter'` per wave
   file — compiler-grade edges (tier `Scip`: SCIP imports, Xcode IndexStore)
   are spared; they merge on their own cadence. Pruned files lose **all**
   their edges (every tier is stale for a deleted file).
2. inserts the fresh edges with `ON CONFLICT DO NOTHING` (a surviving
   compiler edge outranks the tree-sitter resolution);
3. leaves hyperedges untouched.

Additional full-rebuild fallbacks (soundness guards):

- a dirty name appears in any inherit clause — name-uniqueness could flip
  INHERITS/IMPLEMENTS edges and hyperedge membership;
- fresh index (no prior manifest), `--full`, a SCIP file, `--indexstore` or a
  fresh Xcode IndexStore build, an explicit `--ambiguous` flip.

`IndexStats.partial` reports which path ran (`… changed, partial edge
rebuild` in the CLI output). `wave_rename_matches_full_index` proves the wave
result is byte-identical to a from-scratch index.

Analytics (community, PageRank, betweenness, fan-in/out) are still computed
over the full graph — they are global by definition — and persisted via
targeted `UPDATE … json_set` statements, not a full node rewrite.

## Canonical ordering (determinism)

Incremental updates re-insert changed rows at new rowids, so **graph loaders
must not depend on rowid order**: `graph_nodes()` / `graph_edges()` use
`ORDER BY id` / `ORDER BY src,dst,relation`. Community re-labeling and float
accumulation follow construction order; without canonical ordering, an
incrementally-updated graph and a fresh index would differ in community ids
(caught by `partial_edge_rebuild_matches_full_index`).

## Always-warm freshness (MCP)

Three layers keep query results current without the agent ever paying for it:

1. **FS watcher** (`spawn_fs_watcher`, codegraph-mcp): the MCP server watches
   the repo (debounced 400 ms, build/dep dirs filtered) and heals the index in
   the background — by the time the next tool call arrives, `maybe_refresh` is
   a no-op. Best-effort: if the watcher can't start, per-query self-heal works
   exactly as before.
2. **Auto-embed** (`auto_embed_changed`, codegraph-cli): after a committed
   index, the changed nodes are re-embedded so `semantic_search` stays as
   fresh as the graph. Opt-in by having run `semantic-index` once (that stamps
   the model); skipped loudly on model mismatch or no reachable embedder —
   two embedding spaces must never mix. Runs outside the write transaction;
   large batches (> 2000 symbols) defer to the explicit `semantic-index`.
3. **`context` snippets**: each ranked symbol carries its signature line
   (read once per file, budget-accounted), so orientation costs one tool call
   instead of one call plus N file reads.

## Cache invalidation

`meta.generation` is a monotonic counter bumped once per committed index.
The MCP server keys its graph-snapshot cache on it (mtime alone has 1-second
granularity on some filesystems).

## Search & vectors (v5)

- **FTS**: `nodes_fts` is an external-content FTS5 table over `nodes` kept in
  sync by `AFTER INSERT/DELETE/UPDATE OF name,parts,label,language` triggers —
  no manual FTS bookkeeping anywhere; `parts` (subword split) is a real column
  computed by the SQL scalar `cg_subwords` at write time. `rebuild_fts` is the
  fts5 `'rebuild'` command, needed only after migrations.
- **Vectors**: embeddings live in `vec_nodes`, a sqlite-vec `vec0` virtual
  table (`knn()` does indexed K-nearest-neighbor; stored L2-normalized so L2
  order == cosine order, reported score = 1 − d²/2). The v5 migration moves
  rows from the legacy `vectors` blob table and drops it. A dimension change
  (different embedding model) rebuilds the table — `semantic-index` re-embeds
  everything on model switch anyway.

## Why the parser is a spec-driven walk, not tree-sitter `.scm` queries

Evaluated (again — see RESOLUTION.md for the earlier stack-graphs study) and
rejected: the extraction logic that matters is *context-sensitive* — `this`
rebinding across non-arrow function literals, receiver classification,
enclosing-class tracking, Swift subscript/IIFE guards, C declarator chains,
TS parameter-properties. Tree-sitter's query API matches subtrees without
ancestor context, so each `.scm` pack would still need the same Rust context
walk around it — two mechanisms instead of one, with 13 languages of
regression risk and no user-visible gain. The per-language `LangSpec` table
(crates/codegraph-parse) stays the single extension point.
