use crew_core::codex::proto::ServerNotification;
use crew_core::codex::{CodexTranslator, Incoming};
use crew_core::event::{Event, ToolKind};
use serde_json::{Value, json};

const TID: &str = "00000000-0000-4000-8000-000000000001";

/// Reads a fixture the way the client would: a message with `method` and
/// `id` is a server request, anything else a notification.
fn incoming(line: &str) -> Incoming {
    let v: Value = serde_json::from_str(line).unwrap();
    notification_or_request(v)
}

fn notification_or_request(v: Value) -> Incoming {
    match v.get("id").and_then(Value::as_i64) {
        Some(id) if v.get("method").is_some() => Incoming::ServerRequest {
            id: crew_core::codex::RequestId::Int(id),
            method: v["method"].as_str().unwrap().to_string(),
            params: v["params"].clone(),
        },
        _ => Incoming::Notification(ServerNotification::parse(v)),
    }
}

fn fixture(name: &str) -> Vec<Incoming> {
    let path = format!("{}/tests/fixtures/codex/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(incoming)
        .collect()
}

fn translate(t: &mut CodexTranslator, msgs: &[Incoming]) -> Vec<Event> {
    msgs.iter().flat_map(|m| t.on(m)).collect()
}

fn msg(v: Value) -> Incoming {
    notification_or_request(v)
}

fn item(method: &str, item: Value) -> Incoming {
    msg(json!({"method": method, "params": {"item": item, "threadId": TID, "turnId": "t"}}))
}

#[test]
fn command_decline_and_final_answer() {
    let mut t = CodexTranslator::new();
    let events = translate(&mut t, &fixture("command_and_decline.jsonl"));
    let [
        Event::SessionStarted {
            session_id,
            model,
            cwd,
        },
        Event::Message { text: progress },
        Event::ToolCall {
            id: id1,
            name,
            kind,
            target,
            ..
        },
        Event::ToolResult {
            id: rid1,
            is_error: err1,
            ..
        },
        Event::ToolCall { id: id2, .. },
        Event::ToolResult {
            id: rid2,
            is_error: err2,
            ..
        },
        Event::TurnEnd {
            ok,
            summary,
            duration_ms,
            num_turns,
        },
    ] = events.as_slice()
    else {
        panic!("unexpected events: {events:#?}");
    };
    assert_eq!(session_id, TID);
    assert_eq!(model, "gpt-6-luna");
    assert_eq!(cwd, "/work/demo");
    assert!(progress.contains("do the steps in order"));
    assert_eq!(name, "Bash");
    assert_eq!(*kind, ToolKind::Run);
    assert!(target.as_deref().unwrap().contains("hello.txt"));
    assert_eq!(id1, rid1);
    assert!(!err1);
    assert_eq!(id2, rid2);
    assert_ne!(id1, id2);
    assert!(err2, "a declined command is an error");
    assert!(*ok);
    assert!(summary.contains("CREW-OK"));
    assert!(*duration_ms > 0);
    assert_eq!(*num_turns, 1);
}

#[test]
fn file_change_then_interrupt_leaves_open_command() {
    let mut t = CodexTranslator::new();
    let events = translate(&mut t, &fixture("file_change_interrupt.jsonl"));
    let first_call = events
        .iter()
        .position(|e| matches!(e, Event::ToolCall { .. }))
        .unwrap();
    let Event::ToolCall {
        id,
        name,
        kind,
        target,
        input,
    } = &events[first_call]
    else {
        panic!("{events:#?}");
    };
    assert_eq!(name, "Write");
    assert_eq!(*kind, ToolKind::Create);
    assert_eq!(target.as_deref(), Some("/work/demo/notes.md"));
    assert_eq!(
        input,
        &json!({"path": "/work/demo/notes.md", "diff": "crew\n"})
    );
    assert!(
        matches!(&events[first_call + 1], Event::ToolResult { id: rid, is_error: false, .. } if rid == id)
    );

    // The turn was interrupted while a command waited for approval.
    let Some(Event::TurnEnd { ok, summary, .. }) = events.last() else {
        panic!("{events:#?}");
    };
    assert!(!ok);
    assert_eq!(summary, "");
    let open_id = events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::ToolCall { id, .. } => Some(id.clone()),
            _ => None,
        })
        .unwrap();
    assert_ne!(&open_id, id);

    let closed = t.close_open("interrupted");
    assert_eq!(
        closed,
        vec![Event::ToolResult {
            id: open_id,
            is_error: true,
            output: "interrupted".into()
        }]
    );
    assert!(t.close_open("again").is_empty());
}

