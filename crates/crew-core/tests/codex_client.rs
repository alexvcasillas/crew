use std::path::{Path, PathBuf};
use std::time::Duration;

use crew_core::codex::proto::{
    ApprovalDecision, ApprovalPolicy, ApprovalResponse, ElicitationResponse, InitializeParams,
    ItemStatus, ModelListResponse, SandboxMode, ServerNotification, ServerRequest, ThreadItem,
    ThreadStartParams, ThreadStartResponse, TurnStatus, method,
};
use crew_core::codex::{CodexConn, CodexError, Incoming, RequestId};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(20);

fn fake() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake/codex_app_server.py")
}

/// Writes the scenario next to a temp dir that must outlive the connection.
fn start(steps: Value) -> (tempfile::TempDir, CodexConn, UnboundedReceiver<Incoming>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scenario.json");
    std::fs::write(&path, json!({ "steps": steps }).to_string()).unwrap();
    let mut cmd = Command::new(fake());
    cmd.arg(&path);
    let (conn, rx) = CodexConn::spawn(cmd).unwrap();
    (dir, conn, rx)
}

async fn next(rx: &mut UnboundedReceiver<Incoming>) -> Incoming {
    timeout(LIMIT, rx.recv())
        .await
        .expect("timed out waiting for a message")
        .expect("connection closed")
}

async fn next_notification(rx: &mut UnboundedReceiver<Incoming>) -> ServerNotification {
    match next(rx).await {
        Incoming::Notification(n) => n,
        other => panic!("expected a notification, got {other:?}"),
    }
}

#[tokio::test]
async fn initialize_handshake_and_model_list() {
    let (_dir, conn, mut rx) = start(json!([
        {"expect": "initialize", "match": {"clientInfo": {"name": "crew"}, "capabilities": {"experimentalApi": true}}},
        {"respond": "initialize", "result": {
            "userAgent": "crew/0.165.0", "codexHome": "/work/codex-home",
            "platformFamily": "unix", "platformOs": "macos"}},
        {"send": {"method": "remoteControl/status/changed", "params": {"status": "disabled"}, "emittedAtMs": 0}},
        {"expect": "initialized"},
        {"send": {"method": "account/updated", "params": {"authMode": "chatgpt"}, "emittedAtMs": 0}},
        {"expect": "model/list"},
        {"respond": "model/list", "result": {"data": [
            {"id": "gpt-6-luna", "model": "gpt-6-luna", "displayName": "GPT-6-Luna",
             "description": "Fast", "hidden": false, "isDefault": false,
             "defaultReasoningEffort": "medium",
             "supportedReasoningEfforts": [{"reasoningEffort": "low", "description": "Quick"}],
             "brandNewField": {"x": 1}},
            {"id": "gpt-6.1-sol", "model": "gpt-6.1-sol", "displayName": "GPT-6.1-Sol",
             "isDefault": true}
        ], "nextCursor": null}}
    ]));

    let init = timeout(LIMIT, conn.initialize(&InitializeParams::crew("0.1.0")))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(init.user_agent, "crew/0.165.0");
    assert_eq!(init.platform_os, "macos");

    let first = next_notification(&mut rx).await;
    let ServerNotification::Raw(raw) = first else {
        panic!("expected Raw, got {first:?}");
    };
    assert_eq!(raw["method"], "remoteControl/status/changed");

    let models: ModelListResponse = timeout(LIMIT, conn.call(method::MODEL_LIST, &json!({})))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(models.data.len(), 2);
    assert_eq!(models.data[0].display_name, "GPT-6-Luna");
    assert_eq!(
        models.data[0].default_reasoning_effort.as_deref(),
        Some("medium")
    );
    assert_eq!(
        models.data[0].supported_reasoning_efforts[0].reasoning_effort,
        "low"
    );
    assert!(models.data[1].is_default);
    assert!(!models.data[1].hidden);

    let second = next_notification(&mut rx).await;
    assert!(matches!(second, ServerNotification::Raw(ref v) if v["method"] == "account/updated"));
}

