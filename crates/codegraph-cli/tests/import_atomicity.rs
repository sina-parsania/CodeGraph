//! P0 import regressions: exactly ONE activation operation, real writer-lock
//! exclusion, staging isolation, durability, and rollback. Every test here
//! failed (or was unwritable) against the double-rename implementation.

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_codegraph"))
}

fn cg(dir: &Path, cache: &Path, args: &[&str]) -> std::process::Output {
    let mut c = bin();
    c.args(args)
        .current_dir(dir)
        .env("CODEGRAPH_CACHE_DIR", cache);
    c.output().unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ok, "git {args:?} must succeed");
}

/// RAII temp dir (tempfile) — unique per test regardless of PID.
fn setup(src: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let cache = tmp.path().join("cache");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.name", "qa"]);
    git(&repo, &["config", "user.email", "qa@test.invalid"]);
    std::fs::write(repo.join("a.py"), src).unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "base"]);
    (tmp, repo, cache)
}

fn db_dir_of(cache: &Path) -> PathBuf {
    fn rec(d: &Path) -> Option<PathBuf> {
        for e in std::fs::read_dir(d).ok()?.flatten() {
            let p = e.path();
            if p.file_name().is_some_and(|n| n == "graph.db") {
                return Some(p.parent().unwrap().to_path_buf());
            }
            if p.is_dir() {
                if let Some(f) = rec(&p) {
                    return Some(f);
                }
            }
        }
        None
    }
    rec(cache).expect("graph.db must exist in the cache")
}

/// Files only an import could have left behind (staging, backups, staging
/// locks). The active graph's own `graph.lock`/`-wal`/`-shm` are LIVE state,
/// not residue — they are asserted separately where activation semantics
/// require their absence.
fn import_residue(db_dir: &Path) -> Vec<String> {
    std::fs::read_dir(db_dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("import-") || n.ends_with(".bak") || n.ends_with(".tmp"))
        .collect()
}

/// After a SUCCESSFUL activation no stale WAL/SHM of the replaced graph may
/// sit next to the new file (SQLite would replay a foreign WAL into it).
fn assert_no_stale_wal(db_dir: &Path) {
    for side in ["graph.db-wal", "graph.db-shm"] {
        assert!(
            !db_dir.join(side).exists(),
            "{side} must not survive activation"
        );
    }
}