#[test]
fn mcp_tool_call_declined() {
    let mut t = CodexTranslator::new();
    let events = translate(&mut t, &fixture("mcp_tool_call.jsonl"));
    let call = events
        .iter()
        .find(|e| matches!(e, Event::ToolCall { name, .. } if name == "mcp__crew__ping"))
        .unwrap();
    let Event::ToolCall { id, kind, .. } = call else {
        unreachable!()
    };
    assert_eq!(*kind, ToolKind::Other);
    assert!(events.iter().any(|e| matches!(e,
        Event::ToolResult { id: rid, is_error: true, output }
            if rid == id && output.contains("user rejected MCP tool call"))));
}

#[test]
fn mcp_success_uses_result_text() {
    let mut t = CodexTranslator::new();
    let base = json!({"type": "mcpToolCall", "id": "m1", "server": "crew", "tool": "ping", "arguments": {"a": 1}});
    let started = t.on(&item(
        "item/started",
        json!({"type": "mcpToolCall", "id": "m1", "server": "crew", "tool": "ping", "arguments": {"a": 1}, "status": "inProgress"}),
    ));
    assert!(matches!(&started[0], Event::ToolCall { input, .. } if input == &json!({"a": 1})));
    let mut done = base.clone();
    done["status"] = json!("completed");
    done["result"] = json!({"content": [{"type": "text", "text": "pong"}]});
    let out = t.on(&item("item/completed", done));
    assert_eq!(
        out,
        vec![Event::ToolResult {
            id: "m1".into(),
            is_error: false,
            output: "pong".into()
        }]
    );
}

#[test]
fn one_call_and_result_per_changed_file() {
    let mut t = CodexTranslator::new();
    let changes = json!([
        {"path": "/work/demo/a.rs", "kind": {"type": "update", "move_path": null}, "diff": "@@ -1 +1 @@\n-a\n+b\n"},
        {"path": "/work/demo/b.rs", "kind": {"type": "add"}, "diff": "x\n"},
        {"path": "/work/demo/c.rs", "kind": {"type": "delete"}, "diff": ""}
    ]);
    let started = t.on(&item(
        "item/started",
        json!({"type": "fileChange", "id": "f1", "changes": changes, "status": "inProgress"}),
    ));
    let kinds: Vec<_> = started
        .iter()
        .map(|e| match e {
            Event::ToolCall { name, kind, .. } => (name.as_str(), kind.clone()),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            ("Edit", ToolKind::Edit),
            ("Write", ToolKind::Create),
            ("Edit", ToolKind::Edit)
        ]
    );
    let done = t.on(&item(
        "item/completed",
        json!({"type": "fileChange", "id": "f1", "changes": changes, "status": "failed"}),
    ));
    assert_eq!(done.len(), 3);
    let call_ids: Vec<_> = started
        .iter()
        .map(|e| match e {
            Event::ToolCall { id, .. } => id.clone(),
            _ => unreachable!(),
        })
        .collect();
    for (e, id) in done.iter().zip(&call_ids) {
        assert!(matches!(e, Event::ToolResult { id: rid, is_error: true, .. } if rid == id));
    }
    assert!(t.close_open("x").is_empty());
}

#[test]
fn long_command_output_is_truncated_like_claude_results() {
    let mut t = CodexTranslator::new();
    let cmd = |status: &str, out: Value, code: Value| {
        json!({"type": "commandExecution", "id": "c1", "command": "yes", "status": status,
               "aggregatedOutput": out, "exitCode": code, "commandActions": []})
    };
    t.on(&item(
        "item/started",
        cmd("inProgress", Value::Null, Value::Null),
    ));
    let big = "y\n".repeat(10_000);
    let out = t.on(&item("item/completed", cmd("failed", json!(big), json!(2))));
    let [
        Event::ToolResult {
            is_error, output, ..
        },
    ] = out.as_slice()
    else {
        panic!("{out:#?}");
    };
    assert!(is_error);
    assert!(output.len() < 9000);
    assert!(output.ends_with("(truncated)"));
}

#[test]
fn nonzero_exit_is_an_error_even_when_completed() {
    let mut t = CodexTranslator::new();
    let cmd = |status: &str, code: Value| {
        json!({"type": "commandExecution", "id": "c1", "command": "false", "status": status,
               "aggregatedOutput": "", "exitCode": code})
    };
    t.on(&item("item/started", cmd("inProgress", Value::Null)));
    let out = t.on(&item("item/completed", cmd("completed", json!(1))));
    assert!(matches!(&out[0], Event::ToolResult { is_error: true, .. }));
}

