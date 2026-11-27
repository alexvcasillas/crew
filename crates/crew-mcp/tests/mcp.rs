//! Drives the crew-mcp binary over stdio with raw JSON-RPC lines, against a
//! small fake crewd on the socket.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crew_core::ids::RunId;
use crew_proto::{ClientMsg, PROTO_VERSION, Request, ServerMsg, read_frame, write_frame};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixListener;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(20);

type Answer = Arc<dyn Fn(&Request) -> Result<Value, String> + Send + Sync>;

/// Listens on `<home>/crewd.sock` like crewd: Hello first, then requests.
/// Sends every request after Hello to the returned receiver.
fn fake_crewd(home: &Path, delay: Duration, answer: Answer) -> mpsc::UnboundedReceiver<Request> {
    let listener = UnixListener::bind(home.join("crewd.sock")).unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let tx = tx.clone();
            let answer = answer.clone();
            tokio::spawn(async move {
                while let Ok(Some(ClientMsg::Request { id, req })) =
                    read_frame::<_, ClientMsg>(&mut stream).await
                {
                    let result = match &req {
                        Request::Hello { version } => {
                            assert_eq!(*version, PROTO_VERSION);
                            Ok(Value::Null)
                        }
                        other => {
                            tx.send(other.clone()).unwrap();
                            tokio::time::sleep(delay).await;
                            answer(other)
                        }
                    };
                    write_frame(&mut stream, &ServerMsg::Response { id, result })
                        .await
                        .unwrap();
                }
            });
        }
    });
    rx
}

struct Mcp {
    _child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    run: RunId,
}

impl Mcp {
    fn start(home: &Path) -> Mcp {
        Self::start_with(home, &[])
    }

    fn start_with(home: &Path, extra: &[&str]) -> Mcp {
        let run = RunId::new();
        let mut child = Command::new(env!("CARGO_BIN_EXE_crew-mcp"))
            .args(["--run", &run.to_string()])
            .args(extra)
            .env("CREW_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        Mcp {
            _child: child,
            stdin,
            stdout,
            run,
        }
    }

    async fn send(&mut self, msg: Value) {
        let mut line = msg.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    async fn recv(&mut self) -> Value {
        let line = timeout(WAIT, self.stdout.next_line())
            .await
            .expect("crew-mcp did not answer")
            .unwrap()
            .expect("crew-mcp closed stdout");
        serde_json::from_str(&line).unwrap()
    }

    async fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        let reply = self.recv().await;
        assert_eq!(reply["jsonrpc"], "2.0");
        assert_eq!(reply["id"], id);
        reply
    }

    /// Calls a tool and returns `(text, is_error)`.
    async fn call(&mut self, id: u64, tool: &str, args: Value) -> (String, bool) {
        let reply = self
            .request(id, "tools/call", json!({"name": tool, "arguments": args}))
            .await;
        let result = &reply["result"];
        let content = result["content"].as_array().expect("content");
        assert_eq!(content.len(), 1, "{reply}");
        assert_eq!(content[0]["type"], "text");
        (
            content[0]["text"].as_str().unwrap().to_string(),
            result["isError"].as_bool().unwrap_or(false),
        )
    }
}

fn home() -> (TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().to_path_buf();
    (tmp, home)
}

fn answer(f: impl Fn(&Request) -> Result<Value, String> + Send + Sync + 'static) -> Answer {
    Arc::new(f)
}

