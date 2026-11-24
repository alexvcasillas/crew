use crew_core::claude::{StreamParser, parse_line};
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
                reason: None,
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

fn fixture_lines(name: &str) -> Vec<String> {
    let path = format!(
        "{}/tests/fixtures/claude/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(path).unwrap();
    text.lines().map(str::to_string).collect()
}

fn denied(tool: &str, input: serde_json::Value) -> Event {
    Event::PermissionDenied {
        tool: tool.into(),
        input,
        reason: None,
    }
}

fn blocked(tool: &str, input: serde_json::Value, reason: &str) -> Event {
    Event::PermissionDenied {
        tool: tool.into(),
        input,
        reason: Some(reason.into()),
    }
}

fn stream(name: &str) -> Vec<Event> {
    let mut parser = StreamParser::new();
    fixture_lines(name)
        .iter()
        .flat_map(|line| parser.parse(line))
        .collect()
}

#[test]
fn stream_parser_drops_noise_and_reports_each_denial_once() {
    // A denial with a tool use id waits for its tool result, which never
    // comes here, so it is reported with the result.
    assert_eq!(
        stream("denied_stream.jsonl"),
        vec![
            started(),
            denied("Write", json!({"file_path": "/work/demo/a.txt"})),
            Event::Raw {
                line: r#"{"type":"system","subtype":"permission_denied","session_id":"00000000-0000-4000-8000-000000000001"}"#.into()
            },
            denied("Bash", json!({"command": "rm -rf /work/demo/tmp"})),
            denied("Bash", json!({"command": "ls"})),
            Event::TurnEnd {
                ok: true,
                summary: "Could not.".into(),
                duration_ms: 900,
                num_turns: 2
            },
        ]
    );
}

#[test]
fn parse_line_drops_thinking_tokens_and_commands_changed() {
    let lines = fixture_lines("denied_stream.jsonl");
    assert!(parse_line(&lines[1]).is_empty());
    assert!(parse_line(&lines[2]).is_empty());
    assert_eq!(
        parse_line(&lines[3]),
        vec![denied("Bash", json!({"command": "rm -rf /work/demo/tmp"}))]
    );
}

#[test]
fn stream_parser_keeps_result_denials_when_none_were_streamed() {
    let mut parser = StreamParser::new();
    let events: Vec<Event> = fixture_lines("error_result.jsonl")
        .iter()
        .flat_map(|line| parser.parse(line))
        .collect();
    assert_eq!(events, parse_fixture("error_result.jsonl"));
}

#[test]
fn a_safety_check_refusal_in_a_tool_result_is_a_denial_with_its_reason() {
    let input = json!({"command": "rm -rf /work/demo/build"});
    let events = stream("auto_blocked_result.jsonl");
    assert_eq!(
        events,
        vec![
            started(),
            Event::ToolCall {
                id: "toolu_01".into(),
                name: "Bash".into(),
                kind: ToolKind::Run,
                target: Some("rm -rf /work/demo/build".into()),
                input: input.clone(),
            },
            blocked("Bash", input, "Destructive or unwanted actions"),
            Event::ToolResult {
                id: "toolu_01".into(),
                is_error: true,
                output: "Permission for this action has been denied by the Claude Code auto mode classifier. Reason: [Destructive or unwanted actions] Removing a folder outright. Find another way, or stop and explain.".into(),
            },
            Event::Message {
                text: "I couldn't remove it.".into()
            },
            Event::TurnEnd {
                ok: true,
                summary: "I couldn't remove it.".into(),
                duration_ms: 900,
                num_turns: 2
            },
        ]
    );
}

#[test]
fn a_permission_denied_line_and_its_refused_result_make_one_denial() {
    let denials: Vec<Event> = stream("auto_blocked_system.jsonl")
        .into_iter()
        .filter(|e| matches!(e, Event::PermissionDenied { .. }))
        .collect();
    assert_eq!(
        denials,
        vec![
            blocked(
                "Bash",
                json!({"command": "git push --force"}),
                "Unauthorized remote changes"
            ),
            blocked(
                "Write",
                json!({"file_path": "/work/demo/.env"}),
                "Credential access"
            ),
        ]
    );
    // Each denial comes just before its tool result.
    let events = stream("auto_blocked_system.jsonl");
    let at = |want: &dyn Fn(&Event) -> bool| events.iter().position(want).unwrap();
    let denial = at(&|e| matches!(e, Event::PermissionDenied { tool, .. } if tool == "Bash"));
    let result = at(&|e| matches!(e, Event::ToolResult { id, .. } if id == "toolu_01"));
    assert_eq!(denial + 1, result);
}

#[test]
fn parse_line_reads_the_reason_of_a_permission_denied_line() {
    let lines = fixture_lines("auto_blocked_system.jsonl");
    assert_eq!(
        parse_line(&lines[4]),
        vec![blocked(
            "Write",
            json!({"file_path": "/work/demo/.env"}),
            "Credential access"
        )]
    );
    assert_eq!(
        parse_line(&lines[2]),
        vec![denied("Bash", json!({"command": "git push --force"}))]
    );
}

#[test]
fn an_ordinary_error_result_is_no_denial() {
    let mut parser = StreamParser::new();
    let call = json!({"type":"assistant","message":{"content":[
        {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"false"}}]}})
    .to_string();
    let result = json!({"type":"user","message":{"content":[
        {"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"exit 1"}]}})
    .to_string();
    parser.parse(&call);
    let events = parser.parse(&result);
    assert!(matches!(events.as_slice(), [Event::ToolResult { .. }]));
}

/// A run that `cat`s a file quoting the safety check's refusal, then
/// fails. `denials` is its result's list of denied calls.
fn quoting_run(denials: serde_json::Value) -> Vec<Event> {
    let mut parser = StreamParser::new();
    let lines = [
        json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cat notes.txt; false"}}]}}),
        json!({"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"t1","is_error":true,
             "content":"notes: Permission for this action has been denied by the Claude Code auto mode classifier. Reason: [Destructive or unwanted actions]"}]}}),
        json!({"type":"result","subtype":"success","is_error":false,"result":"ok",
               "duration_ms":1,"num_turns":1,"permission_denials":denials}),
    ];
    lines
        .iter()
        .flat_map(|line| parser.parse(&line.to_string()))
        .filter(|e| matches!(e, Event::PermissionDenied { .. }))
        .collect()
}

#[test]
fn output_that_quotes_a_refusal_is_no_denial() {
    assert_eq!(quoting_run(json!([])), vec![]);
}

#[test]
fn a_quoted_refusal_counts_when_the_result_lists_the_call() {
    let input = json!({"command": "cat notes.txt; false"});
    assert_eq!(
        quoting_run(json!([{"tool_name":"Bash","tool_input":input,"tool_use_id":"t1"}])),
        vec![blocked("Bash", input, "Destructive or unwanted actions")]
    );
}

#[test]
fn a_plain_denial_has_no_reason() {
    let mut parser = StreamParser::new();
    let line = json!({"type":"result","subtype":"success","is_error":false,"result":"ok",
        "duration_ms":1,"num_turns":1,"permission_denials":[
            {"tool_name":"Bash","tool_input":{"command":"ls"},"tool_use_id":"t9"}]})
    .to_string();
    let events = parser.parse(&line);
    assert_eq!(events[0], denied("Bash", json!({"command": "ls"})));
}

fn usage(tokens: u64, window: Option<u64>) -> Event {
    Event::Usage {
        context_tokens: tokens,
        context_window: window,
    }
}

#[test]
fn stream_parser_reports_context_usage_of_the_main_agent() {
    let events: Vec<Event> = stream("usage.jsonl")
        .into_iter()
        .filter(|e| matches!(e, Event::Usage { .. } | Event::TurnEnd { .. }))
        .collect();
    // The repeated total and the subagent's line give nothing. The result
    // picks the main model's window out of two, matching the init model
    // by prefix.
    assert_eq!(
        events,
        vec![
            usage(2 + 28472 + 12333, None),
            usage(3 + 10 + 40812, None),
            usage(3 + 10 + 40812, Some(1_000_000)),
            Event::TurnEnd {
                ok: true,
                summary: "Done.".into(),
                duration_ms: 1200,
                num_turns: 2,
            },
        ]
    );
}

#[test]
fn result_without_a_matching_model_gives_no_window() {
    let mut parser = StreamParser::new();
    parser.parse(
        r#"{"type":"system","subtype":"init","session_id":"s","model":"claude-x","cwd":"/w"}"#,
    );
    let events = parser.parse(
        r#"{"type":"result","subtype":"success","num_turns":1,"result":"ok","modelUsage":{"a":{"inputTokens":1,"contextWindow":10},"b":{"inputTokens":1,"contextWindow":20}}}"#,
    );
    assert!(!events.iter().any(|e| matches!(e, Event::Usage { .. })));
}

fn result_events(model: &str, assistant: bool, model_usage: &str) -> Vec<Event> {
    let mut parser = StreamParser::new();
    parser.parse(&format!(
        r#"{{"type":"system","subtype":"init","session_id":"s","model":"{model}","cwd":"/w"}}"#
    ));
    if assistant {
        parser.parse(
            r#"{"type":"assistant","message":{"role":"assistant","content":[],"usage":{"input_tokens":7,"cache_read_input_tokens":3,"cache_creation_input_tokens":0}}}"#,
        );
    }
    parser.parse(&format!(
        r#"{{"type":"result","subtype":"success","num_turns":1,"result":"ok","modelUsage":{model_usage}}}"#
    ))
}

#[test]
fn result_without_assistant_usage_gives_no_usage() {
    let events = result_events(
        "claude-x",
        false,
        r#"{"claude-x":{"inputTokens":900,"contextWindow":10}}"#,
    );
    assert!(!events.iter().any(|e| matches!(e, Event::Usage { .. })));
}

#[test]
fn a_single_model_usage_entry_must_match_the_main_model() {
    let other = r#"{"claude-y":{"inputTokens":1,"contextWindow":10}}"#;
    let events = result_events("claude-x", true, other);
    assert!(!events.iter().any(|e| matches!(
        e,
        Event::Usage {
            context_window: Some(_),
            ..
        }
    )));
    let same = r#"{"claude-x[1m]":{"inputTokens":1,"contextWindow":10}}"#;
    let events = result_events("claude-x", true, same);
    assert!(events.contains(&usage(10, Some(10))));
}
