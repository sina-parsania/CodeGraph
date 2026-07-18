//! Persistent SQLite store for the CodeGraph knowledge graph.

pub mod cypher;

use std::path::Path;

use codegraph_core::{
    Coverage, Edge, Hyperedge, HyperedgeMember, InheritKind, Metadata, Node, NodeLabel, RawCall, RawField,
    RawImport, RawInherit, RawLocal,
};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("serialization: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Msg(String),
}

type Result<T> = std::result::Result<T, StoreError>;

const SCHEMA_VERSION: i64 = 7;

/// Register the sqlite-vec extension for EVERY connection this process opens
/// (auto_extension is process-global). Powers the `vec_nodes` KNN virtual table.
fn register_vec_extension() {
    type InitFn = unsafe extern "C" fn(
        *mut rusqlite::ffi::sqlite3,
        *mut *mut std::os::raw::c_char,
        *const rusqlite::ffi::sqlite3_api_routines,
    ) -> std::os::raw::c_int;
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<*const (), InitFn>(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    });
}

/// FTS index as an EXTERNAL-CONTENT fts5 table over `nodes`, kept in sync by
/// triggers — no manual per-file FTS bookkeeping in the indexer, and a `DELETE
/// FROM nodes` can never leave orphaned FTS rows. `parts` is a real (generated
/// at write time) column on `nodes` so the content mapping is total and
/// `INSERT INTO nodes_fts(nodes_fts) VALUES('rebuild')` works.
/// The UPDATE trigger fires only on the indexed columns, so analytics updates
/// (community/pagerank/betweenness/data) don't churn the FTS index.
const FTS_DDL: &str = "
    CREATE VIRTUAL TABLE IF NOT EXISTS nodes_fts USING fts5(
      name, parts, doc_text, label, language, content='nodes', content_rowid='rowid');
    CREATE TRIGGER IF NOT EXISTS nodes_fts_ai AFTER INSERT ON nodes BEGIN
      INSERT INTO nodes_fts(rowid, name, parts, doc_text, label, language)
      VALUES (new.rowid, new.name, new.parts, new.doc_text, new.label, new.language);
    END;
    CREATE TRIGGER IF NOT EXISTS nodes_fts_ad AFTER DELETE ON nodes BEGIN
      INSERT INTO nodes_fts(nodes_fts, rowid, name, parts, doc_text, label, language)
      VALUES ('delete', old.rowid, old.name, old.parts, old.doc_text, old.label, old.language);
    END;
    CREATE TRIGGER IF NOT EXISTS nodes_fts_au AFTER UPDATE OF name, parts, doc_text, label, language ON nodes BEGIN
      INSERT INTO nodes_fts(nodes_fts, rowid, name, parts, doc_text, label, language)
      VALUES ('delete', old.rowid, old.name, old.parts, old.doc_text, old.label, old.language);
      INSERT INTO nodes_fts(rowid, name, parts, doc_text, label, language)
      VALUES (new.rowid, new.name, new.parts, new.doc_text, new.label, new.language);
    END;";

/// Split an identifier into lowercase subwords: camelCase, PascalCase, snake_case,
/// kebab-case, and digit boundaries (`OrderCheckoutSession` → `order checkout session`,
/// `HTTPServer2Go` → `http server 2 go`). Indexed alongside the raw name so FTS
/// matches mid-identifier words natively — no query-side hacks.
pub fn subwords(name: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = name.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                parts.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if !cur.is_empty() {
            let prev = chars[i - 1];
            let boundary = (c.is_uppercase() && prev.is_lowercase())
                || (c.is_uppercase() && prev.is_uppercase() && chars.get(i + 1).is_some_and(|n| n.is_lowercase()))
                || (c.is_ascii_digit() != prev.is_ascii_digit() && prev.is_alphanumeric());
            if boundary {
                parts.push(std::mem::take(&mut cur));
            }
        }
        cur.push(c.to_ascii_lowercase());
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    if parts.len() <= 1 {
        return String::new(); // single token adds nothing over the name column
    }
    parts.join(" ")
}

