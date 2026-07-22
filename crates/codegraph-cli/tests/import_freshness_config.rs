//! External-review regressions (round 2, items 5–7): atomic import, strict
//! freshness, and --path-based config resolution.

use std::process::Command;

fn cg(dir: &std::path::Path, cache: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_codegraph"))
        .args(args)
        .current_dir(dir)
        .env("CODEGRAPH_CACHE_DIR", cache)
        .output()
        .unwrap()
}

fn setup(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let tmp = std::env::temp_dir().join(format!("cg_ifc_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    (tmp.clone(), tmp.join("cache"))
}

/// R5: a corrupt artifact must be rejected with the EXISTING graph untouched.
#[test]
fn import_corrupt_artifact_rolls_back() {
    let (tmp, cache) = setup("corrupt");
    std::fs::write(tmp.join("a.py"), "def live():\n    pass\n").unwrap();
    let out = cg(&tmp, &cache, &["index", "."]);
    assert!(out.status.success());
    // locate the live db and snapshot its bytes
    let db = walk_for_db(&cache);
    let before = std::fs::read(&db).unwrap();

    std::fs::write(tmp.join("bad.zst"), b"definitely not zstd").unwrap();
    let out = cg(&tmp, &cache, &["import", "bad.zst"]);
    assert!(
        !out.status.success(),
        "corrupt artifact must fail the import"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("corrupt") || err.contains("rejected"), "{err}");
    assert_eq!(
        std::fs::read(&db).unwrap(),
        before,
        "existing graph must be byte-untouched"
    );
    assert!(
        !db.with_extension("db.import-tmp").exists(),
        "no temp residue"
    );
}

/// R5: an artifact that decompresses past the cap is refused (zstd bomb guard).
#[test]
fn import_oversized_artifact_refused() {
    let (tmp, cache) = setup("oversize");
    std::fs::write(tmp.join("a.py"), "def live():\n    pass\n").unwrap();
    cg(&tmp, &cache, &["index", "."]);
    // a real (valid) artifact...
    let out = cg(&tmp, &cache, &["export"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let art = tmp.join(".codegraph/graph.db.zst");
    assert!(art.exists());
    // ...but a 0 MB cap: refuse loudly, keep the live graph
    let out = Command::new(env!("CARGO_BIN_EXE_codegraph"))
        .args(["import", ".codegraph/graph.db.zst"])
        .current_dir(&tmp)
        .env("CODEGRAPH_CACHE_DIR", &cache)
        .env("CODEGRAPH_IMPORT_MAX_MB", "0")
        .output()
        .unwrap();
    assert!(!out.status.success(), "over-cap artifact must be refused");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cap"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// R6: when auto-reindex FAILS, the CLI must refuse (strict default) and serve
/// only with --allow-stale.
#[test]
fn strict_freshness_refuses_stale_serve() {
    let (tmp, cache) = setup("stale");
    std::fs::write(tmp.join("a.py"), "def live():\n    pass\n").unwrap();
    assert!(cg(&tmp, &cache, &["index", "."]).status.success());

    let run = |extra: &[&str], allow_env: bool| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_codegraph"));
        c.args(["search", "live"])
            .args(extra)
            .current_dir(&tmp)
            .env("CODEGRAPH_CACHE_DIR", &cache)
            .env("CODEGRAPH_TEST_FAIL_REFRESH", "1");
        if allow_env {
            c.env("CODEGRAPH_ALLOW_STALE", "1");
        }
        c.output().unwrap()
    };
    let out = run(&[], false);
    assert!(
        !out.status.success(),
        "failed reindex must refuse to serve (strict default)"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("stale"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // explicit opt-ins serve the last snapshot
    for (extra, allow_env) in [(&["--allow-stale"][..], false), (&[][..], true)] {
        let out = run(extra, allow_env);
        assert!(out.status.success(), "opt-in stale serve must work");
        assert!(String::from_utf8_lossy(&out.stdout).contains("live"));
    }
}

/// R7: config must resolve from --path (not cwd), and a wrong-typed field must
/// error with file + field — never silently default the whole config.
#[test]
fn config_resolves_from_path_and_errors_loud() {
    let (tmp, cache) = setup("config");
    let repo = tmp.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("a.py"), "def live():\n    pass\n").unwrap();
    // valid project config in the TARGET repo
    std::fs::write(repo.join(".codegraph.toml"), "[llm]\nrerank = false\n").unwrap();
    // run from OUTSIDE the repo with --path: must succeed and read that config
    let outside = tmp.join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_codegraph"))
        .args(["index", repo.to_str().unwrap()])
        .current_dir(&outside)
        .env("CODEGRAPH_CACHE_DIR", &cache)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // wrong TYPE in one field: loud, contextual error naming file and field
    std::fs::write(repo.join(".codegraph.toml"), "[llm]\nrerank = \"yes\"\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_codegraph"))
        .args([
            "search",
            "live",
            "--path",
            repo.to_str().unwrap(),
            "--no-autoheal",
        ])
        .current_dir(&outside)
        .env("CODEGRAPH_CACHE_DIR", &cache)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "typed config error must not silently default"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(".codegraph.toml") && err.contains("rerank"),
        "error must name file and field: {err}"
    );
}

fn walk_for_db(cache: &std::path::Path) -> std::path::PathBuf {
    fn rec(d: &std::path::Path) -> Option<std::path::PathBuf> {
        for e in std::fs::read_dir(d).ok()?.flatten() {
            let p = e.path();
            if p.file_name().is_some_and(|n| n == "graph.db") {
                return Some(p);
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

/// P0: the size cap env must parse STRICTLY — malformed or overflowing values
/// are actionable errors, never silent defaults.
#[test]
fn import_env_cap_is_strict() {
    let (tmp, cache) = setup("envcap");
    std::fs::write(tmp.join("a.py"), "def live():\n    pass\n").unwrap();
    cg(&tmp, &cache, &["index", "."]);
    assert!(cg(&tmp, &cache, &["export"]).status.success());
    for (val, needle) in [
        ("banana", "not a valid size"),
        ("-5", "not a valid size"),
        ("0", "forbids every import"),
        ("999999999999999999", "overflows"),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_codegraph"))
            .args(["import", ".codegraph/graph.db.zst"])
            .current_dir(&tmp)
            .env("CODEGRAPH_CACHE_DIR", &cache)
            .env("CODEGRAPH_IMPORT_MAX_MB", val)
            .output()
            .unwrap();
        assert!(!out.status.success(), "cap={val} must be rejected");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains(needle),
            "cap={val}: expected {needle:?} in: {err}"
        );
    }
    // no abandoned temp files after handled failures
    let db_dir = walk_for_db(&cache).parent().unwrap().to_path_buf();
    let leftovers: Vec<_> = std::fs::read_dir(&db_dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("import-"))
        .collect();
    assert!(leftovers.is_empty(), "no temp residue: {leftovers:?}");
}

/// P1: `config` recovery subcommands work on MALFORMED TOML; typed commands
/// fail loudly.
#[test]
fn malformed_config_remains_repairable() {
    let (tmp, cache) = setup("cfgfix");
    std::fs::write(tmp.join("a.py"), "def live():\n    pass\n").unwrap();
    std::fs::write(tmp.join(".codegraph.toml"), "this is = = broken [[[").unwrap();
    // recovery command must SUCCEED (it exists to fix the file)
    let out = cg(&tmp, &cache, &["config"]);
    assert!(
        out.status.success(),
        "config (recovery) must work on malformed TOML: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // a typed command fails loudly, naming the file
    let out = cg(&tmp, &cache, &["search", "live", "--no-autoheal"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains(".codegraph.toml"));
}
