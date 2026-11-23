use crew_core::claude::parse_line;
use crew_core::event::{Event, ToolKind};
use serde_json::json;

const SID: &str = "00000000-0000-4000-8000-000000000001";

fn parse_fixture(name: &str) -> Vec<Event> {
    let path = format!(
        "{}/tests/fixtures/claude/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(path).unwrap();
    text.lines().flat_map(parse_line).collect()
}

fn started() -> Event {
    Event::SessionStarted {
        session_id: SID.into(),
        model: "claude-haiku-4-5".into(),
        cwd: "/work/demo".into(),
    }
}

#[test]
fn read_file() {
    assert_eq!(
        parse_fixture("read_file.jsonl"),
        vec![
            started(),
            Event::ToolCall {
                id: "toolu_01".into(),
                name: "Read".into(),
                kind: ToolKind::Read,
                target: Some("/work/demo/a.txt".into()),
                input: json!({"file_path": "/work/demo/a.txt"}),
            },
            Event::ToolResult {
                id: "toolu_01".into(),
                is_error: false,
                output: "1\thello\n".into()
            },
            Event::Message {
                text: "hello".into()
            },
            Event::TurnEnd {
                ok: true,
                summary: "hello".into(),
                duration_ms: 1200,
                num_turns: 2
            },
        ]
    );
}

#[test]
fn edit_and_test() {
    assert_eq!(
        parse_fixture("edit_and_test.jsonl"),
        vec![
            started(),
            Event::Message {
                text: "Fixing it.".into()
            },
            Event::ToolCall {
                id: "toolu_01".into(),
                name: "Edit".into(),
                kind: ToolKind::Edit,
                target: Some("/work/demo/src/lib.rs".into()),
                input: json!({"file_path": "/work/demo/src/lib.rs", "old_string": "a", "new_string": "b"}),
            },
            Event::ToolResult {
                id: "toolu_01".into(),
                is_error: false,
                output: "edited\nok".into()
            },
            Event::ToolCall {
                id: "toolu_02".into(),
                name: "Bash".into(),
                kind: ToolKind::Run,
                target: Some("cargo test".into()),
                input: json!({"command": "cargo test"}),
            },
            Event::ToolResult {
                id: "toolu_02".into(),
                is_error: true,
                output: "test failed".into()
            },
            Event::TurnEnd {
                ok: true,
                summary: "Fixed.".into(),
                duration_ms: 3400,
                num_turns: 3
            },
        ]
    );
}

#[test]
fn thinking() {
    assert_eq!(
        parse_fixture("thinking.jsonl"),
        vec![
            started(),
            Event::Thinking {
                text: "Let me think.".into()
            },
            Event::Message {
                text: "Done thinking.".into()
            },
            Event::TurnEnd {
                ok: true,
                summary: "Done thinking.".into(),
                duration_ms: 800,
                num_turns: 1
            },
        ]
    );
}

#[test]
fn error_result() {
    assert_eq!(
        parse_fixture("error_result.jsonl"),
        vec![
            started(),
            Event::PermissionDenied {
                tool: "Bash".into(),
                input: json!({"command": "rm -rf /work/demo/tmp"}),
            },
            Event::TurnEnd {
                ok: false,
                summary: "".into(),
                duration_ms: 5000,
                num_turns: 10
            },
        ]
    );
}

#[test]
fn noise() {
    assert_eq!(
        parse_fixture("noise.jsonl"),
        vec![
            Event::Raw {
                line: "not json at all".into()
            },
            Event::Raw {
                line: r#"{"type":"brand_new","x":1}"#.into()
            },
            Event::Raw {
                line: r#"{"type":"system","subtype":"brand_new_subtype"}"#.into()
            },
        ]
    );
}

#[test]
fn tool_result_output_truncates_at_8192_bytes() {
    // 'é' is 2 bytes, so byte 8192 falls on a boundary; the odd prefix shifts it mid-char.
    let content = format!("a{}", "é".repeat(5000));
    let line = json!({"type":"user","message":{"content":[
        {"type":"tool_result","tool_use_id":"toolu_01","content":content}]}})
    .to_string();
    let events = parse_line(&line);
    let Event::ToolResult { output, .. } = &events[0] else {
        panic!("expected tool result");
    };
    let suffix = "\n… (truncated)";
    assert!(output.ends_with(suffix));
    let body = &output[..output.len() - suffix.len()];
    assert_eq!(body.len(), 8191);
    assert!(content.starts_with(body));

    let short = json!({"type":"user","message":{"content":[
        {"type":"tool_result","tool_use_id":"t","content":"x".repeat(8192)}]}})
    .to_string();
    let Event::ToolResult { output, .. } = &parse_line(&short)[0] else {
        panic!()
    };
    assert_eq!(output.len(), 8192);
}

#[test]
fn blank_line_yields_nothing() {
    assert!(parse_line("").is_empty());
    assert!(parse_line("  \t ").is_empty());
}

#[test]
fn tool_target_falls_back_to_pattern_then_url_then_none() {
    let target = |input: serde_json::Value| {
        let line = json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","id":"t","name":"Grep","input":input}]}})
        .to_string();
        let Event::ToolCall { target, .. } = parse_line(&line).remove(0) else {
            panic!("expected tool call");
        };
        target
    };
    assert_eq!(target(json!({"pattern": "foo"})), Some("foo".into()));
    assert_eq!(
        target(json!({"url": "https://example.com"})),
        Some("https://example.com".into())
    );
    assert_eq!(target(json!({"other": 1})), None);
}
