//! Discard the worktree: its folder goes, and the branch too if you ask.

use crew_core::ids::ThreadId;
use crew_core::worktree::Diff;
use crew_proto::{Request, WorktreeState};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::*;

use super::super::workspace::Workspace;
use super::{DialogKind, heading};
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px};

/// File rows the dialog lists before "and N more files".
const ROWS: usize = 2;

/// The confirm button: how many files go, or the worktree when clean.
pub fn discard_label(files: usize) -> String {
    match files {
        0 => "Discard worktree".into(),
        1 => "Discard 1 file".into(),
        n => format!("Discard {n} files"),
    }
}

/// The row after the listed files.
pub fn more_files(n: usize) -> String {
    if n == 1 {
        "and 1 more file".into()
    } else {
        format!("and {n} more files")
    }
}

impl Workspace {
    /// Asks before discarding `thread`'s worktree, listing what is lost.
    pub(crate) fn open_discard_dialog(
        &mut self,
        thread: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .state()
            .thread(thread)
            .is_none_or(|t| t.worktree.is_none())
        {
            return;
        }
        self.show_dialog(
            DialogKind::Discard {
                thread,
                diff: None,
                delete_branch: false,
            },
            window,
            cx,
        );
        let answer = self.client.request(Request::ThreadDiff { thread });
        let task = cx.spawn(async move |this, cx| {
            let diff = answer
                .await
                .and_then(|v| serde_json::from_value::<Diff>(v).map_err(|e| e.to_string()));
            this.update(cx, |this, cx| {
                if let Some(open) = &mut this.dialog
                    && let DialogKind::Discard {
                        thread: shown,
                        diff: slot,
                        ..
                    } = &mut open.kind
                    && *shown == thread
                {
                    match diff {
                        Ok(diff) => *slot = Some(diff),
                        Err(e) => eprintln!("crew: couldn't read the changes to discard: {e}"),
                    }
                    cx.notify();
                }
            })
            .ok();
        });
        if let Some(open) = &mut self.dialog {
            open.task = Some(task);
        }
    }

    /// How many uncommitted files a discard loses: the diff's, else the
    /// card's count until the diff is read.
    pub(super) fn discard_count(&self, thread: ThreadId, diff: Option<&Diff>) -> usize {
        match (diff, self.channel_view.card_worktree(thread)) {
            (Some(diff), _) => diff.files.len(),
            (None, Some(WorktreeState::Uncommitted { files })) => files as usize,
            _ => 0,
        }
    }

    fn toggle_delete_branch(&mut self, cx: &mut Context<Self>) {
        if let Some(open) = &mut self.dialog
            && let DialogKind::Discard { delete_branch, .. } = &mut open.kind
        {
            *delete_branch = !*delete_branch;
            cx.notify();
        }
    }

    pub(super) fn discard_body(
        &self,
        thread: ThreadId,
        diff: Option<&Diff>,
        delete_branch: bool,
        this: WeakEntity<Self>,
    ) -> AnyElement {
        let branch = self
            .state()
            .thread(thread)
            .and_then(|t| t.worktree.as_ref())
            .map(|wt| wt.branch.clone())
            .unwrap_or_default();
        let label = format!(
            "Discard the worktree? This deletes the folder for {branch}. Uncommitted changes can't be recovered."
        );
        let body = div()
            .child("This deletes the folder for ")
            .child(
                div()
                    .font_family(code_family())
                    .text_size(scale::mono(12.))
                    .text_color(color("text-primary"))
                    .child(branch),
            )
            .child(". Uncommitted changes can't be recovered.")
            .flex()
            .flex_wrap()
            .gap(ui_px(3.));
        let row = || {
            div()
                .flex()
                .items_center()
                .gap(ui_px(10.))
                .h(scale::row(32.))
                .px(ui_px(10.))
                .text_size(scale::text(12.))
                .text_color(color("text-secondary"))
        };
        let files = diff.map(|d| d.files.as_slice()).unwrap_or_default();
        let mut rows: Vec<AnyElement> = files
            .iter()
            .take(ROWS)
            .enumerate()
            .map(|(ix, file)| {
                let text = format!(
                    "{} {} +{} −{}",
                    file.status, file.path, file.added, file.removed
                );
                row()
                    .id(("discard-file", ix))
                    .font_family(code_family())
                    .aria_label(text)
                    .child(
                        div()
                            .w(scale::mono(10.))
                            .text_size(scale::mono(10.))
                            .text_color(color("warning"))
                            .child(file.status.to_string()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(color("text-primary"))
                            .child(file.path.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(ui_px(4.))
                            .child(
                                div()
                                    .text_color(color("success"))
                                    .child(format!("+{}", file.added)),
                            )
                            .child(
                                div()
                                    .text_color(color("danger"))
                                    .child(format!("−{}", file.removed)),
                            ),
                    )
                    .test_support()
                    .into_any_element()
            })
            .collect();
        if files.len() > ROWS {
            let more = more_files(files.len() - ROWS);
            rows.push(
                row()
                    .id("discard-more")
                    .aria_label(more.clone())
                    .child(div().w(ui_px(10.)))
                    .child(more)
                    .test_support()
                    .into_any_element(),
            );
        }
        let last = rows.len().saturating_sub(1);
        let rows: Vec<AnyElement> = rows
            .into_iter()
            .enumerate()
            .map(|(ix, row)| {
                let line = div().child(row);
                if ix < last {
                    line.border_b_1().border_color(color("border-100"))
                } else {
                    line
                }
                .into_any_element()
            })
            .collect();
        let has_rows = !rows.is_empty();
        div()
            .flex()
            .flex_col()
            .gap(ui_px(12.))
            .child(heading(
                "discard-heading",
                "Discard the worktree?".into(),
                Some(body.into_any_element()),
                label,
            ))
            .children(has_rows.then(|| {
                div()
                    .border_1()
                    .border_color(color("border-100"))
                    .rounded(ui_px(6.))
                    .overflow_hidden()
                    .children(rows)
            }))
            .child(
                Checkbox::new("discard-delete-branch")
                    .label("Also delete the branch")
                    .checked(delete_branch)
                    .on_click(move |_, _, cx| {
                        this.update(cx, |this, cx| this.toggle_delete_branch(cx))
                            .ok();
                    }),
            )
            .into_any_element()
    }
}
