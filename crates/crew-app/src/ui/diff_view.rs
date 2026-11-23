//! Review changes: an agent's uncommitted work in the thread's worktree
//! as a diff, and what to do with it: commit, ask for changes or discard.
//! It takes the place of the sidebar, the main area and the inspector.

use std::collections::HashSet;
use std::path::PathBuf;

use crew_core::ids::ThreadId;
use crew_core::store::Author;
use crew_core::worktree::Diff;
use crew_proto::Request;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{Disableable as _, IconName, Selectable as _, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::dot;
use super::inspector::INSPECTOR_WIDTH;
use super::workspace::{SIDEBAR_WIDTH, Workspace};
use crate::diff_parse::{FileDiff, FileKind, LineKind, parse};
use crate::fonts::mono_font;
use crate::theme::color;

actions!(
    review,
    [
        /// Leave the review for the thread.
        CloseReview,
        /// Leave the commit message for the review.
        LeaveMessage,
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
const NOTE: &str = "Nothing is pushed. Committing stays on your machine, and the branch is yours to merge or open a PR from.";

/// Esc leaves the commit message first, then the review.
pub fn bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("escape", CloseReview, Some(REVIEW_CONTEXT)),
        KeyBinding::new("escape", LeaveMessage, Some("ReviewMessage > Input")),
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
    /// The file's one-line note, such as for a binary file.
    Note(usize),
    /// A large file waiting for "Show diff".
    Folded(usize),
}

/// The open review of a thread's changes.
pub(super) struct Review {
    pub(super) thread: ThreadId,
    /// Whether a diff was read yet.
    loaded: bool,
    /// Parsed from the last diff read. The one source of file counts.
    files: Vec<FileDiff>,
    rows: Vec<Row>,
    /// Each file's header row.
    file_rows: Vec<usize>,
    list: ListState,
    /// Large files you asked to see, by path.
    shown: HashSet<String>,
    message: Entity<TextareaState>,
    /// The draft from the agent's latest reply, empty if none.
    draft: String,
    _message_events: Subscription,
    /// A commit or discard was sent and has not been answered.
    busy: bool,
    /// Why the last read failed, shown in place of the diff.
    failed: Option<String>,
    focus: FocusHandle,
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

/// The diff list's rows, and where each file starts.
fn diff_rows(files: &[FileDiff], shown: &HashSet<String>) -> (Vec<Row>, Vec<usize>) {
    let mut rows = Vec::new();
    let mut starts = Vec::new();
    for (f, file) in files.iter().enumerate() {
        starts.push(rows.len());
        rows.push(Row::File(f));
        if file.note.is_some() {
            rows.push(Row::Note(f));
        } else if file.line_count() > LARGE_FILE_LINES && !shown.contains(&file.path) {
            rows.push(Row::Folded(f));
        } else {
            for (h, hunk) in file.hunks.iter().enumerate() {
                rows.push(Row::Hunk(f, h));
                rows.extend((0..hunk.lines.len()).map(|l| Row::Line(f, h, l)));
            }
        }
    }
    (rows, starts)
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
        self.review = Some(Review {
            thread,
            loaded: false,
            files: Vec::new(),
            rows: Vec::new(),
            file_rows: Vec::new(),
            list: ListState::new(0, ListAlignment::Top, px(800.)),
            shown: HashSet::new(),
            message,
            draft,
            _message_events: events,
            busy: false,
            failed: None,
            focus,
        });
        self.load_thread_diff(thread, cx);
        cx.notify();
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
        self.commit_messages.insert(
            review.thread,
            SavedMessage {
                text,
                draft: review.draft,
            },
        );
        true
    }

    /// The commit message drafted from the thread's latest agent reply.
    fn current_draft(&self, thread: ThreadId) -> String {
        self.state()
            .messages(thread)
            .iter()
            .rev()
            .find(|m| matches!(m.author, Author::Agent(_)))
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
        let text = review.message.read(cx).value().to_string();
        if text.is_empty() || text == review.draft {
            let new = draft.clone();
            review
                .message
                .update(cx, |input, cx| input.set_value(new, window, cx));
        }
        review.draft = draft;
        cx.notify();
    }

    /// The agent whose changes these are: the latest to reply, else the
    /// latest to run.
    fn review_agent(&self, thread: ThreadId) -> Option<String> {
        let state = self.state();
        let replied = state
            .messages(thread)
            .iter()
            .rev()
            .find_map(|m| match m.author {
                Author::Agent(a) => Some(a),
                _ => None,
            });
        let ran = state.thread_runs(thread).last().map(|r| r.agent);
        replied
            .or(ran)
            .and_then(|a| state.agent(a))
            .map(|a| a.spec.name.clone())
    }

    /// Reads the open thread's uncommitted changes, for the inspector and
    /// the review. Only the latest read lands, and a failed read keeps
    /// what was shown.
    pub(super) fn load_thread_diff(&mut self, thread: ThreadId, cx: &mut Context<Self>) {
        self.diff_seq += 1;
        let seq = self.diff_seq;
        let answer = self.client.request(Request::ThreadDiff { thread });
        let background = cx.background_executor().clone();
        let parse_it = self.review.as_ref().is_some_and(|r| r.thread == thread);
        self.inspector.diff_task = Some(cx.spawn(async move |this, cx| {
            let read = answer
                .await
                .and_then(|v| serde_json::from_value::<Diff>(v).map_err(|e| e.to_string()));
            let read = match read {
                Ok(diff) if parse_it => {
                    let patch = diff.patch.clone();
                    let files = background.spawn(async move { parse(&patch) }).await;
                    Ok((diff, Some(files)))
                }
                Ok(diff) => Ok((diff, None)),
                Err(e) => Err(e),
            };
            this.update(cx, |this, cx| {
                if this.diff_seq != seq || this.nav.thread != Some(thread) {
                    return;
                }
                let review = this.review.as_mut().filter(|r| r.thread == thread);
                match read {
                    Ok((diff, files)) => {
                        if let (Some(review), Some(files)) = (review, files) {
                            review.loaded = true;
                            review.failed = None;
                            review.files = files;
                            review
                                .shown
                                .retain(|p| review.files.iter().any(|f| &f.path == p));
                            rebuild_rows(review);
                        }
                        this.inspector.diff = Some(diff);
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
        if let Some(review) = &self.review
            && let Some(&row) = review.file_rows.get(file)
        {
            review.list.scroll_to(ListOffset {
                item_ix: row,
                offset_in_item: px(0.),
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

    /// Whether Commit can be pressed: there are changes, a message, and
    /// nothing else is being sent.
    pub(super) fn review_can_commit(&self, cx: &App) -> bool {
        self.review.as_ref().is_some_and(|r| {
            !r.busy && !r.files.is_empty() && !r.message.read(cx).value().trim().is_empty()
        })
    }

    /// Whether the open thread has a worktree with changes to review.
    pub(super) fn can_review(&self, thread: ThreadId) -> bool {
        let has_worktree = self
            .state()
            .thread(thread)
            .is_some_and(|t| t.worktree.is_some());
        has_worktree
            && self.nav.thread == Some(thread)
            && self
                .inspector
                .diff
                .as_ref()
                .is_some_and(|d| !d.files.is_empty())
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
        let answer = self.client.request(Request::Commit { thread, message });
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
        let ask = format!(
            "Discard {whose} changes? The worktree is removed. A branch with commits is kept."
        );
        let this = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm = this.clone();
            dialog
                .w(px(420.))
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
        review.busy = true;
        let thread = review.thread;
        let answer = self.client.request(Request::Discard { thread });
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

    /// Opens the worktree folder in the editor app, or in Finder.
    fn open_in_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self
            .review
            .as_ref()
            .and_then(|r| self.state().thread(r.thread))
            .and_then(|t| t.worktree.as_ref())
            .map(|wt| wt.path.clone())
        else {
            return;
        };
        let opened = cx
            .background_executor()
            .spawn(async move { open_folder(path, editor_app()) });
        self.spawn_request(window, cx, async move |_, _| {
            opened.await.map_err(|e| (e, None))
        });
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
                div()
                    .id(("diff-file", f))
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(40.))
                    .px(px(16.))
                    .when(f > 0, |row| row.mt(px(16.)))
                    .border_t_1()
                    .border_b_1()
                    .border_color(color("border-100"))
                    .bg(color("bg-100"))
                    .font_family(mono_font())
                    .text_size(px(12.))
                    .aria_label(file.path.clone())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(color("text-primary"))
                            .child(file.path.clone()),
                    )
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
                    .h(px(28.))
                    .pl(px(GUTTER * 2. + 16.))
                    .bg(color("surface-100"))
                    .border_b_1()
                    .border_color(color("border-100"))
                    .font_family(mono_font())
                    .text_size(px(12.))
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
                        .w(px(GUTTER))
                        .pr(px(8.))
                        .flex()
                        .justify_end()
                        .text_color(color("text-gutter"))
                        .children(n.map(|n| n.to_string()))
                };
                div()
                    .id(("diff-row", ix))
                    .w_full()
                    .flex()
                    .h(px(LINE_HEIGHT))
                    .items_center()
                    .when_some(tint, |row, tint| row.bg(color(tint)))
                    .font_family(mono_font())
                    .text_size(px(12.))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .aria_label(label)
                    .child(gutter(line.old))
                    .child(gutter(line.new))
                    .child(
                        div()
                            .flex_none()
                            .w(px(16.))
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
            Row::Note(f) => {
                let note = review.files[f].note.clone().unwrap_or_default();
                div()
                    .id(("diff-row", ix))
                    .w_full()
                    .px(px(16.))
                    .py(px(10.))
                    .text_size(px(12.))
                    .text_color(color("text-tertiary"))
                    .aria_label(note.clone())
                    .child(note)
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
                    .gap(px(12.))
                    .px(px(16.))
                    .py(px(10.))
                    .text_size(px(12.))
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
        let where_ = format!("{title} · #{channel}");
        let left = div()
            .flex()
            .items_center()
            .gap(px(12.))
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
                    .font_weight(FontWeight::SEMIBOLD)
                    .aria_label(heading.clone())
                    .child(heading)
                    .test_support(),
            )
            .child(
                div()
                    .id("review-where")
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.))
                    .text_color(color("text-tertiary"))
                    .aria_label(where_.clone())
                    .child(where_)
                    .test_support(),
            )
            .into_any_element();
        let right = div()
            .flex()
            .items_center()
            .gap(px(8.))
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
                    .p(px(2.))
                    .gap(px(2.))
                    .border_1()
                    .border_color(color("border-200"))
                    .rounded(px(6.))
                    .child(
                        Button::new("review-unified")
                            .ghost()
                            .xsmall()
                            .selected(true)
                            .label("Unified"),
                    )
                    .child(
                        Button::new("review-split")
                            .ghost()
                            .xsmall()
                            .label("Split")
                            .disabled(true)
                            .tooltip("Split view is coming soon."),
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
        div()
            .id("review-files")
            .flex()
            .flex_col()
            .flex_none()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .border_r_1()
            .border_color(color("border-100"))
            .overflow_y_scroll()
            .child(
                div()
                    .id("review-summary")
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .h(px(40.))
                    .px(px(16.))
                    .text_size(px(12.))
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
            .children(
                files
                    .into_iter()
                    .enumerate()
                    .map(|(ix, (path, kind, added, removed))| {
                        let seen = viewed.contains(&path);
                        div()
                            .id(("review-file", ix))
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .h(px(28.))
                            .px(px(16.))
                            .cursor_pointer()
                            .hover(|s| s.bg(color("surface-100")))
                            .font_family(mono_font())
                            .text_size(px(12.))
                            .aria_label(format!("{} {path} +{added} −{removed}", kind.letter()))
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(10.))
                                    .text_size(px(10.))
                                    .font_weight(FontWeight::MEDIUM)
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
                    }),
            )
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
            .child(body)
    }

    fn render_what_next(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let Some(review) = &self.review else {
            return div().into_any_element();
        };
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
                .h(px(28.))
                .text_size(px(12.))
                .child(
                    div()
                        .w(px(64.))
                        .text_color(color("text-tertiary"))
                        .child(label),
                )
                .child(
                    div()
                        .id(id)
                        .min_w_0()
                        .truncate()
                        .font_family(mono_font())
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
            .gap(px(12.))
            .w(px(INSPECTOR_WIDTH))
            .h_full()
            .p(px(16.))
            .border_l_1()
            .border_color(color("border-100"))
            .overflow_y_scroll()
            .child(
                div()
                    .text_size(px(14.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("What next"),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .id("review-message-caption")
                            .text_size(px(12.))
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
                            .font_family(mono_font())
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
                    .gap(px(8.))
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
                    .text_size(px(12.))
                    .line_height(px(18.))
                    .text_color(color("text-tertiary"))
                    .child(NOTE),
            )
            .into_any_element()
    }
}

fn rebuild_rows(review: &mut Review) {
    let (rows, starts) = diff_rows(&review.files, &review.shown);
    if rows != review.rows {
        review.list.reset(rows.len());
    }
    review.rows = rows;
    review.file_rows = starts;
}

fn plus_minus(added: u32, removed: u32) -> impl IntoElement {
    div()
        .flex()
        .flex_none()
        .gap(px(4.))
        .font_family(mono_font())
        .text_size(px(12.))
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

/// `EDITOR_APP`, in debug builds only, such as `Zed`.
fn editor_app() -> Option<String> {
    #[cfg(debug_assertions)]
    {
        std::env::var("EDITOR_APP")
            .ok()
            .filter(|a| !a.trim().is_empty())
    }
    #[cfg(not(debug_assertions))]
    {
        None
    }
}

/// Opens `path` with `open`, in `app` if given. Blocks until `open`
/// returns, so run it off the UI thread.
fn open_folder(path: PathBuf, app: Option<String>) -> Result<(), String> {
    let mut open = std::process::Command::new("open");
    if let Some(app) = &app {
        open.arg("-a").arg(app);
    }
    let status = open.arg(&path).status().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(match app {
            Some(app) => {
                format!("{app} couldn't open the folder. Check that the app is installed.")
            }
            None => "The folder couldn't be opened. Refresh to check it still exists.".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{LARGE_FILE_LINES, Row, diff_rows, draft_commit_message};
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
        let (rows, starts) = diff_rows(&files, &HashSet::new());
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
        let (rows, _) = diff_rows(&files, &shown);
        assert_eq!(rows.len(), 4 + 2 + LARGE_FILE_LINES + 1);
    }
}
