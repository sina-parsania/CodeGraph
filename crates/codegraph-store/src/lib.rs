//! Persistent SQLite store for the CodeGraph knowledge graph.

use std::path::Path;

use codegraph_core::{Edge, Hyperedge, HyperedgeMember, Node};
use rusqlite::{params, Connection, OptionalExtension};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("serialization: {0}")]
    Json(#[from] serde_json::Error),
}

type Result<T> = std::result::Result<T, StoreError>;

const SCHEMA_VERSION: i64 = 1;

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
             CREATE TABLE IF NOT EXISTS edges(
               src TEXT, dst TEXT, relation TEXT, tier TEXT, confidence TEXT,
               src_file TEXT, src_line INTEGER, data TEXT NOT NULL,
               PRIMARY KEY(src, dst, relation));
             CREATE INDEX IF NOT EXISTS idx_edges_dst ON edges(dst);
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
             CREATE VIRTUAL TABLE IF NOT EXISTS nodes_fts USING fts5(
               id UNINDEXED, name, label, language);",
        )?;
        let current: Option<i64> = self
            .conn
            .query_row("SELECT version FROM schema_version LIMIT 1", [], |r| r.get(0))
            .optional()?;
        if current.is_none() {
            self.conn
                .execute("INSERT INTO schema_version(version) VALUES(?1)", [SCHEMA_VERSION])?;
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
             INSERT INTO nodes_fts(id,name,label,language) SELECT id,name,label,language FROM nodes;",
        )?;
        Ok(())
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
        let mut bytes = Vec::with_capacity(v.len() * 4);
        for f in v {
            bytes.extend_from_slice(&f.to_le_bytes());
        }
        self.conn.execute(
            "INSERT OR REPLACE INTO vectors(node_id, vec) VALUES(?1, ?2)",
            params![node_id, bytes],
        )?;
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
    fn vector_roundtrip() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_node(&node("v")).unwrap();
        s.upsert_vector("v", &[0.1, 0.2, 0.3]).unwrap();
        let all = s.all_vectors().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].1.len(), 3);
        assert!((all[0].1[1] - 0.2).abs() < 1e-6);
    }
}
