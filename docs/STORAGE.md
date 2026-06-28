# Storage design — why SQLite, one DB per project

A deep-research pass (four independent analyses + synthesis, grounded in the source)
asked: should the graph be a **DB, JSON, or another format**? One store or many?
The verdict is unanimous and decisive.

## Verdict: keep SQLite, one graph DB per project (in a central cache)

SQLite (rusqlite, bundled, FTS5, WAL) is the correct engine — **no alternative survives
this tool's constraints**: single static binary, no daemon, FTS5 + arbitrary read-only
SQL as first-class features, instant invalidation, ACID.

Measured ground truth (largest real repo, 23.5k nodes / 30.7k edges): **1.3s full index,
<10 ms cold queries, 0.15s no-op reindex**, DB ~50–90 MB (fully page-cache resident under
the 256 MB mmap window).

The decisive insight: **graph traversal is not served from the DB.** There are zero
recursive CTEs — callers/callees/blast-radius/path/PageRank all load the whole graph into
`petgraph` in memory and run BFS/A\*/Brandes there. So "do graph-native DBs beat SQLite?"
compares against something that doesn't exist; the real baseline is in-memory petgraph,
already optimal at this scale. SQLite's job is a blob/document KV + FTS5 + vector-BLOB
dump + read-only SQL — and it does all of that with guarantees that match every constraint.

### Why not the alternatives

| Option                                         | Why rejected                                                                                                                                                                                                                                                         |
| ---------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| JSON / flat-file / bincode as system-of-record | Regresses point queries (`get_node`/`search`/`callers`) from <10 ms indexed lookups to "deserialize the whole 25k-node graph first" — and the CLI opens fresh per invocation, so it can never amortize the load. Brittle untagged schema vs an additive JSON column. |
| Postgres / Neo4j                               | Break the no-daemon / single-static-binary constraint for zero benefit at 25k nodes.                                                                                                                                                                                 |
| RocksDB / LMDB / redb / sled                   | Force reimplementing FTS5 **and** losing arbitrary SQL, while gaining nothing (traversal isn't in the DB). sled is dev-stalled; RocksDB drags in a C/C++ toolchain.                                                                                                  |
| DuckDB                                         | Columnar OLAP is structurally wrong for point-lookup-by-id + tiny incremental upserts; heavy compute already lives in petgraph.                                                                                                                                      |
| KuzuDB                                         | Forces a Cypher rewrite that **breaks the arbitrary-SQL feature**, optimizes the one path that's already <10 ms, young on-disk format.                                                                                                                               |
| Global single DB / per-file shards             | Lose corruption isolation + per-project WAL concurrency. Cross-project queries are served on demand by read-only `ATTACH`.                                                                                                                                           |

### Topology: one DB per project (current)

Each project's graph **is** the unit of every expensive query, so there's nothing to shard
and everything to lose from a global DB (cross-project write serialization; one corrupt file
killing every project). "The MCP opens several DBs per session" is an argument **for**
per-project files (N independent WALs, zero cross-project lock contention). Cross-project
queries, if ever needed, use read-only `ATTACH` — no layout change. Vectors stay **in-file**
(atomic prune in the same transaction; single-file portability for `VACUUM INTO` + zstd export).

## Team correctness — a project has ~20 developers

A shared/concurrent project must never skew results or produce false positives. Four
independent mechanisms guarantee this:

1. **Deterministic build** — the same commit produces a byte-identical graph. Verified
   empirically on a 13.6k-node NestJS backend: two full re-indexes yield identical
   node + **community-id** + edge hashes. Parsing order is stable (rayon `collect` preserves
   input order), and Louvain / PageRank / betweenness iterate in node-index order with no
   RNG. → 20 devs indexing the same commit get the same graph.
2. **Central cache, never in the repo** — graphs live under `~/.cache/codegraph/<hash>/graph.db`
   (per-user), so they're never committed or shared. A teammate on another branch can't query a
   graph that doesn't match their tree. Each dev (or CI job) indexes their own checkout; the
   sha-256 incremental keeps that ~0.15s. `codegraph projects` lists them; `codegraph gc` reclaims.
3. **Snapshot-isolated reads** — WAL means a query during a reindex sees a consistent old
   snapshot until the writer commits, then the new one. Never a torn/partial state, because
   the entire index (parse → edges → analytics → FTS) runs in **one transaction**.
4. **No lock errors under concurrency** — `busy_timeout=5000` turns a reader/writer overlap
   (e.g., MCP query during a CLI reindex) into a brief wait instead of a hard `SQLITE_BUSY`.
   SQLite serializes the single writer; two concurrent reindexes of the same DB block, never
   corrupt.

## Implemented now (cheap, high-value)

- **Pragmas**: `busy_timeout=5000`, `temp_store=MEMORY`, `cache_size=-65536` (64 MiB).
- **Bug fix**: `delete_file_data` now prunes the `vectors` table for a file's nodes — previously
  renamed/removed symbols left orphaned embeddings that polluted semantic search and grew the DB.
- **Central cache** (`~/.cache/codegraph/`, keyed by project path) so repos stay pristine and a
  graph is never committed/shared (team-safety, above); legacy in-repo `.codegraph/` is auto-migrated.

## Roadmap (research-backed, deferred with concrete triggers)

- **LoadedGraph cache in the MCP server** — the headline perf win: memoize the built petgraph
  per (db_path, graph.db generation), invalidate on reindex, LRU-bound. Skips the all_nodes +
  all_edges + rebuild on every traversal call in a session.
- **Warm connection per project DB** in the MCP session (cache prepared statements) instead of
  `Store::open` per call.
- **Vector cheap wins**: store L2-normalized vectors (cosine → a single dot product), batch-fetch
  hits via `WHERE id IN (...)` instead of N+1. **Trigger for `sqlite-vec` (ANN)**: vector count
  past ~100k (when "any input" embeddings grow) — single C file, preserves the one-file model.
  Reject `sqlite-vss` and an mmap sidecar.
- **Incremental per-file FTS** (delete-by-file + reinsert) instead of full rebuild each reindex.
- **MCP project→db registry** + read-only `ATTACH` for opt-in cross-project queries.
- **JSON/typed-column de-duplication** + external-content FTS5 — worth doing before "any input"
  multiplies node counts, but it touches every read path: a follow-up, not a cheap-now change.

## Known boundaries (honest)

- The `betweenness` column is **approximate** for any project >1500 nodes (pivot-sampled; zeroed
  above 200k) — treat it as a heuristic, not exact centrality.
- An `index` globally serializes writes on its project DB (one transaction). Correct + fast for
  single-CLI use; two concurrent indexers of the _same_ DB block (never corrupt).
