//! Review changes: an agent's uncommitted work in the thread's worktree
//! as a diff, and what to do with it: commit, ask for changes or discard.
//! It takes the place of the sidebar, the main area and the inspector.

use std::collections::HashSet;
use std::ops::Range;

use crew_core::ids::{AgentId, ThreadId};
use crew_core::store::Author;
use crew_proto::{OutsideFile, Request, ThreadChanges};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{Disableable as _, Icon, IconName, Selectable as _, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::{dot, spinner};
use super::folder::{
    BRANCH_ICON, FILE_ICON, FolderMode, and_list, files_count, folder_mode, in_place_chip,
    mode_chip, small_icon, warning_note,
};
use super::inspector::INSPECTOR_WIDTH;
use super::markdown_view::render_doc;
use super::workspace::Workspace;
use crate::checks::{Check, CheckState, checks};
use crate::comments::{Comment, comments, place, reviewer_replies};
use crate::diff_parse::{FileDiff, FileKind, Line, LineKind, parse, split_pairs};
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_family, ui_px, weight};

actions!(
    review,
    [
        /// Leave the review for the thread.
        CloseReview,
        /// Leave the commit message for the review.
        LeaveMessage,
        /// Scroll to the next hunk.
        NextChange,
        /// Scroll to the hunk before.
        PreviousChange,
    ]
);

const REVIEW_CONTEXT: &str = "Review";
const MESSAGE_CONTEXT: &str = "ReviewMessage";
/// Files with more lines than this wait for "Show diff".
pub const LARGE_FILE_LINES: usize = 2000;
/// The longest first line a drafted commit message keeps.
const SUBJECT_MAX: usize = 72;
/// The most body lines a drafted commit message keeps.
const BODY_MAX_LINES: usize = 20;
const GUTTER: f32 = 44.;
const LINE_HEIGHT: f32 = 20.;
const NOTHING_TO_REVIEW: &str = "No changes to review.";
const OUTSIDE_TITLE: &str = "Comments outside the diff";
const NOTE: &str = "Nothing is pushed. Committing stays on your machine, and the branch is yours to merge or open a PR from.";
const IN_PLACE_NOTE: &str =
    "In-place threads commit on each repo's current branch. Nothing is pushed.";
const OUTSIDE_THREAD: &str = "changed outside this thread";
const NOT_IN_GIT: &str = "Not in git, can't review or undo.";

/// Esc leaves the commit message first, then the review.
pub fn bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("escape", CloseReview, Some(REVIEW_CONTEXT)),
        KeyBinding::new("escape", LeaveMessage, Some("ReviewMessage > Input")),
        KeyBinding::new("j", NextChange, Some("Review && !Input")),
        KeyBinding::new("k", PreviousChange, Some("Review && !Input")),
    ]
}

/// A commit message kept while its thread's review is closed.
pub(super) struct SavedMessage {
    text: String,
    /// The draft it started from: if `text` is still that, a newer draft
    /// may replace it.
    draft: String,
}

/// One row of the diff list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    File(usize),
    Hunk(usize, usize),
    Line(usize, usize, usize),
    /// A split row: the hunk's line on the left, by index, and the one on
    /// the right. Context is on both sides.
    Pair(usize, usize, Option<usize>, Option<usize>),
    /// The file's one-line note, such as for a binary file.
    Note(usize),
    /// A large file waiting for "Show diff".
    Folded(usize),
    /// The title above comments that aren't on a shown line.
    OutsideTitle,
    /// A review comment, by its index, under the row before it.
    Comment(usize),
}

/// The open review of a thread's changes.
pub(super) struct Review {
    pub(super) thread: ThreadId,
    /// Whether a diff was read yet.
    loaded: bool,
    /// Parsed from the last diff read. The one source of file counts.
    files: Vec<FileDiff>,
    /// Lines side by side, as chosen with Unified / Split.
    split: bool,
    rows: Vec<Row>,
    /// Each file's header row.
    file_rows: Vec<usize>,
    list: ListState,
    /// Large files you asked to see, by path.
    shown: HashSet<String>,
    /// Findings from the reviewers' latest replies.
    comments: Vec<Comment>,
    /// The latest reviewer and how many comments its reply left.
    left: Option<(String, usize)>,
    message: Entity<TextareaState>,
    /// The draft from the agent's latest reply, empty if none.
    draft: String,
    _message_events: Subscription,
    /// A commit or discard was sent and has not been answered.
    busy: bool,
    /// Why the last read failed, shown in place of the diff.
    failed: Option<String>,
    /// The file you last picked in the list, by path.
    current_file: Option<String>,
    /// A file to scroll to once the diff is read.
    reveal_file: Option<String>,
    /// The checks the agent ran, read again when the thread's activity
    /// changes.
    checks: Vec<Check>,
    /// What the comments and checks were read from.
    synced: Option<(usize, usize, usize, usize)>,
    /// Why Open in editor failed, shown above the diff.
    open_error: Option<String>,
    focus: FocusHandle,
    /// Set for a folder project: the files by repo, a commit per repo
    /// and the files git can't see.
    folder: Option<FolderReview>,
    /// The hunk header j or k last went to, by row.
    change_row: Option<usize>,
}

/// The parts of a review only a folder project has.
pub(super) struct FolderReview {
    mode: FolderMode,
    /// In the order crewd lists the repos, each with its files.
    groups: Vec<RepoGroup>,
    /// Files the thread changed outside any repo.
    outside_git: Vec<OutsideFile>,
    /// A commit message per repo, made as repos show up.
    messages: Vec<RepoMessage>,
}

/// One repo's files in [`Review::files`].
#[derive(Debug, Clone, PartialEq)]
struct RepoGroup {
    /// Relative to the project folder; empty for the folder itself.
    repo: String,
    branch: Option<String>,
    files: Range<usize>,
    /// Files of the range, by path, the thread didn't change.
    outside: HashSet<String>,
}

impl RepoGroup {
    /// The files a commit or discard takes: the thread's own.
    fn own(&self, files: &[FileDiff]) -> usize {
        files[self.files.clone()]
            .iter()
            .filter(|f| !self.outside.contains(&f.path))
            .count()
    }
}

struct RepoMessage {
    repo: String,
    input: Entity<TextareaState>,
    _events: Subscription,
}

/// The files of `changes` repo by repo, with each repo's place in them.
/// Paths keep the repo in front, as the patch names them.
fn group_files(changes: &ThreadChanges) -> (Vec<FileDiff>, Vec<RepoGroup>) {
    let mut files = Vec::new();
    let mut groups = Vec::new();
    for repo in changes.repos.iter().filter(|r| !r.diff.files.is_empty()) {
        let start = files.len();
        files.extend(parse(&repo.diff.patch));
        let outside = repo
            .outside_thread
            .iter()
            .map(|p| repo_path(&repo.repo, p))
            .collect();
        groups.push(RepoGroup {
            repo: repo.repo.clone(),
            branch: repo.branch.clone(),
            files: start..files.len(),
            outside,
        });
    }
    (files, groups)
}

/// `path` of `repo` as the project folder has it.
fn repo_path(repo: &str, path: &str) -> String {
    if repo.is_empty() {
        path.to_string()
    } else {
        format!("{repo}/{path}")
    }
}

/// `path` without its repo in front.
fn in_repo<'a>(repo: &str, path: &'a str) -> &'a str {
    if repo.is_empty() {
        return path;
    }
    path.strip_prefix(repo)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(path)
}

/// The row j (`forward`) or k goes to from `top`: the next or previous
/// hunk's header.
fn next_hunk(rows: &[Row], top: usize, forward: bool) -> Option<usize> {
    let hunk = |ix: &usize| matches!(rows[*ix], Row::Hunk(..));
    if forward {
        (top + 1..rows.len()).find(hunk)
    } else {
        (0..top).rev().find(hunk)
    }
}

impl Review {
    /// Measures every row again, as after a text size change.
    pub(super) fn remeasure(&self) {
        self.list.remeasure();
    }
}

/// A commit message from an agent's reply: its first line, at most
/// [`SUBJECT_MAX`] characters, then a blank line and at most
/// [`BODY_MAX_LINES`] lines of the rest.
pub fn draft_commit_message(reply: &str) -> String {
    let mut lines = reply.lines().skip_while(|l| l.trim().is_empty());
    let subject: String = lines
        .next()
        .unwrap_or_default()
        .trim()
        .chars()
        .take(SUBJECT_MAX)
        .collect();
    let mut body: Vec<&str> = lines
        .skip_while(|l| l.trim().is_empty())
        .take(BODY_MAX_LINES)
        .map(str::trim_end)
        .collect();
    while body.last().is_some_and(|l| l.is_empty()) {
        body.pop();
    }
    let subject = subject.trim_end();
    if body.is_empty() {
        subject.to_string()
    } else {
        format!("{subject}\n\n{}", body.join("\n"))
    }
}

/// Whether a file's lines are drawn: it has no note and is small or
/// asked for.
fn lines_drawn(file: &FileDiff, shown: &HashSet<String>) -> bool {
    file.note.is_none() && (file.line_count() <= LARGE_FILE_LINES || shown.contains(&file.path))
}

/// The diff list's rows, and where each file starts. Comments go under
/// their line, or above the first file when their line isn't drawn.
fn diff_rows(
    files: &[FileDiff],
    shown: &HashSet<String>,
    comments: &[Comment],
    split: bool,
) -> (Vec<Row>, Vec<usize>) {
    let placement = place(files, |f| lines_drawn(&files[f], shown), comments);
    let mut rows = Vec::new();
    if !placement.outside.is_empty() {
        rows.push(Row::OutsideTitle);
        rows.extend(placement.outside.iter().map(|&c| Row::Comment(c)));
    }
    let mut starts = Vec::new();
    for (f, file) in files.iter().enumerate() {
        starts.push(rows.len());
        rows.push(Row::File(f));
        if file.note.is_some() {
            rows.push(Row::Note(f));
        } else if !lines_drawn(file, shown) {
            rows.push(Row::Folded(f));
        } else {
            for (h, hunk) in file.hunks.iter().enumerate() {
                rows.push(Row::Hunk(f, h));
                let under = |rows: &mut Vec<Row>, l: usize| {
                    if let Some(under) = placement.under.get(&(f, h, l)) {
                        rows.extend(under.iter().map(|&c| Row::Comment(c)));
                    }
                };
                if split {
                    for (left, right) in split_pairs(hunk) {
                        rows.push(Row::Pair(f, h, left, right));
                        if let Some(l) = left {
                            under(&mut rows, l);
                        }
                        // Context is the same line on both sides.
                        if let Some(l) = right.filter(|&r| Some(r) != left) {
                            under(&mut rows, l);
                        }
                    }
                } else {
                    for l in 0..hunk.lines.len() {
                        rows.push(Row::Line(f, h, l));
                        under(&mut rows, l);
                    }
                }
            }
        }
    }
    (rows, starts)
}

/// Something a row stands for that survives a rebuild: a file by its
/// path, then what is in it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Anchor {
    /// The comments listed above the first file.
    Outside,
    File(String),
    Hunk(String, usize),
    Line(String, usize, usize),
    Note(String),
    Folded(String),
}