#[tokio::test(flavor = "multi_thread")]
async fn handshake_and_tools_list() {
    let (_tmp, home) = home();
    let mut mcp = Mcp::start(&home);

    // Claude probes this before initialize.
    mcp.send(json!({"jsonrpc": "2.0", "id": "server-discover-probe-1", "method": "server/discover", "params": {}}))
        .await;
    let reply = mcp.recv().await;
    assert_eq!(reply["id"], "server-discover-probe-1");
    assert_eq!(reply["error"]["code"], -32601);

    let reply = mcp
        .request(
            0,
            "initialize",
            json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "claude-code", "version": "2.1.330"}
            }),
        )
        .await;
    assert_eq!(reply["result"]["protocolVersion"], "2025-11-25");
    assert!(reply["result"]["capabilities"]["tools"].is_object());
    assert_eq!(reply["result"]["serverInfo"]["name"], "crew");

    // A notification gets no reply, so the next line answers tools/list.
    mcp.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;
    let reply = mcp.request(1, "tools/list", json!({})).await;
    let tools = reply["result"]["tools"].as_array().unwrap();
    let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["approve", "handoff"]);
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert!(!tool["description"].as_str().unwrap().is_empty());
    }
    let handoff = tools.iter().find(|t| t["name"] == "handoff").unwrap();
    assert_eq!(
        handoff["inputSchema"]["required"],
        json!(["agent", "brief"])
    );

    let reply = mcp.request(2, "resources/list", json!({})).await;
    assert_eq!(reply["error"]["code"], -32601);

    let reply = mcp.request(3, "ping", json!({})).await;
    assert_eq!(reply["result"], json!({}));
}

