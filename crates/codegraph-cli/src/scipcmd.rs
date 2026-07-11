//! `codegraph scip` — one command to compiler-grade precision: detect the
//! project's language, run the matching SCIP indexer **if it's installed** (then
//! merge the `.scip`), otherwise print the exact install + run commands. Never
//! crashes — SCIP is an optional precision upgrade over the tree-sitter core.

use std::path::Path;
use std::process::Command;

use anyhow::Result;

use crate::index;

pub(crate) struct Indexer {
    lang: &'static str,
    pub(crate) bin: &'static str,
    pub(crate) args: &'static [&'static str],
    install: &'static str,
}

/// Pick the SCIP indexer from marker files in the repo root.
pub(crate) fn detect(root: &Path) -> Option<Indexer> {
    let has = |f: &str| root.join(f).exists();
    if has("tsconfig.json") || has("package.json") {
        Some(Indexer { lang: "typescript", bin: "scip-typescript", args: &["index"], install: "npm i -g @sourcegraph/scip-typescript" })
    } else if has("Cargo.toml") {
        Some(Indexer { lang: "rust", bin: "rust-analyzer", args: &["scip", "."], install: "rustup component add rust-analyzer" })
    } else if has("pyproject.toml") || has("setup.py") || has("requirements.txt") {
        Some(Indexer { lang: "python", bin: "scip-python", args: &["index"], install: "npm i -g @sourcegraph/scip-python" })
    } else if has("pom.xml") || has("build.gradle") || has("build.gradle.kts") {
        Some(Indexer { lang: "java", bin: "scip-java", args: &["index"], install: "coursier install scip-java" })
    } else if has("go.mod") {
        Some(Indexer { lang: "go", bin: "scip-go", args: &[], install: "go install github.com/sourcegraph/scip-go/cmd/scip-go@latest" })
    } else {
        None
    }
}

pub(crate) fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|d| {
                let p = d.join(bin);
                p.is_file() || p.with_extension("exe").is_file()
            })
        })
        .unwrap_or(false)
}

pub fn run(root: &Path) -> Result<()> {
    let Some(ix) = detect(root) else {
        println!("No known SCIP indexer maps to this project. The tree-sitter core works without SCIP;");
        println!("for compiler-grade precision, see https://github.com/sourcegraph/scip#tools.");
        return Ok(());
    };
    if !on_path(ix.bin) {
        println!("Detected a {} project — its SCIP indexer '{}' is not installed.", ix.lang, ix.bin);
        println!("  install:  {}", ix.install);
        println!("  run:      cd {} && {} {}", root.display(), ix.bin, ix.args.join(" "));
        println!("  merge:    codegraph index {} --scip index.scip", root.display());
        return Ok(());
    }
    println!("Running {} {} in {} …", ix.bin, ix.args.join(" "), root.display());
    match Command::new(ix.bin).args(ix.args).current_dir(root).status() {
        Ok(s) if s.success() => {
            let scip = root.join("index.scip");
            if scip.exists() {
                let stats = index::index_dir(root, &index::db_path(root), true, Some(&scip), false, None)?;
                println!(
                    "Merged SCIP → {} nodes, {} edges (+{} compiler-grade tier-A).",
                    stats.nodes, stats.edges, stats.scip_edges
                );
            } else {
                println!("Indexer finished but produced no index.scip.");
            }
        }
        Ok(s) => println!("Indexer exited with status {s}."),
        Err(e) => println!("Could not run {}: {e}", ix.bin),
    }
    Ok(())
}
