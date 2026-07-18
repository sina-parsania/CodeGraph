//! Field-bug regressions for the MCP server (stdio, real binary):
//! 1. an index-generation bump UNDER a running server must be visible on the
//!    very next tool call (no stale in-memory graph);
//! 2. an EMPTY graph must never produce clean empty answers — tools refuse
//!    with a diagnosis, `stats` stays reachable and reports EMPTY_GRAPH;
//! 3. a dead root path must refuse to serve at startup.

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
    let want_reply = msg.contains("\"id\"");
    if !want_reply {
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

fn init_msgs() -> [&'static str; 2] {
    [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    ]
}

fn call_tool(
    child: &mut Child,
    reader: &mut BufReader<&mut std::process::ChildStdout>,
    id: u32,
    tool: &str,
    args: &str,
) -> serde_json::Value {
    let msg = format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{tool}","arguments":{args}}}}}"#
    );
    rpc(child, reader, &msg).unwrap()
}

#[test]
fn generation_bump_under_running_server_is_served_fresh() {
    let tmp = std::env::temp_dir().join(format!("cg_mcpfresh_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    std::env::set_var("CODEGRAPH_CACHE_DIR", tmp.join("cache"));
    std::fs::write(tmp.join("a.py"), "def first_fn():\n    return 1\n").unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_codegraph"))
        .args(["mcp", "--path"])
        .arg(&tmp)
        .env("CODEGRAPH_CACHE_DIR", tmp.join("cache"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(&mut stdout);
    for m in init_msgs() {
        rpc(&mut child, &mut reader, m);
    }

    let r1 = call_tool(
        &mut child,
        &mut reader,
        2,
        "search",
        r#"{"query":"first_fn"}"#,
    );
    assert!(
        r1.to_string().contains("first_fn"),
        "baseline symbol must be served: {r1}"
    );

    // BUMP THE GENERATION UNDER THE SERVER: new file, out-of-band index run
    // (a second process — exactly the field scenario).
    std::fs::write(tmp.join("b.py"), "def second_fn():\n    return 2\n").unwrap();
    let st = Command::new(env!("CARGO_BIN_EXE_codegraph"))
        .arg("index")
        .arg(&tmp)
        .env("CODEGRAPH_CACHE_DIR", tmp.join("cache"))
        .output()
        .unwrap();
    assert!(st.status.success());

    // The server's very next answer must include the new symbol (>1s debounce window).
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let r2 = call_tool(
        &mut child,
        &mut reader,
        3,
        "search",
        r#"{"query":"second_fn"}"#,
    );
    assert!(
        r2.to_string().contains("second_fn"),
        "generation bump under a running server must be served fresh: {r2}"
    );

    let _ = child.kill();
    std::env::remove_var("CODEGRAPH_CACHE_DIR");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn empty_graph_refuses_clean_empty_answers() {
    let tmp = std::env::temp_dir().join(format!("cg_mcpempty_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    // directory EXISTS but holds nothing indexable — the confidently-empty trap
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("README"), "no code here").unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_codegraph"))
        .args(["mcp", "--path"])
        .arg(&tmp)
        .env("CODEGRAPH_CACHE_DIR", tmp.join("cache"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(&mut stdout);
    for m in init_msgs() {
        rpc(&mut child, &mut reader, m);
    }

    // callers on an empty graph: must NOT be a clean "no callers" — must carry
    // the EMPTY diagnosis (isError result carrying the message).
    let r = call_tool(
        &mut child,
        &mut reader,
        2,
        "callers",
        r#"{"name":"anything"}"#,
    );
    let text = r.to_string();
    assert!(
        text.contains("EMPTY") || r["result"]["isError"].as_bool() == Some(true),
        "empty graph must refuse, not answer cleanly: {text}"
    );
    assert!(
        !text.contains("\"callers\":[]"),
        "clean empty callers list is the lie we banned: {text}"
    );

    // stats stays reachable and names the problem
    let s = call_tool(&mut child, &mut reader, 3, "stats", r#"{}"#);
    assert!(
        s.to_string().contains("EMPTY_GRAPH"),
        "stats must diagnose emptiness: {s}"
    );

    let _ = child.kill();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn dead_root_refuses_to_serve() {
    let out = Command::new(env!("CARGO_BIN_EXE_codegraph"))
        .args(["mcp", "--path", "/nonexistent/moved-repo"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "a dead root must fail startup");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("does not exist"),
        "startup error must diagnose: {err}"
    );
}
