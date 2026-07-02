//! Persistent SQLite store for the CodeGraph knowledge graph.

use std::path::Path;

use codegraph_core::{
    Coverage, Edge, Hyperedge, HyperedgeMember, InheritKind, Node, RawCall, RawField, RawImport, RawInherit, RawLocal,
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

const SCHEMA_VERSION: i64 = 3;

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

/// SQL scalar `cg_subwords(name)` so FTS rebuilds stay pure SQL.
fn register_subwords_fn(conn: &Connection) -> Result<()> {
    use rusqlite::functions::FunctionFlags;
    conn.create_scalar_function(
        "cg_subwords",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let name: String = ctx.get(0)?;
            Ok(subwords(&name))
        },
    )?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub struct ManifestEntry {
    pub file_path: String,
    pub sha256: String,
    pub mtime: i64,
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
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Store> {
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
        register_subwords_fn(&conn)?;
        let store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version(version INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS nodes(
               id TEXT PRIMARY KEY, name TEXT, label TEXT, language TEXT, file_path TEXT,
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
             CREATE TABLE IF NOT EXISTS vectors(
               node_id TEXT PRIMARY KEY, vec BLOB NOT NULL);
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
             CREATE TABLE IF NOT EXISTS locals(
               caller_id TEXT, var_name TEXT, type_name TEXT, file_path TEXT);
             CREATE INDEX IF NOT EXISTS idx_locals_file ON locals(file_path);
             CREATE VIRTUAL TABLE IF NOT EXISTS nodes_fts USING fts5(
               id UNINDEXED, name, parts, label, language);
             CREATE TABLE IF NOT EXISTS meta(
               key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS cochanges(
               file_a TEXT, file_b TEXT, n INTEGER,
               PRIMARY KEY(file_a, file_b)) WITHOUT ROWID;",
        )?;
        // Additive column migrations for pre-existing DBs (best-effort; the next
        // index rebuilds calls anyway). Ignore "duplicate column" on re-run.
        for stmt in [
            "ALTER TABLE calls ADD COLUMN receiver TEXT",
            "ALTER TABLE calls ADD COLUMN enclosing_class TEXT",
        ] {
            let _ = self.conn.execute(stmt, []);
        }
        let current: Option<i64> = self
            .conn
            .query_row("SELECT version FROM schema_version LIMIT 1", [], |r| r.get(0))
            .optional()?;
        match current {
            None => {
                self.conn
                    .execute("INSERT INTO schema_version(version) VALUES(?1)", [SCHEMA_VERSION])?;
            }
            Some(v) if v < SCHEMA_VERSION => {
                // FTS shape changed (v3 adds `parts`): rebuild in place from `nodes`
                // so old DBs keep working without a manual reindex.
                self.conn.execute_batch(
                    "DROP TABLE IF EXISTS nodes_fts;
                     CREATE VIRTUAL TABLE nodes_fts USING fts5(id UNINDEXED, name, parts, label, language);",
                )?;
                self.rebuild_fts()?;
                self.conn.execute("UPDATE schema_version SET version = ?1", [SCHEMA_VERSION])?;
            }
            Some(_) => {}
        }
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
            "INSERT INTO nodes(id,name,label,language,file_path,line_start,line_end,community,pagerank,betweenness,data)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
             ON CONFLICT(id) DO UPDATE SET name=?2,label=?3,language=?4,file_path=?5,line_start=?6,line_end=?7,community=?8,pagerank=?9,betweenness=?10,data=?11",
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
        let mut out = Vec::new();
        for hid in ids {
            let data: String = self
                .conn
                .query_row("SELECT data FROM hyperedges WHERE id=?1", [&hid], |r| r.get(0))?;
            let h: Hyperedge = serde_json::from_str(&data)?;
            let mut mstmt = self
                .conn
                .prepare("SELECT hyperedge_id,node_id,role FROM hyperedge_members WHERE hyperedge_id=?1")?;
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

    pub fn rebuild_fts(&self) -> Result<()> {
        self.conn.execute_batch(
            "DELETE FROM nodes_fts;
             INSERT INTO nodes_fts(id,name,parts,label,language)
               SELECT id, name, cg_subwords(name), label, language FROM nodes;",
        )?;
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
               AND n.file_path NOT LIKE '%test%' AND n.file_path NOT LIKE '%spec%'
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
            "SELECT COUNT(*) FROM calls WHERE callee_name = ?1
             AND (file_path LIKE '%test%' OR file_path LIKE '%spec%' OR file_path LIKE '%Tests%')",
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
    pub fn search_fts(&self, query: &str, limit: usize) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT n.data FROM nodes_fts f JOIN nodes n ON n.id = f.id WHERE nodes_fts MATCH ?1 LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![query, limit as i64], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    /// Forgiving search: exact FTS first (precise), and if that's empty fall back
    /// to an OR-of-prefixes over the query's identifier tokens — so a camelCase
    /// fragment like `OrderCheckout` finds `OrderCheckoutSession` (one FTS token).
    /// Multiple words are OR'd (multi-term search). FTS special chars are stripped.
    pub fn search_smart(&self, raw: &str, limit: usize) -> Result<Vec<Node>> {
        let exact = self.search_fts(raw, limit)?;
        if !exact.is_empty() {
            return Ok(exact);
        }
        // Fallback: split the query into identifier subwords (camel/snake aware —
        // `OrderCheckout` → order checkout) and OR-prefix them; the FTS `parts`
        // column indexes node names the same way, so mid-identifier words match.
        let mut seen = std::collections::HashSet::new();
        let fts = raw
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .flat_map(|t| {
                let sw = subwords(t);
                if sw.is_empty() { vec![t.to_string()] } else { sw.split(' ').map(str::to_string).collect() }
            })
            .filter(|t| t.len() > 1)
            .filter(|t| seen.insert(t.to_lowercase()))
            .map(|t| format!("{t}*"))
            .collect::<Vec<_>>()
            .join(" OR ");
        if fts.is_empty() {
            return Ok(exact);
        }
        self.search_fts(&fts, limit)
    }

    /// Regex search over symbol names (anywhere in the name, not just a prefix) —
    /// for patterns FTS can't express (middle fragments, alternations, anchors).
    pub fn search_regex(&self, pattern: &str, limit: usize) -> Result<Vec<Node>> {
        let re = regex::Regex::new(pattern).map_err(|e| StoreError::Msg(format!("bad regex: {e}")))?;
        let mut stmt = self.conn.prepare("SELECT data FROM nodes WHERE name <> ''")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            let n: Node = serde_json::from_str(&r?)?;
            if re.is_match(&n.name) {
                out.push(n);
                if out.len() >= limit {
                    break;
                }
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

    /// Coverage for `callers(name)`: how many of the textual call sites naming
    /// `name` actually resolved into a `Calls` edge to a node of that name. The
    /// difference is the count dropped (ambiguous / external / unresolved) — a
    /// real signal that the precise callers list may be incomplete.
    pub fn coverage_for_callers(&self, name: &str) -> Result<Coverage> {
        let total: i64 =
            self.conn.query_row("SELECT COUNT(*) FROM calls WHERE callee_name = ?1", [name], |r| r.get(0))?;
        let resolved: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM calls c WHERE c.callee_name = ?1 AND EXISTS(
                 SELECT 1 FROM edges e JOIN nodes n ON n.id = e.dst
                 WHERE e.src = c.caller_id AND e.relation = 'Calls' AND n.name = ?1)",
            [name],
            |r| r.get(0),
        )?;
        Ok(Coverage::callers(name, resolved as usize, total as usize))
    }

    /// Coverage for `callees(caller_id)`: how many of the caller's outbound call
    /// sites resolved to an internal definition. Dropped = external (library) or
    /// unresolved calls absent from the callees list.
    pub fn coverage_for_callees(&self, caller_id: &str) -> Result<Coverage> {
        let total: i64 =
            self.conn.query_row("SELECT COUNT(*) FROM calls WHERE caller_id = ?1", [caller_id], |r| r.get(0))?;
        let resolved: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM calls c WHERE c.caller_id = ?1 AND EXISTS(
                 SELECT 1 FROM edges e JOIN nodes n ON n.id = e.dst
                 WHERE e.src = ?1 AND e.relation = 'Calls' AND n.name = c.callee_name)",
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

    /// Batch-store vectors in ONE transaction — 40k+ individual inserts otherwise
    /// autocommit one row at a time (minutes). Vectors stored L2-normalized so
    /// semantic scoring is a plain dot product (== cosine).
    pub fn upsert_vectors(&self, items: &[(String, Vec<f32>)]) -> Result<()> {
        self.conn.execute_batch("BEGIN")?;
        {
            let mut stmt =
                self.conn.prepare("INSERT OR REPLACE INTO vectors(node_id, vec) VALUES(?1, ?2)")?;
            for (id, v) in items {
                let n = codegraph_core::normalize(v);
                let mut bytes = Vec::with_capacity(n.len() * 4);
                for f in &n {
                    bytes.extend_from_slice(&f.to_le_bytes());
                }
                stmt.execute(params![id, bytes])?;
            }
        }
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    pub fn all_vectors(&self) -> Result<Vec<(String, Vec<f32>)>> {
        let mut stmt = self.conn.prepare("SELECT node_id, vec FROM vectors")?;
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
        // Prune embeddings for this file's nodes BEFORE the nodes go (vectors are
        // keyed by node_id, not file_path) — otherwise renamed/removed symbols
        // leave orphaned vectors that pollute semantic search and grow the DB.
        self.conn.execute(
            "DELETE FROM vectors WHERE node_id IN (SELECT id FROM nodes WHERE file_path = ?1)",
            [file_path],
        )?;
        self.conn.execute("DELETE FROM nodes WHERE file_path = ?1", [file_path])?;
        self.conn.execute("DELETE FROM calls WHERE file_path = ?1", [file_path])?;
        self.conn.execute("DELETE FROM inherits WHERE file_path = ?1", [file_path])?;
        self.conn.execute("DELETE FROM fields WHERE file_path = ?1", [file_path])?;
        self.conn.execute("DELETE FROM locals WHERE file_path = ?1", [file_path])?;
        self.conn.execute("DELETE FROM imports WHERE file_path = ?1", [file_path])?;
        Ok(())
    }

    /// All manifest rows (path + sha + mtime) — for the staleness probe.
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

    pub fn begin(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN")?;
        Ok(())
    }

    pub fn commit(&self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    pub fn bulk_upsert_nodes(&self, nodes: &[Node]) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO nodes(id,name,label,language,file_path,line_start,line_end,community,pagerank,betweenness,data)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
             ON CONFLICT(id) DO UPDATE SET name=?2,label=?3,language=?4,file_path=?5,line_start=?6,line_end=?7,community=?8,pagerank=?9,betweenness=?10,data=?11",
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

/// Run an arbitrary READ-ONLY SQL query against the graph database. The
/// connection is opened read-only, so writes (INSERT/UPDATE/DELETE/DROP) fail
/// at the engine. Returns (column names, rows-as-strings), capped at `limit`.
pub fn query_readonly(db: &Path, sql: &str, limit: usize) -> Result<(Vec<String>, Vec<Vec<String>>)> {
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