#[tokio::test]
async fn responses_out_of_order_reach_their_callers() {
    let (_dir, conn, _rx) = start(json!([
        {"expect": "thread/start", "as": "a", "match": {
            "cwd": "/work/demo", "sandbox": "workspace-write", "approvalPolicy": "on-request",
            "developerInstructions": "be brief"}},
        {"expect": "model/list", "as": "b"},
        {"respond": "b", "result": {"data": [], "nextCursor": null}},
        {"sleep": 0.1},
        {"respond": "a", "result": {"thread": {"id": "t-1", "path": "/work/rollout.jsonl"}, "model": "gpt-6-luna"}}
    ]));

    let params = ThreadStartParams {
        cwd: Some("/work/demo".into()),
        model: Some("gpt-6-luna".into()),
        approval_policy: Some(ApprovalPolicy::OnRequest),
        sandbox: Some(SandboxMode::WorkspaceWrite),
        developer_instructions: Some("be brief".into()),
        config: None,
    };
    let a = conn.call::<_, ThreadStartResponse>(method::THREAD_START, &params);
    let empty = json!({});
    let b = conn.call::<_, ModelListResponse>(method::MODEL_LIST, &empty);
    let (a, b) = timeout(LIMIT, async { tokio::join!(a, b) }).await.unwrap();
    let a = a.unwrap();
    assert_eq!(a.thread.id, "t-1");
    assert_eq!(a.thread.path.as_deref(), Some("/work/rollout.jsonl"));
    assert_eq!(a.model, "gpt-6-luna");
    assert!(b.unwrap().data.is_empty());
}

#[tokio::test]
async fn error_response_becomes_rpc_error() {
    let (_dir, conn, _rx) = start(json!([
        {"expect": "turn/interrupt"},
        {"respond": "turn/interrupt", "error": {"code": -32600, "message": "no active turn"}}
    ]));
    let err = timeout(
        LIMIT,
        conn.request(
            method::TURN_INTERRUPT,
            json!({"threadId": "t", "turnId": "u"}),
        ),
    )
    .await
    .unwrap()
    .unwrap_err();
    match err {
        CodexError::Rpc { code, message, .. } => {
            assert_eq!(code, -32600);
            assert_eq!(message, "no active turn");
        }
        other => panic!("expected Rpc, got {other:?}"),
    }
}