/// P0 regression (double rename): a valid export→import round trip must exit
/// ZERO, print the success message, leave a queryable graph restamped to the
/// destination repo, and leave no staging/backup/WAL residue.
#[test]
fn import_round_trip_exits_zero_and_activates() {
    let (_t, repo, cache) = setup("def live_symbol():\n    pass\n");
    assert!(cg(&repo, &cache, &["index", "."]).status.success());
    assert!(cg(&repo, &cache, &["export"]).status.success());
    let art = repo.join(".codegraph/graph.db.zst");
    assert!(art.exists());

    // import into a SECOND repo (fresh cache entry → identity must restamp)
    let (_t2, repo2, cache2) = setup("def live_symbol():\n    pass\n");
    std::fs::create_dir_all(repo2.join(".codegraph")).unwrap();
    std::fs::copy(&art, repo2.join(".codegraph/graph.db.zst")).unwrap();
    let out = cg(&repo2, &cache2, &["import", ".codegraph/graph.db.zst"]);
    assert!(
        out.status.success(),
        "valid import must exit 0 — stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("imported"), "success message: {stdout}");

    // no staging/backup/WAL residue in the destination graph dir
    let db_dir = db_dir_of(&cache2);
    assert_eq!(import_residue(&db_dir), Vec::<String>::new(), "no residue");
    assert_no_stale_wal(&db_dir);

    // graph is live, identity restamped to repo2: the identity gate would
    // fail this query if repo_root still pointed at the exporter
    let out = cg(&repo2, &cache2, &["search", "live_symbol", "--no-autoheal"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("live_symbol"));
}

/// Activation failure (destination not replaceable) must exit nonzero, name
/// the failure, and leave NO staging residue — never a half-activated state.
#[test]
fn activation_failure_reports_and_cleans_staging() {
    let (_t, repo, cache) = setup("def live_symbol():\n    pass\n");
    assert!(cg(&repo, &cache, &["index", "."]).status.success());
    assert!(cg(&repo, &cache, &["export"]).status.success());
    let db_dir = db_dir_of(&cache);
    let db = db_dir.join("graph.db");
    // make the destination un-replaceable: a NON-EMPTY DIRECTORY at the db
    // path makes rename(2) fail with a real filesystem error
    std::fs::remove_file(&db).unwrap();
    std::fs::create_dir(&db).unwrap();
    std::fs::write(db.join("occupied"), b"x").unwrap();

    let out = cg(&repo, &cache, &["import", ".codegraph/graph.db.zst"]);
    assert!(
        !out.status.success(),
        "activation failure must exit nonzero"
    );
    assert!(
        db.join("occupied").exists(),
        "pre-existing destination state must survive a failed activation"
    );
    assert_eq!(
        import_residue(&db_dir),
        Vec::<String>::new(),
        "failed activation must leave no staging residue"
    );
}

/// A failure BEFORE activation keeps the old graph byte-identical (corrupt
/// artifact case is covered elsewhere; this exercises the validation stage).
#[test]
fn pre_activation_failure_keeps_old_graph_bytes() {
    let (_t, repo, cache) = setup("def live_symbol():\n    pass\n");
    assert!(cg(&repo, &cache, &["index", "."]).status.success());
    assert!(cg(&repo, &cache, &["export"]).status.success());
    let db = db_dir_of(&cache).join("graph.db");
    let before = std::fs::read(&db).unwrap();
    // truncated artifact: valid zstd header, corrupt stream → fails in staging
    let art = repo.join(".codegraph/graph.db.zst");
    let bytes = std::fs::read(&art).unwrap();
    std::fs::write(repo.join("cut.zst"), &bytes[..bytes.len() / 2]).unwrap();
    let out = cg(&repo, &cache, &["import", "cut.zst"]);
    assert!(!out.status.success());
    assert_eq!(
        std::fs::read(&db).unwrap(),
        before,
        "old graph must be byte-identical after a pre-activation failure"
    );
    assert_eq!(
        import_residue(db.parent().unwrap()),
        Vec::<String>::new(),
        "pre-activation failure must leave no staging residue"
    );
}

fn lock_path_of(cache: &Path) -> PathBuf {
    db_dir_of(cache).join("graph.lock")
}

/// Import must never proceed without the writer lock: with the lock held by
/// another open-file-description and a zero wait budget, import fails with a
/// CONTENTION error and the graph is untouched.
#[test]
fn import_refuses_when_lock_contended() {
    let (_t, repo, cache) = setup("def live_symbol():\n    pass\n");
    assert!(cg(&repo, &cache, &["index", "."]).status.success());
    assert!(cg(&repo, &cache, &["export"]).status.success());
    let db = db_dir_of(&cache).join("graph.db");
    let before = std::fs::read(&db).unwrap();

    // hold the real flock from this process (independent descriptor)
    let holder = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path_of(&cache))
        .unwrap();
    holder.try_lock().expect("test must win the free lock");

    let out = bin()
        .args(["import", ".codegraph/graph.db.zst"])
        .current_dir(&repo)
        .env("CODEGRAPH_CACHE_DIR", &cache)
        .env("CODEGRAPH_LOCK_WAIT_SECS", "0")
        .output()
        .unwrap();
    assert!(!out.status.success(), "lock contention must refuse import");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("held") || err.contains("another"),
        "contention must be named as contention: {err}"
    );
    assert_eq!(std::fs::read(&db).unwrap(), before, "graph untouched");
}

