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
        Some("Hello there red")
    );
    let long = "word ".repeat(40);
    let title = title_from(&long).unwrap();
    assert_eq!(title.chars().count(), 80);
    assert!(title.ends_with('…'));
    assert_eq!(title_from(" \n \u{7}\n"), None);
    // Local command echoes are not what the person asked.
    assert_eq!(title_from("<command-name>/clear</command-name>"), None);
    // Terminal escapes go whole: colours, titles and links.
    assert_eq!(
        title_from(
            "\u{1b}[1;32mGreen\u{1b}[0m and \u{1b}]0;window\u{7}bold\u{1b}]8;;http://x\u{1b}\\ link"
        )
        .as_deref(),
        Some("Green and bold link")
    );
    // A lone escape at the end does not eat the title.
    assert_eq!(title_from("ok \u{1b}").as_deref(), Some("ok"));
}

#[test]
fn local_command_echoes_are_not_prompts() {
    for text in [
        "<command-name>/clear</command-name>\n<command-message>clear</command-message>\n<command-args></command-args>",
        "<command-message>review is running</command-message>",
        "<command-args>--fast</command-args>",
        "<local-command-stdout>Cleared</local-command-stdout>",
        "  <local-command-caveat>Caveat</local-command-caveat>",
    ] {
        let line = serde_json::json!({"type": "user", "message": {"content": text}}).to_string();
        assert!(parse_line(&line).is_empty(), "{text}");
        let line = serde_json::json!({"type": "user", "message": {"content": [{"type": "text", "text": text}]}}).to_string();
        assert!(parse_line(&line).is_empty(), "{text}");
    }
    // Ordinary text that mentions a tag is still a prompt.
    let line = serde_json::json!({"type": "user", "message": {"content": "why does <command-name> show?"}}).to_string();
    assert_eq!(parse_line(&line).len(), 1);
}

#[test]
fn only_what_the_person_typed_is_a_prompt() {
    use crew_core::claude::transcript::Prompts;
    let text = fixture("filtered.jsonl");
    let mut prompts = Prompts::default();
    let items: Vec<TranscriptItem> = text
        .lines()
        .flat_map(|line| prompts.take(parse(line)))
        .collect();
    assert_eq!(
        items,
        vec![
            // Text next to a tool result is not a prompt; the result is kept.
            event(Event::ToolResult {
                id: "toolu_09".into(),
                is_error: false,
                output: "ok".into(),
            }),
            prompt("Typed with a source."),
            // Queued while the agent was busy, shown once.
            prompt("Also add a test"),
            // Queued input shows whatever its origin says.
            prompt("Message from a teammate"),
            prompt("Typed after"),
        ]
    );
}

#[test]
fn wrapped_text_is_never_a_title() {
    for text in [
        "<system-reminder>x</system-reminder>",
        "<task-notification>x</task-notification>",
        "<user-prompt-submit-hook>x</user-prompt-submit-hook>",
        "<local-command-stdout>x</local-command-stdout>",
        "<command-message>x</command-message>",
        "<bash-input>ls</bash-input>",
    ] {
        assert_eq!(title_from(text), None, "{text}");
    }
    // A shell command typed with `!` is still shown as the person's.
    let line =
        serde_json::json!({"type": "user", "message": {"content": "<bash-input>ls</bash-input>"}})
            .to_string();
    assert_eq!(parse_line(&line).len(), 1);
    // Text that only looks like a tag stays a prompt.
    for text in ["<3 thanks", "<Foo> is the type", "a <b>bold</b> ask"] {
        assert_eq!(title_from(text).as_deref(), Some(text), "{text}");
    }
}

#[test]
fn queued_prompts_are_not_dropped_by_other_text() {
    use crew_core::claude::transcript::Prompts;
    let queued = serde_json::json!({"type": "attachment", "attachment": {"type": "queued_command", "prompt": "Run it", "origin": {"kind": "human"}}}).to_string();
    let typed =
        |t: &str| serde_json::json!({"type": "user", "message": {"content": t}}).to_string();
    let mut prompts = Prompts::default();
    assert_eq!(prompts.take(parse(&queued)), vec![prompt("Run it")]);
    assert_eq!(prompts.take(parse(&typed("Other"))), vec![prompt("Other")]);
    assert!(prompts.take(parse(&typed("Run it"))).is_empty());
    // Typed again later, it shows.
    assert_eq!(
        prompts.take(parse(&typed("Run it"))),
        vec![prompt("Run it")]
    );
}

#[test]
fn queued_prompts_match_once_and_expire() {
    use crew_core::claude::transcript::Prompts;
    let at = |second: u32| format!("2026-11-21T10:{:02}:{:02}.000Z", second / 60, second % 60);
    let queued = |second: u32| {
        serde_json::json!({"type": "attachment", "timestamp": at(second),
            "attachment": {"type": "queued_command", "prompt": "Run it", "origin": {"kind": "whatever"}}})
        .to_string()
    };
    let typed = |second: u32| {
        serde_json::json!({"type": "user", "timestamp": at(second), "message": {"content": "Run it"}})
            .to_string()
    };
    let mut prompts = Prompts::default();
    assert_eq!(prompts.take(parse(&queued(0))), vec![prompt("Run it")]);
    // The first copy within two minutes is the same prompt being sent.
    assert!(prompts.take(parse(&typed(30))).is_empty());
    // Consumed: the next one is typed again.
    assert_eq!(prompts.take(parse(&typed(40))), vec![prompt("Run it")]);

    // After two minutes the queued entry is gone.
    let mut prompts = Prompts::default();
    prompts.take(parse(&queued(0)));
    assert_eq!(prompts.take(parse(&typed(121))), vec![prompt("Run it")]);
}
