//! Static release/action consistency — version pins, asset names, and the
//! Windows executable rule must never drift apart (field bug: the action's
//! default pin pointed at a release with no sha256sums.txt).

fn repo_file(rel: &str) -> String {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

#[test]
fn action_default_version_matches_workspace() {
    let cargo = repo_file("Cargo.toml");
    let version = cargo
        .lines()
        .find_map(|l| l.trim().strip_prefix("version = \""))
        .and_then(|v| v.strip_suffix('"'))
        .expect("workspace version");
    let action = repo_file("action/action.yml");
    assert!(
        action.contains(&format!("default: 'v{version}'")),
        "action default pin must match workspace version v{version} — a stale pin \
         downloads a release whose asset layout may predate sha256sums.txt"
    );
}

#[test]
fn action_assets_match_release_workflow_output() {
    let action = repo_file("action/action.yml");
    let workflow = repo_file(".github/workflows/release.yml");
    for asset in [
        "codegraph-linux-x64",
        "codegraph-linux-arm64",
        "codegraph-macos-arm64",
        "codegraph-macos-x64",
        "codegraph-windows-x64.exe",
    ] {
        assert!(
            action.contains(asset),
            "action must map an OS/arch to {asset}"
        );
        assert!(
            workflow.contains(asset),
            "release workflow must build {asset}"
        );
    }
    assert!(
        workflow.contains("sha256sums.txt"),
        "workflow must publish checksums"
    );
    assert!(
        action.contains("sha256sums.txt"),
        "action must verify checksums"
    );
}

#[test]
fn windows_install_uses_exe_name() {
    let action = repo_file("action/action.yml");
    assert!(
        action.contains("BIN_NAME=codegraph.exe"),
        "Windows PATH lookup needs codegraph.exe — installing a suffixless binary silently breaks"
    );
}

#[test]
fn no_fragile_head_pipelines_in_action() {
    let action = repo_file("action/action.yml");
    assert!(
        !action.contains("| head -1"),
        "head pipelines under pipefail are fragile — select deterministically with jq first()"
    );
}
