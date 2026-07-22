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

/// GOLDEN CONTRACT: the public shapes of `changes` and `dead_code` (v1.37
/// clients depend on them). Accidental array→object flips, field renames, or
/// in-place type changes fail here.
#[test]
fn mcp_contract_shapes_are_stable() {
    let tmp = std::env::temp_dir().join(format!("cg_contractmcp_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let ok = Command::new("git")
        .args(["-C", tmp.to_str().unwrap(), "init", "-q"])
        .status()
        .unwrap()
        .success();
    assert!(ok);
    for a in [
        ["config", "user.name", "qa"],
        ["config", "user.email", "q@q.q"],
    ] {
        assert!(Command::new("git")
            .arg("-C")
            .arg(&tmp)
            .args(a)
            .status()
            .unwrap()
            .success());
    }
    std::fs::write(
        tmp.join("a.py"),
        "def used():\n    pass\n\ndef caller():\n    used()\n\ndef dead_zzz():\n    pass\n",
    )
    .unwrap();
    assert!(Command::new("git")
        .arg("-C")
        .arg(&tmp)
        .args(["add", "-A"])
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .arg("-C")
        .arg(&tmp)
        .args(["commit", "-qm", "x"])
        .status()
        .unwrap()
        .success());
    std::fs::write(
        tmp.join("a.py"),
        "def used():\n    pass\n\ndef caller():\n    used()\n\ndef dead_zzz():\n    return 1\n",
    )
    .unwrap();

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

    // dead_code: ARRAY of rows with the v1.37 field set (incl. kind)
    let dc: serde_json::Value =
        serde_json::from_str(&call(&mut child, &mut reader, 2, "dead_code", "{}")).unwrap();
    assert!(dc.is_array(), "dead_code must stay an ARRAY: {dc}");
    let row = &dc.as_array().unwrap()[0];
    for k in ["name", "kind", "file", "line"] {
        assert!(row.get(k).is_some(), "dead_code row must keep `{k}`: {row}");
    }
    assert_ne!(
        row["kind"],
        "Function".to_lowercase(),
        "kind is the real NodeLabel"
    );

    // dead_code_v2: provenance-carrying object with INDEPENDENT freshness
    // and evidence dimensions
    let v2: serde_json::Value =
        serde_json::from_str(&call(&mut child, &mut reader, 3, "dead_code_v2", "{}")).unwrap();
    assert!(
        v2.get("candidates").is_some()
            && v2.get("freshness").is_some()
            && v2.get("evidence").is_some(),
        "{v2}"
    );
    assert_eq!(
        v2["evidence"]["kind"], "lower_bound",
        "dead-code evidence is a lower bound by contract: {v2}"
    );
    assert_eq!(
        v2["freshness"]["kind"], "fresh",
        "healthy refresh must stamp fresh: {v2}"
    );
    assert!(
        v2["generation"].is_number(),
        "stamped graph must expose its generation: {v2}"
    );

    // changes: v1.37 field TYPES preserved + additive metadata
    let ch: serde_json::Value = serde_json::from_str(&call(
        &mut child,
        &mut reader,
        4,
        "changes",
        r#"{"base":"HEAD"}"#,
    ))
    .unwrap();
    let sym = &ch["affected_symbols"][0];
    assert!(sym["fan_in"].is_number(), "fan_in stays a number: {sym}");
    assert!(sym["tested"].is_boolean(), "tested stays a bool: {sym}");
    assert!(sym["risk"].is_string(), "risk stays a tier string: {sym}");
    assert!(
        sym.get("tested_evidence").is_some(),
        "additive evidence field present"
    );
    assert!(
        ch.get("freshness").is_some()
            && ch.get("evidence").is_some()
            && ch.get("generation").is_some(),
        "freshness/evidence/provenance exposed: {ch}"
    );

    let _ = child.kill();
    let _ = std::fs::remove_dir_all(&tmp);
}

/// P1 freshness contract over real MCP stdio: strict mode FAILS the call on a
/// refresh failure; explicit stale opt-in serves WITH `freshness = stale`
/// alongside independent evidence — never as a plain fresh/exact answer.
#[test]
fn mcp_strict_refresh_fails_and_stale_optin_is_stamped() {
    let tmp = std::env::temp_dir().join(format!("cg_mcpstale_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(
        tmp.join("a.py"),
        "def live():\n    pass\n\ndef dead_zzz():\n    pass\n",
    )
    .unwrap();
    // graph must exist before the failing-refresh phase
    let ok = Command::new(env!("CARGO_BIN_EXE_codegraph"))
        .args(["index", "."])
        .current_dir(&tmp)
        .env("CODEGRAPH_CACHE_DIR", tmp.join("cache"))
        .status()
        .unwrap()
        .success();
    assert!(ok);

    let spawn_server = |allow_stale: bool| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_codegraph"));
        c.args(["mcp", "--path"])
            .arg(&tmp)
            .env("CODEGRAPH_CACHE_DIR", tmp.join("cache"))
            .env("CODEGRAPH_NO_EMBEDDER", "1")
            .env("CODEGRAPH_TEST_FAIL_REFRESH", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if allow_stale {
            c.env("CODEGRAPH_ALLOW_STALE", "1");
        }
        c.spawn().unwrap()
    };
    let init = |child: &mut Child, reader: &mut BufReader<&mut std::process::ChildStdout>| {
        rpc(
            child,
            reader,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
        );
        rpc(
            child,
            reader,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        );
    };

    // STRICT (default): the tool call must FAIL with a freshness error
    let mut child = spawn_server(false);
    let mut stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(&mut stdout);
    init(&mut child, &mut reader);
    let resp = rpc(
        &mut child,
        &mut reader,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"dead_code_v2","arguments":{}}}"#,
    )
    .unwrap();
    let text = resp.to_string();
    assert!(
        text.contains("FRESHNESS FAILURE") || resp.get("error").is_some(),
        "strict mode must fail the call on refresh failure: {text}"
    );
    let _ = child.kill();

    // STALE OPT-IN: the call succeeds and the answer says stale + lower_bound
    let mut child = spawn_server(true);
    let mut stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(&mut stdout);
    init(&mut child, &mut reader);
    let v2: serde_json::Value =
        serde_json::from_str(&call(&mut child, &mut reader, 2, "dead_code_v2", "{}")).unwrap();
    assert_eq!(
        v2["freshness"]["kind"], "stale",
        "stale serve must be stamped stale: {v2}"
    );
    assert_eq!(
        v2["evidence"]["kind"], "lower_bound",
        "evidence stays independent of freshness: {v2}"
    );
    let _ = child.kill();
    let _ = std::fs::remove_dir_all(&tmp);
}