/// Lock-file I/O failure is DISTINCT from contention and equally fatal —
/// import must not continue unlocked.
#[test]
fn import_refuses_on_lock_file_io_failure() {
    let (_t, repo, cache) = setup("def live_symbol():\n    pass\n");
    assert!(cg(&repo, &cache, &["index", "."]).status.success());
    assert!(cg(&repo, &cache, &["export"]).status.success());
    let lock = lock_path_of(&cache);
    // a DIRECTORY at the lock path makes open() fail with a real I/O error
    std::fs::remove_file(&lock).ok();
    std::fs::create_dir(&lock).unwrap();
    let out = cg(&repo, &cache, &["import", ".codegraph/graph.db.zst"]);
    assert!(!out.status.success(), "lock I/O failure must refuse import");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("lock") && !err.contains("held by"),
        "I/O failure must not read as contention: {err}"
    );
}

/// Staging isolation: another process's staging file (any name we didn't
/// create) must survive our import untouched — random staging names mean no
/// import can delete another's work-in-progress.
#[test]
fn foreign_staging_file_survives_import() {
    let (_t, repo, cache) = setup("def live_symbol():\n    pass\n");
    assert!(cg(&repo, &cache, &["index", "."]).status.success());
    assert!(cg(&repo, &cache, &["export"]).status.success());
    let db_dir = db_dir_of(&cache);
    let foreign = db_dir.join("graph.db.import-someoneelse.tmp");
    std::fs::write(&foreign, b"another importer's staging bytes").unwrap();

    let out = cg(&repo, &cache, &["import", ".codegraph/graph.db.zst"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read(&foreign).unwrap(),
        b"another importer's staging bytes",
        "a foreign staging file must never be deleted or truncated"
    );
    std::fs::remove_file(&foreign).unwrap();
    assert_eq!(
        import_residue(&db_dir),
        Vec::<String>::new(),
        "own staging cleaned"
    );
    assert_no_stale_wal(&db_dir);
}

/// Two concurrent importers on the SAME graph must serialize on the real
/// flock and both succeed — no clobbered staging, no torn activation.
#[test]
fn two_concurrent_importers_serialize_and_succeed() {
    let (_t, repo, cache) = setup("def live_symbol():\n    pass\n");
    assert!(cg(&repo, &cache, &["index", "."]).status.success());
    assert!(cg(&repo, &cache, &["export"]).status.success());

    let spawn = || {
        let mut c = bin();
        c.args(["import", ".codegraph/graph.db.zst"])
            .current_dir(&repo)
            .env("CODEGRAPH_CACHE_DIR", &cache)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        c.spawn().unwrap()
    };
    let a = spawn();
    let b = spawn();
    let ra = a.wait_with_output().unwrap();
    let rb = b.wait_with_output().unwrap();
    for (tag, r) in [("A", &ra), ("B", &rb)] {
        assert!(
            r.status.success(),
            "importer {tag} failed: {}",
            String::from_utf8_lossy(&r.stderr)
        );
    }
    let db_dir = db_dir_of(&cache);
    assert_eq!(import_residue(&db_dir), Vec::<String>::new(), "no residue");
    assert_no_stale_wal(&db_dir);
    let out = cg(&repo, &cache, &["search", "live_symbol", "--no-autoheal"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("live_symbol"));
}

/// A strict, malformed lock-wait budget must be an actionable error, never a
/// silent default.
#[test]
fn lock_wait_env_parses_strictly() {
    let (_t, repo, cache) = setup("def live_symbol():\n    pass\n");
    assert!(cg(&repo, &cache, &["index", "."]).status.success());
    assert!(cg(&repo, &cache, &["export"]).status.success());
    let out = bin()
        .args(["import", ".codegraph/graph.db.zst"])
        .current_dir(&repo)
        .env("CODEGRAPH_CACHE_DIR", &cache)
        .env("CODEGRAPH_LOCK_WAIT_SECS", "soon")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("CODEGRAPH_LOCK_WAIT_SECS"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