impl Anchor {
    /// What to look for when this is gone.
    fn parent(&self) -> Option<Anchor> {
        match self {
            Anchor::Line(path, h, _) => Some(Anchor::Hunk(path.clone(), *h)),
            Anchor::Hunk(path, _) | Anchor::Note(path) | Anchor::Folded(path) => {
                Some(Anchor::File(path.clone()))
            }
            Anchor::File(_) | Anchor::Outside => None,
        }
    }
}

/// What the row at `ix` stands for. A comment stands for the row above
/// it, as the cards are not part of the diff.
fn anchor_at(rows: &[Row], files: &[FileDiff], ix: usize) -> Option<Anchor> {
    let mut ix = ix.min(rows.len().checked_sub(1)?);
    while matches!(rows[ix], Row::Comment(_)) && ix > 0 {
        ix -= 1;
    }
    let path = |f: usize| files.get(f).map(|f| f.path.clone());
    match rows[ix] {
        Row::OutsideTitle | Row::Comment(_) => Some(Anchor::Outside),
        Row::File(f) => path(f).map(Anchor::File),
        Row::Hunk(f, h) => path(f).map(|p| Anchor::Hunk(p, h)),
        Row::Line(f, h, l) => path(f).map(|p| Anchor::Line(p, h, l)),
        Row::Pair(f, h, left, right) => {
            let l = left.or(right)?;
            path(f).map(|p| Anchor::Line(p, h, l))
        }
        Row::Note(f) => path(f).map(Anchor::Note),
        Row::Folded(f) => path(f).map(Anchor::Folded),
    }
}

/// The row that stands for `anchor`, else for what it was part of.
fn find_anchor(rows: &[Row], files: &[FileDiff], anchor: &Anchor) -> Option<usize> {
    let mut anchor = Some(anchor.clone());
    while let Some(a) = anchor {
        let found = rows.iter().position(|row| {
            let same_file = |f: usize, path: &String| files.get(f).is_some_and(|x| &x.path == path);
            match (&a, *row) {
                (Anchor::Outside, Row::OutsideTitle) => true,
                (Anchor::File(p), Row::File(f)) => same_file(f, p),
                (Anchor::Hunk(p, h), Row::Hunk(f, rh)) => rh == *h && same_file(f, p),
                (Anchor::Line(p, h, l), Row::Line(f, rh, rl)) => {
                    rh == *h && rl == *l && same_file(f, p)
                }
                (Anchor::Line(p, h, l), Row::Pair(f, rh, left, right)) => {
                    rh == *h && (left == Some(*l) || right == Some(*l)) && same_file(f, p)
                }
                (Anchor::Note(p), Row::Note(f)) => same_file(f, p),
                (Anchor::Folded(p), Row::Folded(f)) => same_file(f, p),
                _ => false,
            }
        });
        if found.is_some() {
            return found;
        }
        anchor = a.parent();
    }
    None
}

/// The row at the top of the list, to come back to after a rebuild.
struct Kept {
    anchor: Option<Anchor>,
    /// How far into that row the list was scrolled. Zero for a card, whose
    /// height can change.
    offset: Pixels,
    /// Its place before, for when nothing matches.
    ix: usize,
}

/// `M`, `A`, `D`, `R` and their colour tokens.
fn letter_colour(kind: &FileKind) -> &'static str {
    match kind {
        FileKind::Modified => "warning",
        FileKind::Added => "success",
        FileKind::Deleted => "danger",
        FileKind::Renamed(_) => "text-secondary",
    }
}

fn files_word(n: usize) -> &'static str {
    if n == 1 { "file" } else { "files" }
}