#[test]
fn failed_turn_reports_error_then_turn_end() {
    let mut t = CodexTranslator::new();
    let out = t.on(&msg(json!({"method": "turn/completed", "params": {
        "threadId": TID,
        "turn": {"id": "t", "status": "failed", "error": {"message": "rate limited"}, "durationMs": 5}}})));
    assert_eq!(
        out,
        vec![
            Event::Error {
                message: "rate limited".into()
            },
            Event::TurnEnd {
                ok: false,
                summary: "".into(),
                duration_ms: 5,
                num_turns: 1
            }
        ]
    );
}

#[test]
fn summary_does_not_leak_into_the_next_turn() {
    let mut t = CodexTranslator::new();
    let answer = |text: &str| {
        item(
            "item/completed",
            json!({"type": "agentMessage", "id": "m", "text": text, "phase": "final_answer"}),
        )
    };
    let end = || {
        msg(
            json!({"method": "turn/completed", "params": {"threadId": TID, "turn": {"id": "t", "status": "completed"}}}),
        )
    };
    assert!(t.on(&answer("first")).is_empty());
    assert!(matches!(&t.on(&end())[0], Event::TurnEnd { summary, .. } if summary == "first"));
    assert!(matches!(&t.on(&end())[0], Event::TurnEnd { summary, .. } if summary.is_empty()));
}

#[test]
fn deltas_and_diffs_are_ignored_and_unknown_is_raw() {
    let mut t = CodexTranslator::new();
    for line in [
        json!({"method": "item/agentMessage/delta", "params": {"itemId": "m", "delta": "x"}}),
        json!({"method": "turn/diff/updated", "params": {"diff": "diff --git a b"}}),
        json!({"method": "thread/status/changed", "params": {"status": {"type": "idle"}}}),
        json!({"method": "item/commandExecution/requestApproval", "id": 0, "params": {}}),
    ] {
        assert!(t.on(&msg(line)).is_empty());
    }
    let out = t.on(&msg(
        json!({"method": "warning", "params": {"message": "hm"}}),
    ));
    assert!(matches!(out.as_slice(), [Event::Raw { line }] if line.contains("warning")));
}

#[test]
fn session_starts_once_per_run() {
    let mut t = CodexTranslator::new();
    let started = |id: &str| {
        msg(
            json!({"method": "thread/started", "params": {"thread": {"id": id, "model": "m", "cwd": "/work/demo"}}}),
        )
    };
    let first = t.on(&started("thr-1"));
    assert_eq!(
        first,
        vec![Event::SessionStarted {
            session_id: "thr-1".into(),
            model: "m".into(),
            cwd: "/work/demo".into(),
        }]
    );
    assert!(t.on(&started("thr-1")).is_empty(), "a resume repeats it");

    // The driver may report the session from the start reply first.
    let mut t = CodexTranslator::new();
    let reported = t.session_started("thr-2", "m", "/work/demo");
    assert_eq!(reported.len(), 1);
    assert!(t.session_started("thr-2", "m", "/work/demo").is_empty());
    assert!(t.on(&started("thr-2")).is_empty());
}

#[test]
fn mcp_declined_or_failed_without_error_says_declined() {
    for status in ["declined", "failed"] {
        let mut t = CodexTranslator::new();
        let out = t.on(&item(
            "item/completed",
            json!({"type": "mcpToolCall", "id": "m1", "server": "other", "tool": "x", "arguments": {}, "status": status, "result": null, "error": null}),
        ));
        assert_eq!(
            out.last(),
            Some(&Event::ToolResult {
                id: "m1".into(),
                is_error: true,
                output: "Declined".into()
            }),
            "{status}"
        );
    }
}

#[test]
fn reasoning_and_plan_become_thinking() {
    let mut t = CodexTranslator::new();
    let events = translate(&mut t, &fixture("reasoning_and_plan.jsonl"));
    assert_eq!(
        events,
        vec![
            Event::Thinking {
                text: "Looking at the cart code.\n\nTotals live in cart.py.".into()
            },
            Event::Thinking {
                text: "1. Read cart.py\n2. Fix the total".into()
            },
        ]
    );
}

#[test]
fn empty_reasoning_and_plan_give_nothing() {
    let mut t = CodexTranslator::new();
    for it in [
        json!({"type": "reasoning", "id": "r", "summary": ["  ", ""], "content": ["x"]}),
        json!({"type": "reasoning", "id": "r2"}),
        json!({"type": "plan", "id": "p", "text": " \n"}),
    ] {
        assert!(t.on(&item("item/completed", it)).is_empty());
    }
}
