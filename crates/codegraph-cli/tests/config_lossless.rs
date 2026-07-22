//! P1 config-losslessness regressions: `set`/`unset` must never clobber a
//! malformed file, backups are mandatory before destructive writes, and
//! comments/unrelated keys survive mutations byte-faithfully.

use std::path::Path;
use std::process::Command;

fn cg(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_codegraph"))
        .args(args)
        .current_dir(dir)
        // point the GLOBAL config into the sandbox so tests never touch ~
        .env("XDG_CONFIG_HOME", dir.join("xdg"))
        .env("CODEGRAPH_CACHE_DIR", dir.join("cache"))
        .output()
        .unwrap()
}

/// A successful mutation preserves comments and unrelated keys (toml_edit),
/// updates exactly the requested key, and leaves a backup.
#[test]
fn set_preserves_comments_and_unrelated_keys() {
    let t = tempfile::TempDir::new().unwrap();
    let cfg = t.path().join(".codegraph.toml");
    std::fs::write(
        &cfg,
        "# my precious comment\n[llm]\nprovider = \"ollama\" # inline note\nrerank = true\n",
    )
    .unwrap();
    let out = cg(t.path(), &["config", "set", "llm.model", "m7", "--local"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let after = std::fs::read_to_string(&cfg).unwrap();
    assert!(after.contains("# my precious comment"), "{after}");
    assert!(after.contains("# inline note"), "{after}");
    assert!(after.contains("provider = \"ollama\""), "{after}");
    assert!(after.contains("rerank = true"), "{after}");
    assert!(after.contains("model = \"m7\""), "{after}");
    assert!(
        t.path().join(".codegraph.toml.bak").exists(),
        "backup must exist after a destructive write"
    );
}

/// `set` and `unset` on MALFORMED TOML must error contextually (naming the
/// file and pointing at `config edit`) and leave the file byte-identical.
#[test]
fn set_unset_on_malformed_toml_leave_bytes_unchanged() {
    let t = tempfile::TempDir::new().unwrap();
    let cfg = t.path().join(".codegraph.toml");
    let broken = "this is = = not valid toml [[[ # but MY bytes\n";
    std::fs::write(&cfg, broken).unwrap();
    for args in [
        &["config", "set", "llm.model", "x", "--local"][..],
        &["config", "unset", "llm.model", "--local"][..],
    ] {
        let out = cg(t.path(), args);
        assert!(!out.status.success(), "{args:?} must refuse malformed TOML");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains(".codegraph.toml") && err.contains("config edit"),
            "error must name the file and direct to config edit: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&cfg).unwrap(),
            broken,
            "malformed config must stay byte-identical after {args:?}"
        );
    }
}

/// Same contract for the GLOBAL config file.
#[test]
fn malformed_global_config_is_never_overwritten() {
    let t = tempfile::TempDir::new().unwrap();
    let gdir = t.path().join("xdg").join("codegraph");
    std::fs::create_dir_all(&gdir).unwrap();
    let gcfg = gdir.join("config.toml");
    let broken = "[llm\nbroken = \n";
    std::fs::write(&gcfg, broken).unwrap();
    let out = cg(t.path(), &["config", "set", "llm.model", "x"]);
    assert!(!out.status.success());
    assert_eq!(std::fs::read_to_string(&gcfg).unwrap(), broken);
}

/// Backup failure must abort BEFORE the destructive write.
#[test]
fn backup_failure_leaves_original_intact() {
    let t = tempfile::TempDir::new().unwrap();
    let cfg = t.path().join(".codegraph.toml");
    let orig = "[llm]\nmodel = \"keepme\"\n";
    std::fs::write(&cfg, orig).unwrap();
    // a DIRECTORY at the backup path makes fs::copy fail with a real error
    std::fs::create_dir(t.path().join(".codegraph.toml.bak")).unwrap();
    let out = cg(t.path(), &["config", "set", "llm.model", "x", "--local"]);
    assert!(!out.status.success(), "failed backup must abort the write");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("back up"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), orig);
}

/// `config path` and `config edit` stay usable on malformed TOML — they are
/// the designated repair tools.
#[test]
#[cfg(unix)]
fn path_and_edit_remain_usable_on_malformed_toml() {
    let t = tempfile::TempDir::new().unwrap();
    let cfg = t.path().join(".codegraph.toml");
    std::fs::write(&cfg, "broken = = [[[\n").unwrap();
    let out = cg(t.path(), &["config", "path"]);
    assert!(
        out.status.success(),
        "config path must work on malformed TOML: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // an "editor" that repairs the file: edit must let it run and then accept
    let fixer = t.path().join("fixer.sh");
    std::fs::write(
        &fixer,
        "#!/bin/sh\nprintf '[llm]\\nmodel = \"fixed\"\\n' > \"$1\"\n",
    )
    .unwrap();
    let mut perms = std::fs::metadata(&fixer).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&fixer, perms).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_codegraph"))
        .args(["config", "edit", "--local"])
        .current_dir(t.path())
        .env("XDG_CONFIG_HOME", t.path().join("xdg"))
        .env("CODEGRAPH_CACHE_DIR", t.path().join("cache"))
        .env("VISUAL", &fixer)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "config edit must remain usable to repair malformed TOML: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&cfg).unwrap(),
        "[llm]\nmodel = \"fixed\"\n"
    );
}

/// Missing config: `set` creates the file; `unset` reports not-set without
/// inventing a file.
#[test]
fn missing_config_set_creates_unset_reports() {
    let t = tempfile::TempDir::new().unwrap();
    let cfg = t.path().join(".codegraph.toml");
    let out = cg(t.path(), &["config", "unset", "llm.model", "--local"]);
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("not set"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let out = cg(t.path(), &["config", "set", "llm.model", "m1", "--local"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(std::fs::read_to_string(&cfg).unwrap().contains("m1"));
}