#[tokio::test]
async fn server_request_is_answered_by_its_id() {
    // The server request reuses id 1, the same number as the client's
    // pending request, so routing must go by shape, not by id.
    let (_dir, conn, mut rx) = start(json!([
        {"expect": "turn/start", "as": "turn"},
        {"request": "item/commandExecution/requestApproval", "id": 1, "params": {
            "kind": "command", "threadId": "t-1", "turnId": "u-1", "itemId": "exec-1",
            "startedAtMs": 0, "command": "/bin/zsh -lc 'mkdir sub'", "cwd": "/work/demo",
            "commandActions": [{"type": "unknown", "command": "mkdir sub"}]},
         "reply": {"decision": "accept"}},
        {"send": {"method": "serverRequest/resolved", "params": {"threadId": "t-1", "requestId": 1}}},
        {"request": "mcpServer/elicitation/request", "id": 2, "params": {
            "threadId": "t-1", "turnId": "u-1", "serverName": "crew", "mode": "form",
            "_meta": {"codex_approval_kind": "mcp_tool_call"},
            "message": "Allow the crew MCP server to run tool \"ping\"?",
            "requestedSchema": {"type": "object", "properties": {}}},
         "reply": {"action": "accept", "content": {}, "_meta": null}},
        {"respond": "turn", "result": {"turn": {"id": "u-1", "status": "inProgress", "items": [], "error": null}}}
    ]));

    let turn = {
        let conn = conn.clone();
        tokio::spawn(async move {
            conn.request(
                method::TURN_START,
                json!({"threadId": "t-1", "input": [{"type": "text", "text": "hi", "text_elements": []}]}),
            )
            .await
        })
    };

    let Incoming::ServerRequest { id, method, params } = next(&mut rx).await else {
        panic!("expected a server request");
    };
    assert_eq!(id, RequestId::Int(1));
    let ServerRequest::CommandApproval(p) = ServerRequest::parse(&method, &params) else {
        panic!("expected a command approval for {method}");
    };
    assert_eq!(p.item_id, "exec-1");
    assert_eq!(p.command.as_deref(), Some("/bin/zsh -lc 'mkdir sub'"));
    assert_eq!(p.command_actions[0].command, "mkdir sub");
    conn.reply(
        id,
        serde_json::to_value(ApprovalResponse::new(ApprovalDecision::Accept)).unwrap(),
    )
    .await
    .unwrap();

    let resolved = next_notification(&mut rx).await;
    assert!(
        matches!(resolved, ServerNotification::Raw(ref v) if v["method"] == "serverRequest/resolved")
    );

    let Incoming::ServerRequest { id, method, params } = next(&mut rx).await else {
        panic!("expected a server request");
    };
    assert_eq!(id, RequestId::Int(2));
    let ServerRequest::McpElicitation(p) = ServerRequest::parse(&method, &params) else {
        panic!("expected an elicitation for {method}");
    };
    assert_eq!(p.server_name, "crew");
    assert_eq!(p.mode, "form");
    conn.reply(
        id,
        serde_json::to_value(ElicitationResponse::accept()).unwrap(),
    )
    .await
    .unwrap();

    let turn = timeout(LIMIT, turn).await.unwrap().unwrap().unwrap();
    assert_eq!(turn["turn"]["id"], "u-1");
}

#[tokio::test]
async fn typed_notifications_and_unknown_ones_as_raw() {
    let (_dir, conn, mut rx) = start(json!([
        {"send": {"method": "some/new/thing", "params": {"x": 1}, "emittedAtMs": 0}},
        {"send": {"method": "item/agentMessage/delta", "params": {
            "threadId": "t-1", "turnId": "u-1", "itemId": "msg_1", "delta": "Hel"}}},
        {"send": {"method": "item/started", "params": {"item": {
            "type": "commandExecution", "id": "exec-1", "command": "/bin/zsh -lc ls",
            "cwd": "/work/demo", "status": "inProgress",
            "commandActions": [{"type": "listFiles", "command": "ls", "path": null}],
            "aggregatedOutput": null, "exitCode": null},
            "threadId": "t-1", "turnId": "u-1", "startedAtMs": 0}}},
        {"send": {"method": "item/completed", "params": {"item": {
            "type": "agentMessage", "id": "msg_1", "text": "Hello", "phase": "final_answer"},
            "threadId": "t-1", "turnId": "u-1", "completedAtMs": 0}}},
        {"send": {"method": "item/completed", "params": {"item": {
            "type": "hologram", "id": "h-1", "glow": true},
            "threadId": "t-1", "turnId": "u-1"}}},
        {"send": {"method": "turn/diff/updated", "params": {
            "threadId": "t-1", "turnId": "u-1", "diff": "diff --git a/x b/x\n"}}},
        {"send": {"method": "turn/diff/updated", "params": 5}},
        {"send": {"method": "turn/completed", "params": {"threadId": "t-1", "turn": {
            "id": "u-1", "status": "interrupted", "items": [],
            "error": {"message": "stopped", "codexErrorInfo": null}}}}},
        {"send": {"method": "turn/completed", "params": {"threadId": "t-1", "turn": {
            "id": "u-2", "status": "pondering"}}}}
    ]));

    let raw = next_notification(&mut rx).await;
    assert_eq!(
        raw,
        ServerNotification::Raw(
            json!({"method": "some/new/thing", "params": {"x": 1}, "emittedAtMs": 0})
        )
    );

    let ServerNotification::AgentMessageDelta(d) = next_notification(&mut rx).await else {
        panic!("expected a delta");
    };
    assert_eq!((d.item_id.as_str(), d.delta.as_str()), ("msg_1", "Hel"));

    let ServerNotification::ItemStarted(n) = next_notification(&mut rx).await else {
        panic!("expected item/started");
    };
    let ThreadItem::CommandExecution(c) = n.item else {
        panic!("expected a command item");
    };
    assert_eq!(c.status, ItemStatus::InProgress);
    assert_eq!(c.command_actions[0].command, "ls");
    assert_eq!(c.exit_code, None);

    let ServerNotification::ItemCompleted(n) = next_notification(&mut rx).await else {
        panic!("expected item/completed");
    };
    let ThreadItem::AgentMessage(m) = n.item else {
        panic!("expected an agent message");
    };
    assert_eq!(m.text, "Hello");
    assert_eq!(m.phase.as_deref(), Some("final_answer"));

    let ServerNotification::ItemCompleted(n) = next_notification(&mut rx).await else {
        panic!("expected item/completed");
    };
    assert!(matches!(n.item, ThreadItem::Other(ref v) if v["type"] == "hologram"));

    let ServerNotification::TurnDiffUpdated(diff) = next_notification(&mut rx).await else {
        panic!("expected a diff");
    };
    assert_eq!(diff.diff, "diff --git a/x b/x\n");

    let bad = next_notification(&mut rx).await;
    assert!(matches!(bad, ServerNotification::Raw(ref v) if v["params"] == 5));

    let ServerNotification::TurnCompleted(t) = next_notification(&mut rx).await else {
        panic!("expected turn/completed");
    };
    assert_eq!(t.turn.status, TurnStatus::Interrupted);
    assert_eq!(t.turn.error.unwrap().message, "stopped");

    let ServerNotification::TurnCompleted(t) = next_notification(&mut rx).await else {
        panic!("expected turn/completed");
    };
    assert_eq!(t.turn.status, TurnStatus::Unknown);
    drop(conn);
}

