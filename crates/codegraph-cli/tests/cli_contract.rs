//! Release-gate contract tests — every field-reported CLI bug stays fixed:
//! - `callers --files` is a name-level UNION with a pure machine stdout
//!   (path lines only; human notes on stderr, never `#` comment lines);
//! - definition files stay as labeled evidence rows;
//! - `stats` aliases `status` (agents guess the MCP tool name for the CLI).

use std::process::Command;

fn run(dir: &std::path::Path, cache: &std::path::Path, args: &[&str]) -> (String, String, bool) {
    let out = Command::new(env!("CARGO_BIN_EXE_codegraph"))
        .args(args)
        .current_dir(dir)
        .env("CODEGRAPH_CACHE_DIR", cache)
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

#[test]
fn callers_files_union_with_pure_machine_stdout() {
    let tmp = std::env::temp_dir().join(format!("cg_contract_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let cache = tmp.join("cache");
    // TWO same-name definitions + one caller of each — the unpinned answer
    // must be the union, not a dominant-definition narrowing.
    std::fs::write(
        tmp.join("a.py"),
        "def shared():\n    return 1\n\ndef use_a():\n    shared()\n",
    )
    .unwrap();
    std::fs::write(
        tmp.join("b.py"),
        "def shared():\n    return 2\n\ndef use_b():\n    shared()\n",
    )
    .unwrap();
    let (_, _, ok) = run(&tmp, &cache, &["index", "."]);
    assert!(ok, "index must succeed");

    let (stdout, stderr, ok) = run(
        &tmp,
        &cache,
        &["callers", "shared", "--files", "--no-autoheal"],
    );
    assert!(ok);
    // machine contract: every stdout line is a path row (optionally ~ / tagged)
    assert!(
        !stdout.lines().any(|l| l.trim_start().starts_with('#')),
        "no # comment lines on stdout: {stdout}"
    );
    // union: both callers' files present as evidence (resolution may drop the
    // ambiguous call edges, but the textual layer must name both files)
    assert!(stdout.contains("a.py"), "union must include a.py: {stdout}");
    assert!(stdout.contains("b.py"), "union must include b.py: {stdout}");
    // the multi-definition note lives on stderr, not in the machine output
    assert!(
        stderr.contains("2 definitions"),
        "human note goes to stderr: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn stats_is_a_status_alias() {
    let tmp = std::env::temp_dir().join(format!("cg_statsalias_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let (stdout, _, ok) = run(&tmp, &tmp.join("cache"), &["stats"]);
    assert!(ok, "`codegraph stats` must work as an alias");
    assert!(
        stdout.contains("codegraph"),
        "alias prints the status card: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}
