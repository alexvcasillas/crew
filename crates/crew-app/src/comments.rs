//! Review comments: findings from reviewer replies, and where each one
//! goes in the diff.

use std::collections::HashMap;

use crew_core::agent::PermissionMode;
use crew_core::ids::{AgentId, ThreadId};
use crew_core::review::parse_findings;
use crew_core::store::{Author, StoredMessage};

use crate::diff_parse::{FileDiff, FileKind, LineKind};
use crate::model::State;

/// A finding from a reviewer's reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// The reviewer's name.
    pub agent: String,
    pub path: String,
    pub line: u32,
    pub body: String,
}

/// A diff line by its file, hunk and line index.
pub type LineKey = (usize, usize, usize);

/// Where each comment, by index, is drawn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Placement {
    /// Comments under a drawn line, in reply order.
    pub under: HashMap<LineKey, Vec<usize>>,
    /// Comments whose file isn't in the diff or whose line isn't drawn.
    pub outside: Vec<usize>,
}

/// Places `comments` on the lines of `files`. `drawn` says whether a
/// file's lines are shown. A comment names a line of the new file, or of
/// the old one for a deleted file.
pub fn place(files: &[FileDiff], drawn: impl Fn(usize) -> bool, comments: &[Comment]) -> Placement {
    let mut placement = Placement::default();
    for (c, comment) in comments.iter().enumerate() {
        let spot = files
            .iter()
            .position(|f| f.path == comment.path)
            .filter(|&f| drawn(f))
            .and_then(|f| line_of(&files[f], comment.line).map(|(h, l)| (f, h, l)));
        match spot {
            Some(key) => placement.under.entry(key).or_default().push(c),
            None => placement.outside.push(c),
        }
    }
    placement
}

/// The hunk and line index of line `n` of `file`.
fn line_of(file: &FileDiff, n: u32) -> Option<(usize, usize)> {
    let deleted = file.kind == FileKind::Deleted;
    file.hunks.iter().enumerate().find_map(|(h, hunk)| {
        hunk.lines
            .iter()
            .position(|line| {
                let number = if deleted { line.old } else { line.new };
                line.kind != LineKind::NoNewline && number == Some(n)
            })
            .map(|l| (h, l))
    })
}

/// The latest reply of each read-only agent since the thread's latest run
/// that may have changed files, oldest first.
pub fn reviewer_replies(state: &State, thread: ThreadId) -> Vec<(AgentId, &StoredMessage)> {
    let runs = state.thread_runs(thread);
    let since = state
        .editing_run(thread)
        .and_then(|edit| runs.iter().position(|r| r.id == edit.id))
        .map_or(0, |ix| ix + 1);
    let after: Vec<_> = runs[since..].iter().map(|r| r.id).collect();
    let read_only = |agent: AgentId| {
        state
            .agent(agent)
            .is_some_and(|a| a.spec.permission_mode == PermissionMode::ReadOnly)
    };
    let mut replies: Vec<(AgentId, &StoredMessage)> = Vec::new();
    for message in state.messages(thread).iter().rev() {
        let Author::Agent(agent) = message.author else {
            continue;
        };
        let counted = message.run.is_some_and(|run| after.contains(&run));
        if counted && read_only(agent) && !replies.iter().any(|(a, _)| *a == agent) {
            replies.push((agent, message));
        }
    }
    replies.reverse();
    replies
}

/// The findings in `replies`, with paths relative to `root`.
pub fn comments(state: &State, replies: &[(AgentId, &StoredMessage)], root: &str) -> Vec<Comment> {
    replies
        .iter()
        .flat_map(|(agent, message)| {
            let name = state
                .agent(*agent)
                .map(|a| a.spec.name.clone())
                .unwrap_or_default();
            parse_findings(&message.body, root)
                .into_iter()
                .map(move |f| Comment {
                    agent: name.clone(),
                    path: f.path,
                    line: f.line,
                    body: f.comment,
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Comment, place};
    use crate::diff_parse::parse;

    const PATCH: &str = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n\
                         @@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n\
                         @@ -10,2 +10,3 @@\n ten\n+new\n eleven\n\
                         diff --git a/gone.rs b/gone.rs\ndeleted file mode 100644\n--- a/gone.rs\n+++ /dev/null\n\
                         @@ -1,2 +0,0 @@\n-x\n-y\n\
                         diff --git a/big.txt b/big.txt\nnew file mode 100644\n--- /dev/null\n+++ b/big.txt\n\
                         @@ -0,0 +1 @@\n+z\n";

    fn comment(path: &str, line: u32) -> Comment {
        Comment {
            agent: "Scout".into(),
            path: path.into(),
            line,
            body: "Look.".into(),
        }
    }

    #[test]
    fn a_comment_goes_under_its_new_line() {
        let files = parse(PATCH);
        let comments = [
            comment("src/a.rs", 2),
            comment("src/a.rs", 11),
            comment("src/a.rs", 2),
            comment("src/a.rs", 12),
        ];
        let got = place(&files, |_| true, &comments);
        // Line 2 is the added TWO, after the removed two.
        assert_eq!(got.under.get(&(0, 0, 2)), Some(&vec![0, 2]));
        assert_eq!(got.under.get(&(0, 1, 1)), Some(&vec![1]));
        assert_eq!(got.under.get(&(0, 1, 2)), Some(&vec![3]));
        assert!(got.outside.is_empty());
    }

    #[test]
    fn a_deleted_file_uses_old_lines() {
        let files = parse(PATCH);
        let got = place(&files, |_| true, &[comment("gone.rs", 2)]);
        assert_eq!(got.under.get(&(1, 0, 1)), Some(&vec![0]));
    }

    #[test]
    fn comments_off_the_diff_go_outside() {
        let files = parse(PATCH);
        let comments = [
            comment("other.rs", 1),
            // Not a line in any hunk.
            comment("src/a.rs", 6),
            // A file whose lines are folded.
            comment("big.txt", 1),
        ];
        let got = place(&files, |f| f != 2, &comments);
        assert!(got.under.is_empty());
        assert_eq!(got.outside, [0, 1, 2]);
        // Shown, the folded file takes its comment.
        let got = place(&files, |_| true, &comments);
        assert_eq!(got.under.get(&(2, 0, 0)), Some(&vec![2]));
        assert_eq!(got.outside, [0, 1]);
    }
}