#[tokio::test]
async fn process_exit_fails_pending_requests_and_ends_the_stream() {
    let (_dir, conn, mut rx) = start(json!([
        {"expect": "thread/start", "as": "a"},
        {"expect": "model/list", "as": "b"},
        {"exit": 1}
    ]));

    let a = conn.request(method::THREAD_START, json!({}));
    let b = conn.request(method::MODEL_LIST, json!({}));
    let (a, b) = timeout(LIMIT, async { tokio::join!(a, b) }).await.unwrap();
    assert!(matches!(a, Err(CodexError::Closed)), "{a:?}");
    assert!(matches!(b, Err(CodexError::Closed)), "{b:?}");

    assert!(timeout(LIMIT, rx.recv()).await.unwrap().is_none());

    let late = timeout(LIMIT, conn.request(method::MODEL_LIST, json!({})))
        .await
        .unwrap();
    assert!(matches!(late, Err(CodexError::Closed)), "{late:?}");
}

#[tokio::test]
async fn process_exit_is_noticed_while_a_child_holds_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scenario.json");
    let steps = json!([{"expect": "model/list"}, {"exit": 1}]);
    std::fs::write(&path, json!({ "steps": steps }).to_string()).unwrap();
    // The background sleep keeps the stdout pipe open after the fake exits.
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(r#"sleep 30 & exec "$0" "$1""#)
        .arg(fake())
        .arg(&path);
    let (conn, _rx) = CodexConn::spawn(cmd).unwrap();

    let res = timeout(
        Duration::from_secs(10),
        conn.request(method::MODEL_LIST, json!({})),
    )
    .await
    .expect("a pending request should fail soon after the process exits");
    assert!(matches!(res, Err(CodexError::Closed)), "{res:?}");
}

