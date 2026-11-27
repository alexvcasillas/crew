//! Archive: the thread moves to Archived. Its worktree stays.

use crew_core::ids::ThreadId;
use crew_proto::WorktreeState;
use gpui_kit::*;

use super::super::workspace::Workspace;
use super::{DialogKind, heading};
use crate::theme::color;
use crate::theme::scale::{self, ui_px};

/// What archiving `title` in `#channel` does.
pub fn archive_body(title: &str, channel: &str) -> String {
    format!("“{title}” moves to Archived in #{channel}. You can bring it back any time.")
}

/// Said when the thread is a scheduled job's.
pub fn job_note(job: &str) -> String {
    format!("Its scheduled job, {job}, will be deleted too.")
}

/// Said when the worktree has `files` uncommitted files.
pub fn uncommitted_note(files: u32) -> String {
    let files = if files == 1 {
        "1 uncommitted file".to_string()
    } else {
        format!("{files} uncommitted files")
    };
    format!(
        "The worktree and its {files} stay on disk. Discard it separately if you don't need them."
    )
}

impl Workspace {
    /// Asks before archiving `thread`.
    pub(crate) fn open_archive_dialog(
        &mut self,
        thread: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state().thread(thread).is_none() {
            return;
        }
        self.show_dialog(DialogKind::Archive { thread }, window, cx);
    }

    pub(super) fn archive_dialog_body(&self, thread: ThreadId) -> AnyElement {
        let state = self.state();
        let Some(t) = state.thread(thread) else {
            return div().into_any_element();
        };
        let channel = state
            .channel(t.channel)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let body = archive_body(&t.title, &channel);
        let uncommitted = match self.channel_view.card_worktree(thread) {
            Some(WorktreeState::Uncommitted { files }) if t.worktree.is_some() => Some(files),
            _ => None,
        };
        let job = state.thread_job(thread).map(|j| job_note(&j.name));
        div()
            .flex()
            .flex_col()
            .gap(ui_px(12.))
            .child(heading(
                "archive-heading",
                "Archive this thread?".into(),
                Some(body.clone().into_any_element()),
                format!("Archive this thread? {body}"),
            ))
            .children(job.map(|note| note_box("archive-job-note", note)))
            .children(uncommitted.map(|files| note_box("archive-note", uncommitted_note(files))))
            .into_any_element()
    }
}

/// A note under the heading, in a hairline box.
fn note_box(id: &'static str, note: String) -> impl IntoElement {
    div()
        .id(id)
        .px(ui_px(12.))
        .py(scale::row(10.))
        .border_1()
        .border_color(color("border-100"))
        .rounded(ui_px(6.))
        .text_size(scale::text(12.))
        .line_height(scale::line(18.))
        .text_color(color("text-secondary"))
        .aria_label(note.clone())
        .child(note)
        .test_support()
}