/// SQL scalars: `cg_subwords(name)` so FTS rebuilds stay pure SQL, and
/// `cg_is_test_path(path)` so SQL queries share the graph builder's token-aware
/// test-file predicate (a `LIKE '%test%'` would wrongly match `latest_prices.rs`).
fn register_sql_fns(conn: &Connection) -> Result<()> {
    use rusqlite::functions::FunctionFlags;
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    conn.create_scalar_function("cg_subwords", 1, flags, |ctx| {
        let name: String = ctx.get(0)?;
        Ok(subwords(&name))
    })?;
    conn.create_scalar_function("cg_is_test_path", 1, flags, |ctx| {
        let path: String = ctx.get(0)?;
        Ok(codegraph_core::is_test_path(&path))
    })?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub struct ManifestEntry {
    pub file_path: String,
    pub sha256: String,
    pub mtime: i64,
}

/// Everything OTHER files' name-resolution can observe about one file:
/// its Function/Method definitions (id → name; ids encode class nesting) and a
/// sorted list of every other observable item — non-fn nodes (classes, routes,
/// docs…), inherits, typed fields. Read from the live tables BEFORE
/// `delete_file_data` to get the pre-edit shape; compare against the parsed
/// shape to classify an edit for the wave-propagation partial rebuild:
/// equal → body-only; only fn_defs differ → re-resolve callers of the dirty
/// names; `other` differs → full rebuild.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileShape {
    pub fn_defs: std::collections::BTreeMap<String, String>,
    pub other: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContextEntry {
    pub path: String,
    pub summary: String,
    pub added_at: i64,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        register_vec_extension();
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Store> {
        register_vec_extension();
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Store> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "mmap_size", 268_435_456i64)?;
        // Concurrent MCP sessions may open several project DBs; wait briefly on a
        // writer rather than erroring with SQLITE_BUSY. Keep temp + cache in RAM.
        conn.pragma_update(None, "busy_timeout", 5000i64)?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        conn.pragma_update(None, "cache_size", -65536i64)?;
        register_sql_fns(&conn)?;
        let store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        // Fast path: schema already current — the common case for every MCP tool
        // call (each opens a fresh connection). Skips the ~30-statement DDL batch
        // and the ALTER TABLE probes.
        let current: Option<i64> = self
            .conn
            .query_row("SELECT version FROM schema_version LIMIT 1", [], |r| r.get(0))
            .optional()
            .unwrap_or(None); // missing table => full migrate below
        if current == Some(SCHEMA_VERSION) {
            return Ok(());
        }
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version(version INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS nodes(
               id TEXT PRIMARY KEY, name TEXT, parts TEXT, doc_text TEXT, label TEXT, language TEXT, file_path TEXT,
               line_start INTEGER, line_end INTEGER, community INTEGER, pagerank REAL,
               betweenness REAL, data TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS idx_nodes_file ON nodes(file_path);
             CREATE INDEX IF NOT EXISTS idx_nodes_name ON nodes(name);
             CREATE TABLE IF NOT EXISTS edges(
               src TEXT, dst TEXT, relation TEXT, tier TEXT, confidence TEXT,
               src_file TEXT, src_line INTEGER, data TEXT NOT NULL,
               PRIMARY KEY(src, dst, relation));
             CREATE INDEX IF NOT EXISTS idx_edges_dst ON edges(dst);
             CREATE INDEX IF NOT EXISTS idx_edges_src ON edges(src);
             CREATE INDEX IF NOT EXISTS idx_edges_src_file ON edges(src_file);
             CREATE TABLE IF NOT EXISTS hyperedges(
               id TEXT PRIMARY KEY, relation TEXT, label TEXT, confidence TEXT, tier TEXT,
               data TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS hyperedge_members(
               hyperedge_id TEXT, node_id TEXT, role TEXT,
               PRIMARY KEY(hyperedge_id, node_id));
             CREATE INDEX IF NOT EXISTS idx_hmembers_node ON hyperedge_members(node_id);
             CREATE TABLE IF NOT EXISTS manifest(
               file_path TEXT PRIMARY KEY, sha256 TEXT NOT NULL, mtime INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS adrs(
               id TEXT PRIMARY KEY, title TEXT, body TEXT, created_at INTEGER);
             CREATE TABLE IF NOT EXISTS traces(
               id TEXT PRIMARY KEY, payload TEXT, ingested_at INTEGER);
             CREATE TABLE IF NOT EXISTS results(
               id INTEGER PRIMARY KEY AUTOINCREMENT, question TEXT, answer TEXT,
               outcome TEXT, created_at INTEGER);
             CREATE TABLE IF NOT EXISTS contexts(
               path TEXT, summary TEXT, added_at INTEGER, PRIMARY KEY(path, summary));
             CREATE TABLE IF NOT EXISTS calls(
               caller_id TEXT, callee_name TEXT, line INTEGER, file_path TEXT,
               receiver TEXT, enclosing_class TEXT);
             CREATE INDEX IF NOT EXISTS idx_calls_file ON calls(file_path);
             CREATE INDEX IF NOT EXISTS idx_calls_callee ON calls(callee_name);
             CREATE INDEX IF NOT EXISTS idx_calls_caller ON calls(caller_id);
             CREATE TABLE IF NOT EXISTS inherits(
               impl_name TEXT, super_name TEXT, kind TEXT, file_path TEXT);
             CREATE INDEX IF NOT EXISTS idx_inherits_file ON inherits(file_path);
             CREATE TABLE IF NOT EXISTS fields(
               class_id TEXT, field_name TEXT, type_name TEXT, file_path TEXT);
             CREATE INDEX IF NOT EXISTS idx_fields_file ON fields(file_path);
             CREATE TABLE IF NOT EXISTS imports(
               file_path TEXT, name TEXT, module TEXT);
             CREATE INDEX IF NOT EXISTS idx_imports_file ON imports(file_path);
             CREATE INDEX IF NOT EXISTS idx_imports_file_name ON imports(file_path, name);
             CREATE TABLE IF NOT EXISTS locals(
               caller_id TEXT, var_name TEXT, type_name TEXT, file_path TEXT);
             CREATE INDEX IF NOT EXISTS idx_locals_file ON locals(file_path);
             CREATE TABLE IF NOT EXISTS type_refs(
               file_path TEXT, type_name TEXT);
             CREATE INDEX IF NOT EXISTS idx_type_refs_file ON type_refs(file_path);
             CREATE INDEX IF NOT EXISTS idx_type_refs_name ON type_refs(type_name);
             CREATE TABLE IF NOT EXISTS meta(
               key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS cochanges(
               file_a TEXT, file_b TEXT, n INTEGER,
               PRIMARY KEY(file_a, file_b)) WITHOUT ROWID;",
        )?;
        // Additive column migrations for pre-existing DBs (best-effort; ignore
        // "duplicate column" on re-run).
        for stmt in [
            "ALTER TABLE calls ADD COLUMN receiver TEXT",
            "ALTER TABLE calls ADD COLUMN enclosing_class TEXT",
            "ALTER TABLE nodes ADD COLUMN parts TEXT",
            "ALTER TABLE nodes ADD COLUMN doc_text TEXT",
        ] {
            let _ = self.conn.execute(stmt, []);
        }
        let current: Option<i64> = self
            .conn
            .query_row("SELECT version FROM schema_version LIMIT 1", [], |r| r.get(0))
            .optional()?;
        match current {
            None => {
                self.conn.execute_batch(FTS_DDL)?;
                self.conn
                    .execute("INSERT INTO schema_version(version) VALUES(?1)", [SCHEMA_VERSION])?;
            }
            Some(v) if v < SCHEMA_VERSION => {
                // v3: FTS gains `parts`. v4: node-id scheme changed (class-qualified
                // method ids) — clear the manifest so the next index reparses every
                // file. v5: FTS becomes external-content + triggers, `parts` becomes
                // a real column, vectors move to the sqlite-vec `vec_nodes` table.
                if v < 4 {
                    self.conn.execute_batch("DELETE FROM manifest;")?;
                }
                self.conn.execute_batch(
                    "DROP TRIGGER IF EXISTS nodes_fts_ai;
                     DROP TRIGGER IF EXISTS nodes_fts_ad;
                     DROP TRIGGER IF EXISTS nodes_fts_au;
                     DROP TABLE IF EXISTS nodes_fts;",
                )?;
                // Populate `parts` + `doc_text` BEFORE the triggers exist (no FTS churn).
                // v6: Document CONTENT becomes searchable (localization keys,
                // wiki text) — capped to bound the index.
                self.conn.execute("UPDATE nodes SET parts = cg_subwords(name)", [])?;
                self.conn.execute(
                    "UPDATE nodes SET doc_text = substr(json_extract(data,'$.metadata.text'),1,8000) WHERE label = 'Document'",
                    [],
                )?;
                self.conn.execute_batch(FTS_DDL)?;
                self.rebuild_fts()?;
                self.migrate_legacy_vectors()?;
                self.conn.execute("UPDATE schema_version SET version = ?1", [SCHEMA_VERSION])?;
            }
            Some(_) => {}
        }
        Ok(())
    }

    /// v5: move embeddings from the legacy `vectors` blob table into the
    /// sqlite-vec `vec_nodes` virtual table (indexed KNN), then drop the legacy
    /// table. Rows with a deviating dimension are skipped (corrupt/mixed-model).
    fn migrate_legacy_vectors(&self) -> Result<()> {
        if !self.table_exists("vectors")? {
            return Ok(());
        }
        let rows: Vec<(String, Vec<u8>)> = {
            let mut stmt = self.conn.prepare("SELECT node_id, vec FROM vectors")?;
            let it = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)))?;
            it.collect::<rusqlite::Result<_>>()?
        };
        if let Some((_, first)) = rows.first() {
            let dim = first.len() / 4;
            if dim > 0 {
                self.ensure_vec_table(dim)?;
                let mut stmt =
                    self.conn.prepare("INSERT INTO vec_nodes(node_id, embedding) VALUES(?1, ?2)")?;
                for (id, bytes) in &rows {
                    if bytes.len() == dim * 4 {
                        let _ = stmt.execute(params![id, bytes]);
                    }
                }
            }
        }
        self.conn.execute("DROP TABLE vectors", [])?;
        Ok(())
    }

    fn table_exists(&self, name: &str) -> Result<bool> {
        let hit: Option<String> = self
            .conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type IN ('table','view') AND name = ?1",
                [name],
                |r| r.get(0),
            )
            .optional()?;
        Ok(hit.is_some())
    }

    /// Create (or re-create on dimension change) the sqlite-vec KNN table. The
    /// dimension is fixed per table, so switching embedding models rebuilds it —
    /// callers re-embed everything on model switch anyway (`semantic-index`).
    fn ensure_vec_table(&self, dim: usize) -> Result<()> {
        let cur = self.meta_get("vec_dim")?.and_then(|s| s.parse::<usize>().ok());
        if cur == Some(dim) && self.table_exists("vec_nodes")? {
            return Ok(());
        }
        self.conn.execute_batch(&format!(
            "DROP TABLE IF EXISTS vec_nodes;
             CREATE VIRTUAL TABLE vec_nodes USING vec0(node_id TEXT PRIMARY KEY, embedding FLOAT[{dim}]);"
        ))?;
        self.meta_set("vec_dim", &dim.to_string())?;
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT version FROM schema_version LIMIT 1", [], |r| r.get(0))?)
    }

    pub fn upsert_node(&self, n: &Node) -> Result<()> {
        let data = serde_json::to_string(n)?;
        let label = enum_str(&n.label)?;
        self.conn.execute(
            "INSERT INTO nodes(id,name,parts,doc_text,label,language,file_path,line_start,line_end,community,pagerank,betweenness,data)
             VALUES(?1,?2,cg_subwords(?2),substr(json_extract(?11,'$.metadata.text'),1,8000),?3,?4,?5,?6,?7,?8,?9,?10,?11)
             ON CONFLICT(id) DO UPDATE SET name=?2,parts=cg_subwords(?2),doc_text=substr(json_extract(?11,'$.metadata.text'),1,8000),label=?3,language=?4,file_path=?5,line_start=?6,line_end=?7,community=?8,pagerank=?9,betweenness=?10,data=?11",
            params![n.id, n.name, label, n.language, n.file_path, n.line_start, n.line_end, n.community, n.pagerank, n.betweenness, data],
        )?;
        Ok(())
    }

    pub fn get_node(&self, id: &str) -> Result<Option<Node>> {
        let data: Option<String> = self
            .conn
            .query_row("SELECT data FROM nodes WHERE id=?1", [id], |r| r.get(0))
            .optional()?;
        match data {
            Some(s) => Ok(Some(serde_json::from_str(&s)?)),
            None => Ok(None),
        }
    }

    pub fn upsert_edge(&self, e: &Edge) -> Result<()> {
        let data = serde_json::to_string(e)?;
        self.conn.execute(
            "INSERT INTO edges(src,dst,relation,tier,confidence,src_file,src_line,data)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(src,dst,relation) DO UPDATE SET tier=?4,confidence=?5,src_file=?6,src_line=?7,data=?8",
            params![e.src, e.dst, enum_str(&e.relation)?, enum_str(&e.tier)?, enum_str(&e.confidence)?, e.src_file, e.src_line, data],
        )?;
        Ok(())
    }

    pub fn get_edges_for_node(&self, id: &str) -> Result<Vec<Edge>> {
        let mut stmt = self
            .conn
            .prepare("SELECT data FROM edges WHERE src=?1 OR dst=?1")?;
        let rows = stmt.query_map([id], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row?)?);
        }
        Ok(out)
    }

    pub fn upsert_hyperedge(&self, h: &Hyperedge, members: &[HyperedgeMember]) -> Result<()> {
        let data = serde_json::to_string(h)?;
        self.conn.execute(
            "INSERT INTO hyperedges(id,relation,label,confidence,tier,data) VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(id) DO UPDATE SET relation=?2,label=?3,confidence=?4,tier=?5,data=?6",
            params![h.id, enum_str(&h.relation)?, h.label, enum_str(&h.confidence)?, enum_str(&h.tier)?, data],
        )?;
        self.conn
            .execute("DELETE FROM hyperedge_members WHERE hyperedge_id=?1", [&h.id])?;
        for m in members {
            self.conn.execute(
                "INSERT OR REPLACE INTO hyperedge_members(hyperedge_id,node_id,role) VALUES(?1,?2,?3)",
                params![m.hyperedge_id, m.node_id, m.role],
            )?;
        }
        Ok(())
    }

    pub fn get_hyperedges_for_node(&self, node_id: &str) -> Result<Vec<(Hyperedge, Vec<HyperedgeMember>)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT hyperedge_id FROM hyperedge_members WHERE node_id=?1")?;
        let ids: Vec<String> = stmt
            .query_map([node_id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut hstmt = self.conn.prepare("SELECT data FROM hyperedges WHERE id=?1")?;
        let mut mstmt = self
            .conn
            .prepare("SELECT hyperedge_id,node_id,role FROM hyperedge_members WHERE hyperedge_id=?1")?;
        let mut out = Vec::new();
        for hid in ids {
            let data: String = hstmt.query_row([&hid], |r| r.get(0))?;
            let h: Hyperedge = serde_json::from_str(&data)?;
            let members = mstmt
                .query_map([&hid], |r| {
                    Ok(HyperedgeMember {
                        hyperedge_id: r.get(0)?,
                        node_id: r.get(1)?,
                        role: r.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            out.push((h, members));
        }
        Ok(out)
    }

    /// Rebuild the external-content FTS index from `nodes`. Only needed after a
    /// schema migration — normal writes keep it in sync via triggers.
    pub fn rebuild_fts(&self) -> Result<()> {
        self.conn.execute("INSERT INTO nodes_fts(nodes_fts) VALUES('rebuild')", [])?;
        Ok(())
    }

    /// Edges whose metadata.justification matches (e.g. reuse IndexStore edges
    /// across incremental reindexes without re-reading the store).
    pub fn edges_by_justification(&self, j: &str) -> Result<Vec<Edge>> {
        let mut stmt = self.conn.prepare(
            "SELECT data FROM edges WHERE json_extract(data,'$.metadata.justification') = ?1",
        )?;
        let rows = stmt.query_map([j], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    /// sha256 over the canonical structural dump (nodes by id, edges by key) —
    /// two indexes of the same tree MUST produce the same hash (the determinism
    /// brand guarantee, verified by `codegraph verify-determinism` + CI).
    pub fn canonical_hash(&self) -> Result<String> {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        let mut stmt = self.conn.prepare("SELECT data FROM nodes ORDER BY id")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for r in rows {
            h.update(r?.as_bytes());
        }
        let mut stmt = self.conn.prepare("SELECT data FROM edges ORDER BY src, dst, relation")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for r in rows {
            h.update(r?.as_bytes());
        }
        Ok(format!("{:x}", h.finalize()))
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT OR REPLACE INTO meta(key,value) VALUES(?1,?2)", params![key, value])?;
        Ok(())
    }

    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    /// Replace the git co-change pairs (files that historically change together).
    pub fn save_cochanges(&self, pairs: &[(String, String, u32)]) -> Result<()> {
        // No own BEGIN/COMMIT: index_dir already runs inside a transaction
        // (nested BEGIN is an error); called nowhere else.
        self.conn.execute("DELETE FROM cochanges", [])?;
        let mut stmt =
            self.conn.prepare("INSERT OR REPLACE INTO cochanges(file_a,file_b,n) VALUES(?1,?2,?3)")?;
        for (a, b, n) in pairs {
            stmt.execute(params![a, b, n])?;
        }
        Ok(())
    }

    /// Strongest co-change pairs repo-wide (for the report).
    pub fn top_cochanges(&self, limit: usize) -> Result<Vec<(String, String, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT file_a, file_b, n FROM cochanges ORDER BY n DESC, file_a, file_b LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Files that historically change together with `file`, strongest first.
    pub fn cochanges_for(&self, file: &str, limit: usize) -> Result<Vec<(String, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT CASE WHEN file_a = ?1 THEN file_b ELSE file_a END AS other, n
             FROM cochanges WHERE file_a = ?1 OR file_b = ?1
             ORDER BY n DESC, other LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![file, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Dead-code CANDIDATES: functions/methods that no call site in the repo even
    /// NAMES (the raw `calls` table holds every textual call site, so this is
    /// stronger evidence than resolved-edges-only), excluding entry points, route
    /// handlers, and test files. Candidates, not verdicts — dynamic dispatch,
    /// exports, and reflection can't be seen statically.
    pub fn dead_code_candidates(&self, limit: usize) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT n.data FROM nodes n
             WHERE n.label IN ('Function','Method')
               AND n.name NOT IN ('main','init','new','setup','run','constructor')
               AND NOT cg_is_test_path(n.file_path)
               AND NOT EXISTS (SELECT 1 FROM calls c WHERE c.callee_name = n.name)
               AND NOT EXISTS (SELECT 1 FROM nodes r WHERE r.label = 'Route'
                               AND json_extract(r.data,'$.metadata.handler') = n.name)
             ORDER BY n.file_path, n.line_start LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    /// Textual call sites naming `name` (fan-in signal — every call site, resolved or not).
    pub fn call_site_count(&self, name: &str) -> Result<usize> {
        let n: i64 =
            self.conn.query_row("SELECT COUNT(*) FROM calls WHERE callee_name = ?1", [name], |r| r.get(0))?;
        Ok(n as usize)
    }

    /// Is `name` covered by a test? Prefers RESOLVED Tests edges; falls back to
    /// textual call sites in test-looking files (covers unresolved calls).
    pub fn has_test_reference(&self, name: &str) -> Result<bool> {
        let via_edge: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM edges e JOIN nodes n ON n.id = e.dst
             WHERE e.relation = 'Tests' AND n.name = ?1",
            [name],
            |r| r.get(0),
        )?;
        if via_edge > 0 {
            return Ok(true);
        }
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM calls WHERE callee_name = ?1 AND cg_is_test_path(file_path)",
            [name],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Non-File symbols defined in a file (for diff → affected-symbol mapping).
    pub fn symbols_in_file(&self, file: &str) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT data FROM nodes WHERE file_path = ?1 AND label IN ('Function','Method','Class','Interface','Enum','Type') ORDER BY line_start",
        )?;
        let rows = stmt.query_map([file], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    pub fn save_manifest(&self, file_path: &str, sha256: &str, mtime: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO manifest(file_path,sha256,mtime) VALUES(?1,?2,?3)",
            params![file_path, sha256, mtime],
        )?;
        Ok(())
    }

    pub fn manifest_for(&self, file_path: &str) -> Result<Option<ManifestEntry>> {
        Ok(self
            .conn
            .query_row(
                "SELECT file_path,sha256,mtime FROM manifest WHERE file_path=?1",
                [file_path],
                |r| Ok(ManifestEntry { file_path: r.get(0)?, sha256: r.get(1)?, mtime: r.get(2)? }),
            )
            .optional()?)
    }

    pub fn add_context(&self, path: &str, summary: &str, added_at: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO contexts(path,summary,added_at) VALUES(?1,?2,?3)",
            params![path, summary, added_at],
        )?;
        Ok(())
    }

    pub fn contexts_for(&self, path_prefix: &str) -> Result<Vec<ContextEntry>> {
        let pattern = format!("{}%", path_prefix);
        let mut stmt = self
            .conn
            .prepare("SELECT path,summary,added_at FROM contexts WHERE path LIKE ?1 ORDER BY added_at")?;
        let rows = stmt.query_map([pattern], |r| {
            Ok(ContextEntry { path: r.get(0)?, summary: r.get(1)?, added_at: r.get(2)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn export_zst(&self, out: &Path) -> Result<()> {
        let tmp = out.with_extension("tmpdb");
        let _ = std::fs::remove_file(&tmp);
        let tmp_sql = tmp.to_string_lossy().replace('\'', "''");
        self.conn
            .execute_batch(&format!("VACUUM INTO '{}'", tmp_sql))?;
        let bytes = std::fs::read(&tmp)?;
        let compressed = zstd::encode_all(&bytes[..], 3)?;
        std::fs::write(out, compressed)?;
        let _ = std::fs::remove_file(&tmp);
        Ok(())
    }

    pub fn import_zst(zst: &Path, db_out: &Path) -> Result<Store> {
        let compressed = std::fs::read(zst)?;
        let bytes = zstd::decode_all(&compressed[..])?;
        std::fs::write(db_out, bytes)?;
        Store::open(db_out)
    }
    /// FTS with RANKING (previously rowid-ordered + truncated — the actual
    /// definition of a searched name could fall out of the limit entirely):
    /// bm25 relevance + definition-label boost (agents want the def, not the
    /// twelfth test that mentions it) + exact-name boost + light test-file
    /// penalty as a tiebreak. Deterministic weights.
    pub fn search_fts(&self, query: &str, limit: usize) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT n.data FROM nodes_fts f JOIN nodes n ON n.rowid = f.rowid
             WHERE nodes_fts MATCH ?1
             ORDER BY bm25(nodes_fts, 10.0, 6.0, 1.0, 0.0, 0.0)
               - CASE n.label
                   WHEN 'Function' THEN 6.0 WHEN 'Method' THEN 6.0
                   WHEN 'Class' THEN 5.0 WHEN 'Interface' THEN 5.0
                   WHEN 'Enum' THEN 4.0 WHEN 'Route' THEN 4.0
                   WHEN 'File' THEN 1.0 ELSE 0.0 END
               - CASE WHEN lower(n.name) = lower(trim(?1, '\"*')) THEN 8.0 ELSE 0.0 END
               + CASE WHEN n.file_path LIKE '%test%' OR n.file_path LIKE '%spec%' THEN 1.5 ELSE 0.0 END,
               n.id
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![query, limit as i64], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    /// Forgiving search: exact FTS first (precise), then AND-of-prefixes over
    /// the query's identifier subwords (all terms must hit — the right answer
    /// for dotted/hyphenated names like `gem.preparation.failed_to_load` or
    /// `feature-discovery.controller`), then OR-of-prefixes as the loosest
    /// net. A final RUST re-rank pushes verbatim matches to the top: exact
    /// name, filename stem, or a Document whose text contains the raw query.
    pub fn search_smart(&self, raw: &str, limit: usize) -> Result<Vec<Node>> {
        let over = limit.saturating_mul(3).max(24);
        let mut hits = self.search_fts(raw, over).unwrap_or_default();
        if hits.is_empty() {
            let mut seen = std::collections::HashSet::new();
            let terms: Vec<String> = raw
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .flat_map(|t| {
                    let sw = subwords(t);
                    if sw.is_empty() { vec![t.to_string()] } else { sw.split(' ').map(str::to_string).collect() }
                })
                .filter(|t| t.len() > 1)
                .filter(|t| seen.insert(t.to_lowercase()))
                .map(|t| format!("{t}*"))
                .collect();
            if terms.is_empty() {
                return Ok(hits);
            }
            hits = self.search_fts(&terms.join(" AND "), over).unwrap_or_default();
            if hits.is_empty() && terms.len() > 1 {
                hits = self.search_fts(&terms.join(" OR "), over).unwrap_or_default();
            }
        }
        // Verbatim re-rank (stable): FTS relevance decided the pool; exactness
        // decides the podium.
        let q = raw.trim().to_lowercase();
        let bonus = |n: &Node| -> i32 {
            let name = n.name.to_lowercase();
            let file_name = n.file_path.rsplit('/').next().unwrap_or("").to_lowercase();
            let stem = file_name.rsplit_once('.').map(|(s, _)| s).unwrap_or(&file_name);
            if name == q {
                return 4;
            }
            if stem == q || file_name == q {
                return 3;
            }
            // `search` is the IDENTIFIER tool (conceptual queries route to
            // semantic_search): any code symbol outranks a Document fragment —
            // doc-text mentions filling ranks 2..20 was field-measured noise.
            if !matches!(n.label, NodeLabel::Document) {
                return 2;
            }
            if n.metadata.get("text").and_then(|v| v.as_str()).is_some_and(|t| t.to_lowercase().contains(&q)) {
                return 1;
            }
            0
        };
        let mut ranked: Vec<(i32, usize, Node)> =
            hits.into_iter().enumerate().map(|(i, n)| (bonus(&n), i, n)).collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        // One row per DOCUMENT FILE (chunks of the same .md collapse), and when
        // CODE answered the query, docs are capped to a footnote — a doc-heavy
        // repo (.claude/ rules, wikis) otherwise floods ranks 2..N with
        // mentions of the identifier (field-measured noise).
        const MAX_DOCS_WITH_CODE_HITS: usize = 5;
        let has_code = ranked.iter().any(|(_, _, n)| n.label != NodeLabel::Document);
        let mut doc_files = std::collections::HashSet::new();
        Ok(ranked
            .into_iter()
            .filter(|(_, _, n)| {
                n.label != NodeLabel::Document
                    || (doc_files.insert(n.file_path.clone())
                        && (!has_code || doc_files.len() <= MAX_DOCS_WITH_CODE_HITS))
            })
            .take(limit)
            .map(|(_, _, n)| n)
            .collect())
    }

    /// Field/property declarations matching a name — variables aren't graph
    /// nodes (by design: noise), but "where is variable X declared" deserves a
    /// real answer: (field, declared type, file).
    pub fn field_matches(&self, name: &str) -> Result<Vec<(String, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT field_name, type_name, file_path FROM fields WHERE field_name = ?1
             UNION SELECT var_name, type_name, file_path FROM locals WHERE var_name = ?1 AND type_name <> ''
             LIMIT 25",
        )?;
        let rows = stmt.query_map([name], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Regex search over symbol names (anywhere in the name, not just a prefix) —
    /// for patterns FTS can't express (middle fragments, alternations, anchors).
    pub fn search_regex(&self, pattern: &str, limit: usize) -> Result<Vec<Node>> {
        let re = regex::Regex::new(pattern).map_err(|e| StoreError::Msg(format!("bad regex: {e}")))?;
        // Match on the name column first; deserialize the (potentially large)
        // data JSON only for the ≤limit hits — not for every node in the table.
        let mut stmt = self.conn.prepare("SELECT id, name FROM nodes WHERE name <> ''")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut ids = Vec::new();
        for r in rows {
            let (id, name) = r?;
            if re.is_match(&name) {
                ids.push(id);
                if ids.len() >= limit {
                    break;
                }
            }
        }
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(n) = self.get_node(&id)? {
                out.push(n);
            }
        }
        Ok(out)
    }


    /// All Function/Method definitions named `name`, each with its RESOLVED-caller
    /// count — the candidate list for disambiguating an ambiguous query. Rivals
    /// either silently union all same-name definitions or refuse; we ASK.
    pub fn definitions_of(&self, name: &str) -> Result<Vec<(Node, usize)>> {
        let mut stmt = self.conn.prepare(
            "SELECT n.data, (SELECT COUNT(DISTINCT e.src) FROM edges e
                             WHERE e.relation = 'Calls' AND e.dst = n.id) AS nc
             FROM nodes n WHERE n.name = ?1 AND n.label IN ('Function','Method')
             ORDER BY nc DESC, n.file_path",
        )?;
        let rows = stmt.query_map([name], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = Vec::new();
        for r in rows {
            let (data, nc) = r?;
            out.push((serde_json::from_str(&data)?, nc as usize));
        }
        Ok(out)
    }

    /// Callers of ONE specific definition (pinned by node id) — never unions
    /// same-name definitions.
    pub fn callers_of_id(&self, id: &str) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT s.data FROM edges e JOIN nodes s ON s.id = e.src
             WHERE e.relation = 'Calls' AND e.dst = ?1",
        )?;
        let rows = stmt.query_map([id], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    pub fn callers_of(&self, name: &str) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT s.data FROM edges e              JOIN nodes t ON t.id = e.dst              JOIN nodes s ON s.id = e.src              WHERE e.relation = 'Calls' AND t.name = ?1",
        )?;
        let rows = stmt.query_map([name], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    /// Every distinct source file the index knows about — the candidate set a
    /// rename must scan so an UNCAPTURED reference (a call form the parser missed)
    /// can't slip through a "0 captured calls = complete" gate and corrupt code.
    pub fn indexed_files(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT DISTINCT file_path FROM nodes WHERE file_path <> ''")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Count of call sites naming `name`, grouped by file — the expected number
    /// of call-token occurrences per file, for the rename occurrence-completeness gate.
    pub fn call_sites_by_file(&self, name: &str) -> Result<std::collections::HashMap<String, usize>> {
        let mut stmt = self
            .conn
            .prepare("SELECT file_path, COUNT(*) FROM calls WHERE callee_name = ?1 GROUP BY file_path")?;
        let rows = stmt.query_map([name], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A call site is EXTERNALLY BOUND when its file imports the callee name —
    /// or the RECEIVER it is called through (`ns.method()`, `svc.run()`) — from
    /// a package specifier (TS/JS import that is neither relative nor a
    /// resolved alias). Such sites can never resolve in-repo — evidence, not a
    /// guess — so coverage keeps them out of the denominator.
    const EXTERNAL_BOUND: &'static str = "EXISTS(
        SELECT 1 FROM imports i WHERE i.file_path = c.file_path
        AND (i.name = c.callee_name OR i.name = json_extract(c.receiver,'$.Named'))
        AND substr(i.module,1,1) NOT IN ('.','/','#')
        AND (i.file_path LIKE '%.ts' OR i.file_path LIKE '%.tsx' OR i.file_path LIKE '%.js'
             OR i.file_path LIKE '%.jsx' OR i.file_path LIKE '%.mjs'))";

    /// A call site is UNRESOLVABLE when NO in-repo definition carries the callee
    /// name at all (jest globals, stdlib/array methods, framework decorators).
    /// A perfect in-repo resolver could never produce this edge, so it does not
    /// belong in a recall denominator either. Pure graph evidence.
    const NO_INREPO_DEF: &'static str = "NOT EXISTS(
        SELECT 1 FROM nodes n WHERE n.name = c.callee_name
        AND n.label IN ('Function','Method','Class'))";

    /// Where a TYPE is USED: DI/typed fields, typed locals, in-repo imports of
    /// the name, and subtypes. This is how classes/interfaces are "called" —
    /// a NestJS service injected via `constructor(private s: FooService)` has
    /// ZERO call sites naming it, and answering "no callers" for it is the
    /// worst kind of miss (confident absence). Returns (file, evidence) pairs.
    pub fn type_usages(&self, name: &str) -> Result<Vec<(String, String)>> {
        let mut out: Vec<(String, String)> = Vec::new();
        let push_all = |sql: &str, evidence: &str, out: &mut Vec<(String, String)>| -> Result<()> {
            let mut stmt = self.conn.prepare(sql)?;
            let rows = stmt.query_map([name], |r| r.get::<_, String>(0))?;
            for r in rows {
                out.push((r?, evidence.to_string()));
            }
            Ok(())
        };
        push_all(
            "SELECT DISTINCT file_path FROM nodes WHERE name = ?1
             AND label IN ('Class','Interface','Enum','Type','Function','Method')",
            "definition",
            &mut out,
        )?;
        push_all("SELECT DISTINCT file_path FROM fields WHERE type_name = ?1", "field/DI type", &mut out)?;
        push_all("SELECT DISTINCT file_path FROM locals WHERE type_name = ?1", "typed local", &mut out)?;
        // in-repo imports only — python modules are dotted (not './'-prefixed),
        // so exclude ONLY the TS/JS external-package shape, not python
        push_all(
            "SELECT DISTINCT file_path FROM imports WHERE name = ?1 AND NOT (
                substr(module,1,1) NOT IN ('.','/')
                AND (file_path LIKE '%.ts' OR file_path LIKE '%.tsx' OR file_path LIKE '%.js'
                     OR file_path LIKE '%.jsx' OR file_path LIKE '%.mjs'))",
            "import",
            &mut out,
        )?;
        push_all("SELECT DISTINCT file_path FROM inherits WHERE super_name = ?1", "subtype", &mut out)?;
        // singleton/static member access: `Foo.shared.bar()` records receiver
        // "Foo.shared" on the call site — evidence the type is used there
        push_all(
            "SELECT DISTINCT file_path FROM calls
             WHERE json_extract(receiver,'$.Named') = ?1
                OR json_extract(receiver,'$.Named') LIKE ?1 || '.%'",
            "static member access",
            &mut out,
        )?;
        push_all(
            "SELECT DISTINCT file_path FROM type_refs WHERE type_name = ?1",
            "type reference",
            &mut out,
        )?;
        // docs/wiki mentioning the name (doc CONTENT is FTS-indexed since v1.32)
        {
            let mut stmt = self.conn.prepare(
                "SELECT DISTINCT n.file_path FROM nodes_fts f JOIN nodes n ON n.rowid = f.rowid
                 WHERE nodes_fts MATCH '\"' || ?1 || '\"' AND n.label = 'Document' LIMIT 20",
            )?;
            let rows = stmt.query_map([name], |r| r.get::<_, String>(0))?;
            for r in rows {
                out.push((r?, "doc mention".to_string()));
            }
        }
        out.sort();
        out.dedup_by(|a, b| a.0 == b.0); // one row per file, first evidence wins
        Ok(out)
    }

    /// The names a function's body CALLS that did not resolve into any edge —
    /// the outbound textual layer behind `callees` (in-repo-resolvable names
    /// only; externally-bound / no-in-repo-def names are excluded as noise).
    pub fn unresolved_callee_names(&self, caller_id: &str) -> Result<Vec<String>> {
        let sql = format!(
            "SELECT DISTINCT c.callee_name FROM calls c WHERE c.caller_id = ?1
             AND c.callee_name NOT LIKE 'route.%'
             AND NOT EXISTS (SELECT 1 FROM edges e JOIN nodes n ON n.id = e.dst
                             WHERE e.src = ?1 AND e.relation = 'Calls' AND n.name = c.callee_name)
             AND NOT ({} OR {})
             ORDER BY c.callee_name",
            Self::EXTERNAL_BOUND,
            Self::NO_INREPO_DEF
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([caller_id], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Repo-wide count of call sites no in-repo resolver could ever bind:
    /// externally-import-bound OR naming no in-repo definition.
    pub fn external_bound_call_sites(&self) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM calls c WHERE {} OR {}",
                Self::EXTERNAL_BOUND,
                Self::NO_INREPO_DEF
            ),
            [],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// Coverage for `callers(name)`: how many of the textual call sites naming
    /// `name` actually resolved into a `Calls` edge to a node of that name. The
    /// difference is the count dropped (ambiguous / external / unresolved) — a
    /// real signal that the precise callers list may be incomplete.
    pub fn coverage_for_callers(&self, name: &str) -> Result<Coverage> {
        let total: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM calls c WHERE c.callee_name = ?1 AND NOT {}", Self::EXTERNAL_BOUND),
            [name],
            |r| r.get(0),
        )?;
        // resolved applies the SAME site filter as total — otherwise a site that
        // is both externally-bound and resolved makes resolved > total and
        // saturating_sub masks real gaps as "complete".
        let resolved: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM calls c WHERE c.callee_name = ?1 AND NOT {} AND EXISTS(
                     SELECT 1 FROM edges e JOIN nodes n ON n.id = e.dst
                     WHERE e.src = c.caller_id AND e.relation = 'Calls' AND e.confidence <> 'Ambiguous' AND n.name = ?1)",
                Self::EXTERNAL_BOUND
            ),
            [name],
            |r| r.get(0),
        )?;
        Ok(Coverage::callers(name, resolved as usize, total as usize))
    }

    /// Coverage for `callees(caller_id)`: how many of the caller's outbound call
    /// sites resolved to an internal definition. Dropped = external (library) or
    /// unresolved calls absent from the callees list.
    pub fn coverage_for_callees(&self, caller_id: &str) -> Result<Coverage> {
        let total: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM calls c WHERE c.caller_id = ?1 AND NOT ({} OR {})",
                Self::EXTERNAL_BOUND,
                Self::NO_INREPO_DEF
            ),
            [caller_id],
            |r| r.get(0),
        )?;
        // same site filter as total (see coverage_for_callers)
        let resolved: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM calls c WHERE c.caller_id = ?1 AND NOT ({} OR {}) AND EXISTS(
                     SELECT 1 FROM edges e JOIN nodes n ON n.id = e.dst
                     WHERE e.src = ?1 AND e.relation = 'Calls' AND e.confidence <> 'Ambiguous' AND n.name = c.callee_name)",
                Self::EXTERNAL_BOUND,
                Self::NO_INREPO_DEF
            ),
            [caller_id],
            |r| r.get(0),
        )?;
        Ok(Coverage::callees(resolved as usize, total as usize))
    }

    pub fn all_nodes(&self) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare("SELECT data FROM nodes")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    pub fn all_edges(&self) -> Result<Vec<Edge>> {
        let mut stmt = self.conn.prepare("SELECT data FROM edges")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    /// Nodes for graph/snapshot loading with the ONE heavy payload stripped:
    /// Document chunk text lives in `metadata.text` and can dominate the DB, and
    /// no graph traversal or MCP listing needs it (get_node serves the full row).
    pub fn graph_nodes(&self) -> Result<Vec<Node>> {
        // ORDER BY id: graph construction order must be CANONICAL, not rowid
        // order — incremental reindexes re-insert changed nodes at new rowids,
        // and community re-labeling / float accumulation follow node order.
        let mut stmt =
            self.conn.prepare("SELECT json_remove(data, '$.metadata.text') FROM nodes ORDER BY id")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    /// Edge topology from the typed columns — no per-row JSON parse. Metadata is
    /// dropped (graph traversal never reads it); use `all_edges` when it matters.
    pub fn graph_edges(&self) -> Result<Vec<Edge>> {
        // Canonical order for the same reason as graph_nodes.
        let mut stmt = self.conn.prepare(
            "SELECT src,dst,relation,tier,confidence,src_file,src_line FROM edges ORDER BY src,dst,relation",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
                r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (src, dst, rel, tier, conf, src_file, src_line) = r?;
            out.push(Edge {
                src, dst,
                relation: enum_from(&rel)?,
                tier: enum_from(&tier)?,
                confidence: enum_from(&conf)?,
                src_file,
                src_line: src_line as u32,
                metadata: Metadata::new(),
            });
        }
        Ok(out)
    }

    /// Persist per-node analytics + fan-in/out WITHOUT re-serializing every
    /// node's JSON in Rust: typed columns plus `json_set` keep `data` in sync in
    /// one UPDATE (SQLite edits the JSON in C). This is the difference between an
    /// incremental reindex re-writing 50 MB of node JSON and touching 5 columns.
    /// Items: (id, community, pagerank, betweenness, fan_in, fan_out).
    pub fn update_analytics(&self, items: &[(String, u32, f64, f64, u32, u32)]) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "UPDATE nodes SET community=?2, pagerank=?3, betweenness=?4,
               data = json_set(data,'$.community',?2,'$.pagerank',?3,'$.betweenness',?4,
                               '$.metadata.fan_in',?5,'$.metadata.fan_out',?6)
             WHERE id=?1",
        )?;
        for (id, c, pr, bw, fi, fo) in items {
            stmt.execute(params![id, c, pr, bw, fi, fo])?;
        }
        Ok(())
    }

    /// Bump the monotonic index generation (cache-invalidation key). Called once
    /// per committed index; readers use `generation()` below.
    pub fn bump_generation(&self) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key,value) VALUES('generation','1')
             ON CONFLICT(key) DO UPDATE SET value = CAST(value AS INTEGER) + 1",
            [],
        )?;
        Ok(())
    }

    pub fn find_by_name(&self, name: &str) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare("SELECT data FROM nodes WHERE name = ?1")?;
        let rows = stmt.query_map([name], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    pub fn upsert_vector(&self, node_id: &str, v: &[f32]) -> Result<()> {
        self.upsert_vectors(std::slice::from_ref(&(node_id.to_string(), v.to_vec())))
    }

    /// Batch-store vectors in ONE transaction into the sqlite-vec `vec_nodes`
    /// table (indexed KNN — see `knn`). Stored L2-normalized so L2 distance
    /// order == cosine order. A dimension change (new embedding model) rebuilds
    /// the table; callers re-embed everything on model switch anyway.
    pub fn upsert_vectors(&self, items: &[(String, Vec<f32>)]) -> Result<()> {
        let Some(dim) = items.first().map(|(_, v)| v.len()).filter(|&d| d > 0) else {
            return Ok(());
        };
        self.ensure_vec_table(dim)?;
        let txn = self.txn()?;
        {
            // vec0 has no upsert — delete-then-insert on the primary key.
            let mut del = self.conn.prepare("DELETE FROM vec_nodes WHERE node_id = ?1")?;
            let mut ins =
                self.conn.prepare("INSERT INTO vec_nodes(node_id, embedding) VALUES(?1, ?2)")?;
            for (id, v) in items {
                if v.len() != dim {
                    continue; // mixed dims in one batch: skip, never corrupt the table
                }
                let n = codegraph_core::normalize(v);
                let mut bytes = Vec::with_capacity(n.len() * 4);
                for f in &n {
                    bytes.extend_from_slice(&f.to_le_bytes());
                }
                del.execute([id])?;
                ins.execute(params![id, bytes])?;
            }
        }
        txn.commit()
    }

    pub fn all_vectors(&self) -> Result<Vec<(String, Vec<f32>)>> {
        if !self.table_exists("vec_nodes")? {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare("SELECT node_id, embedding FROM vec_nodes")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)))?;
        let mut out = Vec::new();
        for r in rows {
            let (id, bytes) = r?;
            let v = bytes
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            out.push((id, v));
        }
        Ok(out)
    }

    /// K-nearest-neighbor semantic lookup via sqlite-vec. Returns
    /// (node_id, cosine similarity) best-first. Vectors are stored normalized,
    /// so L2 distance d relates to cosine c by c = 1 − d²/2 (order-preserving).
    pub fn knn(&self, query: &[f32], k: usize) -> Result<Vec<(String, f32)>> {
        if k == 0 || query.is_empty() || !self.table_exists("vec_nodes")? {
            return Ok(Vec::new());
        }
        let q = codegraph_core::normalize(query);
        let mut bytes = Vec::with_capacity(q.len() * 4);
        for f in &q {
            bytes.extend_from_slice(&f.to_le_bytes());
        }
        let mut stmt = self.conn.prepare(
            "SELECT node_id, distance FROM vec_nodes
             WHERE embedding MATCH ?1 AND k = ?2 ORDER BY distance",
        )?;
        let rows = stmt.query_map(params![bytes, k as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)? as f32))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (id, d) = r?;
            out.push((id, 1.0 - d * d / 2.0));
        }
        Ok(out)
    }

    pub fn save_calls(&self, file_path: &str, calls: &[RawCall]) -> Result<()> {
        self.conn.execute("DELETE FROM calls WHERE file_path = ?1", [file_path])?;
        let mut stmt = self.conn.prepare(
            "INSERT INTO calls(caller_id, callee_name, line, file_path, receiver, enclosing_class) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for c in calls {
            let receiver = serde_json::to_string(&c.receiver)?;
            stmt.execute(params![c.caller_id, c.callee_name, c.line, file_path, receiver, c.enclosing_class])?;
        }
        Ok(())
    }

    pub fn all_calls(&self) -> Result<Vec<RawCall>> {
        let mut stmt =
            self.conn.prepare("SELECT caller_id, callee_name, line, receiver, enclosing_class FROM calls")?;
        let rows = stmt.query_map([], |r| {
            let receiver = r
                .get::<_, Option<String>>(3)?
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            Ok(RawCall {
                caller_id: r.get(0)?,
                callee_name: r.get(1)?,
                line: r.get::<_, i64>(2)? as u32,
                receiver,
                enclosing_class: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_file_data(&self, file_path: &str) -> Result<()> {
        // Prune embeddings for this file's nodes BEFORE the nodes go (keyed by
        // node_id, not file_path) — otherwise renamed/removed symbols leave
        // orphaned vectors that pollute semantic search and grow the DB.
        // FTS rows need no manual pruning: the nodes_fts triggers handle it.
        if self.table_exists("vec_nodes")? {
            let ids: Vec<String> = {
                let mut stmt = self.conn.prepare("SELECT id FROM nodes WHERE file_path = ?1")?;
                let it = stmt.query_map([file_path], |r| r.get::<_, String>(0))?;
                it.collect::<rusqlite::Result<_>>()?
            };
            // vec0 supports DELETE by primary-key equality only — loop, no subquery.
            let mut del = self.conn.prepare("DELETE FROM vec_nodes WHERE node_id = ?1")?;
            for id in &ids {
                del.execute([id])?;
            }
        }
        self.conn.execute("DELETE FROM nodes WHERE file_path = ?1", [file_path])?;
        self.conn.execute("DELETE FROM calls WHERE file_path = ?1", [file_path])?;
        self.conn.execute("DELETE FROM inherits WHERE file_path = ?1", [file_path])?;
        self.conn.execute("DELETE FROM fields WHERE file_path = ?1", [file_path])?;
        self.conn.execute("DELETE FROM locals WHERE file_path = ?1", [file_path])?;
        self.conn.execute("DELETE FROM type_refs WHERE file_path = ?1", [file_path])?;
        self.conn.execute("DELETE FROM imports WHERE file_path = ?1", [file_path])?;
        Ok(())
    }

    /// All manifest rows — for the staleness probe and the phase-1 change scan.
    pub fn manifest_map(&self) -> Result<Vec<ManifestEntry>> {
        let mut stmt = self.conn.prepare("SELECT file_path,sha256,mtime FROM manifest")?;
        let rows = stmt.query_map([], |r| {
            Ok(ManifestEntry { file_path: r.get(0)?, sha256: r.get(1)?, mtime: r.get(2)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn manifest_files(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT file_path FROM manifest")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn delete_manifest(&self, file_path: &str) -> Result<()> {
        self.conn.execute("DELETE FROM manifest WHERE file_path = ?1", [file_path])?;
        Ok(())
    }

    pub fn clear_edges(&self) -> Result<()> {
        self.conn.execute("DELETE FROM edges", [])?;
        Ok(())
    }

    /// Raw call sites captured in one file (the partial-rebuild working set).
    pub fn calls_for_file(&self, file: &str) -> Result<Vec<RawCall>> {
        let mut stmt = self.conn.prepare(
            "SELECT caller_id, callee_name, line, receiver, enclosing_class FROM calls WHERE file_path = ?1",
        )?;
        let rows = stmt.query_map([file], |r| {
            let receiver = r
                .get::<_, Option<String>>(3)?
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            Ok(RawCall {
                caller_id: r.get(0)?,
                callee_name: r.get(1)?,
                line: r.get::<_, i64>(2)? as u32,
                receiver,
                enclosing_class: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Partial rebuild: drop the tree-sitter-resolved edges ORIGINATING in one
    /// file. Compiler-grade edges (tier=Scip: SCIP imports, Xcode IndexStore)
    /// are deliberately spared — they supersede tree-sitter and are re-merged on
    /// their own cadence, not per file edit.
    pub fn delete_tree_sitter_edges_for_file(&self, file: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM edges WHERE src_file = ?1 AND tier = 'TreeSitter'", [file])?;
        Ok(())
    }

    /// ALL edges originating in one file, every tier — for pruned (deleted)
    /// files, where even compiler-grade edges are stale by definition.
    pub fn delete_edges_for_file(&self, file: &str) -> Result<()> {
        self.conn.execute("DELETE FROM edges WHERE src_file = ?1", [file])?;
        Ok(())
    }

    /// The observable shape of one file, from the live tables (see [`FileShape`]).
    pub fn file_shape(&self, file: &str) -> Result<FileShape> {
        let mut shape = FileShape::default();
        let mut stmt = self
            .conn
            .prepare("SELECT id,name,label FROM nodes WHERE file_path = ?1 AND label <> 'File'")?;
        let rows = stmt.query_map([file], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?;
        for r in rows {
            let (id, name, label) = r?;
            if label == "Function" || label == "Method" {
                shape.fn_defs.insert(id, name);
            } else {
                shape.other.push(format!("n\u{1}{id}\u{1}{name}\u{1}{label}"));
            }
        }
        let mut stmt = self
            .conn
            .prepare("SELECT impl_name,super_name,kind FROM inherits WHERE file_path = ?1")?;
        let rows = stmt.query_map([file], |r| {
            Ok(format!("i\u{1}{}\u{1}{}\u{1}{}", r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?;
        for r in rows {
            shape.other.push(r?);
        }
        let mut stmt = self
            .conn
            .prepare("SELECT class_id,field_name,type_name FROM fields WHERE file_path = ?1")?;
        let rows = stmt.query_map([file], |r| {
            Ok(format!("f\u{1}{}\u{1}{}\u{1}{}", r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?;
        for r in rows {
            shape.other.push(r?);
        }
        shape.other.sort_unstable();
        Ok(shape)
    }

    /// Files containing at least one call site naming `name` — the wave set a
    /// dirty definition name propagates to (uses idx_calls_callee).
    pub fn files_with_calls_naming(&self, name: &str) -> Result<Vec<String>> {
        let mut stmt =
            self.conn.prepare("SELECT DISTINCT file_path FROM calls WHERE callee_name = ?1")?;
        let rows = stmt.query_map([name], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Files with a parser-verified CALL SITE naming `name` that plausibly refers
    /// to the asked symbol — the TEXTUAL recall layer behind resolved callers.
    /// Evidence-filtered, never a raw grep: a file that DEFINES `name` itself
    /// binds its own sites locally (they belong to that def's resolved callers),
    /// and a file that imports `name` from an external package can't be calling
    /// the in-repo symbol.
    ///
    /// With multiple same-name definitions, pass `attribute_to` (a definition's
    /// file path) to keep only the sites whose NEAREST definition (longest
    /// shared path prefix) is that one — a vendored mirror's tests attach to
    /// the mirror, not to the primary source.
    pub fn unresolved_call_site_files(&self, name: &str, attribute_to: Option<&str>) -> Result<Vec<String>> {
        let sql = format!(
            "SELECT DISTINCT c.file_path FROM calls c WHERE c.callee_name = ?1
             AND NOT EXISTS (SELECT 1 FROM nodes n WHERE n.name = ?1 AND n.file_path = c.file_path
                             AND n.label IN ('Function','Method','Class'))
             AND NOT {}",
            Self::EXTERNAL_BOUND
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([name], |r| r.get::<_, String>(0))?;
        let files: Vec<String> = rows.collect::<rusqlite::Result<_>>()?;
        let Some(target) = attribute_to else { return Ok(files) };
        let mut stmt =
            self.conn.prepare("SELECT DISTINCT file_path FROM nodes WHERE name = ?1 AND label IN ('Function','Method','Class')")?;
        let def_files: Vec<String> =
            stmt.query_map([name], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<_>>()?;
        if def_files.len() <= 1 {
            return Ok(files); // one definition — everything attributes to it
        }
        fn shared_prefix(a: &str, b: &str) -> usize {
            a.split('/').zip(b.split('/')).take_while(|(x, y)| x == y).count()
        }
        Ok(files
            .into_iter()
            .filter(|f| {
                let best = def_files.iter().map(|d| shared_prefix(f, d)).max().unwrap_or(0);
                shared_prefix(f, target) == best
            })
            .collect())
    }

    /// Does any inherit clause reference `name`? A dirty def name colliding with
    /// an inherit name can flip name-uniqueness for INHERITS/IMPLEMENTS edges
    /// and hyperedge membership — the wave path falls back to full in that case.
    pub fn inherits_name_referenced(&self, name: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM inherits WHERE impl_name = ?1 OR super_name = ?1",
            [name],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Auto-heal: delete edges whose endpoint no longer exists (e.g. compiler-grade
    /// edges reused across a reindex that renamed/removed their nodes). Dropping is
    /// always precision-safe; set-based SQL keeps it deterministic. Returns count.
    pub fn drop_dangling_edges(&self) -> Result<usize> {
        let n = self.conn.execute(
            "DELETE FROM edges WHERE src NOT IN (SELECT id FROM nodes) OR dst NOT IN (SELECT id FROM nodes)",
            [],
        )?;
        Ok(n)
    }

    /// Deterministic graph-invariant gate, run before every index commit.
    /// Returns human-readable violations (empty = healthy):
    /// - dangling edges (an endpoint id with no node row);
    /// - code-node ids violating the lowercase FQN normalization schema;
    /// - tree-sitter CALLS edges missing their `justification` tag (the
    ///   per-tier precision proof obligation).
    ///
    /// Cycles are NOT checked: recursion makes call-graph cycles legitimate.
    pub fn validate_graph(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT e.src, e.dst, e.relation FROM edges e
             LEFT JOIN nodes s ON s.id = e.src LEFT JOIN nodes d ON d.id = e.dst
             WHERE s.id IS NULL OR d.id IS NULL LIMIT 20",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(format!(
                "dangling edge: {} -{}-> {}",
                r.get::<_, String>(0)?, r.get::<_, String>(2)?, r.get::<_, String>(1)?
            ))
        })?;
        for r in rows {
            out.push(r?);
        }
        let mut stmt = self.conn.prepare(
            "SELECT id FROM nodes WHERE label IN ('Function','Method','Class','Interface','Enum','Type','Module','File')
             AND (id GLOB '*[A-Z]*' OR id GLOB '*[^a-zA-Z0-9._]*') LIMIT 20",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(format!("non-normalized FQN: {}", r.get::<_, String>(0)?))
        })?;
        for r in rows {
            out.push(r?);
        }
        let mut stmt = self.conn.prepare(
            "SELECT src, dst FROM edges WHERE relation = 'Calls' AND tier = 'TreeSitter'
             AND json_extract(data,'$.metadata.justification') IS NULL LIMIT 20",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(format!(
                "CALLS edge without justification: {} -> {}",
                r.get::<_, String>(0)?, r.get::<_, String>(1)?
            ))
        })?;
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Insert edges, KEEPING any existing edge with the same (src,dst,relation)
    /// key — in the partial path the only possible conflicts are surviving
    /// compiler-grade edges, which outrank the tree-sitter resolution.
    pub fn bulk_insert_edges_keep_existing(&self, edges: &[Edge]) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO edges(src,dst,relation,tier,confidence,src_file,src_line,data) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(src,dst,relation) DO NOTHING",
        )?;
        for e in edges {
            let data = serde_json::to_string(e)?;
            stmt.execute(params![e.src, e.dst, enum_str(&e.relation)?, enum_str(&e.tier)?, enum_str(&e.confidence)?, e.src_file, e.src_line, data])?;
        }
        Ok(())
    }

    pub fn save_inherits(&self, file_path: &str, items: &[RawInherit]) -> Result<()> {
        self.conn.execute("DELETE FROM inherits WHERE file_path = ?1", [file_path])?;
        let mut stmt = self.conn.prepare(
            "INSERT INTO inherits(impl_name, super_name, kind, file_path) VALUES(?1, ?2, ?3, ?4)",
        )?;
        for it in items {
            let kind = match it.kind { InheritKind::Extends => "Extends", InheritKind::Implements => "Implements" };
            stmt.execute(params![it.impl_name, it.super_name, kind, file_path])?;
        }
        Ok(())
    }

    pub fn all_inherits(&self) -> Result<Vec<RawInherit>> {
        let mut stmt = self.conn.prepare("SELECT impl_name, super_name, kind FROM inherits")?;
        let rows = stmt.query_map([], |r| {
            let kind = if r.get::<_, String>(2)? == "Implements" { InheritKind::Implements } else { InheritKind::Extends };
            Ok(RawInherit { impl_name: r.get(0)?, super_name: r.get(1)?, kind })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn save_fields(&self, file_path: &str, items: &[RawField]) -> Result<()> {
        self.conn.execute("DELETE FROM fields WHERE file_path = ?1", [file_path])?;
        let mut stmt = self
            .conn
            .prepare("INSERT INTO fields(class_id, field_name, type_name, file_path) VALUES(?1, ?2, ?3, ?4)")?;
        for f in items {
            stmt.execute(params![f.class_id, f.field_name, f.type_name, file_path])?;
        }
        Ok(())
    }

    pub fn all_fields(&self) -> Result<Vec<RawField>> {
        let mut stmt = self.conn.prepare("SELECT class_id, field_name, type_name FROM fields")?;
        let rows = stmt.query_map([], |r| {
            Ok(RawField { class_id: r.get(0)?, field_name: r.get(1)?, type_name: r.get(2)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn save_imports(&self, file_path: &str, items: &[RawImport]) -> Result<()> {
        self.conn.execute("DELETE FROM imports WHERE file_path = ?1", [file_path])?;
        let mut stmt =
            self.conn.prepare("INSERT INTO imports(file_path, name, module) VALUES(?1, ?2, ?3)")?;
        for i in items {
            stmt.execute(params![file_path, i.name, i.module])?;
        }
        Ok(())
    }

    pub fn all_imports(&self) -> Result<Vec<RawImport>> {
        let mut stmt = self.conn.prepare("SELECT file_path, name, module FROM imports")?;
        let rows = stmt.query_map([], |r| {
            Ok(RawImport { file_path: r.get(0)?, name: r.get(1)?, module: r.get(2)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn save_locals(&self, file_path: &str, items: &[RawLocal]) -> Result<()> {
        self.conn.execute("DELETE FROM locals WHERE file_path = ?1", [file_path])?;
        let mut stmt = self
            .conn
            .prepare("INSERT INTO locals(caller_id, var_name, type_name, file_path) VALUES(?1, ?2, ?3, ?4)")?;
        for l in items {
            stmt.execute(params![l.caller_id, l.var_name, l.type_name, file_path])?;
        }
        Ok(())
    }

    pub fn save_type_refs(&self, file_path: &str, names: &[String]) -> Result<()> {
        self.conn.execute("DELETE FROM type_refs WHERE file_path = ?1", [file_path])?;
        let mut stmt = self.conn.prepare("INSERT INTO type_refs(file_path, type_name) VALUES(?1, ?2)")?;
        for n in names {
            stmt.execute(params![file_path, n])?;
        }
        Ok(())
    }

    pub fn all_locals(&self) -> Result<Vec<RawLocal>> {
        let mut stmt = self.conn.prepare("SELECT caller_id, var_name, type_name FROM locals")?;
        let rows = stmt.query_map([], |r| {
            Ok(RawLocal { caller_id: r.get(0)?, var_name: r.get(1)?, type_name: r.get(2)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn clear_hyperedges(&self) -> Result<()> {
        self.conn.execute("DELETE FROM hyperedges", [])?;
        self.conn.execute("DELETE FROM hyperedge_members", [])?;
        Ok(())
    }

    pub fn implementers_of(&self, name: &str) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT s.data FROM edges e \
             JOIN nodes t ON t.id = e.dst \
             JOIN nodes s ON s.id = e.src \
             WHERE e.relation IN ('Implements', 'Inherits') AND t.name = ?1",
        )?;
        let rows = stmt.query_map([name], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    pub fn bulk_upsert_nodes(&self, nodes: &[Node]) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO nodes(id,name,parts,doc_text,label,language,file_path,line_start,line_end,community,pagerank,betweenness,data)
             VALUES(?1,?2,cg_subwords(?2),substr(json_extract(?11,'$.metadata.text'),1,8000),?3,?4,?5,?6,?7,?8,?9,?10,?11)
             ON CONFLICT(id) DO UPDATE SET name=?2,parts=cg_subwords(?2),doc_text=substr(json_extract(?11,'$.metadata.text'),1,8000),label=?3,language=?4,file_path=?5,line_start=?6,line_end=?7,community=?8,pagerank=?9,betweenness=?10,data=?11",
        )?;
        for n in nodes {
            let data = serde_json::to_string(n)?;
            let label = enum_str(&n.label)?;
            stmt.execute(params![n.id, n.name, label, n.language, n.file_path, n.line_start, n.line_end, n.community, n.pagerank, n.betweenness, data])?;
        }
        Ok(())
    }

    pub fn bulk_upsert_edges(&self, edges: &[Edge]) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO edges(src,dst,relation,tier,confidence,src_file,src_line,data) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(src,dst,relation) DO UPDATE SET tier=?4,confidence=?5,src_file=?6,src_line=?7,data=?8",
        )?;
        for e in edges {
            let data = serde_json::to_string(e)?;
            stmt.execute(params![e.src, e.dst, enum_str(&e.relation)?, enum_str(&e.tier)?, enum_str(&e.confidence)?, e.src_file, e.src_line, data])?;
        }
        Ok(())
    }

    pub fn edge_count(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT count(*) FROM edges", [], |r| r.get(0))?)
    }

    pub fn nodes_by_label(&self, label: &str) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare("SELECT data FROM nodes WHERE label = ?1")?;
        let rows = stmt.query_map([label], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    pub fn node_count(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT count(*) FROM nodes", [], |r| r.get(0))?)
    }
}

fn enum_str<T: serde::Serialize>(v: &T) -> Result<String> {
    match serde_json::to_value(v)? {
        serde_json::Value::String(s) => Ok(s),
        other => Ok(other.to_string()),
    }
}

/// Inverse of `enum_str`: parse a unit-variant enum from its stored column text.
fn enum_from<T: serde::de::DeserializeOwned>(s: &str) -> Result<T> {
    Ok(serde_json::from_value(serde_json::Value::String(s.to_string()))?)
}

/// RAII transaction: rolls back on drop unless `commit()` was called, so an
/// error mid-index can never leave the connection stuck inside a transaction.
/// Replaces the bare `begin()`/`commit()` pair whose transactional context was
/// tracked only in comments.
pub struct Txn<'a> {
    store: &'a Store,
    done: bool,
}

impl Store {
    pub fn txn(&self) -> Result<Txn<'_>> {
        self.conn.execute_batch("BEGIN")?;
        Ok(Txn { store: self, done: false })
    }
}

impl Txn<'_> {
    pub fn commit(mut self) -> Result<()> {
        self.done = true;
        self.store.conn.execute_batch("COMMIT")?;
        Ok(())
    }
}

impl Drop for Txn<'_> {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.store.conn.execute_batch("ROLLBACK");
        }
    }
}

/// Cheap monotonic freshness key for cache invalidation: the meta `generation`
/// counter bumped once per committed index. Read over a short-lived read-only
/// connection (no migration, no write lock). 0 = pre-generation DB or unreadable.
pub fn generation(db: &Path) -> u64 {
    register_vec_extension();
    Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .ok()
        .and_then(|c| {
            c.query_row("SELECT value FROM meta WHERE key='generation'", [], |r| r.get::<_, String>(0))
                .ok()
        })
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Run an arbitrary READ-ONLY SQL query against the graph database. The
/// connection is opened read-only, so writes (INSERT/UPDATE/DELETE/DROP) fail
/// at the engine. Returns (column names, rows-as-strings), capped at `limit`.
pub fn query_readonly(db: &Path, sql: &str, limit: usize) -> Result<(Vec<String>, Vec<Vec<String>>)> {
    register_vec_extension();
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare(sql)?;
    let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let ncol = cols.len();
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        if out.len() >= limit {
            break;
        }
        let mut row = Vec::with_capacity(ncol);
        for i in 0..ncol {
            row.push(value_ref_to_string(r.get_ref(i)?));
        }
        out.push(row);
    }
    Ok((cols, out))
}

fn value_ref_to_string(v: rusqlite::types::ValueRef<'_>) -> String {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => String::new(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) => f.to_string(),
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
        ValueRef::Blob(_) => "<blob>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codegraph_core::{
        Confidence, EdgeRelation, HyperedgeRelation, Metadata, NodeLabel, ResolutionTier,
    };

    fn node(id: &str) -> Node {
        Node {
            id: id.into(), label: NodeLabel::Function, name: id.into(),
            file_path: "f.rs".into(), line_start: 1, line_end: 2, language: "rust".into(),
            metadata: Metadata::new(), community: None, pagerank: 0.0, betweenness: 0.0,
        }
    }

    #[test]
    fn schema_migrates_and_versions() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.schema_version().unwrap(), SCHEMA_VERSION);
    }

    /// Field-measured search noise stays fixed: identifier search puts CODE
    /// above Document fragments, collapses doc chunks to one row per file,
    /// and caps docs to a footnote when code answered.
    #[test]
    fn search_ranks_code_first_dedups_and_caps_docs() {
        let s = Store::open_in_memory().unwrap();
        let mut nodes = vec![Node { name: "getProfile".into(), ..node("m.getprofile") }];
        for i in 0..8 {
            // two chunks in file 0 (must collapse), one chunk in files 1..8
            for c in 0..if i == 0 { 2 } else { 1 } {
                let mut md = Metadata::new();
                md.insert(
                    "text".to_string(),
                    serde_json::Value::String(format!("rule {i}.{c}: never call getProfile here")),
                );
                nodes.push(Node {
                    label: NodeLabel::Document,
                    name: format!("doc {i}.{c} getProfile mention"),
                    file_path: format!("docs/rule{i}.md"),
                    metadata: md,
                    ..node(&format!("doc.{i}.{c}"))
                });
            }
        }
        s.bulk_upsert_nodes(&nodes).unwrap();
        let hits = s.search_smart("getProfile", 20).unwrap();
        assert_eq!(hits[0].name, "getProfile", "code symbol must outrank doc fragments");
        assert_eq!(hits[0].label, NodeLabel::Function);
        let docs: Vec<&Node> = hits.iter().filter(|n| n.label == NodeLabel::Document).collect();
        assert!(docs.len() <= 5, "docs must be capped to a footnote when code answered: {}", docs.len());
        let mut files: Vec<&str> = docs.iter().map(|n| n.file_path.as_str()).collect();
        files.sort_unstable();
        files.dedup();
        assert_eq!(files.len(), docs.len(), "one row per document file");
    }

    #[test]
    fn node_and_edge_roundtrip_with_fts() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_node(&node("a")).unwrap();
        s.upsert_node(&node("b")).unwrap();
        s.upsert_edge(&Edge {
            src: "a".into(), dst: "b".into(), relation: EdgeRelation::Calls,
            tier: ResolutionTier::TreeSitter, confidence: Confidence::Extracted,
            src_file: "f.rs".into(), src_line: 1, metadata: Metadata::new(),
        })
        .unwrap();
        assert_eq!(s.get_node("a").unwrap().unwrap().name, "a");
        assert_eq!(s.get_edges_for_node("a").unwrap().len(), 1);
        assert_eq!(s.get_edges_for_node("b").unwrap().len(), 1);
        s.rebuild_fts().unwrap();
    }

    #[test]
    fn hyperedge_roundtrip() {
        let s = Store::open_in_memory().unwrap();
        for id in ["a", "b", "c"] {
            s.upsert_node(&node(id)).unwrap();
        }
        let h = Hyperedge {
            id: "h1".into(), relation: HyperedgeRelation::Implement, label: "impls".into(),
            confidence: Confidence::Extracted, tier: ResolutionTier::TreeSitter, metadata: Metadata::new(),
        };
        let members: Vec<HyperedgeMember> = ["a", "b", "c"]
            .iter()
            .map(|n| HyperedgeMember { hyperedge_id: "h1".into(), node_id: (*n).into(), role: None })
            .collect();
        s.upsert_hyperedge(&h, &members).unwrap();
        let got = s.get_hyperedges_for_node("b").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1.len(), 3);
    }

    #[test]
    fn coverage_counts_dropped_calls() {
        let s = Store::open_in_memory().unwrap();
        for id in ["foo", "c1", "c2", "c3"] {
            s.upsert_node(&node(id)).unwrap();
        }
        // c1 resolves to foo; c2 and c3 call "foo" textually but never resolved.
        s.upsert_edge(&Edge {
            src: "c1".into(), dst: "foo".into(), relation: EdgeRelation::Calls,
            tier: ResolutionTier::TreeSitter, confidence: Confidence::Extracted,
            src_file: "f.rs".into(), src_line: 1, metadata: Metadata::new(),
        })
        .unwrap();
        let raw = |caller: &str| codegraph_core::RawCall {
            caller_id: caller.into(), callee_name: "foo".into(), line: 1,
            receiver: codegraph_core::Receiver::Bare, enclosing_class: None,
        };
        s.save_calls("f.rs", &[raw("c1"), raw("c2"), raw("c3")]).unwrap();

        let cov = s.coverage_for_callers("foo").unwrap();
        assert_eq!(cov.total_call_sites, 3);
        assert_eq!(cov.resolved, 1);
        assert_eq!(cov.dropped, 2);
        assert!(cov.may_be_incomplete);

        // c1's single outbound call to foo resolved → callees coverage is complete.
        let out = s.coverage_for_callees("c1").unwrap();
        assert_eq!(out.total_call_sites, 1);
        assert_eq!(out.resolved, 1);
        assert!(!out.may_be_incomplete);
    }

    #[test]
    fn manifest_and_context_roundtrip() {
        let s = Store::open_in_memory().unwrap();
        s.save_manifest("f.rs", "deadbeef", 123).unwrap();
        assert_eq!(s.manifest_for("f.rs").unwrap().unwrap().sha256, "deadbeef");
        assert!(s.manifest_for("missing").unwrap().is_none());
        s.add_context("src/auth", "handles login", 1).unwrap();
        let ctx = s.contexts_for("src/").unwrap();
        assert_eq!(ctx.len(), 1);
        assert_eq!(ctx[0].summary, "handles login");
    }

    #[test]
    fn zst_export_import_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.db");
        let s = Store::open(&db).unwrap();
        s.upsert_node(&node("x")).unwrap();
        let zst = dir.path().join("g.db.zst");
        s.export_zst(&zst).unwrap();
        assert!(zst.metadata().unwrap().len() > 0);
        let db2 = dir.path().join("g2.db");
        let s2 = Store::import_zst(&zst, &db2).unwrap();
        assert_eq!(s2.get_node("x").unwrap().unwrap().name, "x");
    }

    #[test]
    fn delete_file_data_prunes_vectors() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_node(&node("sym")).unwrap();
        s.upsert_vector("sym", &[0.1, 0.2, 0.3]).unwrap();
        assert_eq!(s.all_vectors().unwrap().len(), 1);
        s.delete_file_data("f.rs").unwrap();
        assert_eq!(s.all_vectors().unwrap().len(), 0, "embeddings must be pruned with their file's nodes");
    }

    #[test]
    fn query_readonly_reads_and_blocks_writes() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.db");
        Store::open(&db).unwrap().upsert_node(&node("x")).unwrap();
        let (cols, rows) = query_readonly(&db, "SELECT COUNT(*) AS n FROM nodes", 10).unwrap();
        assert_eq!(cols, vec!["n"]);
        assert_eq!(rows[0][0], "1");
        assert!(query_readonly(&db, "DELETE FROM nodes", 10).is_err());
    }

    #[test]
    fn fts_search_finds_node() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_node(&node("helper")).unwrap();
        s.upsert_node(&node("widget")).unwrap();
        s.rebuild_fts().unwrap();
        let hits = s.search_fts("helper", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "helper");
    }

    #[test]
    fn callers_of_query() {
        use codegraph_core::{Confidence, Edge, EdgeRelation, Metadata, ResolutionTier};
        let s = Store::open_in_memory().unwrap();
        s.upsert_node(&node("main")).unwrap();
        s.upsert_node(&node("helper")).unwrap();
        s.upsert_edge(&Edge {
            src: "main".into(), dst: "helper".into(), relation: EdgeRelation::Calls,
            tier: ResolutionTier::TreeSitter, confidence: Confidence::Inferred,
            src_file: "f.rs".into(), src_line: 1, metadata: Metadata::new(),
        }).unwrap();
        let callers = s.callers_of("helper").unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].name, "main");
        assert!(s.callers_of("main").unwrap().is_empty());
    }

    #[test]
    fn vector_roundtrip_stores_normalized() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_node(&node("v")).unwrap();
        s.upsert_vector("v", &[0.1, 0.2, 0.3]).unwrap();
        let all = s.all_vectors().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].1.len(), 3);
        // Stored L2-normalized: unit magnitude, direction preserved (v[1]/v[0] == 2.0).
        let mag: f32 = all[0].1.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((mag - 1.0).abs() < 1e-5, "vector stored normalized");
        assert!((all[0].1[1] / all[0].1[0] - 2.0).abs() < 1e-5, "direction preserved");
    }

    #[test]
    fn fts_stays_in_sync_via_triggers() {
        let s = Store::open_in_memory().unwrap();
        // insert → searchable, no manual FTS call
        s.upsert_node(&node("helper")).unwrap();
        assert_eq!(s.search_fts("helper", 10).unwrap().len(), 1);
        // idempotent upsert → no duplicate rows
        s.upsert_node(&node("helper")).unwrap();
        assert_eq!(s.search_fts("helper", 10).unwrap().len(), 1);
        // rename (same id) → old name gone, new one found
        let mut renamed = node("helper");
        renamed.name = "assistant".into();
        s.upsert_node(&renamed).unwrap();
        assert!(s.search_fts("helper", 10).unwrap().is_empty());
        assert_eq!(s.search_fts("assistant", 10).unwrap().len(), 1);
        // delete → pruned
        s.delete_file_data("f.rs").unwrap();
        assert!(s.search_fts("assistant", 10).unwrap().is_empty());
    }

    #[test]
    fn knn_returns_nearest_first() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_vectors(&[
            ("x".into(), vec![1.0, 0.0, 0.0]),
            ("y".into(), vec![0.0, 1.0, 0.0]),
            ("xy".into(), vec![1.0, 1.0, 0.0]),
        ])
        .unwrap();
        let hits = s.knn(&[1.0, 0.1, 0.0], 2).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, "x", "closest vector first");
        assert!(hits[0].1 > hits[1].1, "scores descend (cosine)");
        assert!(hits[0].1 <= 1.0 + 1e-5);
    }

    #[test]
    fn graph_nodes_strips_doc_text_only() {
        let s = Store::open_in_memory().unwrap();
        let mut doc = node("doc1");
        doc.metadata.insert("text".into(), serde_json::json!("a huge chunk"));
        doc.metadata.insert("content_type".into(), serde_json::json!("md"));
        s.upsert_node(&doc).unwrap();
        let light = &s.graph_nodes().unwrap()[0];
        assert!(!light.metadata.contains_key("text"), "chunk text stripped");
        assert_eq!(light.metadata.get("content_type"), Some(&serde_json::json!("md")));
        assert_eq!(light.id, "doc1");
        // the full row still has it
        assert!(s.get_node("doc1").unwrap().unwrap().metadata.contains_key("text"));
    }

    #[test]
    fn graph_edges_matches_all_edges_topology() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_node(&node("a")).unwrap();
        s.upsert_node(&node("b")).unwrap();
        s.upsert_edge(&Edge {
            src: "a".into(), dst: "b".into(), relation: EdgeRelation::Calls,
            tier: ResolutionTier::Scip, confidence: Confidence::Inferred,
            src_file: "f.rs".into(), src_line: 7, metadata: Metadata::new(),
        })
        .unwrap();
        let (full, light) = (s.all_edges().unwrap(), s.graph_edges().unwrap());
        assert_eq!(full.len(), 1);
        assert_eq!(light, full, "typed-column load must equal the JSON load");
    }

    #[test]
    fn update_analytics_syncs_columns_and_json() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_node(&node("f")).unwrap();
        s.update_analytics(&[("f".into(), 2, 0.5, 1.5, 3, 4)]).unwrap();
        let n = s.get_node("f").unwrap().unwrap();
        assert_eq!(n.community, Some(2));
        assert_eq!(n.pagerank, 0.5);
        assert_eq!(n.betweenness, 1.5);
        assert_eq!(n.metadata.get("fan_in"), Some(&serde_json::json!(3)));
        assert_eq!(n.metadata.get("fan_out"), Some(&serde_json::json!(4)));
    }

    #[test]
    fn generation_bumps_and_reads() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("g.db");
        let s = Store::open(&db).unwrap();
        assert_eq!(generation(&db), 0);
        s.bump_generation().unwrap();
        s.bump_generation().unwrap();
        assert_eq!(generation(&db), 2);
    }

    #[test]
    fn txn_rolls_back_on_drop() {
        let s = Store::open_in_memory().unwrap();
        {
            let _t = s.txn().unwrap();
            s.upsert_node(&node("ghost")).unwrap();
            // dropped without commit → rollback
        }
        assert!(s.get_node("ghost").unwrap().is_none(), "uncommitted txn must roll back");
        let t = s.txn().unwrap();
        s.upsert_node(&node("kept")).unwrap();
        t.commit().unwrap();
        assert!(s.get_node("kept").unwrap().is_some());
    }

    #[test]
    fn calls_roundtrip_and_prune() {
        use codegraph_core::RawCall;
        let s = Store::open_in_memory().unwrap();
        s.save_calls("a.rs", &[RawCall { caller_id: "a.main".into(), callee_name: "helper".into(), line: 2, receiver: Default::default(), enclosing_class: None }]).unwrap();
        assert_eq!(s.all_calls().unwrap().len(), 1);
        s.delete_file_data("a.rs").unwrap();
        assert_eq!(s.all_calls().unwrap().len(), 0);
    }
}

#[cfg(test)]
mod subword_tests {
    #[test]
    fn subwords_splits_camel_snake_digits() {
        assert_eq!(super::subwords("OrderCheckoutSession"), "order checkout session");
        assert_eq!(super::subwords("HTTPServer2Go"), "http server 2 go");
        assert_eq!(super::subwords("snake_case_name"), "snake case name");
        assert_eq!(super::subwords("plain"), ""); // single token adds nothing
    }
}
