//! The checks an agent ran: test, lint and build commands found in a
//! thread's events, with whether each one passed.

use crew_core::event::{Event, ToolKind};
use crew_core::ids::RunId;

use crate::model::LoggedEvent;

/// How a check ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    Running,
    Passed,
    Failed,
}

/// One check command and how its latest run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub command: String,
    pub state: CheckState,
    /// The run that ran it last.
    pub run: RunId,
}

/// `cd <dir> &&` prefixes dropped.
fn without_cd(mut command: &str) -> &str {
    loop {
        let trimmed = command.trim_start();
        let Some(rest) = trimmed.strip_prefix("cd ") else {
            return trimmed;
        };
        match rest.split_once("&&") {
            Some((_, after)) => command = after,
            None => return trimmed,
        }
    }
}

/// Whether `command` runs a test, lint, type check or build.
pub fn is_check(command: &str) -> bool {
    let command = without_cd(command);
    let mut words = command.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    let rest: Vec<&str> = words.collect();
    let second = |set: &[&str]| rest.first().is_some_and(|w| set.contains(w));
    match first {
        "cargo" => {
            let sub = rest.iter().find(|w| !w.starts_with('+'));
            sub.is_some_and(|w| ["test", "clippy", "check", "fmt", "build", "nextest"].contains(w))
        }
        "npm" | "pnpm" | "yarn" | "bun" => {
            let sub = match rest.first() {
                Some(&"run") => rest.get(1),
                other => other,
            };
            sub.is_some_and(|w| ["test", "lint", "build", "typecheck", "check"].contains(w))
        }
        "npx" | "bunx" => second(&["tsc", "eslint", "vitest", "jest", "biome", "prettier"]),
        "tsc" | "eslint" | "vitest" | "jest" | "pytest" | "ruff" | "mypy" => true,
        "go" => second(&["test", "vet", "build"]),
        "swift" => second(&["test", "build"]),
        "make" => second(&["test", "check", "lint"]),
        "xcodebuild" => rest.contains(&"test"),
        _ => false,
    }
}

/// The checks in `events`, in the order they last ran. A command that ran
/// again counts once, with its latest state.
pub fn checks(events: &[LoggedEvent]) -> Vec<Check> {
    let mut found: Vec<(String, Check)> = Vec::new();
    for logged in events {
        let Event::ToolCall {
            id,
            kind: ToolKind::Run,
            target: Some(command),
            ..
        } = &logged.event
        else {
            continue;
        };
        if !is_check(command) {
            continue;
        }
        let state = events
            .iter()
            .find_map(|e| match &e.event {
                Event::ToolResult {
                    id: got, is_error, ..
                } if e.run == logged.run && got == id => Some(if *is_error {
                    CheckState::Failed
                } else {
                    CheckState::Passed
                }),
                _ => None,
            })
            .unwrap_or(CheckState::Running);
        found.retain(|(c, _)| c != command);
        found.push((
            command.clone(),
            Check {
                command: command.clone(),
                state,
                run: logged.run,
            },
        ));
    }
    found.into_iter().map(|(_, c)| c).collect()
}

#[cfg(test)]
mod tests {
    use crew_core::event::{Event, ToolKind};
    use crew_core::ids::RunId;

    use super::{CheckState, checks, is_check};
    use crate::model::LoggedEvent;

    fn call(run: RunId, seq: u64, id: &str, command: &str) -> LoggedEvent {
        LoggedEvent::new(
            run,
            seq,
            Event::ToolCall {
                id: id.into(),
                name: "Bash".into(),
                kind: ToolKind::Run,
                target: Some(command.into()),
                input: serde_json::Value::Null,
            },
        )
    }

    fn result(run: RunId, seq: u64, id: &str, is_error: bool) -> LoggedEvent {
        LoggedEvent::new(
            run,
            seq,
            Event::ToolResult {
                id: id.into(),
                is_error,
                output: String::new(),
            },
        )
    }

    #[test]
    fn test_lint_and_build_commands_match() {
        for command in [
            "cargo test",
            "cargo test --workspace",
            "cargo clippy --all-targets -- -D warnings",
            "cargo +nightly fmt --all",
            "cargo nextest run",
            "npm test",
            "npm run lint",
            "pnpm build",
            "yarn typecheck",
            "bun run check",
            "npx tsc --noEmit",
            "bunx vitest run",
            "tsc -p .",
            "eslint src",
            "pytest -q",
            "ruff check .",
            "mypy pkg",
            "go test ./...",
            "go vet ./...",
            "swift build",
            "make lint",
            "xcodebuild -scheme App test",
            "cd crates/app && cargo test",
            "cd a && cd b && npm run lint",
        ] {
            assert!(is_check(command), "{command}");
        }
    }

    #[test]
    fn other_commands_do_not_match() {
        for command in [
            "",
            "cat test.txt",
            "ls -la",
            "git status",
            "cargo run",
            "cargo add serde",
            "npm install",
            "npm run dev",
            "npx create-app",
            "go run main.go",
            "make install",
            "echo cargo test",
            "cd x",
            "cd x && ls",
        ] {
            assert!(!is_check(command), "{command}");
        }
    }

    #[test]
    fn state_comes_from_the_result() {
        let run = RunId::new();
        let events = [
            call(run, 1, "a", "cargo test"),
            result(run, 2, "a", false),
            call(run, 3, "b", "cargo clippy"),
            result(run, 4, "b", true),
            call(run, 5, "c", "npm run lint"),
        ];
        let got: Vec<_> = checks(&events)
            .into_iter()
            .map(|c| (c.command, c.state))
            .collect();
        assert_eq!(
            got,
            [
                ("cargo test".to_string(), CheckState::Passed),
                ("cargo clippy".to_string(), CheckState::Failed),
                ("npm run lint".to_string(), CheckState::Running),
            ]
        );
    }

    #[test]
    fn the_latest_run_of_a_command_wins() {
        let (a, b) = (RunId::new(), RunId::new());
        let events = [
            call(a, 1, "x", "cargo test"),
            result(a, 2, "x", true),
            call(a, 3, "y", "cargo fmt"),
            result(a, 4, "y", false),
            // The same call id in another run is a different call.
            call(b, 1, "x", "cargo test"),
            result(b, 2, "x", false),
        ];
        let got = checks(&events);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].command, "cargo fmt");
        assert_eq!(got[1].command, "cargo test");
        assert_eq!(got[1].state, CheckState::Passed);
        assert_eq!(got[1].run, b);
    }

    #[test]
    fn non_check_calls_and_other_tools_are_skipped() {
        let run = RunId::new();
        let events = [
            call(run, 1, "a", "ls"),
            LoggedEvent::new(
                run,
                2,
                Event::ToolCall {
                    id: "b".into(),
                    name: "Read".into(),
                    kind: ToolKind::Read,
                    target: Some("cargo test".into()),
                    input: serde_json::Value::Null,
                },
            ),
        ];
        assert!(checks(&events).is_empty());
    }
}
