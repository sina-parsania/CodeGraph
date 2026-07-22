//! External-review regressions (round 2): review/`changes` correctness and
//! dead-code false positives — each test failed before its fix.

use std::process::Command;

fn cg(dir: &std::path::Path, cache: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_codegraph"))
        .args(args)
        .current_dir(dir)
        .env("CODEGRAPH_CACHE_DIR", cache)
        .output()
        .unwrap()
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ok, "git {args:?} must succeed");
}

fn setup_repo(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let tmp = std::env::temp_dir().join(format!("cg_review_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    git(&tmp, &["init", "-q"]);
    git(&tmp, &["config", "user.name", "qa"]);
    git(&tmp, &["config", "user.email", "qa@test.invalid"]);
    (tmp.clone(), tmp.join("cache"))
}

/// R1: an invalid base ref must FAIL LOUD — empty stdout from a failed git
/// diff was previously reported as a clean "no changes" (false success).
#[test]
fn changes_with_invalid_base_fails_loud() {
    let (tmp, cache) = setup_repo("badbase");
    std::fs::write(tmp.join("a.py"), "def f():\n    pass\n").unwrap();
    git(&tmp, &["add", "-A"]);
    git(&tmp, &["commit", "-qm", "x"]);
    cg(&tmp, &cache, &["index", "."]);

    let out = cg(&tmp, &cache, &["changes", "--base", "no-such-ref-xyz"]);
    assert!(!out.status.success(), "invalid base must exit nonzero");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("no-such-ref-xyz"),
        "stderr must name the bad ref: {err}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("no changes"),
        "must never claim 'no changes' on git failure: {stdout}"
    );
}

/// R2: two same-name functions — coverage and fan-in must key on the node ID,
/// never leak between the twins. `covered` has a resolved test caller;
/// `helper` (same name, other file… here distinct names per twin-pair to pin
/// identity) stays NO-TESTS.
#[test]
fn same_name_symbols_do_not_share_metrics() {
    let (tmp, cache) = setup_repo("samename");
    // twin definitions of `process` in two files; only a.py's is tested
    // (resolved via import narrowing), b.py's twin must NOT inherit that.
    std::fs::write(tmp.join("a.py"), "def process():\n    return 1\n").unwrap();
    std::fs::write(tmp.join("b.py"), "def process():\n    return 2\n").unwrap();
    std::fs::write(
        tmp.join("test_a.py"),
        "from a import process\n\ndef test_process():\n    process()\n",
    )
    .unwrap();
    git(&tmp, &["add", "-A"]);
    git(&tmp, &["commit", "-qm", "base"]);
    cg(&tmp, &cache, &["index", "."]);

    // touch BOTH twins so both appear in the diff
    std::fs::write(tmp.join("a.py"), "def process():\n    return 10\n").unwrap();
    std::fs::write(tmp.join("b.py"), "def process():\n    return 20\n").unwrap();
    let out = cg(&tmp, &cache, &["changes", "--base", "HEAD"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let a_line = text
        .lines()
        .find(|l| l.contains("a.py:"))
        .expect("a.py row: {text}");
    let b_line = text
        .lines()
        .find(|l| l.contains("b.py:"))
        .expect("b.py row");
    assert!(
        a_line.contains("tested"),
        "a.py twin has a RESOLVED test caller: {a_line}"
    );
    assert!(
        !b_line.contains(" tested"),
        "b.py twin must NOT inherit the sibling's coverage: {b_line}"
    );
}

/// R2: a deleted code file must surface as UNKNOWN — not silently vanish from
/// the risk view (a risk gate passing on missing data is a false pass).
#[test]
fn deleted_file_shows_unknown() {
    let (tmp, cache) = setup_repo("deleted");
    std::fs::write(tmp.join("gone.py"), "def doomed():\n    pass\n").unwrap();
    std::fs::write(tmp.join("keep.py"), "def kept():\n    pass\n").unwrap();
    git(&tmp, &["add", "-A"]);
    git(&tmp, &["commit", "-qm", "base"]);
    cg(&tmp, &cache, &["index", "."]);
    std::fs::remove_file(tmp.join("gone.py")).unwrap();
    cg(&tmp, &cache, &["index", "."]); // heal: graph no longer has gone.py
    let out = cg(&tmp, &cache, &["changes", "--base", "HEAD", "--md"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("UNKNOWN") && text.contains("gone.py"),
        "deleted file must appear as UNKNOWN: {text}"
    );
}

/// R3: qualified calls (`IndexLock::acquire()`), macro-wrapped calls, and
/// inline #[test] fns must not be dead-code candidates.
#[test]
fn dead_code_qualified_macro_and_test_fns() {
    let (tmp, cache) = setup_repo("dead");
    std::fs::write(
        tmp.join("lib.rs"),
        concat!(
            "struct Lock;\nimpl Lock {\n    fn acquire() {}\n}\n\n",
            "fn used_qualified() { Lock::acquire(); }\n\n",
            "fn used_in_macro() {}\nfn caller() { assert_eq!(used_in_macro(), ()); }\n\n",
            "fn used_as_value() {}\n",
            "fn used_as_callback() {}\n",
            "extern \"C\" fn ffi_callback_zz() {}\n",
            "pub fn exported_api_zz() {}\n",
            "fn wire() {\n",
            "    let handler = used_as_value;\n",
            "    handler();\n",
            "    std::iter::once(1).map(|_| ()).for_each(drop);\n",
            "    Some(1).map(wrap(used_as_callback));\n",
            "}\n",
            "fn wrap(f: fn()) -> fn(i32) -> Option<i32> { let _ = f; |_| None }\n\n",
            "fn truly_dead_zzz() {}\n\n",
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn inline_test_fn() { super::caller(); super::wire(); }\n}\n"
        ),
    )
    .unwrap();
    git(&tmp, &["add", "-A"]);
    git(&tmp, &["commit", "-qm", "x"]);
    cg(&tmp, &cache, &["index", "."]);
    let out = cg(&tmp, &cache, &["dead-code"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.contains("acquire "),
        "qualified call names its target: {text}"
    );
    assert!(
        !text.contains("used_in_macro"),
        "macro-wrapped call is textual evidence: {text}"
    );
    assert!(
        !text.contains("inline_test_fn"),
        "#[test] fns are never candidates: {text}"
    );
    assert!(
        !text.contains("used_as_value"),
        "a function bound as a value must not be a certain candidate: {text}"
    );
    assert!(
        !text.contains("used_as_callback"),
        "a function passed as an argument must not be a certain candidate: {text}"
    );
    assert!(
        !text.contains("ffi_callback_zz"),
        "an extern \"C\" fn is FFI surface, not a certain candidate: {text}"
    );
    assert!(
        !text.contains("exported_api_zz "),
        "a public export is unknowable, not a certain candidate: {text}"
    );
    assert!(
        text.contains("truly_dead_zzz"),
        "actually-dead fn must still be reported: {text}"
    );
    assert!(
        text.contains("suppressed from candidates"),
        "typed suppression must be visible: {text}"
    );
}

/// P1: deleted DOCUMENTATION must not become UNKNOWN — only deleted SOURCE
/// loses graph coverage.
#[test]
fn deleted_doc_is_not_unknown() {
    let (tmp, cache) = setup_repo("deldoc");
    std::fs::write(tmp.join("README.md"), "# docs\n").unwrap();
    std::fs::write(tmp.join("keep.py"), "def kept():\n    pass\n").unwrap();
    git(&tmp, &["add", "-A"]);
    git(&tmp, &["commit", "-qm", "base"]);
    cg(&tmp, &cache, &["index", "."]);
    std::fs::remove_file(tmp.join("README.md")).unwrap();
    cg(&tmp, &cache, &["index", "."]);
    let out = cg(&tmp, &cache, &["changes", "--base", "HEAD", "--md"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.contains("UNKNOWN"),
        "a deleted doc is not lost graph coverage: {text}"
    );
}

/// P1: pure rename and rename+modify keep working, with the old path surfaced
/// rather than silently lost; paths with spaces and non-ASCII survive -z parsing.
#[test]
fn rename_and_exotic_paths_are_parsed() {
    let (tmp, cache) = setup_repo("rename");
    std::fs::write(tmp.join("old name.py"), "def fn_ren():\n    return 1\n").unwrap();
    std::fs::write(tmp.join("ключ.py"), "def uni():\n    return 2\n").unwrap();
    git(&tmp, &["add", "-A"]);
    git(&tmp, &["commit", "-qm", "base"]);
    cg(&tmp, &cache, &["index", "."]);
    // pure rename (space in both names) + modify the unicode file
    git(&tmp, &["mv", "old name.py", "new name.py"]);
    std::fs::write(tmp.join("ключ.py"), "def uni():\n    return 3\n").unwrap();
    cg(&tmp, &cache, &["index", "."]);
    let out = cg(&tmp, &cache, &["changes", "--base", "HEAD"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("fn_ren"),
        "renamed file's symbols reviewed: {text}"
    );
    assert!(
        text.contains("uni"),
        "non-ASCII path parsed and reviewed: {text}"
    );
}

/// P1 base-side: a deleted source file with MULTIPLE symbols surfaces each of
/// them at symbol granularity — never collapsed to a bare path.
#[test]
fn deleted_source_reviews_each_symbol() {
    let (tmp, cache) = setup_repo("delsyms");
    std::fs::write(
        tmp.join("gone.py"),
        "def first_victim():\n    pass\n\ndef second_victim():\n    pass\n",
    )
    .unwrap();
    std::fs::write(tmp.join("keep.py"), "def kept():\n    pass\n").unwrap();
    git(&tmp, &["add", "-A"]);
    git(&tmp, &["commit", "-qm", "base"]);
    cg(&tmp, &cache, &["index", "."]);
    std::fs::remove_file(tmp.join("gone.py")).unwrap();
    cg(&tmp, &cache, &["index", "."]);
    let out = cg(&tmp, &cache, &["changes", "--base", "HEAD"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("first_victim") && text.contains("second_victim"),
        "every deleted symbol must appear by name: {text}"
    );
    assert!(
        text.contains("gone.py"),
        "the base-side path stays visible: {text}"
    );
}

/// P1 base-side: removing ONE function from a file that still exists surfaces
/// exactly that symbol as removed.
#[test]
fn removed_function_from_existing_file_is_surfaced() {
    let (tmp, cache) = setup_repo("removedfn");
    std::fs::write(
        tmp.join("mod.py"),
        "def survivor():\n    pass\n\ndef casualty():\n    pass\n",
    )
    .unwrap();
    git(&tmp, &["add", "-A"]);
    git(&tmp, &["commit", "-qm", "base"]);
    cg(&tmp, &cache, &["index", "."]);
    std::fs::write(tmp.join("mod.py"), "def survivor():\n    pass\n").unwrap();
    let out = cg(&tmp, &cache, &["changes", "--base", "HEAD"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("casualty") && text.contains("removed"),
        "the removed function must appear as removed: {text}"
    );
    assert!(
        text.contains("survivor"),
        "kept symbol still reviewed: {text}"
    );
}

/// P1 base-side: rename PLUS modification that drops a symbol — the old-side
/// symbol must surface even though the diff is a rename.
#[test]
fn rename_with_dropped_symbol_reviews_old_side() {
    let (tmp, cache) = setup_repo("renamedrop");
    std::fs::write(
        tmp.join("old.py"),
        "def stays_around():\n    return 1\n\ndef vanishes_in_rename():\n    return 2\n",
    )
    .unwrap();
    git(&tmp, &["add", "-A"]);
    git(&tmp, &["commit", "-qm", "base"]);
    cg(&tmp, &cache, &["index", "."]);
    git(&tmp, &["mv", "old.py", "new.py"]);
    // keep >50% similarity so git still detects the rename
    std::fs::write(tmp.join("new.py"), "def stays_around():\n    return 1\n").unwrap();
    git(&tmp, &["add", "-A"]);
    cg(&tmp, &cache, &["index", "."]);
    let out = cg(&tmp, &cache, &["changes", "--base", "HEAD"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("vanishes_in_rename"),
        "symbol lost across the rename must surface: {text}"
    );
    assert!(text.contains("stays_around"), "new side reviewed: {text}");
}

/// P1 base-side (early-continue regression): rename to a SYMBOL-FREE file —
/// the new side has no symbols, but the old side must still be reviewed.
#[test]
fn rename_to_symbol_free_file_still_reviews_old_side() {
    let (tmp, cache) = setup_repo("renameempty");
    std::fs::write(
        tmp.join("full.py"),
        "def emptied_out():\n    return 1\n# a comment that survives the emptying, keeping rename similarity\n# more shared trailing content for the similarity detector to hold on to\n# line three of ballast\n",
    )
    .unwrap();
    git(&tmp, &["add", "-A"]);
    git(&tmp, &["commit", "-qm", "base"]);
    cg(&tmp, &cache, &["index", "."]);
    git(&tmp, &["mv", "full.py", "hollow.py"]);
    std::fs::write(
        tmp.join("hollow.py"),
        "# a comment that survives the emptying, keeping rename similarity\n# more shared trailing content for the similarity detector to hold on to\n# line three of ballast\n",
    )
    .unwrap();
    git(&tmp, &["add", "-A"]);
    cg(&tmp, &cache, &["index", "."]);
    let out = cg(&tmp, &cache, &["changes", "--base", "HEAD"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("emptied_out"),
        "old-side symbol must be reviewed even when the new side has no symbols: {text}"
    );
}

/// P1 coverage: an existing, committed, indexable file the GRAPH has never
/// seen must be UNKNOWN — existence on disk is not coverage.
#[test]
fn unindexed_existing_source_is_unknown() {
    let (tmp, cache) = setup_repo("unindexed");
    std::fs::write(tmp.join("seen.py"), "def seen():\n    pass\n").unwrap();
    git(&tmp, &["add", "-A"]);
    git(&tmp, &["commit", "-qm", "base"]);
    cg(&tmp, &cache, &["index", "."]);
    // a NEW committed file the graph has never indexed (changes runs with
    // --no-autoheal so the graph stays behind on purpose)
    std::fs::write(tmp.join("ghost.py"), "def ghost():\n    pass\n").unwrap();
    git(&tmp, &["add", "-A"]);
    git(&tmp, &["commit", "-qm", "add ghost"]);
    std::fs::write(tmp.join("ghost.py"), "def ghost():\n    return 1\n").unwrap();
    let out = cg(
        &tmp,
        &cache,
        &["changes", "--base", "HEAD", "--no-autoheal"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("UNKNOWN") && text.contains("ghost.py"),
        "unindexed existing source must be UNKNOWN, not a clean pass: {text}"
    );
}

/// P1 coverage: a file the indexer SKIPS (invalid UTF-8 → unparseable) must
/// be UNKNOWN, never mistaken for legitimately symbol-free.
#[test]
fn unparseable_source_is_unknown() {
    let (tmp, cache) = setup_repo("parsefail");
    std::fs::write(tmp.join("ok.py"), "def fine():\n    pass\n").unwrap();
    std::fs::write(tmp.join("bad.py"), [0xff, 0xfe, 0x00, 0x80, b'\n']).unwrap();
    git(&tmp, &["add", "-A"]);
    git(&tmp, &["commit", "-qm", "base"]);
    cg(&tmp, &cache, &["index", "."]);
    let mut bytes = vec![0xff, 0xfe, 0x00, 0x81, b'\n'];
    bytes.extend_from_slice(b"def hidden(): pass\n");
    std::fs::write(tmp.join("bad.py"), bytes).unwrap();
    let out = cg(&tmp, &cache, &["changes", "--base", "HEAD"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("UNKNOWN") && text.contains("bad.py"),
        "an unindexed/unparseable source must be UNKNOWN: {text}"
    );
}

/// P1: a COPIED file (git status C) is parsed and reviewed on its new path,
/// with no phantom "lost" symbols (the copy carries them all).
#[test]
fn copied_file_is_reviewed() {
    let (tmp, cache) = setup_repo("copied");
    std::fs::write(tmp.join("src.py"), "def copied_fn():\n    return 1\n").unwrap();
    git(&tmp, &["add", "-A"]);
    git(&tmp, &["commit", "-qm", "base"]);
    cg(&tmp, &cache, &["index", "."]);
    std::fs::copy(tmp.join("src.py"), tmp.join("copy.py")).unwrap();
    // touching the source in the same diff is what makes git emit C status
    std::fs::write(
        tmp.join("src.py"),
        "def copied_fn():\n    return 1\n# touched\n",
    )
    .unwrap();
    git(&tmp, &["add", "-A"]);
    cg(&tmp, &cache, &["index", "."]);
    let out = cg(&tmp, &cache, &["changes", "--base", "HEAD"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("copied_fn"), "copied file reviewed: {text}");
    assert!(
        !text.contains("removed"),
        "a faithful copy must not report lost symbols: {text}"
    );
}

/// P1 parsing: tab-in-filename survives NUL-delimited status parsing.
#[test]
fn tab_in_filename_is_parsed() {
    let (tmp, cache) = setup_repo("tabname");
    std::fs::write(tmp.join("has\ttab.py"), "def tabbed():\n    return 1\n").unwrap();
    git(&tmp, &["add", "-A"]);
    git(&tmp, &["commit", "-qm", "base"]);
    cg(&tmp, &cache, &["index", "."]);
    std::fs::write(tmp.join("has\ttab.py"), "def tabbed():\n    return 2\n").unwrap();
    let out = cg(&tmp, &cache, &["changes", "--base", "HEAD"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("tabbed"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// P1: an indexable file that legitimately has no symbols (comment-only) is
/// NOT unknown — it exists and parsed; nothing is missing.
#[test]
fn comment_only_source_is_not_unknown() {
    let (tmp, cache) = setup_repo("commentonly");
    std::fs::write(tmp.join("notes.py"), "# just a comment\n").unwrap();
    git(&tmp, &["add", "-A"]);
    git(&tmp, &["commit", "-qm", "base"]);
    cg(&tmp, &cache, &["index", "."]);
    std::fs::write(tmp.join("notes.py"), "# a different comment\n").unwrap();
    let out = cg(&tmp, &cache, &["changes", "--base", "HEAD", "--md"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.contains("UNKNOWN"),
        "comment-only file is fully known: {text}"
    );
}
