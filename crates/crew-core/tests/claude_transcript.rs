use crew_core::claude::transcript::{TranscriptItem, parse, parse_line, title_from};
use crew_core::event::{Event, ToolKind};
use serde_json::json;

const SID: &str = "00000000-0000-4000-8000-0000000000a1";

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/transcript/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(path).unwrap()
}

fn prompt(text: &str) -> TranscriptItem {
    TranscriptItem::UserPrompt(text.into())
}

fn event(e: Event) -> TranscriptItem {
    TranscriptItem::Event(e)
}

#[test]
fn a_session_file_becomes_prompts_and_events() {
    let items: Vec<TranscriptItem> = fixture("session.jsonl")
        .lines()
        .flat_map(parse_line)
        .collect();
    assert_eq!(
        items,
        vec![
            prompt("Fix the failing test\nin src/lib.rs please"),
            event(Event::Thinking {
                text: "Look at the test first.".into()
            }),
            event(Event::ToolCall {
                id: "toolu_01".into(),
                name: "Read".into(),
                kind: ToolKind::Read,
                target: Some("/work/demo/src/lib.rs".into()),
                input: json!({"file_path": "/work/demo/src/lib.rs"}),
            }),
            event(Event::ToolResult {
                id: "toolu_01".into(),
                is_error: false,
                output: "1\tpub fn add() {}\n".into(),
            }),
            event(Event::Message {
                text: "The test expects add to return a sum.".into()
            }),
            event(Event::ToolCall {
                id: "toolu_02".into(),
                name: "Bash".into(),
                kind: ToolKind::Run,
                target: Some("cargo test".into()),
                input: json!({"command": "cargo test", "description": "Run the tests"}),
            }),
            event(Event::ToolResult {
                id: "toolu_02".into(),
                is_error: true,
                output: "test result: FAILED".into(),
            }),
            event(Event::Message {
                text: "Fixed it.".into()
            }),
            event(Event::TurnEnd {
                ok: true,
                summary: String::new(),
                duration_ms: 8000,
                num_turns: 0,
            }),
            prompt("Now look at this"),
            event(Event::Error {
                message: "Overloaded".into()
            }),
        ]
    );
}

#[test]
fn line_keys_are_read_from_every_line() {
    let text = fixture("session.jsonl");
    let lines: Vec<&str> = text.lines().collect();

    let first = parse(lines[0]);
    assert_eq!(first.info.session_id.as_deref(), Some(SID));
    assert_eq!(first.info.cwd, None);
    assert_eq!(first.info.timestamp, None);
    assert!(first.items.is_empty());

    let ask = parse(lines[3]);
    assert_eq!(ask.info.session_id.as_deref(), Some(SID));
    assert_eq!(ask.info.cwd.as_deref(), Some("/work/demo"));
    assert_eq!(ask.info.entrypoint.as_deref(), Some("cli"));
    // 2026-11-20T10:00:01Z in Unix milliseconds.
    assert_eq!(ask.info.timestamp, Some(1_795_168_801_000));

    // Hidden lines still tell where and when the session is.
    let meta = parse(lines[2]);
    assert!(meta.items.is_empty());
    assert_eq!(meta.info.cwd.as_deref(), Some("/work/demo"));
}

#[test]
fn snake_case_session_ids_are_read_too() {
    let got = parse(r#"{"type":"mode","mode":"normal","session_id":"abc"}"#);
    assert_eq!(got.info.session_id.as_deref(), Some("abc"));
    assert!(got.items.is_empty());
}

#[test]
fn odd_lines_give_nothing() {
    for line in [
        "",
        "   ",
        "not json",
        "[1,2]",
        r#"{"type":"user","message":{"content":"   "}}"#,
        r#"{"type":"user","message":{"content":[{"type":"image"}]}}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"redacted_thinking"}]}}"#,
        r#"{"type":"system","subtype":"informational","content":"hi"}"#,
        r#"{"type":"custom-title","customTitle":"x"}"#,
        r#"{"type":"user","message":{"content":"x"},"isMeta":true}"#,
        r#"{"type":"user","message":{"content":"x"},"isSidechain":true}"#,
    ] {
        assert!(parse_line(line).is_empty(), "{line:?}");
    }
}

#[test]
fn api_error_replies_become_errors() {
    let line = r#"{"type":"assistant","isApiErrorMessage":true,"message":{"id":"m","content":[{"type":"text","text":"API Error: 529"}]}}"#;
    assert_eq!(
        parse_line(line),
        vec![event(Event::Error {
            message: "API Error: 529".into()
        })]
    );
}

#[test]
fn titles_take_the_first_line_clean_and_short() {
    assert_eq!(
        title_from("Fix the failing test\nin src/lib.rs").as_deref(),
        Some("Fix the failing test")
    );
    assert_eq!(
        title_from("\n\n  Hello\tthere\u{1b}[31m red\u{202e}  ").as_deref(),
        Some("Hello there[31m red")
    );
    let long = "word ".repeat(40);
    let title = title_from(&long).unwrap();
    assert_eq!(title.chars().count(), 80);
    assert!(title.ends_with('…'));
    assert_eq!(title_from(" \n \u{7}\n"), None);
    // Local command echoes are not what the person asked.
    assert_eq!(title_from("<command-name>/clear</command-name>"), None);
}
