//! What agents changed, worked out from their tool calls.

use std::collections::{HashMap, HashSet};

use crate::event::{Event, ToolKind};

/// Lines an edit or a new file adds and removes.
pub fn changed_lines(kind: &ToolKind, input: &serde_json::Value) -> Option<(u32, u32)> {
    let lines = |v: &serde_json::Value| {
        v.as_str()
            .map_or(0, |s| u32::try_from(s.lines().count()).unwrap_or(u32::MAX))
    };
    // Codex gives a unified diff instead of old and new text.
    if let Some(diff) = input.get("diff").and_then(|d| d.as_str()) {
        return match kind {
            ToolKind::Edit => Some(patch_lines(diff)),
            ToolKind::Create => Some((lines(&input["diff"]), 0)),
            _ => None,
        };
    }
    match kind {
        ToolKind::Edit => {
            let edits = match input.get("edits").and_then(|e| e.as_array()) {
                Some(edits) => edits.iter().collect(),
                None => vec![input],
            };
            Some(edits.iter().fold((0, 0), |(a, r), e| {
                (a + lines(&e["new_string"]), r + lines(&e["old_string"]))
            }))
        }
        ToolKind::Create => Some((lines(&input["content"]), 0)),
        _ => None,
    }
}

/// What a unified diff line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchMark {
    Added,
    Removed,
    /// File headers, hunk heads and context.
    Plain,
}

/// Marks each line of a unified diff. `---` and `+++` are file headers
/// only before the first `@@`; inside a hunk they are changed lines.
pub fn patch_marks(diff: &str) -> impl Iterator<Item = (&str, PatchMark)> {
    let mut in_hunk = false;
    diff.lines().map(move |line| {
        if line.starts_with("@@") {
            in_hunk = true;
        }
        let header = !in_hunk && (line.starts_with("---") || line.starts_with("+++"));
        let mark = if header {
            PatchMark::Plain
        } else if line.starts_with('+') {
            PatchMark::Added
        } else if line.starts_with('-') {
            PatchMark::Removed
        } else {
            PatchMark::Plain
        };
        (line, mark)
    })
}

/// Lines a unified diff adds and removes.
pub fn patch_lines(diff: &str) -> (u32, u32) {
    let (mut added, mut removed) = (0u32, 0u32);
    for (_, mark) in patch_marks(diff) {
        match mark {
            PatchMark::Added => added = added.saturating_add(1),
            PatchMark::Removed => removed = removed.saturating_add(1),
            PatchMark::Plain => {}
        }
    }
    (added, removed)
}

/// Files and lines a run's edits touched, by its tool calls. Calls that
/// failed don't count. Lines are what the edits wrote and replaced, not a
/// diff.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EditCounts {
    pub files: u32,
    pub added: u32,
    pub removed: u32,
}

/// Adds up the edit and write calls among `events`, one run's in order.
pub fn run_edits<'a>(events: impl IntoIterator<Item = &'a Event>) -> EditCounts {
    let mut calls: HashMap<&str, (Option<String>, (u32, u32))> = HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    let mut failed: HashSet<&str> = HashSet::new();
    for event in events {
        match event {
            Event::ToolCall {
                id,
                kind,
                target,
                input,
                ..
            } => {
                let Some(lines) = changed_lines(kind, input) else {
                    continue;
                };
                let path = target
                    .clone()
                    .or_else(|| input["file_path"].as_str().map(str::to_string))
                    .or_else(|| input["path"].as_str().map(str::to_string));
                if calls.insert(id, (path, lines)).is_none() {
                    order.push(id);
                }
            }
            Event::ToolResult { id, is_error, .. } if *is_error => {
                failed.insert(id);
            }
            _ => {}
        }
    }
    let mut counts = EditCounts::default();
    let mut files: HashSet<String> = HashSet::new();
    for id in order {
        if failed.contains(id) {
            continue;
        }
        let (path, (added, removed)) = &calls[id];
        counts.added = counts.added.saturating_add(*added);
        counts.removed = counts.removed.saturating_add(*removed);
        files.insert(path.clone().unwrap_or_else(|| id.to_string()));
    }
    counts.files = u32::try_from(files.len()).unwrap_or(u32::MAX);
    counts
}

#[cfg(test)]
mod tests {
    use crate::edits::{EditCounts, run_edits};
    use crate::event::{Event, ToolKind};

    fn call(id: &str, kind: ToolKind, target: &str, input: serde_json::Value) -> Event {
        Event::ToolCall {
            id: id.into(),
            name: "Edit".into(),
            kind,
            target: Some(target.into()),
            input,
        }
    }

    #[test]
    fn a_run_counts_its_edited_files_once_and_skips_failed_calls() {
        let events = [
            call(
                "1",
                ToolKind::Edit,
                "src/a.rs",
                serde_json::json!({"old_string": "a", "new_string": "b\nc"}),
            ),
            call(
                "2",
                ToolKind::Edit,
                "src/a.rs",
                serde_json::json!({"old_string": "x\ny", "new_string": "z"}),
            ),
            call(
                "3",
                ToolKind::Create,
                "src/new.rs",
                serde_json::json!({"content": "1\n2\n3\n"}),
            ),
            call(
                "4",
                ToolKind::Edit,
                "src/bad.rs",
                serde_json::json!({"old_string": "q", "new_string": "r"}),
            ),
            Event::ToolResult {
                id: "4".into(),
                is_error: true,
                output: "no match".into(),
            },
            call("5", ToolKind::Read, "src/a.rs", serde_json::json!({})),
        ];
        assert_eq!(
            run_edits(&events),
            EditCounts {
                files: 2,
                added: 6,
                removed: 3
            }
        );
        assert_eq!(run_edits(&[]), EditCounts::default());
    }
}
