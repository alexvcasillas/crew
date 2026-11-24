//! The checks an agent ran: test, lint and build commands found in a
//! thread's events, with whether each one passed.

use std::collections::HashMap;

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
    /// The tool call id of that run.
    pub call: String,
}

/// The shell command inside `sh -c '...'`, `/bin/zsh -lc "..."` and the
/// like, else `command` itself.
fn unwrap_shell(command: &str) -> &str {
    let command = command.trim();
    let mut words = command.splitn(3, char::is_whitespace);
    let (Some(shell), Some(flag), Some(rest)) = (words.next(), words.next(), words.next()) else {
        return command;
    };
    let name = shell.rsplit('/').next().unwrap_or(shell);
    let is_flag = flag.starts_with('-') && flag.ends_with('c') && !flag.starts_with("--");
    if !["sh", "bash", "zsh"].contains(&name) || !is_flag {
        return command;
    }
    let rest = rest.trim();
    for quote in ['\'', '"'] {
        if let Some(inner) = rest.strip_prefix(quote).and_then(|r| r.strip_suffix(quote)) {
            return inner;
        }
    }
    rest
}

/// Whether `word` is a `NAME=value` environment assignment.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// The command that does the work: shell wrappers, `cd <dir> &&` (or
/// `;`) prefixes and leading `NAME=value` assignments dropped.
fn core_command(command: &str) -> &str {
    let mut command = unwrap_shell(command);
    loop {
        let trimmed = command.trim_start();
        if let Some(rest) = trimmed.strip_prefix("cd ")
            && let Some((_, after)) = rest.split_once("&&").or_else(|| rest.split_once(';'))
        {
            command = unwrap_shell(after);
            continue;
        }
        let first = trimmed.split_whitespace().next().unwrap_or("");
        if is_assignment(first) {
            command = unwrap_shell(&trimmed[first.len()..]);
            continue;
        }
        return trimmed;
    }
}

/// Whether `command` runs a test, lint, type check or build.
pub fn is_check(command: &str) -> bool {
    let command = core_command(command);
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
        "npx" | "bunx" => rest
            .iter()
            .find(|w| !w.starts_with('-'))
            .is_some_and(|w| ["tsc", "eslint", "vitest", "jest", "biome", "prettier"].contains(w)),
        "tsc" | "eslint" | "vitest" | "jest" | "pytest" | "ruff" | "mypy" => true,
        "go" => second(&["test", "vet", "build"]),
        "swift" => second(&["test", "build"]),
        "make" => second(&["test", "check", "lint"]),
        "xcodebuild" => rest.contains(&"test"),
        _ => false,
    }
}

/// The checks in `events`, in the order they last ran. A command that ran
/// again counts once, with its latest state. Only calls of `runs` count. A call with no result whose
/// run is not in `live` never ended and is ignored.
pub fn checks(events: &[LoggedEvent], runs: &[RunId], live: &[RunId]) -> Vec<Check> {
    let mut results: HashMap<(RunId, &str), bool> = HashMap::new();
    for logged in events {
        if let Event::ToolResult { id, is_error, .. } = &logged.event {
            results.insert((logged.run, id), *is_error);
        }
    }
    let mut found: Vec<Check> = Vec::new();
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
        if !runs.contains(&logged.run) || !is_check(command) {
            continue;
        }
        let state = match results.get(&(logged.run, id.as_str())) {
            Some(true) => CheckState::Failed,
            Some(false) => CheckState::Passed,
            None if live.contains(&logged.run) => CheckState::Running,
            None => continue,
        };
        found.retain(|c| c.command != *command);
        found.push(Check {
            command: command.clone(),
            state,
            run: logged.run,
            call: id.clone(),
        });
    }
    found
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
            "cd x; cargo test",
            "FOO=1 cargo test",
            "CI=1 NODE_ENV=test npm test",
            "cd x && RUST_LOG=debug cargo clippy",
            "npx -y tsc --noEmit",
            "bunx --bun vitest",
            "/bin/zsh -lc 'cargo test --workspace'",
            "bash -c \"cd app && npm run lint\"",
            "sh -lc 'FOO=1 pytest'",
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
            "FOO=1 ls",
            "npx -y create-app",
            "/bin/zsh -lc 'mkdir sub'",
            "zsh -lc 'echo cargo test'",
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
        let got: Vec<_> = checks(&events, &[run], &[run])
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
        let got = checks(&events, &[a, b], &[a, b]);
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
        assert!(checks(&events, &[run], &[run]).is_empty());
    }

    #[test]
    fn a_call_that_never_ended_in_a_finished_run_is_ignored() {
        let (a, b) = (RunId::new(), RunId::new());
        let events = [
            call(a, 1, "x", "cargo test"),
            result(a, 2, "x", false),
            // The rerun was interrupted: no result, and its run is over.
            call(b, 1, "x", "cargo test"),
        ];
        let got = checks(&events, &[a, b], &[]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].state, CheckState::Passed);
        assert_eq!(got[0].run, a);
        // While its run is live, the rerun shows as running.
        let got = checks(&events, &[a, b], &[b]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].state, CheckState::Running);
    }

    #[test]
    fn a_running_check_passes_when_its_result_arrives() {
        let run = RunId::new();
        let mut events = vec![call(run, 1, "a", "cargo test")];
        assert_eq!(
            checks(&events, &[run], &[run])[0].state,
            CheckState::Running
        );
        events.push(result(run, 2, "a", false));
        assert_eq!(checks(&events, &[run], &[run])[0].state, CheckState::Passed);
    }

    #[test]
    fn only_the_given_runs_count() {
        let (mine, other) = (RunId::new(), RunId::new());
        let events = [
            call(mine, 1, "a", "cargo test"),
            result(mine, 2, "a", false),
            call(other, 1, "b", "npm test"),
            result(other, 2, "b", false),
        ];
        let got = checks(&events, &[mine], &[]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].command, "cargo test");
    }
}