#[tokio::test]
async fn closed_stream_over_plain_io_fails_pending_requests() {
    let (client_end, server_end) = tokio::io::duplex(4096);
    let (read, write) = tokio::io::split(client_end);
    let (conn, mut rx) = CodexConn::from_io(read, write);

    let pending = {
        let conn = conn.clone();
        tokio::spawn(async move { conn.request("initialize", json!({})).await })
    };
    // Read the request line, then hang up.
    let mut server_end = tokio::io::BufReader::new(server_end);
    let mut line = String::new();
    timeout(
        LIMIT,
        tokio::io::AsyncBufReadExt::read_line(&mut server_end, &mut line),
    )
    .await
    .unwrap()
    .unwrap();
    let sent: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(sent, json!({"id": 1, "method": "initialize", "params": {}}));
    drop(server_end);

    let res = timeout(LIMIT, pending).await.unwrap().unwrap();
    assert!(matches!(res, Err(CodexError::Closed)), "{res:?}");
    assert!(timeout(LIMIT, rx.recv()).await.unwrap().is_none());
}

#[test]
fn request_params_serialise_in_codex_shape() {
    let p = ThreadStartParams {
        cwd: Some("/work/demo".into()),
        sandbox: Some(SandboxMode::ReadOnly),
        approval_policy: Some(ApprovalPolicy::OnRequest),
        ..Default::default()
    };
    assert_eq!(
        serde_json::to_value(&p).unwrap(),
        json!({"cwd": "/work/demo", "sandbox": "read-only", "approvalPolicy": "on-request"})
    );
    assert_eq!(
        serde_json::to_value(ApprovalResponse::new(ApprovalDecision::Cancel)).unwrap(),
        json!({"decision": "cancel"})
    );
    assert_eq!(
        serde_json::to_value(ElicitationResponse::decline()).unwrap(),
        json!({"action": "decline", "content": null, "_meta": null})
    );
}

#[test]
fn unknown_server_request_is_raw() {
    let params = json!({"conversationId": "c", "callId": "x"});
    assert_eq!(
        ServerRequest::parse("execCommandApproval", &params),
        ServerRequest::Raw(params.clone())
    );
    // A known method with params it cannot read also stays raw.
    assert_eq!(
        ServerRequest::parse(method::FILE_CHANGE_APPROVAL, &json!("nope")),
        ServerRequest::Raw(json!("nope"))
    );
}

#[tokio::test]
async fn unknown_server_request_gets_an_error_reply() {
    let (_dir, conn, mut rx) = start(json!([
        {"request": "item/tool/requestUserInput", "id": 7, "params": {"threadId": "t-1"},
         "replyError": {"code": -32601, "message": "not supported"}},
        {"send": {"method": "after/error", "params": {}}}
    ]));

    let Incoming::ServerRequest { id, method, params } = next(&mut rx).await else {
        panic!("expected a server request");
    };
    assert_eq!(id, RequestId::Int(7));
    assert!(matches!(
        ServerRequest::parse(&method, &params),
        ServerRequest::Raw(_)
    ));
    conn.reply_error(id, -32601, "not supported").await.unwrap();

    // The fake only gets here if it accepted the error reply.
    let after = next_notification(&mut rx).await;
    assert!(matches!(after, ServerNotification::Raw(ref v) if v["method"] == "after/error"));
}

#[tokio::test]
async fn odd_server_request_id_is_kept_and_echoed() {
    let (_dir, conn, mut rx) = start(json!([
        {"request": "item/tool/requestUserInput", "id": {"odd": [1]}, "params": {},
         "reply": {"ok": true}},
        {"send": {"method": "after/reply", "params": {}}}
    ]));

    let Incoming::ServerRequest { id, .. } = next(&mut rx).await else {
        panic!("expected a server request");
    };
    assert_eq!(id, RequestId::Other(json!({"odd": [1]})));
    conn.reply(id, json!({"ok": true})).await.unwrap();

    let after = next_notification(&mut rx).await;
    assert!(matches!(after, ServerNotification::Raw(ref v) if v["method"] == "after/reply"));
}

/// A connection over an in-memory pipe; the returned half plays the server.
fn duplex(
    max_line: usize,
) -> (
    CodexConn,
    UnboundedReceiver<Incoming>,
    tokio::io::DuplexStream,
) {
    let (client_end, server_end) = tokio::io::duplex(1 << 16);
    let (read, write) = tokio::io::split(client_end);
    let (conn, rx) = CodexConn::from_io_with_max_line(read, write, max_line);
    (conn, rx, server_end)
}