impl Workspace {
    /// Shows the thread's changes in place of the body.
    pub(super) fn open_review(
        &mut self,
        thread: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.nav.thread != Some(thread) {
            self.open_thread(thread, window, cx);
        }
        let draft = self.current_draft(thread);
        let text = match self.commit_messages.remove(&thread) {
            Some(saved) if saved.text != saved.draft && !saved.text.is_empty() => saved.text,
            _ => draft.clone(),
        };
        // Set before the field is first drawn, so it shows.
        let message = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(6)
                .placeholder("Say what changed and why.")
                .default_value(text)
        });
        let events = cx.subscribe(&message, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let folder = folder_mode(self.state(), thread).map(|mode| FolderReview {
            mode,
            groups: Vec::new(),
            outside_git: Vec::new(),
            messages: Vec::new(),
        });
        self.review = Some(Review {
            thread,
            loaded: false,
            files: Vec::new(),
            split: self.diff_split,
            rows: Vec::new(),
            file_rows: Vec::new(),
            list: ListState::new(0, ListAlignment::Top, px(800.)),
            shown: HashSet::new(),
            comments: Vec::new(),
            left: None,
            message,
            draft,
            _message_events: events,
            busy: false,
            failed: None,
            current_file: None,
            reveal_file: None,
            checks: Vec::new(),
            synced: None,
            open_error: None,
            focus,
            folder,
            change_row: None,
        });
        self.sync_review_comments(cx);
        self.load_thread_diff(thread, cx);
        cx.notify();
    }

    /// Shows the thread's changes, scrolled to `file` once they are read.
    pub(super) fn open_review_at(
        &mut self,
        thread: ThreadId,
        file: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_review(thread, window, cx);
        if let Some(review) = self.review.as_mut() {
            review.reveal_file = Some(file);
        }
    }

    /// The file the review is on, by path.
    #[cfg(test)]
    pub(super) fn review_file(&self) -> Option<String> {
        self.review.as_ref().and_then(|r| r.current_file.clone())
    }

    /// Goes back to the thread.
    pub(super) fn close_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.drop_review(cx) {
            self.composer
                .update(cx, |composer, cx| composer.focus(window, cx));
            cx.notify();
        }
    }

    /// Closes the review, keeping its message for next time. Whether one
    /// was open.
    pub(super) fn drop_review(&mut self, cx: &App) -> bool {
        let Some(review) = self.review.take() else {
            return false;
        };
        let text = review.message.read(cx).value().to_string();
        for message in review.folder.iter().flat_map(|f| &f.messages) {
            self.repo_messages.insert(
                (review.thread, message.repo.clone()),
                SavedMessage {
                    text: message.input.read(cx).value().to_string(),
                    draft: review.draft.clone(),
                },
            );
        }
        self.commit_messages.insert(
            review.thread,
            SavedMessage {
                text,
                draft: review.draft,
            },
        );
        true
    }

    /// Makes a commit message for each repo that shows up in a folder
    /// review: the one you left there, else the draft.
    pub(super) fn ensure_repo_messages(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(review) = &self.review else {
            return;
        };
        let Some(folder) = &review.folder else {
            return;
        };
        let missing: Vec<String> = folder
            .groups
            .iter()
            .map(|g| g.repo.clone())
            .filter(|repo| !folder.messages.iter().any(|m| &m.repo == repo))
            .collect();
        if missing.is_empty() {
            return;
        }
        let (thread, draft) = (review.thread, review.draft.clone());
        let mut made = Vec::new();
        for repo in missing {
            let text = match self.repo_messages.remove(&(thread, repo.clone())) {
                Some(saved) if saved.text != saved.draft && !saved.text.is_empty() => saved.text,
                _ => draft.clone(),
            };
            let input = cx.new(|cx| {
                TextareaState::new(window, cx)
                    .rows(3)
                    .placeholder("Say what changed and why.")
                    .default_value(text)
            });
            let events = cx.subscribe(&input, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            });
            made.push(RepoMessage {
                repo,
                input,
                _events: events,
            });
        }
        if let Some(folder) = self.review.as_mut().and_then(|r| r.folder.as_mut()) {
            folder.messages.extend(made);
        }
    }

    /// The commit message drafted from the reply of the run that made the
    /// changes, else from the thread's latest agent reply.
    fn current_draft(&self, thread: ThreadId) -> String {
        let state = self.state();
        let run = state.editing_run(thread).map(|r| r.id);
        let replies = || {
            state
                .messages(thread)
                .iter()
                .rev()
                .filter(|m| matches!(m.author, Author::Agent(_)))
        };
        run.and_then(|run| replies().find(|m| m.run == Some(run)))
            .or_else(|| replies().next())
            .map(|m| draft_commit_message(&m.body))
            .unwrap_or_default()
    }

    /// Puts a newer draft in the message, unless you wrote your own.
    pub(super) fn sync_review_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self.review.as_ref().map(|r| r.thread) else {
            return;
        };
        let draft = self.current_draft(thread);
        let Some(review) = &mut self.review else {
            return;
        };
        if draft == review.draft {
            return;
        }
        let inputs = std::iter::once(review.message.clone()).chain(
            review
                .folder
                .iter()
                .flat_map(|f| f.messages.iter().map(|m| m.input.clone())),
        );
        for input in inputs {
            let text = input.read(cx).value().to_string();
            if text.is_empty() || text == review.draft {
                let new = draft.clone();
                input.update(cx, |input, cx| input.set_value(new, window, cx));
            }
        }
        review.draft = draft;
        cx.notify();
    }

    /// Reads the reviewers' comments afresh, and redraws the rows when
    /// they changed.
    pub(super) fn sync_review_comments(&mut self, cx: &mut Context<Self>) {
        let Some(thread) = self.review.as_ref().map(|r| r.thread) else {
            return;
        };
        let state = self.state();
        let key = (
            state.events(thread).len(),
            state.messages(thread).len(),
            state.thread_runs(thread).len(),
            state.live_runs(thread).len(),
        );
        if self.review.as_ref().is_some_and(|r| r.synced == Some(key)) {
            return;
        }
        let root = self
            .review_root(thread)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let replies = reviewer_replies(state, thread);
        let mut found = Vec::new();
        let mut left = None;
        for (i, reply) in replies.iter().enumerate() {
            let own = comments(state, std::slice::from_ref(reply), &root);
            if i + 1 == replies.len()
                && !own.is_empty()
                && let Some(agent) = state.agent(reply.0)
            {
                left = Some((agent.spec.name.clone(), own.len()));
            }
            found.extend(own);
        }
        let checks = self.review_checks(thread);
        let Some(review) = &mut self.review else {
            return;
        };
        review.synced = Some(key);
        review.left = left;
        let mut changed = false;
        if checks != review.checks {
            review.checks = checks;
            changed = true;
        }
        if found != review.comments {
            review.comments = found;
            rebuild_rows(review);
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    /// Each comment pinned under a line: the file, the line's new
    /// number and the card's header.
    #[cfg(test)]
    pub(super) fn pinned_comments(&self) -> Vec<(String, Option<u32>, String)> {
        let Some(review) = &self.review else {
            return Vec::new();
        };
        let mut under = None;
        let mut pinned = Vec::new();
        for row in &review.rows {
            match *row {
                Row::Line(f, h, l) => {
                    let file = &review.files[f];
                    under = Some((file.path.clone(), file.hunks[h].lines[l].new));
                }
                Row::Pair(f, h, left, right) => {
                    let file = &review.files[f];
                    let lines = &file.hunks[h].lines;
                    let line = right.map(|r| lines[r].new).or(left.map(|l| lines[l].old));
                    under = Some((file.path.clone(), line.flatten()));
                }
                Row::Comment(c) => {
                    if let Some((path, line)) = under.clone() {
                        pinned.push((path, line, comment_header(&review.comments[c])));
                    }
                }
                _ => under = None,
            }
        }
        pinned
    }

    /// The list's top row.
    #[cfg(test)]
    pub(super) fn review_top(&self) -> Option<usize> {
        self.review
            .as_ref()
            .map(|r| r.list.logical_scroll_top().item_ix)
    }

    /// Scrolls the list so row `ix` is at the top.
    #[cfg(test)]
    pub(super) fn scroll_review_to(&self, ix: usize) {
        if let Some(review) = &self.review {
            review.list.scroll_to(ListOffset {
                item_ix: ix,
                offset_in_item: ui_px(0.),
            });
        }
    }

    /// The headers of the comments listed outside the diff.
    #[cfg(test)]
    pub(super) fn outside_comments(&self) -> Vec<String> {
        let Some(review) = &self.review else {
            return Vec::new();
        };
        let mut rows = review.rows.iter();
        if rows.next() != Some(&Row::OutsideTitle) {
            return Vec::new();
        }
        rows.map_while(|row| match row {
            Row::Comment(c) => Some(comment_header(&review.comments[*c])),
            _ => None,
        })
        .collect()
    }

    /// The agent whose changes these are: the one of the latest run that
    /// may have changed files, else the latest to reply, else the latest
    /// to run.
    fn review_agent(&self, thread: ThreadId) -> Option<String> {
        let id = self.review_agent_id(thread)?;
        self.state().agent(id).map(|a| a.spec.name.clone())
    }

    fn review_agent_id(&self, thread: ThreadId) -> Option<AgentId> {
        let state = self.state();
        let edited = state.editing_run(thread).map(|r| r.agent);
        let replied = state
            .messages(thread)
            .iter()
            .rev()
            .find_map(|m| match m.author {
                Author::Agent(a) => Some(a),
                _ => None,
            });
        let ran = state.thread_runs(thread).last().map(|r| r.agent);
        edited.or(replied).or(ran)
    }

    /// The checks the titled agent ran. One that never ended, because
    /// its run is over, is left out.
    fn review_checks(&self, thread: ThreadId) -> Vec<Check> {
        let state = self.state();
        let Some(agent) = self.review_agent_id(thread) else {
            return Vec::new();
        };
        let runs: Vec<_> = state
            .thread_runs(thread)
            .iter()
            .filter(|r| r.agent == agent)
            .map(|r| r.id)
            .collect();
        let live: Vec<_> = state.live_runs(thread).iter().map(|r| r.id).collect();
        checks(state.events(thread), &runs, &live)
    }

    /// Goes back to the thread with the inspector's Terminal tab on the
    /// command that ran a check.
    fn open_check(&mut self, check: &Check, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self.review.as_ref().map(|r| r.thread) else {
            return;
        };
        self.close_review(window, cx);
        self.show_run_command(thread, check.run, check.call.clone(), window, cx);
    }

    /// Reads the open thread's uncommitted changes, for the inspector and
    /// the review. Only the latest read lands, and a failed read keeps
    /// what was shown.
    pub(super) fn load_thread_diff(&mut self, thread: ThreadId, cx: &mut Context<Self>) {
        self.diff_seq += 1;
        let seq = self.diff_seq;
        let answer = self.client.request(Request::ThreadDiff { thread });
        let background = cx.background_executor().clone();
        // Whether to parse the patch, and whether by repo.
        let parse_it = self
            .review
            .as_ref()
            .filter(|r| r.thread == thread)
            .map(|r| r.folder.is_some());
        self.inspector.diff_task = Some(cx.spawn(async move |this, cx| {
            let read = answer.await.and_then(|v| {
                serde_json::from_value::<ThreadChanges>(v).map_err(|e| e.to_string())
            });
            let read = match (read, parse_it) {
                (Ok(changes), Some(by_repo)) => {
                    let copy = changes.clone();
                    let parsed = background
                        .spawn(async move {
                            if by_repo {
                                let (files, groups) = group_files(&copy);
                                (files, Some(groups))
                            } else {
                                (parse(&copy.patch), None)
                            }
                        })
                        .await;
                    Ok((changes, Some(parsed)))
                }
                (Ok(changes), None) => Ok((changes, None)),
                (Err(e), _) => Err(e),
            };
            this.update(cx, |this, cx| {
                if this.diff_seq != seq || this.nav.thread != Some(thread) {
                    return;
                }
                let review = this.review.as_mut().filter(|r| r.thread == thread);
                match read {
                    Ok((changes, parsed)) => {
                        if let (Some(review), Some((files, groups))) = (review, parsed) {
                            let kept = keep_scroll(review);
                            review.loaded = true;
                            review.failed = None;
                            review.files = files;
                            if let (Some(folder), Some(groups)) = (review.folder.as_mut(), groups) {
                                folder.groups = groups;
                                folder.outside_git = changes.outside_git.clone();
                            }
                            review
                                .shown
                                .retain(|p| review.files.iter().any(|f| &f.path == p));
                            if review
                                .current_file
                                .as_ref()
                                .is_some_and(|p| !review.files.iter().any(|f| &f.path == p))
                            {
                                review.current_file = None;
                            }
                            rebuild_rows_from(review, kept);
                            if let Some(path) = review.reveal_file.take()
                                && let Some(ix) = review.files.iter().position(|f| f.path == path)
                                && let Some(&row) = review.file_rows.get(ix)
                            {
                                review.current_file = Some(path);
                                review.list.scroll_to(ListOffset {
                                    item_ix: row,
                                    offset_in_item: ui_px(0.),
                                });
                            }
                        }
                        this.inspector.diff = Some(changes);
                    }
                    Err(e) => {
                        if let Some(review) = review {
                            review.failed = Some(e);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Reads the review's changes afresh.
    fn load_review(&mut self, cx: &mut Context<Self>) {
        if let Some(thread) = self.review.as_ref().map(|r| r.thread) {
            self.load_thread_diff(thread, cx);
        }
    }

    /// Shows the lines side by side or one after the other, for the rest of
    /// the session.
    fn set_split(&mut self, split: bool, cx: &mut Context<Self>) {
        self.diff_split = split;
        if let Some(review) = &mut self.review
            && review.split != split
        {
            review.split = split;
            rebuild_rows(review);
        }
        cx.notify();
    }

    fn show_large_file(&mut self, file: usize, cx: &mut Context<Self>) {
        if let Some(review) = &mut self.review
            && let Some(f) = review.files.get(file)
        {
            review.shown.insert(f.path.clone());
            rebuild_rows(review);
            cx.notify();
        }
    }

    fn scroll_to_file(&mut self, file: usize, cx: &mut Context<Self>) {
        if let Some(review) = &mut self.review
            && let Some(&row) = review.file_rows.get(file)
        {
            review.current_file = review.files.get(file).map(|f| f.path.clone());
            review.list.scroll_to(ListOffset {
                item_ix: row,
                offset_in_item: ui_px(0.),
            });
            cx.notify();
        }
    }

    fn set_viewed(&mut self, file: usize, viewed: bool, cx: &mut Context<Self>) {
        let Some(review) = &self.review else {
            return;
        };
        let Some(path) = review.files.get(file).map(|f| f.path.clone()) else {
            return;
        };
        let set = self.viewed.entry(review.thread).or_default();
        if viewed {
            set.insert(path);
        } else {
            set.remove(&path);
        }
        cx.notify();
    }

    /// Files you marked as viewed in `thread`, sorted.
    #[cfg(test)]
    pub(super) fn viewed_files(&self, thread: ThreadId) -> Vec<String> {
        let mut files: Vec<String> = self
            .viewed
            .get(&thread)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        files.sort();
        files
    }

    /// How many files the open thread's working copy changes, once known.
    #[cfg(test)]
    pub(super) fn thread_change_count(&self) -> Option<usize> {
        self.inspector.diff.as_ref().map(|d| d.files.len())
    }

    /// The commit message, while the review is open.
    #[cfg(test)]
    pub(super) fn review_message(&self, cx: &App) -> Option<String> {
        self.review
            .as_ref()
            .map(|r| r.message.read(cx).value().to_string())
    }

    /// The commit message of a folder review's group `ix`.
    #[cfg(test)]
    pub(super) fn review_repo_message(&self, ix: usize, cx: &App) -> Option<String> {
        let folder = self.review.as_ref()?.folder.as_ref()?;
        let repo = &folder.groups.get(ix)?.repo;
        folder
            .messages
            .iter()
            .find(|m| &m.repo == repo)
            .map(|m| m.input.read(cx).value().to_string())
    }

    /// Whether Commit can be pressed: there are changes, a message, and
    /// nothing else is being sent.
    pub(super) fn review_can_commit(&self, cx: &App) -> bool {
        self.review.as_ref().is_some_and(|r| {
            !r.busy && !r.files.is_empty() && !r.message.read(cx).value().trim().is_empty()
        })
    }

    /// Whether the open thread has a worktree, or works in place, with
    /// changes to review.
    pub(super) fn can_review(&self, thread: ThreadId) -> bool {
        let state = self.state();
        let has_worktree = state.thread(thread).is_some_and(|t| t.worktree.is_some())
            || folder_mode(state, thread) == Some(FolderMode::InPlace);
        has_worktree
            && self.nav.thread == Some(thread)
            && self
                .inspector
                .diff
                .as_ref()
                .is_some_and(|d| !d.files.is_empty() || !d.outside_git.is_empty())
    }

    /// Files viewed and files to review, for the footer.
    pub(super) fn review_viewed(&self) -> Option<(usize, usize)> {
        let review = self.review.as_ref()?;
        let viewed = self.viewed.get(&review.thread);
        let seen = review
            .files
            .iter()
            .filter(|f| viewed.is_some_and(|v| v.contains(&f.path)))
            .count();
        Some((seen, review.files.len()))
    }

    /// j and k: the next or previous hunk at the top of the list.
    fn step_change(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(review) = &mut self.review else {
            return;
        };
        let top = review.list.logical_scroll_top().item_ix;
        // Near the end the list can't scroll a hunk to the top, so the
        // last one gone to counts unless you scrolled past it.
        let from = match review.change_row {
            Some(row) if row >= top => Some(row),
            _ => None,
        };
        let next = match from {
            Some(row) => next_hunk(&review.rows, row, forward),
            None if forward => {
                (top..review.rows.len()).find(|&ix| matches!(review.rows[ix], Row::Hunk(..)))
            }
            None => next_hunk(&review.rows, top, false),
        };
        if let Some(ix) = next {
            review.change_row = Some(ix);
            review.list.scroll_to(ListOffset {
                item_ix: ix,
                offset_in_item: ui_px(0.),
            });
            cx.notify();
        }
    }

    /// The hunk header row j or k last went to.
    #[cfg(test)]
    pub(super) fn review_change(&self) -> Option<usize> {
        self.review.as_ref().and_then(|r| r.change_row)
    }

    /// The repos a folder review's discard takes: those with files the
    /// thread changed.
    fn discard_repos(&self) -> Vec<String> {
        let Some(review) = &self.review else {
            return Vec::new();
        };
        let Some(folder) = &review.folder else {
            return Vec::new();
        };
        folder
            .groups
            .iter()
            .filter(|g| g.own(&review.files) > 0)
            .map(|g| g.repo.clone())
            .collect()
    }

    /// How a repo is named: its folder, or the project's for the folder
    /// itself.
    fn repo_name(&self, thread: ThreadId, repo: &str) -> String {
        if !repo.is_empty() {
            return repo.to_string();
        }
        let state = self.state();
        state
            .thread_project(thread)
            .and_then(|p| state.project(p))
            .map(|p| p.name.clone())
            .unwrap_or_default()
    }

    /// Whether the commit of group `ix` can be pressed.
    fn repo_can_commit(&self, ix: usize, cx: &App) -> bool {
        let Some(review) = &self.review else {
            return false;
        };
        let Some(folder) = &review.folder else {
            return false;
        };
        let Some(group) = folder.groups.get(ix) else {
            return false;
        };
        let message = folder.messages.iter().find(|m| m.repo == group.repo);
        !review.busy
            && group.own(&review.files) > 0
            && message.is_some_and(|m| !m.input.read(cx).value().trim().is_empty())
    }

    /// Commits the thread's files of group `ix`'s repo.
    fn commit_repo(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.repo_can_commit(ix, cx) {
            return;
        }
        let Some(review) = &mut self.review else {
            return;
        };
        let Some(folder) = &review.folder else {
            return;
        };
        let group = folder.groups[ix].clone();
        let Some(input) = folder
            .messages
            .iter()
            .find(|m| m.repo == group.repo)
            .map(|m| m.input.clone())
        else {
            return;
        };
        review.busy = true;
        let thread = review.thread;
        let paths: Vec<String> = review.files[group.files.clone()]
            .iter()
            .map(|f| f.path.clone())
            .collect();
        let message = input.read(cx).value().to_string();
        let name = self.repo_name(thread, &group.repo);
        let branch = group.branch.clone().unwrap_or_default();
        let answer = self.client.request(Request::Commit {
            thread,
            repo: (!group.repo.is_empty()).then(|| group.repo.clone()),
            message,
        });
        self.spawn_request(window, cx, async move |this, cx| {
            let result = answer.await;
            this.update_in(cx, |this, window, cx| {
                if let Some(review) = &mut this.review {
                    review.busy = false;
                }
                if result.is_ok() {
                    window.push_notification(
                        Notification::success(format!("Committed to {branch} in {name}.")),
                        cx,
                    );
                    if let Some(viewed) = this.viewed.get_mut(&thread) {
                        viewed.retain(|p| !paths.contains(p));
                    }
                    input.update(cx, |input, cx| input.set_value("", window, cx));
                    this.load_review(cx);
                }
                cx.notify();
            })
            .ok();
            result.map(drop).map_err(|e| (e, None))
        });
        cx.notify();
    }

    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.review_can_commit(cx) {
            return;
        }
        let Some(review) = &mut self.review else {
            return;
        };
        review.busy = true;
        let thread = review.thread;
        let message = review.message.read(cx).value().to_string();
        let branch = self
            .state()
            .thread(thread)
            .and_then(|t| t.worktree.as_ref())
            .map(|wt| wt.branch.clone())
            .unwrap_or_default();
        let answer = self.client.request(Request::Commit {
            thread,
            repo: None,
            message,
        });
        self.spawn_request(window, cx, async move |this, cx| {
            let result = answer.await;
            this.update_in(cx, |this, window, cx| {
                if let Some(review) = &mut this.review {
                    review.busy = false;
                }
                if result.is_ok() {
                    window.push_notification(
                        Notification::success(format!("Committed to {branch}.")),
                        cx,
                    );
                    this.forget_review_marks(thread, window, cx);
                    this.load_review(cx);
                }
                cx.notify();
            })
            .ok();
            result.map(drop).map_err(|e| (e, None))
        });
        cx.notify();
    }

    fn ask_to_discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(review) = &self.review else {
            return;
        };
        let whose = self
            .review_agent(review.thread)
            .map_or_else(|| "these".to_string(), |a| format!("{a}'s"));
        let thread = review.thread;
        let ask = match review.folder.as_ref().map(|f| f.mode) {
            None => format!(
                "Discard {whose} changes? The worktree is removed. A branch with commits is kept."
            ),
            Some(mode) => {
                let repos = self.discard_repos();
                if repos.is_empty() {
                    return;
                }
                let names: Vec<String> = repos.iter().map(|r| self.repo_name(thread, r)).collect();
                let names = and_list(&names);
                match mode {
                    FolderMode::InPlace => format!(
                        "Discard {whose} changes in {names}? Files this thread changed go back to what git has. Files not in git stay as they are."
                    ),
                    FolderMode::OwnCopy if repos.len() == 1 => format!(
                        "Discard {whose} changes in {names}? Its worktree is removed. A branch with commits is kept."
                    ),
                    FolderMode::OwnCopy => format!(
                        "Discard {whose} changes in {names}? Their worktrees are removed. A branch with commits is kept."
                    ),
                }
            }
        };
        let this = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm = this.clone();
            dialog
                .w(ui_px(420.))
                .title("Discard changes")
                .child(
                    div()
                        .id("discard-ask")
                        .text_color(color("text-secondary"))
                        .aria_label(ask.clone())
                        .child(ask.clone())
                        .test_support(),
                )
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("discard-cancel")
                                .ghost()
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("discard-confirm")
                                .danger()
                                .label("Discard")
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    confirm.update(cx, |this, cx| this.discard(window, cx)).ok();
                                }),
                        ),
                )
        });
    }

    fn discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(review) = &mut self.review else {
            return;
        };
        let folder = review.folder.is_some();
        review.busy = true;
        let thread = review.thread;
        let repos = if folder {
            let repos = self.discard_repos();
            // The project folder as the one repo is all of them.
            (!repos.iter().any(String::is_empty)).then_some(repos)
        } else {
            None
        };
        let answer = self.client.request(Request::Discard {
            thread,
            repos,
            delete_branch: false,
        });
        self.spawn_request(window, cx, async move |this, cx| {
            let result = answer.await;
            this.update_in(cx, |this, window, cx| {
                if let Some(review) = &mut this.review {
                    review.busy = false;
                }
                if result.is_ok() {
                    this.forget_review_marks(thread, window, cx);
                }
                this.load_review(cx);
                cx.notify();
            })
            .ok();
            result.map(drop).map_err(|e| (e, None))
        });
        cx.notify();
    }

    /// Back to the thread with `@Agent ` in front of its draft, ready to
    /// type.
    fn ask_for_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(review) = &self.review else {
            return;
        };
        let Some(agent) = self.review_agent(review.thread) else {
            return;
        };
        let mention = format!("@{agent} ");
        self.close_review(window, cx);
        self.composer.update(cx, |composer, cx| {
            let draft = composer.text(cx);
            let text = if draft.starts_with(&mention) {
                draft
            } else {
                format!("{mention}{draft}")
            };
            composer.set_text(&text, window, cx);
            composer.focus(window, cx);
        });
    }

    /// After a commit or discard: Viewed marks go, and the message
    /// empties so the next reply can draft a new one.
    fn forget_review_marks(
        &mut self,
        thread: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.viewed.remove(&thread);
        self.commit_messages.remove(&thread);
        if let Some(review) = self.review.as_mut().filter(|r| r.thread == thread) {
            review
                .message
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
    }

    /// Opens the worktree in your editor, or in Finder. With a file
    /// picked in the list, an editor gets that file too. Failing says so
    /// under the title of the diff.
    fn open_in_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self.review.as_ref().map(|r| r.thread) else {
            return;
        };
        let Some(worktree) = self.review_root(thread) else {
            return;
        };
        let app = self.editor_app();
        let Some(review) = self.review.as_mut() else {
            return;
        };
        review.open_error = None;
        let file = review
            .current_file
            .as_ref()
            .and_then(|path| review.files.iter().find(|f| &f.path == path))
            .filter(|f| !matches!(f.kind, FileKind::Deleted))
            .map(|f| worktree.join(&f.path));
        let args = crate::editor::open_args(app.as_deref(), &worktree, file.as_deref());
        let opener = self.opener.clone();
        let opened = cx.background_executor().spawn(async move { opener(args) });
        self.spawn_request(window, cx, async move |this, cx| {
            if let Err(e) = opened.await {
                eprintln!("crew: open failed: {e}");
                this.update_in(cx, |this, _, cx| {
                    if let Some(review) = this.review.as_mut() {
                        review.open_error = Some(crate::editor::open_failed(app.as_deref()));
                        cx.notify();
                    }
                })
                .ok();
            }
            Ok(())
        });
    }

    /// The folder the review's paths are relative to: the thread's
    /// worktree, or the project folder for a thread working in place.
    fn review_root(&self, thread: ThreadId) -> Option<std::path::PathBuf> {
        let state = self.state();
        if let Some(path) = state
            .thread(thread)
            .and_then(|t| t.worktree.as_ref())
            .map(|wt| wt.path.clone())
        {
            return Some(path);
        }
        (folder_mode(state, thread) == Some(FolderMode::InPlace))
            .then(|| state.thread_project(thread))
            .flatten()
            .and_then(|p| state.project(p))
            .map(|p| p.root.clone())
    }

    fn render_review_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(review) = &self.review else {
            return div().into_any_element();
        };
        let Some(&row) = review.rows.get(ix) else {
            return div().into_any_element();
        };
        let viewed = self.viewed.get(&review.thread);
        match row {
            Row::File(f) => {
                let file = &review.files[f];
                let checked = viewed.is_some_and(|v| v.contains(&file.path));
                let group = review
                    .folder
                    .as_ref()
                    .and_then(|d| d.groups.iter().find(|g| g.files.contains(&f)));
                let outside = group.is_some_and(|g| g.outside.contains(&file.path));
                // A folder review names the repo on a chip, then the path in it.
                let repo = group.filter(|g| !g.repo.is_empty()).map(|g| g.repo.clone());
                let shown_path = match group {
                    Some(g) => in_repo(&g.repo, &file.path).to_string(),
                    None => file.path.clone(),
                };
                div()
                    .id(("diff-file", f))
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(ui_px(8.))
                    .h(scale::row(40.))
                    .px(ui_px(16.))
                    .when(f > 0, |row| row.mt(ui_px(16.)))
                    .border_t_1()
                    .border_b_1()
                    .border_color(color("border-100"))
                    .bg(color("bg-100"))
                    .font_family(code_family())
                    .text_size(scale::mono(12.))
                    .aria_label(file.path.clone())
                    .when_some(repo, |row, repo| {
                        row.child(mode_chip(
                            ("diff-file-repo", f),
                            repo,
                            "text-secondary",
                            "surface-300",
                        ))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(color("text-primary"))
                            .child(shown_path),
                    )
                    .when(outside, |row| {
                        row.child(
                            div()
                                .flex_none()
                                .font_family(ui_family())
                                .text_size(scale::text(12.))
                                .text_color(color("text-tertiary"))
                                .child(OUTSIDE_THREAD),
                        )
                    })
                    .child(plus_minus(file.added, file.removed))
                    .child(
                        Checkbox::new(("review-viewed", f))
                            .small()
                            .label("Viewed")
                            .checked(checked)
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                this.set_viewed(f, *checked, cx);
                            })),
                    )
                    .test_support()
                    .into_any_element()
            }
            Row::Hunk(f, h) => {
                let header = review.files[f].hunks[h].header.clone();
                div()
                    .id(("diff-row", ix))
                    .w_full()
                    .flex()
                    .items_center()
                    .h(scale::row(28.))
                    .pl(scale::mono(GUTTER * 2. + 16.))
                    .bg(color("surface-100"))
                    .border_b_1()
                    .border_color(color("border-100"))
                    .font_family(code_family())
                    .text_size(scale::mono(12.))
                    .text_color(color("text-tertiary"))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .aria_label(header.clone())
                    .child(header)
                    .test_support()
                    .into_any_element()
            }
            Row::Line(f, h, l) => {
                let line = &review.files[f].hunks[h].lines[l];
                let (sign, tint) = match line.kind {
                    LineKind::Added => ("+", Some("success-subtle")),
                    LineKind::Removed => ("−", Some("danger-subtle")),
                    LineKind::Context => (" ", None),
                    LineKind::NoNewline => ("", None),
                };
                let number = |n: Option<u32>| n.map_or_else(|| "·".to_string(), |n| n.to_string());
                let text = line.text.replace('\t', "    ");
                let label = if line.kind == LineKind::NoNewline {
                    text.clone()
                } else {
                    format!("{} {} {sign} {text}", number(line.old), number(line.new))
                };
                let gutter = |n: Option<u32>| {
                    div()
                        .flex_none()
                        .w(scale::mono(GUTTER))
                        .pr(ui_px(8.))
                        .flex()
                        .justify_end()
                        .text_color(color("text-gutter"))
                        .children(n.map(|n| n.to_string()))
                };
                div()
                    .id(("diff-row", ix))
                    .w_full()
                    .flex()
                    .h(scale::mono_line(LINE_HEIGHT))
                    .items_center()
                    .when_some(tint, |row, tint| row.bg(color(tint)))
                    .font_family(code_family())
                    .text_size(scale::mono(12.))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .aria_label(label)
                    .child(gutter(line.old))
                    .child(gutter(line.new))
                    .child(
                        div()
                            .flex_none()
                            .w(scale::mono(16.))
                            .text_color(color(match line.kind {
                                LineKind::Added => "success",
                                LineKind::Removed => "danger",
                                _ => "text-tertiary",
                            }))
                            .child(sign),
                    )
                    .child(
                        div()
                            .text_color(color(if line.kind == LineKind::NoNewline {
                                "text-tertiary"
                            } else {
                                "text-primary"
                            }))
                            .child(text),
                    )
                    .test_support()
                    .into_any_element()
            }
            Row::Pair(f, h, left, right) => {
                let lines = &review.files[f].hunks[h].lines;
                let (left, right) = (left.map(|i| &lines[i]), right.map(|i| &lines[i]));
                let label = [side_label(left, true), side_label(right, false)].join(" | ");
                div()
                    .id(("diff-row", ix))
                    .w_full()
                    .flex()
                    .h(scale::mono_line(LINE_HEIGHT))
                    .items_center()
                    .font_family(code_family())
                    .text_size(scale::mono(12.))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .aria_label(label)
                    .child(split_side(("diff-left", ix), left, true))
                    .child(split_side(("diff-right", ix), right, false))
                    .test_support()
                    .into_any_element()
            }
            Row::Note(f) => {
                let note = review.files[f].note.clone().unwrap_or_default();
                div()
                    .id(("diff-row", ix))
                    .w_full()
                    .px(ui_px(16.))
                    .py(scale::row(10.))
                    .text_size(scale::text(12.))
                    .text_color(color("text-tertiary"))
                    .aria_label(note.clone())
                    .child(note)
                    .test_support()
                    .into_any_element()
            }
            Row::OutsideTitle => div()
                .id(("diff-row", ix))
                .w_full()
                .flex()
                .items_center()
                .h(scale::row(40.))
                .px(ui_px(16.))
                .text_size(scale::text(12.))
                .text_color(color("text-secondary"))
                .aria_label(OUTSIDE_TITLE)
                .child(OUTSIDE_TITLE)
                .test_support()
                .into_any_element(),
            Row::Comment(c) => {
                let comment = &review.comments[c];
                let header = comment_header(comment);
                let doc = crate::markdown::parse(&comment.body);
                // In split view the card spans both columns.
                let gutters = if review.split { 1. } else { 2. };
                div()
                    .id(("diff-row", ix))
                    .w_full()
                    .py(scale::row(6.))
                    .pl(scale::mono(GUTTER * gutters + 16.))
                    .pr(ui_px(16.))
                    .child(
                        div()
                            .id(("review-comment", c))
                            .flex()
                            .flex_col()
                            .gap(ui_px(4.))
                            .max_w(ui_px(720.))
                            .px(ui_px(12.))
                            .py(scale::row(10.))
                            .bg(color("surface-raised"))
                            .border_1()
                            .border_color(color("border-200"))
                            .rounded(ui_px(8.))
                            .aria_label(header.clone())
                            .child(
                                div()
                                    .text_size(scale::text(12.))
                                    .font_weight(weight(FontWeight::MEDIUM))
                                    .text_color(color("text-secondary"))
                                    .child(header),
                            )
                            .child(
                                div()
                                    .id(("review-comment-body", c))
                                    .aria_label(comment.body.clone())
                                    .child(render_doc(&doc, &[], &format!("review-comment-{c}")))
                                    .test_support(),
                            )
                            .test_support(),
                    )
                    .test_support()
                    .into_any_element()
            }
            Row::Folded(f) => {
                let file = &review.files[f];
                let note = format!("{} changes {} lines.", file.path, file.line_count());
                div()
                    .id(("diff-row", ix))
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(ui_px(12.))
                    .px(ui_px(16.))
                    .py(scale::row(10.))
                    .text_size(scale::text(12.))
                    .text_color(color("text-tertiary"))
                    .child(
                        div()
                            .id(("diff-folded", f))
                            .aria_label(note.clone())
                            .child(note)
                            .test_support(),
                    )
                    .child(
                        Button::new(("diff-show", f))
                            .outline()
                            .small()
                            .label("Show diff")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.show_large_file(f, cx);
                            })),
                    )
                    .test_support()
                    .into_any_element()
            }
        }
    }

    /// The title bar's content while reviewing.
    pub(super) fn render_review_title(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<(AnyElement, AnyElement)> {
        let review = self.review.as_ref()?;
        let state = self.state();
        let thread = state.thread(review.thread);
        let title = thread.map(|t| t.title.clone()).unwrap_or_default();
        let channel = thread
            .and_then(|t| state.channel(t.channel))
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let heading = self
            .review_agent(review.thread)
            .map_or_else(|| "Changes".to_string(), |a| format!("{a}'s changes"));
        let mode = review.folder.as_ref().map(|f| f.mode);
        let where_ = match mode {
            None => format!("{title} · #{channel}"),
            Some(_) => {
                let project = thread
                    .and_then(|t| state.channel(t.channel))
                    .and_then(|c| state.project(c.project))
                    .map(|p| p.name.clone())
                    .unwrap_or_default();
                format!("{title} · {project} · #{channel}")
            }
        };
        let left = div()
            .flex()
            .items_center()
            .gap(ui_px(12.))
            .min_w_0()
            .child(
                Button::new("review-back")
                    .ghost()
                    .small()
                    .label("‹ Back to thread")
                    .on_click(cx.listener(|this, _, window, cx| this.close_review(window, cx))),
            )
            .child(
                div()
                    .id("review-title")
                    .flex_none()
                    .font_weight(weight(FontWeight::SEMIBOLD))
                    .aria_label(heading.clone())
                    .child(heading)
                    .test_support(),
            )
            .child(
                div()
                    .id("review-where")
                    .min_w_0()
                    .truncate()
                    .text_size(scale::text(12.))
                    .text_color(color("text-tertiary"))
                    .aria_label(where_.clone())
                    .child(where_)
                    .test_support(),
            )
            .when(mode == Some(FolderMode::InPlace), |row| {
                row.child(in_place_chip("review-mode-chip"))
            })
            .into_any_element();
        let right = div()
            .flex()
            .items_center()
            .gap(ui_px(8.))
            .child(
                Button::new("review-refresh")
                    .ghost()
                    .small()
                    .icon(IconName::RefreshCw)
                    .tooltip("Read the changes again")
                    .accessibility_label("Refresh")
                    .on_click(cx.listener(|this, _, _, cx| this.load_review(cx))),
            )
            .child(
                div()
                    .flex()
                    .p(ui_px(2.))
                    .gap(ui_px(2.))
                    .border_1()
                    .border_color(color("border-200"))
                    .rounded(ui_px(6.))
                    .child(
                        Button::new("review-unified")
                            .ghost()
                            .xsmall()
                            .selected(!review.split)
                            .label("Unified")
                            .on_click(cx.listener(|this, _, _, cx| this.set_split(false, cx))),
                    )
                    .child(
                        Button::new("review-split")
                            .ghost()
                            .xsmall()
                            .selected(review.split)
                            .label("Split")
                            .on_click(cx.listener(|this, _, _, cx| this.set_split(true, cx))),
                    ),
            )
            .child(
                Button::new("review-open-editor")
                    .outline()
                    .small()
                    .label("Open in editor")
                    .on_click(cx.listener(|this, _, window, cx| this.open_in_editor(window, cx))),
            )
            .children(self.render_title_actions(cx))
            .into_any_element();
        Some((left, right))
    }

    /// The review in place of the body row.
    pub(super) fn render_review(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(review) = &self.review else {
            return div().into_any_element();
        };
        div()
            .id("review")
            .key_context(REVIEW_CONTEXT)
            .track_focus(&review.focus)
            .on_action(cx.listener(|this, _: &CloseReview, window, cx| {
                this.close_review(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NextChange, _, cx| this.step_change(true, cx)))
            .on_action(cx.listener(|this, _: &PreviousChange, _, cx| {
                this.step_change(false, cx);
            }))
            .flex()
            .flex_1()
            .min_h_0()
            .child(self.render_review_files(cx))
            .child(self.render_review_diff(cx))
            .child(self.render_what_next(cx))
            .test_support()
            .into_any_element()
    }

    fn render_review_files(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let review = self.review.as_ref();
        let files: Vec<(String, FileKind, u32, u32)> = review
            .map(|r| {
                r.files
                    .iter()
                    .map(|f| (f.path.clone(), f.kind.clone(), f.added, f.removed))
                    .collect()
            })
            .unwrap_or_default();
        let viewed = review
            .and_then(|r| self.viewed.get(&r.thread))
            .cloned()
            .unwrap_or_default();
        let added: u32 = files.iter().map(|f| f.2).sum();
        let removed: u32 = files.iter().map(|f| f.3).sum();
        let n = files.len();
        let summary = format!("{n} {} changed · +{added} −{removed}", files_word(n));
        let ran = review
            .map(|r| (r.thread, r.checks.clone()))
            .filter(|(_, c)| !c.is_empty());
        let ran = ran.and_then(|(thread, list)| Some((self.review_agent(thread)?, list)));
        let by_repo = review.is_some_and(|r| r.folder.is_some());
        let repo_rows = if by_repo {
            self.render_folder_files(cx)
        } else {
            Vec::new()
        };
        div()
            .id("review-files")
            .flex()
            .flex_col()
            .flex_none()
            .w(self.sidebar_width())
            .h_full()
            .border_r_1()
            .border_color(color("border-100"))
            .overflow_y_scroll()
            .children(repo_rows)
            .when(!by_repo, |col| {
                col.child(
                    div()
                        .id("review-summary")
                        .flex()
                        .items_center()
                        .gap(ui_px(8.))
                        .h(scale::row(40.))
                        .px(ui_px(16.))
                        .text_size(scale::text(12.))
                        .aria_label(summary)
                        .child(
                            div()
                                .flex_1()
                                .text_color(color("text-secondary"))
                                .child(format!("{n} {} changed", files_word(n))),
                        )
                        .child(plus_minus(added, removed))
                        .test_support(),
                )
            })
            .when(!by_repo, |col| {
                col.children(files.into_iter().enumerate().map(
                    |(ix, (path, kind, added, removed))| {
                        let seen = viewed.contains(&path);
                        div()
                            .id(("review-file", ix))
                            .flex()
                            .items_center()
                            .gap(ui_px(8.))
                            .h(scale::row(28.))
                            .px(ui_px(16.))
                            .cursor_pointer()
                            .hover(|s| s.bg(color("surface-100")))
                            .font_family(code_family())
                            .text_size(scale::mono(12.))
                            .aria_label(format!("{} {path} +{added} −{removed}", kind.letter()))
                            .child(
                                div()
                                    .flex_none()
                                    .w(scale::mono(10.))
                                    .text_size(scale::mono(10.))
                                    .font_weight(weight(FontWeight::MEDIUM))
                                    .text_color(color(letter_colour(&kind)))
                                    .child(kind.letter().to_string()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(color(if seen {
                                        "text-tertiary"
                                    } else {
                                        "text-primary"
                                    }))
                                    .child(path),
                            )
                            .when(seen, |row| row.child(dot("text-tertiary")))
                            .child(plus_minus(added, removed))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.scroll_to_file(ix, cx);
                            }))
                            .test_support()
                    },
                ))
            })
            .when_some(ran, |col, (agent, list)| {
                col.child(
                    div()
                        .id("review-checks")
                        .flex()
                        .flex_col()
                        .mt(ui_px(8.))
                        .child(
                            div()
                                .id("review-checks-title")
                                .aria_label(format!("Checks {agent} ran"))
                                .test_support()
                                .h(scale::row(32.))
                                .px(ui_px(16.))
                                .flex()
                                .items_center()
                                .text_size(scale::text(12.))
                                .text_color(color("text-secondary"))
                                .child(format!("Checks {agent} ran")),
                        )
                        .children(list.into_iter().enumerate().map(|(ix, check)| {
                            let word = match check.state {
                                CheckState::Running => "Running",
                                CheckState::Passed => "Passed",
                                CheckState::Failed => "Failed",
                            };
                            let icon = match check.state {
                                CheckState::Running => {
                                    spinner(("review-check-spinner", ix), 12., "accent-blue")
                                        .into_any_element()
                                }
                                CheckState::Passed => Icon::new(IconName::Check)
                                    .size(ui_px(12.))
                                    .text_color(color("success"))
                                    .into_any_element(),
                                CheckState::Failed => Icon::new(IconName::CircleX)
                                    .size(ui_px(12.))
                                    .text_color(color("danger"))
                                    .into_any_element(),
                            };
                            div()
                                .id(("review-check", ix))
                                .flex()
                                .items_center()
                                .gap(ui_px(8.))
                                .h(scale::row(28.))
                                .px(ui_px(16.))
                                .cursor_pointer()
                                .hover(|s| s.bg(color("surface-100")))
                                .aria_label(format!("{word} {}", check.command))
                                .child(div().flex_none().child(icon))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .font_family(code_family())
                                        .text_size(scale::mono(12.))
                                        .text_color(color("text-primary"))
                                        .child(check.command.clone()),
                                )
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_check(&check, window, cx);
                                }))
                                .test_support()
                        })),
                )
            })
    }

    /// A folder review's file list: the count, each repo's files, then
    /// the files git doesn't have.
    fn render_folder_files(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(review) = &self.review else {
            return Vec::new();
        };
        let Some(folder) = &review.folder else {
            return Vec::new();
        };
        let viewed = self.viewed.get(&review.thread);
        let added: u32 = review.files.iter().map(|f| f.added).sum();
        let removed: u32 = review.files.iter().map(|f| f.removed).sum();
        let n = review.files.len() + folder.outside_git.len();
        let repos = folder.groups.len();
        let count = match repos {
            0 => files_count(n),
            1 => format!("{} in 1 repo", files_count(n)),
            _ => format!("{} in {repos} repos", files_count(n)),
        };
        let mut rows = vec![
            div()
                .id("review-summary")
                .flex()
                .items_center()
                .justify_between()
                .pt(scale::row(12.))
                .pb(scale::row(4.))
                .px(ui_px(16.))
                .aria_label(format!("{count} · +{added} −{removed}"))
                .child(
                    div()
                        .text_size(scale::text(12.))
                        .font_weight(weight(FontWeight::MEDIUM))
                        .text_color(color("text-secondary"))
                        .child(count),
                )
                .child(plus_minus(added, removed))
                .test_support()
                .into_any_element(),
        ];
        for (gi, group) in folder.groups.iter().enumerate() {
            let name = self.repo_name(review.thread, &group.repo);
            let files = files_count(group.files.len());
            rows.push(
                group_header(("review-repo", gi), BRANCH_ICON, "text-primary")
                    .aria_label(format!("{name} · {files}"))
                    .child(div().font_family(code_family()).child(name))
                    .child(
                        div()
                            .font_weight(weight(FontWeight::NORMAL))
                            .text_color(color("text-tertiary"))
                            .child(format!("· {files}")),
                    )
                    .test_support()
                    .into_any_element(),
            );
            let file_rows = group.files.clone().map(|ix| {
                let file = &review.files[ix];
                let path = in_repo(&group.repo, &file.path).to_string();
                let outside = group.outside.contains(&file.path);
                let seen = viewed.is_some_and(|v| v.contains(&file.path));
                let current = review.current_file.as_ref() == Some(&file.path);
                let mut label = format!(
                    "{} {path} +{} −{}",
                    file.kind.letter(),
                    file.added,
                    file.removed
                );
                if outside {
                    label.push_str(&format!(" · {OUTSIDE_THREAD}"));
                }
                let text = if current {
                    "text-primary"
                } else if seen || outside {
                    "text-tertiary"
                } else {
                    "text-secondary"
                };
                div()
                    .id(("review-file", ix))
                    .flex()
                    .items_center()
                    .gap(ui_px(8.))
                    .h(scale::row(28.))
                    .px(ui_px(8.))
                    .rounded(ui_px(6.))
                    .cursor_pointer()
                    .when(current, |row| row.bg(color("surface-300")))
                    .when(!current, |row| row.hover(|s| s.bg(color("surface-100"))))
                    .font_family(code_family())
                    .whitespace_nowrap()
                    .aria_label(label)
                    .child(
                        div()
                            .flex_none()
                            .w(scale::mono(12.))
                            .text_size(scale::mono(10.))
                            .font_weight(weight(FontWeight::MEDIUM))
                            .text_color(color(letter_colour(&file.kind)))
                            .child(file.kind.letter().to_string()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(scale::mono(12.))
                            .text_color(color(text))
                            .child(path),
                    )
                    .when(seen, |row| row.child(dot("text-tertiary")))
                    .child(short_plus_minus(file.added, file.removed))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.scroll_to_file(ix, cx);
                    }))
                    .test_support()
                    .into_any_element()
            });
            rows.push(group_rows(file_rows.collect()));
        }
        if !folder.outside_git.is_empty() {
            let files = files_count(folder.outside_git.len());
            rows.push(
                group_header("review-not-in-git", FILE_ICON, "text-secondary")
                    .aria_label(format!("Not in git · {files}"))
                    .child("Not in git")
                    .child(
                        div()
                            .font_weight(weight(FontWeight::NORMAL))
                            .text_color(color("text-tertiary"))
                            .child(format!("· {files}")),
                    )
                    .test_support()
                    .into_any_element(),
            );
            let mut outside: Vec<AnyElement> = folder
                .outside_git
                .iter()
                .enumerate()
                .map(|(ix, file)| {
                    let what = if file.exists { "edited" } else { "removed" };
                    div()
                        .id(("review-outside", ix))
                        .flex()
                        .items_center()
                        .gap(ui_px(8.))
                        .h(scale::row(28.))
                        .px(ui_px(8.))
                        .whitespace_nowrap()
                        .text_color(color("text-secondary"))
                        .aria_label(format!("{} {what}", file.path))
                        .child(
                            div()
                                .flex_none()
                                .w(scale::mono(12.))
                                .font_family(code_family())
                                .text_size(scale::mono(10.))
                                .font_weight(weight(FontWeight::MEDIUM))
                                .text_color(color("text-tertiary"))
                                .child("·"),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(code_family())
                                .text_size(scale::mono(12.))
                                .child(file.path.clone()),
                        )
                        .child(
                            div()
                                .text_size(scale::text(11.))
                                .text_color(color("text-tertiary"))
                                .child(what),
                        )
                        .test_support()
                        .into_any_element()
                })
                .collect();
            outside.push(
                div()
                    .id("review-outside-note")
                    .pt(scale::row(2.))
                    .pl(ui_px(28.))
                    .pr(ui_px(8.))
                    .text_size(scale::text(12.))
                    .line_height(scale::line(18.))
                    .text_color(color("text-tertiary"))
                    .aria_label(NOT_IN_GIT)
                    .child(NOT_IN_GIT)
                    .test_support()
                    .into_any_element(),
            );
            rows.push(group_rows(outside));
        }
        rows
    }

    fn render_review_diff(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let review = self.review.as_ref();
        let empty = match review {
            Some(r) if !r.loaded => Some(
                r.failed
                    .clone()
                    .unwrap_or_else(|| "Reading the changes…".to_string()),
            ),
            Some(r) if r.files.is_empty() => Some(NOTHING_TO_REVIEW.to_string()),
            Some(_) => None,
            None => Some(NOTHING_TO_REVIEW.to_string()),
        };
        let open_error = review.and_then(|r| r.open_error.clone());
        let body = match (empty, review) {
            (None, Some(review)) => div()
                .id("review-diff")
                .flex_1()
                .min_h_0()
                .child(
                    list(
                        review.list.clone(),
                        cx.processor(|this, ix, _, cx| this.render_review_row(ix, cx)),
                    )
                    .size_full(),
                )
                .test_support()
                .into_any_element(),
            (text, _) => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .id("review-empty")
                        .text_color(color("text-tertiary"))
                        .aria_label(text.clone().unwrap_or_default())
                        .child(text.unwrap_or_default())
                        .test_support(),
                )
                .into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .when_some(open_error, |col, error| {
                col.child(
                    div()
                        .px(ui_px(16.))
                        .py(scale::row(8.))
                        .child(super::settings::form_error("review-open-error", error)),
                )
            })
            .child(body)
    }

    fn render_what_next(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let Some(review) = &self.review else {
            return div().into_any_element();
        };
        if review.folder.is_some() {
            return self.render_folder_next(cx);
        }
        let state = self.state();
        let thread = state.thread(review.thread);
        let branch = thread
            .and_then(|t| t.worktree.as_ref())
            .map_or_else(|| "No worktree".to_string(), |wt| wt.branch.clone());
        let into = thread
            .and_then(|t| state.channel(t.channel))
            .and_then(|c| state.project(c.project))
            .map(|p| p.main_branch.clone())
            .unwrap_or_default();
        let has_worktree = thread.is_some_and(|t| t.worktree.is_some());
        let agent = self.review_agent(review.thread);
        let n = review.files.len();
        let can_commit = self.review_can_commit(cx);
        let drafted = !review.draft.is_empty()
            && review.message.read(cx).value().as_ref() == review.draft.as_str();
        let caption = match &agent {
            Some(agent) if drafted => format!("Commit message · drafted by {agent}"),
            _ => "Commit message".to_string(),
        };
        let field = |label: &'static str, id: &'static str, value: String| {
            div()
                .flex()
                .items_center()
                .h(scale::row(28.))
                .text_size(scale::text(12.))
                .child(
                    div()
                        .w(ui_px(64.))
                        .text_color(color("text-tertiary"))
                        .child(label),
                )
                .child(
                    div()
                        .id(id)
                        .min_w_0()
                        .truncate()
                        .font_family(code_family())
                        .text_size(scale::mono(12.))
                        .aria_label(value.clone())
                        .child(value)
                        .test_support(),
                )
        };
        div()
            .id("review-next")
            .flex()
            .flex_col()
            .flex_none()
            .gap(ui_px(12.))
            .w(scale::chrome(INSPECTOR_WIDTH))
            .h_full()
            .p(ui_px(16.))
            .border_l_1()
            .border_color(color("border-100"))
            .overflow_y_scroll()
            .child(
                div()
                    .text_size(scale::text(14.))
                    .font_weight(weight(FontWeight::SEMIBOLD))
                    .child("What next"),
            )
            .when_some(review.left.clone(), |col, (agent, n)| {
                let text = format!(
                    "{agent} left {n} {}.",
                    if n == 1 { "comment" } else { "comments" }
                );
                col.child(
                    div()
                        .id("review-callout")
                        .px(ui_px(12.))
                        .py(scale::row(10.))
                        .bg(color("surface-100"))
                        .border_1()
                        .border_color(color("border-200"))
                        .rounded(ui_px(8.))
                        .text_size(scale::text(13.))
                        .text_color(color("text-primary"))
                        .aria_label(text.clone())
                        .child(text)
                        .test_support(),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(ui_px(6.))
                    .child(
                        div()
                            .id("review-message-caption")
                            .text_size(scale::text(12.))
                            .text_color(color("text-secondary"))
                            .aria_label(caption.clone())
                            .child(caption)
                            .test_support(),
                    )
                    .child(
                        div()
                            .id("review-message")
                            .key_context(MESSAGE_CONTEXT)
                            .on_action(cx.listener(|this, _: &LeaveMessage, window, cx| {
                                if let Some(review) = &this.review {
                                    review.focus.focus(window, cx);
                                }
                            }))
                            .font_family(code_family())
                            .child(Textarea::new(&review.message).aria_label("Commit message"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                if let Some(review) = &this.review {
                                    review.message.update(cx, |m, cx| m.focus(window, cx));
                                }
                            }))
                            .test_support(),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(field("Branch", "review-branch", branch))
                    .child(field("Into", "review-into", into)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(ui_px(8.))
                    .child(
                        Button::new("review-commit")
                            .primary()
                            .w_full()
                            .label(format!("Commit {n} {}", files_word(n)))
                            .disabled(!can_commit)
                            .on_click(cx.listener(|this, _, window, cx| this.commit(window, cx))),
                    )
                    .child(
                        Button::new("review-ask")
                            .outline()
                            .w_full()
                            .label(agent.as_ref().map_or_else(
                                || "Ask for changes".to_string(),
                                |a| format!("Ask {a} for changes"),
                            ))
                            .disabled(agent.is_none())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.ask_for_changes(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("review-discard")
                            .ghost()
                            .w_full()
                            .text_color(color("danger"))
                            .label("Discard changes")
                            .disabled(!has_worktree || review.busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.ask_to_discard(window, cx)),
                            ),
                    ),
            )
            .child(
                div()
                    .text_size(scale::text(12.))
                    .line_height(scale::line(18.))
                    .text_color(color("text-tertiary"))
                    .child(NOTE),
            )
            .into_any_element()
    }
}

impl Workspace {
    /// What next for a folder project: a commit per repo, what git can't
    /// undo, then asking for changes or discarding them.
    fn render_folder_next(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(review) = &self.review else {
            return div().into_any_element();
        };
        let Some(folder) = &review.folder else {
            return div().into_any_element();
        };
        let thread = review.thread;
        let agent = self.review_agent(thread);
        let mut primary_used = false;
        let mut panels = Vec::new();
        for (gi, group) in folder.groups.iter().enumerate() {
            let own = group.own(&review.files);
            if own == 0 {
                continue;
            }
            let Some(message) = folder.messages.iter().find(|m| m.repo == group.repo) else {
                continue;
            };
            let name = self.repo_name(thread, &group.repo);
            let branch = group.branch.clone().unwrap_or_default();
            let files = files_count(own);
            let can = self.repo_can_commit(gi, cx);
            let primary = can && !primary_used;
            primary_used |= primary;
            let drafted = !review.draft.is_empty()
                && message.input.read(cx).value().as_ref() == review.draft.as_str();
            let caption = match &agent {
                Some(agent) if drafted => format!("Commit message · drafted by {agent}"),
                _ => "Commit message".to_string(),
            };
            let label = format!("Commit {files} in {name}");
            let button = Button::new(("review-commit-repo", gi))
                .w_full()
                .label(label)
                .disabled(!can)
                .on_click(cx.listener(move |this, _, window, cx| this.commit_repo(gi, window, cx)));
            let input = message.input.clone();
            panels.push(
                div()
                    .id(("review-repo-panel", gi))
                    .flex()
                    .flex_col()
                    .gap(ui_px(8.))
                    .p(ui_px(12.))
                    .border_1()
                    .border_color(color("border-100"))
                    .rounded(ui_px(8.))
                    .child(
                        div()
                            .id(("review-repo-head", gi))
                            .flex()
                            .items_center()
                            .gap(ui_px(8.))
                            .aria_label(format!("{name} on {branch} · {files}"))
                            .child(
                                div()
                                    .font_family(code_family())
                                    .font_weight(weight(FontWeight::MEDIUM))
                                    .child(name),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap(ui_px(4.))
                                    .min_w_0()
                                    .text_size(scale::text(12.))
                                    .text_color(color("text-tertiary"))
                                    .child("on")
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .font_family(code_family())
                                            .text_size(scale::mono(12.))
                                            .child(branch),
                                    )
                                    .child(format!("· {files}")),
                            )
                            .test_support(),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(ui_px(6.))
                            .child(
                                div()
                                    .id(("review-repo-caption", gi))
                                    .text_size(scale::text(12.))
                                    .font_weight(weight(FontWeight::MEDIUM))
                                    .text_color(color("text-secondary"))
                                    .aria_label(caption.clone())
                                    .child(caption)
                                    .test_support(),
                            )
                            .child(
                                div()
                                    .id(("review-repo-message", gi))
                                    .key_context(MESSAGE_CONTEXT)
                                    .on_action(cx.listener(|this, _: &LeaveMessage, window, cx| {
                                        if let Some(review) = &this.review {
                                            review.focus.focus(window, cx);
                                        }
                                    }))
                                    .font_family(code_family())
                                    .child(Textarea::new(&input).aria_label("Commit message"))
                                    .on_click(cx.listener(move |_, _, window, cx| {
                                        input.update(cx, |m, cx| m.focus(window, cx));
                                    }))
                                    .test_support(),
                            ),
                    )
                    .child(if primary {
                        button.primary()
                    } else {
                        button.outline()
                    })
                    .into_any_element(),
            );
        }
        let outside: Vec<String> = folder.outside_git.iter().map(|f| f.path.clone()).collect();
        let warning = match outside.as_slice() {
            [] => None,
            [one] => Some(format!(
                "{one} is not in git. It's already changed on disk and Crew can't undo it."
            )),
            many => Some(format!(
                "{} are not in git. They're already changed on disk and Crew can't undo them.",
                and_list(many)
            )),
        };
        let repos: Vec<String> = self
            .discard_repos()
            .iter()
            .map(|r| self.repo_name(thread, r))
            .collect();
        let discard = if repos.is_empty() {
            "Discard changes".to_string()
        } else {
            format!("Discard changes in {}", and_list(&repos))
        };
        let note = if folder.mode == FolderMode::InPlace {
            IN_PLACE_NOTE
        } else {
            NOTE
        };
        div()
            .id("review-next")
            .flex()
            .flex_col()
            .flex_none()
            .w(scale::chrome(INSPECTOR_WIDTH))
            .h_full()
            .border_l_1()
            .border_color(color("border-100"))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .h(scale::row(44.))
                    .px(ui_px(16.))
                    .border_b_1()
                    .border_color(color("border-100"))
                    .text_size(scale::text(14.))
                    .font_weight(weight(FontWeight::SEMIBOLD))
                    .child("What next"),
            )
            .child(
                div()
                    .id("review-next-body")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap(ui_px(16.))
                    .p(ui_px(16.))
                    .overflow_y_scroll()
                    .when_some(review.left.clone(), |col, (agent, n)| {
                        let text = format!(
                            "{agent} left {n} {}.",
                            if n == 1 { "comment" } else { "comments" }
                        );
                        col.child(
                            div()
                                .id("review-callout")
                                .px(ui_px(12.))
                                .py(scale::row(10.))
                                .bg(color("surface-100"))
                                .border_1()
                                .border_color(color("border-200"))
                                .rounded(ui_px(8.))
                                .text_size(scale::text(13.))
                                .text_color(color("text-primary"))
                                .aria_label(text.clone())
                                .child(text)
                                .test_support(),
                        )
                    })
                    .children(panels)
                    .when_some(warning, |col, text| {
                        col.child(warning_note("review-outside-warning", text))
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(ui_px(8.))
                            .child(
                                Button::new("review-ask")
                                    .outline()
                                    .w_full()
                                    .label(agent.as_ref().map_or_else(
                                        || "Ask for changes".to_string(),
                                        |a| format!("Ask {a} for changes"),
                                    ))
                                    .disabled(agent.is_none())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.ask_for_changes(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("review-discard")
                                    .ghost()
                                    .w_full()
                                    .text_color(color("danger"))
                                    .label(discard)
                                    .disabled(repos.is_empty() || review.busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.ask_to_discard(window, cx)
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .id("review-note")
                            .text_size(scale::text(12.))
                            .line_height(scale::line(18.))
                            .text_color(color("text-tertiary"))
                            .aria_label(note)
                            .child(note)
                            .test_support(),
                    ),
            )
            .into_any_element()
    }
}

/// Notes where the list is scrolled, before its rows or files change.
fn keep_scroll(review: &Review) -> Kept {
    let top = review.list.logical_scroll_top();
    let on_card = matches!(review.rows.get(top.item_ix), Some(Row::Comment(_)));
    Kept {
        anchor: anchor_at(&review.rows, &review.files, top.item_ix),
        offset: if on_card {
            ui_px(0.)
        } else {
            top.offset_in_item
        },
        ix: top.item_ix,
    }
}

/// Draws the rows again, keeping the list on the row it was on.
fn rebuild_rows(review: &mut Review) {
    let kept = keep_scroll(review);
    rebuild_rows_from(review, kept);
}

/// [`rebuild_rows`] for a change that already replaced the files.
fn rebuild_rows_from(review: &mut Review, kept: Kept) {
    let (rows, starts) = diff_rows(&review.files, &review.shown, &review.comments, review.split);
    if rows != review.rows {
        let top = kept
            .anchor
            .as_ref()
            .and_then(|a| find_anchor(&rows, &review.files, a))
            .or_else(|| kept.ix.min(rows.len().saturating_sub(1)).into());
        review.list.reset(rows.len());
        if let Some(item_ix) = top.filter(|&ix| ix > 0 || kept.offset > ui_px(0.)) {
            review.list.scroll_to(ListOffset {
                item_ix,
                offset_in_item: kept.offset,
            });
        }
    }
    if rows != review.rows {
        review.change_row = None;
    }
    review.rows = rows;
    review.file_rows = starts;
}

/// A split side's number, sign and text, as it is read back. Empty for
/// no line.
fn side_label(line: Option<&Line>, left: bool) -> String {
    let Some(line) = line else {
        return String::new();
    };
    let text = line.text.replace('\t', "    ");
    if line.kind == LineKind::NoNewline {
        return text;
    }
    let number = if left { line.old } else { line.new };
    let sign = match line.kind {
        LineKind::Added => "+",
        LineKind::Removed => "−",
        _ => " ",
    };
    format!(
        "{} {sign} {text}",
        number.map_or_else(|| "·".into(), |n| n.to_string())
    )
}

/// One column of a split row: its own number gutter, the sign and the
/// text. No line leaves it empty.
fn split_side(id: (&'static str, usize), line: Option<&Line>, left: bool) -> impl IntoElement {
    let (tint, sign, sign_colour) = match line.map(|l| l.kind) {
        Some(LineKind::Added) => (Some("success-subtle"), "+", "success"),
        Some(LineKind::Removed) => (Some("danger-subtle"), "−", "danger"),
        _ => (None, " ", "text-tertiary"),
    };
    let number = line.and_then(|l| if left { l.old } else { l.new });
    let no_newline = line.is_some_and(|l| l.kind == LineKind::NoNewline);
    let text = line.map(|l| l.text.replace('\t', "    "));
    div()
        .id(id)
        .flex_1()
        .min_w_0()
        .h_full()
        .flex()
        .items_center()
        .overflow_hidden()
        .when(!left, |side| {
            side.border_l_1().border_color(color("border-100"))
        })
        .when_some(tint, |side, tint| side.bg(color(tint)))
        .aria_label(side_label(line, left))
        .child(
            div()
                .flex_none()
                .w(scale::mono(GUTTER))
                .pr(ui_px(8.))
                .flex()
                .justify_end()
                .text_color(color("text-gutter"))
                .children(number.map(|n| n.to_string())),
        )
        .child(
            div()
                .flex_none()
                .w(ui_px(16.))
                .text_color(color(sign_colour))
                .child(if no_newline { "" } else { sign }),
        )
        .child(
            div()
                .min_w_0()
                .text_color(color(if no_newline {
                    "text-tertiary"
                } else {
                    "text-primary"
                }))
                .children(text),
        )
        .test_support()
}

/// "{Agent} · {path}:{line}".
fn comment_header(comment: &Comment) -> String {
    format!("{} · {}:{}", comment.agent, comment.path, comment.line)
}

/// A group's title in a folder review's file list.
fn group_header(id: impl Into<ElementId>, icon: &'static [u8], token: &str) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(ui_px(6.))
        .h(scale::row(28.))
        .px(ui_px(16.))
        .mt(ui_px(8.))
        .text_size(scale::text(12.))
        .font_weight(weight(FontWeight::MEDIUM))
        .text_color(color(token))
        .child(small_icon(icon, "text-secondary"))
}

/// The rows under a group's title.
fn group_rows(rows: Vec<AnyElement>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(ui_px(1.))
        .px(ui_px(8.))
        .children(rows)
        .into_any_element()
}

/// `+n −m` at 11 px, leaving out a side that is zero.
fn short_plus_minus(added: u32, removed: u32) -> impl IntoElement {
    div()
        .flex()
        .flex_none()
        .gap(ui_px(4.))
        .font_family(code_family())
        .text_size(scale::mono(11.))
        .when(added > 0, |row| {
            row.child(
                div()
                    .text_color(color("success"))
                    .child(format!("+{added}")),
            )
        })
        .when(removed > 0, |row| {
            row.child(
                div()
                    .text_color(color("danger"))
                    .child(format!("−{removed}")),
            )
        })
}

fn plus_minus(added: u32, removed: u32) -> impl IntoElement {
    div()
        .flex()
        .flex_none()
        .gap(ui_px(4.))
        .font_family(code_family())
        .text_size(scale::mono(12.))
        .child(
            div()
                .text_color(color("success"))
                .child(format!("+{added}")),
        )
        .child(
            div()
                .text_color(color("danger"))
                .child(format!("−{removed}")),
        )
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{
        Anchor, LARGE_FILE_LINES, Row, anchor_at, diff_rows, draft_commit_message, find_anchor,
    };
    use crate::comments::Comment;
    use crate::diff_parse::parse;

    #[test]
    fn a_reply_drafts_the_commit_message() {
        assert_eq!(draft_commit_message("Fixed."), "Fixed.");
        assert_eq!(
            draft_commit_message("\n  Fix the login  \n\n\nIt trims the email.\n\n"),
            "Fix the login\n\nIt trims the email."
        );
        let long = "x".repeat(100);
        assert_eq!(draft_commit_message(&long).chars().count(), 72);
        let many: String = (0..30).map(|i| format!("line {i}\n")).collect();
        let drafted = draft_commit_message(&format!("Subject\n{many}"));
        assert_eq!(drafted.lines().count(), 2 + 20);
        assert!(drafted.ends_with("line 19"));
        assert_eq!(draft_commit_message(""), "");
    }

    #[test]
    fn large_files_fold_until_shown() {
        let big: String = (0..=LARGE_FILE_LINES).map(|i| format!("+{i}\n")).collect();
        let patch = format!(
            "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-x\n+y\n\
             diff --git a/big b/big\nnew file mode 100644\n--- /dev/null\n+++ b/big\n\
             @@ -0,0 +1,{} @@\n{big}",
            LARGE_FILE_LINES + 1
        );
        let files = parse(&patch);
        let (rows, starts) = diff_rows(&files, &HashSet::new(), &[], false);
        assert_eq!(
            rows,
            [
                Row::File(0),
                Row::Hunk(0, 0),
                Row::Line(0, 0, 0),
                Row::Line(0, 0, 1),
                Row::File(1),
                Row::Folded(1),
            ]
        );
        assert_eq!(starts, [0, 4]);
        let shown = HashSet::from(["big".to_string()]);
        let (rows, _) = diff_rows(&files, &shown, &[], false);
        assert_eq!(rows.len(), 4 + 2 + LARGE_FILE_LINES + 1);
    }

    const MODIFIED: &str = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n\
        @@ -1,3 +1,3 @@\n one\n-two\n+deux\n three\n";
    const ADDED: &str = "diff --git a/n b/n\nnew file mode 100644\n--- /dev/null\n+++ b/n\n\
        @@ -0,0 +1,2 @@\n+x\n+y\n";
    const DELETED: &str = "diff --git a/d b/d\ndeleted file mode 100644\n--- a/d\n+++ /dev/null\n\
        @@ -1,2 +0,0 @@\n-x\n-y\n";

    fn comment(path: &str, line: u32) -> Comment {
        Comment {
            agent: "Scout".into(),
            path: path.into(),
            line,
            body: "note".into(),
        }
    }

    #[test]
    fn split_rows_pair_lines_and_keep_comments_under_their_pair() {
        let files = parse(MODIFIED);
        // Line 2 of the new file is `deux`, paired with `two`.
        let (rows, _) = diff_rows(&files, &HashSet::new(), &[comment("a.rs", 2)], true);
        assert_eq!(
            rows,
            [
                Row::File(0),
                Row::Hunk(0, 0),
                Row::Pair(0, 0, Some(0), Some(0)),
                Row::Pair(0, 0, Some(1), Some(2)),
                Row::Comment(0),
                Row::Pair(0, 0, Some(3), Some(3)),
            ]
        );
    }

    #[test]
    fn split_of_added_and_deleted_files_has_one_empty_side() {
        let files = parse(&format!("{ADDED}{DELETED}"));
        // Deleted files are commented by their old line numbers.
        let (rows, _) = diff_rows(&files, &HashSet::new(), &[comment("d", 2)], true);
        assert_eq!(
            rows,
            [
                Row::File(0),
                Row::Hunk(0, 0),
                Row::Pair(0, 0, None, Some(0)),
                Row::Pair(0, 0, None, Some(1)),
                Row::File(1),
                Row::Hunk(1, 0),
                Row::Pair(1, 0, Some(0), None),
                Row::Pair(1, 0, Some(1), None),
                Row::Comment(0),
            ]
        );
    }

    #[test]
    fn the_top_row_is_found_again_after_the_rows_change() {
        let files = parse(MODIFIED);
        let none = HashSet::new();
        let (unified, _) = diff_rows(&files, &none, &[], false);
        let (split, _) = diff_rows(&files, &none, &[], true);
        // `deux` is unified row 4 and the split row 3.
        let at = anchor_at(&unified, &files, 4).unwrap();
        assert_eq!(at, Anchor::Line("a.rs".into(), 0, 2));
        assert_eq!(find_anchor(&split, &files, &at), Some(3));
        let back = anchor_at(&split, &files, 3).unwrap();
        // The pair starts at the deletion, `two`.
        assert_eq!(find_anchor(&unified, &files, &back), Some(3));
        // A comment row stands for the line above it.
        let (with, _) = diff_rows(&files, &none, &[comment("a.rs", 2)], false);
        assert_eq!(with[5], Row::Comment(0));
        assert_eq!(anchor_at(&with, &files, 5), Some(at.clone()));
        assert_eq!(find_anchor(&with, &files, &at), Some(4));
        // A line that is gone falls back to its hunk, then its file.
        let gone = Anchor::Line("a.rs".into(), 0, 99);
        assert_eq!(find_anchor(&unified, &files, &gone), Some(1));
        assert_eq!(
            find_anchor(&unified, &files, &Anchor::Folded("a.rs".into())),
            Some(0)
        );
        assert_eq!(
            find_anchor(&unified, &files, &Anchor::File("other".into())),
            None
        );
    }
}
