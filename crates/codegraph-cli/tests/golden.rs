//! Golden per-language resolution fixtures: tiny programs with EXPECTED edges.
//! This is the executable proof behind "precision-sacred" — every supported
//! language asserts both what MUST resolve (with its justification tier) and
//! what must NOT (`!CALLS` decoys). Runs the real binary end-to-end.
//!
//! Fixture format (`expected.edges`, one assertion per line, `#` comments):
//!   CALLS  b.py:consume -> a.py:helper   @GlobalUnique
//!   !CALLS main.ts:go -> other.ts:doThing
//!   COUNT CALLS 7
//! Symbol refs are `file-suffix:name` or `file-suffix:Class.method`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use codegraph_core::{Edge, EdgeRelation, Node};
use codegraph_store::Store;

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Index a fixture with the real binary into an isolated cache; return the graph.
fn index_fixture(lang: &str) -> (Vec<Node>, Vec<Edge>) {
    let src = fixture_root().join(lang);
    assert!(src.exists(), "missing fixture dir {}", src.display());
    let tmp = std::env::temp_dir().join(format!("cg_golden_{lang}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let cache = tmp.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_codegraph"))
        .args(["index", "--full"])
        .arg(&src)
        .env("CODEGRAPH_CACHE_DIR", &cache)
        // isolate the project registry too: 13 parallel tests must not race on
        // (or pollute) the USER's ~/.config/codegraph/registry.json
        .env("XDG_CONFIG_HOME", tmp.join("config"))
        .output()
        .expect("failed to run codegraph binary");
    assert!(out.status.success(), "index failed: {}", String::from_utf8_lossy(&out.stderr));
    // one project in an isolated cache → exactly one graph.db
    let db = std::fs::read_dir(&cache)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path().join("graph.db"))
        .find(|p| p.exists())
        .expect("no graph.db produced");
    let store = Store::open(&db).unwrap();
    let nodes = store.all_nodes().unwrap();
    let edges = store.all_edges().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    (nodes, edges)
}

/// Resolve `file-suffix:name` (or `file:Class.method`) to node ids.
fn resolve_ref<'a>(nodes: &'a [Node], r: &str) -> Vec<&'a str> {
    let (file, sym) = r.split_once(':').unwrap_or_else(|| panic!("bad symbol ref: {r}"));
    nodes
        .iter()
        .filter(|n| n.file_path.ends_with(file))
        .filter(|n| match sym.split_once('.') {
            Some((cls, m)) => {
                n.name == m && n.id.ends_with(&format!(".{}.{}", cls.to_lowercase(), m.to_lowercase()))
            }
            None => n.name == sym,
        })
        .map(|n| n.id.as_str())
        .collect()
}

fn run_fixture(lang: &str) {
    let (nodes, edges) = index_fixture(lang);
    let by_key: HashMap<(&str, &str), &Edge> = edges
        .iter()
        .filter(|e| e.relation == EdgeRelation::Calls)
        .map(|e| ((e.src.as_str(), e.dst.as_str()), e))
        .collect();
    let expected = std::fs::read_to_string(fixture_root().join(lang).join("expected.edges"))
        .unwrap_or_else(|_| panic!("missing expected.edges for {lang}"));
    for raw in expected.lines() {
        let line = raw.split('#').next().unwrap().trim();
        if line.is_empty() {
            continue;
        }
        if let Some(count) = line.strip_prefix("COUNT CALLS") {
            let want: usize = count.trim().parse().unwrap();
            assert_eq!(by_key.len(), want, "{lang}: total CALLS mismatch — got {:?}", by_key.keys());
            continue;
        }
        let negated = line.starts_with('!');
        let body = line.trim_start_matches('!').trim_start_matches("CALLS").trim();
        let (pair, tier) = match body.split_once('@') {
            Some((p, t)) => (p.trim(), Some(t.trim())),
            None => (body, None),
        };
        let (from, to) = pair.split_once("->").unwrap_or_else(|| panic!("bad line: {raw}"));
        let (srcs, dsts) = (resolve_ref(&nodes, from.trim()), resolve_ref(&nodes, to.trim()));
        if negated {
            for s in &srcs {
                for d in &dsts {
                    assert!(
                        !by_key.contains_key(&(*s, *d)),
                        "{lang}: FORBIDDEN edge exists: {s} -> {d}"
                    );
                }
            }
            continue;
        }
        assert!(!srcs.is_empty(), "{lang}: no node matches {}", from.trim());
        assert!(!dsts.is_empty(), "{lang}: no node matches {}", to.trim());
        let hit = srcs.iter().find_map(|s| dsts.iter().find_map(|d| by_key.get(&(*s, *d))));
        let edge = hit.unwrap_or_else(|| {
            panic!(
                "{lang}: expected edge {} -> {} missing; edges: {:?}",
                from.trim(),
                to.trim(),
                by_key.keys().collect::<Vec<_>>()
            )
        });
        if let Some(t) = tier {
            let j = edge.metadata.get("justification").and_then(|v| v.as_str()).unwrap_or("");
            assert_eq!(j, t, "{lang}: {} -> {} resolved via {j}, expected {t}", from.trim(), to.trim());
        }
    }
}

macro_rules! golden {
    ($($name:ident),+) => {
        $(#[test]
        fn $name() {
            run_fixture(stringify!($name));
        })+
    };
}

golden!(rust, python, javascript, typescript, go, swift, kotlin, java, c, cpp, ruby, csharp, bash);