/// Reads one client line from the server end, so replies are not sent
/// before the request is registered.
async fn read_client_line(server: &mut tokio::io::DuplexStream) -> Value {
    use tokio::io::AsyncReadExt;
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while timeout(LIMIT, server.read_exact(&mut byte))
        .await
        .unwrap()
        .is_ok()
        && byte[0] != b'\n'
    {
        line.push(byte[0]);
    }
    serde_json::from_slice(&line).unwrap()
}

async fn write_server(server: &mut tokio::io::DuplexStream, bytes: &[u8]) {
    use tokio::io::AsyncWriteExt;
    server.write_all(bytes).await.unwrap();
    server.flush().await.unwrap();
}

#[tokio::test]
async fn bad_utf8_and_garbage_lines_do_not_close_the_connection() {
    let (conn, mut rx, mut server) = duplex(CodexConn::MAX_LINE);
    let pending = {
        let conn = conn.clone();
        tokio::spawn(async move { conn.request(method::MODEL_LIST, json!({})).await })
    };
    assert_eq!(read_client_line(&mut server).await["id"], 1);

    write_server(&mut server, b"\xff\xfe not even json\n").await;
    write_server(&mut server, b"{not json}\n").await;
    write_server(
        &mut server,
        b"{\"method\":\"warning\",\"params\":{\"message\":\"caf\xe9\"}}\n",
    )
    .await;
    write_server(&mut server, b"{\"id\":\"str\",\"result\":{}}\n").await;
    write_server(&mut server, b"{\"id\":1,\"result\":{\"data\":[]}}\n").await;

    let res = timeout(LIMIT, pending).await.unwrap().unwrap().unwrap();
    assert_eq!(res, json!({"data": []}));

    let warning = next_notification(&mut rx).await;
    let ServerNotification::Raw(v) = warning else {
        panic!("expected Raw, got {warning:?}");
    };
    assert_eq!(v["params"]["message"], "caf\u{fffd}");
}

#[tokio::test]
async fn over_long_line_is_skipped_and_reading_goes_on() {
    let (conn, mut rx, mut server) = duplex(64);
    let pending = {
        let conn = conn.clone();
        tokio::spawn(async move { conn.request(method::MODEL_LIST, json!({})).await })
    };
    assert_eq!(read_client_line(&mut server).await["id"], 1);

    let long = format!(
        "{{\"method\":\"big\",\"params\":{{\"x\":\"{}\"}}}}\n",
        "a".repeat(1000)
    );
    // Send it in pieces so the cap is hit across several reads.
    for chunk in long.as_bytes().chunks(100) {
        write_server(&mut server, chunk).await;
    }
    write_server(&mut server, b"{\"method\":\"small\"}\n").await;
    write_server(&mut server, b"{\"id\":1,\"result\":{}}\n").await;

    let small = next_notification(&mut rx).await;
    assert!(matches!(small, ServerNotification::Raw(ref v) if v["method"] == "small"));
    let res = timeout(LIMIT, pending).await.unwrap().unwrap();
    assert_eq!(res.unwrap(), json!({}));
}

#[tokio::test]
async fn a_line_split_across_writes_is_read_whole() {
    let (conn, _rx, mut server) = duplex(CodexConn::MAX_LINE);
    let pending = {
        let conn = conn.clone();
        tokio::spawn(async move { conn.request(method::MODEL_LIST, json!({})).await })
    };
    assert_eq!(read_client_line(&mut server).await["id"], 1);

    write_server(&mut server, b"{\"id\":1,\"res").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    write_server(&mut server, b"ult\":{\"data\":[]}}").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    write_server(&mut server, b"\n").await;

    let res = timeout(LIMIT, pending).await.unwrap().unwrap().unwrap();
    assert_eq!(res, json!({"data": []}));
}