#[tokio::test(flavor = "multi_thread")]
async fn handoff_forwards_to_crewd() {
    let (_tmp, home) = home();
    let mut seen = fake_crewd(
        &home,
        Duration::ZERO,
        answer(|_| Ok(json!("Sentinel will pick it up."))),
    );
    let mut mcp = Mcp::start(&home);
    let (text, is_error) = mcp
        .call(
            1,
            "handoff",
            json!({"agent": "Sentinel", "brief": "check the edit"}),
        )
        .await;
    assert_eq!(text, "Sentinel will pick it up.");
    assert!(!is_error);
    assert_eq!(
        seen.recv().await.unwrap(),
        Request::Handoff {
            run: mcp.run,
            agent: "Sentinel".into(),
            brief: "check the edit".into(),
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn crewd_error_becomes_tool_error() {
    let (_tmp, home) = home();
    let _seen = fake_crewd(
        &home,
        Duration::ZERO,
        answer(|_| Err("Not supported yet.".into())),
    );
    let mut mcp = Mcp::start(&home);
    let (text, is_error) = mcp
        .call(1, "handoff", json!({"agent": "Sentinel", "brief": "go"}))
        .await;
    assert_eq!(text, "Not supported yet.");
    assert!(is_error);
}

#[tokio::test(flavor = "multi_thread")]
async fn approve_denies_when_crewd_fails() {
    let (_tmp, home) = home();
    let _seen = fake_crewd(
        &home,
        Duration::ZERO,
        answer(|_| Err("Not supported yet.".into())),
    );
    let mut mcp = Mcp::start(&home);
    let (text, is_error) = mcp
        .call(1, "approve", json!({"tool_name": "Bash", "input": {}}))
        .await;
    assert!(!is_error);
    let decision: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        decision,
        json!({"behavior": "deny", "message": "Not supported yet."})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn approve_denies_an_unreadable_answer() {
    let (_tmp, home) = home();
    let _seen = fake_crewd(&home, Duration::ZERO, answer(|_| Ok(json!("yes"))));
    let mut mcp = Mcp::start(&home);
    let (text, is_error) = mcp
        .call(1, "approve", json!({"tool_name": "Bash", "input": {}}))
        .await;
    assert!(!is_error);
    let decision: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(decision["behavior"], "deny");
    assert_eq!(decision["message"], "Crew sent an answer I can't read.");
}

#[tokio::test(flavor = "multi_thread")]
async fn approve_allow_returns_updated_input() {
    let (_tmp, home) = home();
    let mut seen = fake_crewd(
        &home,
        Duration::ZERO,
        answer(|_| Ok(json!({"allow": true, "message": null}))),
    );
    let mut mcp = Mcp::start(&home);
    let input = json!({"command": "touch x.txt", "description": "Create x.txt"});
    let (text, is_error) = mcp
        .call(
            1,
            "approve",
            json!({"tool_name": "Bash", "input": input, "tool_use_id": "toolu_1"}),
        )
        .await;
    assert!(!is_error);
    let decision: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        decision,
        json!({"behavior": "allow", "updatedInput": input})
    );
    assert_eq!(
        seen.recv().await.unwrap(),
        Request::PermissionAsk {
            run: mcp.run,
            tool: "Bash".into(),
            input,
            tool_use_id: Some("toolu_1".into()),
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn approve_deny_returns_message() {
    let (_tmp, home) = home();
    let _seen = fake_crewd(
        &home,
        Duration::ZERO,
        answer(|_| Ok(json!({"allow": false, "message": "Not now."}))),
    );
    let mut mcp = Mcp::start(&home);
    let (text, is_error) = mcp
        .call(
            1,
            "approve",
            json!({"tool_name": "Bash", "input": {"command": "rm a.txt"}}),
        )
        .await;
    assert!(!is_error);
    let decision: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(decision, json!({"behavior": "deny", "message": "Not now."}));
}

#[tokio::test(flavor = "multi_thread")]
async fn approve_waits_for_a_slow_decision() {
    let (_tmp, home) = home();
    // Longer than the connect timeout, which must not apply once connected.
    let _seen = fake_crewd(
        &home,
        Duration::from_secs(7),
        answer(|_| Ok(json!({"allow": true, "message": null}))),
    );
    let mut mcp = Mcp::start(&home);
    let (text, is_error) = mcp
        .call(1, "approve", json!({"tool_name": "Write", "input": {}}))
        .await;
    assert!(!is_error, "{text}");
    let decision: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(decision["behavior"], "allow");
}

#[tokio::test(flavor = "multi_thread")]
async fn approve_allows_crew_tools_without_asking() {
    // No crewd at all: crew's own tools never need it.
    let (_tmp, home) = home();
    let mut mcp = Mcp::start(&home);
    let input = json!({"agent": "Sentinel", "brief": "check the edit"});
    let (text, is_error) = mcp
        .call(
            1,
            "approve",
            json!({"tool_name": "mcp__crew_run__handoff", "input": input}),
        )
        .await;
    assert!(!is_error);
    let decision: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        decision,
        json!({"behavior": "allow", "updatedInput": input})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unreachable_crewd_returns_error_text() {
    let (_tmp, home) = home();
    let mut mcp = Mcp::start(&home);
    let (text, is_error) = mcp
        .call(1, "handoff", json!({"agent": "Sentinel", "brief": "go"}))
        .await;
    assert_eq!(text, "Crew isn't reachable right now.");
    assert!(is_error);
    // approve never fails: it denies, so Claude gets a clean answer.
    let (text, is_error) = mcp
        .call(2, "approve", json!({"tool_name": "Bash", "input": {}}))
        .await;
    assert!(!is_error);
    let decision: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        decision,
        json!({"behavior": "deny", "message": "Crew isn't reachable right now."})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn silent_crewd_times_out() {
    let (_tmp, home) = home();
    // Bound but never accepted: connect works, Hello never gets an answer.
    let _listener = UnixListener::bind(home.join("crewd.sock")).unwrap();
    let mut mcp = Mcp::start(&home);
    let started = Instant::now();
    let (text, is_error) = mcp
        .call(1, "handoff", json!({"agent": "Sentinel", "brief": "go"}))
        .await;
    assert_eq!(text, "Crew isn't reachable right now.");
    assert!(is_error);
    assert!(started.elapsed() >= Duration::from_secs(4));
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_tool_calls_are_errors() {
    let (_tmp, home) = home();
    let mut mcp = Mcp::start(&home);
    let (_, is_error) = mcp.call(1, "handoff", json!({"agent": "Sentinel"})).await;
    assert!(is_error);
    let reply = mcp
        .request(2, "tools/call", json!({"name": "nope", "arguments": {}}))
        .await;
    assert_eq!(reply["error"]["code"], -32602);
    // Even a malformed approve is a deny, never a tool error.
    let (text, is_error) = mcp.call(3, "approve", json!({"input": {}})).await;
    assert!(!is_error);
    let decision: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(decision["behavior"], "deny");
}

/// A crewd that never answers a permission ask and reports, through the
/// returned receiver, when the connection that asked is closed.
fn silent_crewd(home: &Path) -> mpsc::UnboundedReceiver<&'static str> {
    let listener = UnixListener::bind(home.join("crewd.sock")).unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let tx = tx.clone();
            tokio::spawn(async move {
                while let Ok(Some(ClientMsg::Request { id, req })) =
                    read_frame::<_, ClientMsg>(&mut stream).await
                {
                    match req {
                        Request::Hello { .. } => {
                            let reply = ServerMsg::Response {
                                id,
                                result: Ok(Value::Null),
                            };
                            write_frame(&mut stream, &reply).await.unwrap();
                        }
                        _ => tx.send("asked").unwrap(),
                    }
                }
                tx.send("closed").unwrap();
            });
        }
    });
    rx
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_approve_drops_its_crewd_connection() {
    let (_tmp, home) = home();
    let mut crewd = silent_crewd(&home);
    let mut mcp = Mcp::start(&home);
    mcp.send(json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {"name": "approve", "arguments": {"tool_name": "Bash", "input": {}}},
    }))
    .await;
    assert_eq!(timeout(WAIT, crewd.recv()).await.unwrap(), Some("asked"));

    mcp.send(json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": {"requestId": 7, "reason": "timed out"},
    }))
    .await;
    assert_eq!(timeout(WAIT, crewd.recv()).await.unwrap(), Some("closed"));
    // The cancelled call gets no reply; the next request is answered.
    let reply = mcp.request(8, "ping", json!({})).await;
    assert_eq!(reply["result"], json!({}));
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_flag_limits_the_tools() {
    let (_tmp, home) = home();
    let mut mcp = Mcp::start_with(&home, &["--tools", "handoff"]);
    let reply = mcp.request(1, "tools/list", json!({})).await;
    let names: Vec<&str> = reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["handoff"]);
    let reply = mcp
        .request(
            2,
            "tools/call",
            json!({"name": "approve", "arguments": {"tool_name": "Bash", "input": {}}}),
        )
        .await;
    assert_eq!(reply["error"]["code"], -32602);
}

#[tokio::test(flavor = "multi_thread")]
async fn report_is_served_only_when_named() {
    let (_tmp, home) = home();
    let mut mcp = Mcp::start_with(&home, &["--tools", "handoff,report"]);
    let reply = mcp.request(1, "tools/list", json!({})).await;
    let tools = reply["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["handoff", "report"]);
    let report = tools.iter().find(|t| t["name"] == "report").unwrap();
    assert_eq!(
        report["description"],
        "Tell Crew whether this run found something worth telling the user about. Call it once, at the end."
    );
    assert_eq!(report["inputSchema"]["required"], json!(["found"]));
    let props = &report["inputSchema"]["properties"];
    assert_eq!(props["found"]["type"], "boolean");
    assert_eq!(props["summary"]["type"], "string");

    // Without --tools, a run gets handoff and approve only.
    let mut plain = Mcp::start(&home);
    let reply = plain
        .request(
            2,
            "tools/call",
            json!({"name": "report", "arguments": {"found": true}}),
        )
        .await;
    assert_eq!(reply["error"]["code"], -32602);
}

#[tokio::test(flavor = "multi_thread")]
async fn report_forwards_to_crewd() {
    let (_tmp, home) = home();
    let mut seen = fake_crewd(&home, Duration::ZERO, answer(|_| Ok(Value::Null)));
    let mut mcp = Mcp::start_with(&home, &["--tools", "handoff,report"]);
    let (text, is_error) = mcp
        .call(
            1,
            "report",
            json!({"found": true, "summary": "Two new issues"}),
        )
        .await;
    assert!(!is_error, "{text}");
    assert_eq!(
        seen.recv().await.unwrap(),
        Request::Report {
            run: mcp.run,
            found: true,
            summary: Some("Two new issues".into()),
        }
    );
    let (_, is_error) = mcp.call(2, "report", json!({"found": false})).await;
    assert!(!is_error);
    assert_eq!(
        seen.recv().await.unwrap(),
        Request::Report {
            run: mcp.run,
            found: false,
            summary: None,
        }
    );
    let (text, is_error) = mcp.call(3, "report", json!({"summary": "x"})).await;
    assert!(is_error);
    assert_eq!(text, "report needs found, true or false.");
}

#[test]
fn unknown_tools_are_refused() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_crew-mcp"))
        .args([
            "--run",
            &RunId::new().to_string(),
            "--tools",
            "handoff,nope",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("nope"));
}
