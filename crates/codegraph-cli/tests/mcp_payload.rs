//! Field-bug regressions (v1.36 report): MCP list responses must stay lean
//! enough for the client that consumes them, aliases must match MCP names,
//! and a missing embedder must degrade — never dead-end.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

fn rpc(
    child: &mut Child,
    reader: &mut BufReader<&mut std::process::ChildStdout>,
    msg: &str,
) -> Option<serde_json::Value> {
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(stdin, "{msg}").unwrap();
    stdin.flush().unwrap();
    if !msg.contains("\"id\"") {
        return None;
    }
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap() == 0 {
            panic!("server closed stdio before replying");
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
            if v.get("id").is_some() {
                return Some(v);
            }
        }
    }
}

fn call(
    child: &mut Child,
    reader: &mut BufReader<&mut std::process::ChildStdout>,
    id: u32,
    tool: &str,
    args: &str,
) -> String {
    let msg = format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{tool}","arguments":{args}}}}}"#
    );
    rpc(child, reader, &msg).unwrap()["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string()
}

/// A repo with 300 NestJS routes: the MCP `routes` answer must stay far under
/// the client's tool-result ceiling (field failure: 232 KB, rejected).
#[test]
fn mcp_routes_payload_is_lean_and_paginated() {
    let tmp = std::env::temp_dir().join(format!("cg_payload_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    for c in 0..30 {
        let mut src = format!("@Controller('mod{c}')\nexport class C{c} {{\n");
        for m in 0..10 {
            src.push_str(&format!("  @Get('ep{m}/:id')\n  h{m}() {{}}\n"));
        }
        src.push_str("}\n");
        std::fs::write(tmp.join(format!("c{c}.controller.ts")), src).unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_codegraph"))
        .args(["mcp", "--path"])
        .arg(&tmp)
        .env("CODEGRAPH_CACHE_DIR", tmp.join("cache"))
        // deterministic: never probe local LLM servers from a test
        .env("CODEGRAPH_NO_EMBEDDER", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(&mut stdout);
    rpc(
        &mut child,
        &mut reader,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
    );
    rpc(
        &mut child,
        &mut reader,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    );

    let text = call(&mut child, &mut reader, 2, "routes", "{}");
    assert!(
        text.len() < 25_000,
        "routes payload must stay lean: {} chars",
        text.len()
    );
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["total"], 300);
    assert!(
        !text.contains("pagerank") && !text.contains("betweenness"),
        "full node JSON leaked"
    );
    let first = &v["routes"][0];
    for k in ["method", "path", "file", "line"] {
        assert!(
            first.get(k).is_some(),
            "lean route row must carry {k}: {first}"
        );
    }
    // pagination + filtering
    let page: serde_json::Value = serde_json::from_str(&call(
        &mut child,
        &mut reader,
        3,
        "routes",
        r#"{"limit":5,"offset":295}"#,
    ))
    .unwrap();
    assert_eq!(page["routes"].as_array().unwrap().len(), 5);
    let filt: serde_json::Value = serde_json::from_str(&call(
        &mut child,
        &mut reader,
        4,
        "routes",
        r#"{"path_prefix":"/mod1/"}"#,
    ))
    .unwrap();
    assert_eq!(filt["total"], 10, "{filt}");

    let _ = child.kill();
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Forced no-embedder mode must degrade with an explicit label.
#[test]
fn semantic_search_degrades_without_embedder() {
    let tmp = std::env::temp_dir().join(format!("cg_noembed_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("a.py"), "def target_fn():\n    pass\n").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_codegraph"))
        .args(["mcp", "--path"])
        .arg(&tmp)
        .env("CODEGRAPH_CACHE_DIR", tmp.join("cache"))
        .env("CODEGRAPH_NO_EMBEDDER", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(&mut stdout);
    rpc(
        &mut child,
        &mut reader,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
    );
    rpc(
        &mut child,
        &mut reader,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    );
    let sem = call(
        &mut child,
        &mut reader,
        2,
        "semantic_search",
        r#"{"query":"target_fn"}"#,
    );
    assert!(
        sem.contains("degraded"),
        "must announce lexical fallback: {sem}"
    );
    assert!(
        sem.contains("target_fn"),
        "lexical fallback must still find the symbol: {sem}"
    );
    let stats = call(&mut child, &mut reader, 3, "stats", "{}");
    assert!(
        stats.contains("\"embedder_available\":false"),
        "stats must surface embedder state: {stats}"
    );
    let _ = child.kill();
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Every MCP tool name works as a CLI alias (agents translate constantly).
#[test]
fn mcp_names_alias_cli_subcommands() {
    for alias in [
        "semantic-search",
        "semantic_search",
        "blast-radius",
        "blast_radius",
        "trace-path",
        "trace_path",
        "graph-query",
        "graph_query",
        "dead_code",
        "stats",
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_codegraph"))
            .args([alias, "--help"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "`codegraph {alias} --help` must resolve: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
