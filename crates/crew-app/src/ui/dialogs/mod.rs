//! The thread and channel dialogs: rename, archive, discard the worktree,
//! and channel settings. One is open at a time; what it shows lives in
//! [`OpenDialog`] so an answer from crewd can land in it.

mod archive;
mod channel_settings;
mod discard;
mod rename;

use crew_core::ids::{ChannelId, ThreadId};
use crew_core::worktree::Diff;
use crew_proto::Request;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{Dialog, DialogFooter};
use gpui_kit::component::input::InputState;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::workspace::Workspace;
use crate::theme::color;

use crate::theme::scale::{self, ui_px, weight};
use discard::discard_label;

/// The dialogs' width.
const WIDTH: f32 = 420.;

/// The dialog on screen.
pub(super) struct OpenDialog {
    pub(super) kind: DialogKind,
    /// What crewd or the check said was wrong, shown in the dialog.
    pub(super) error: Option<String>,
    /// Waiting for crewd's answer.
    pub(super) busy: bool,
    /// The request on its way, or the diff being read.
    task: Option<Task<()>>,
}

pub(super) enum DialogKind {
    Rename {
        thread: ThreadId,
        title: Entity<InputState>,
    },
    Archive {
        thread: ThreadId,
    },
    Discard {
        thread: ThreadId,
        /// The uncommitted changes, once read.
        diff: Option<Diff>,
        delete_branch: bool,
    },
    ChannelSettings {
        channel: ChannelId,
        name: Entity<InputState>,
        description: Entity<InputState>,
    },
}

impl Workspace {
    /// Shows `kind`, in place of any dialog open.
    fn show_dialog(&mut self, kind: DialogKind, window: &mut Window, cx: &mut Context<Self>) {
        self.dialog = Some(OpenDialog {
            kind,
            error: None,
            busy: false,
            task: None,
        });
        let this = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let closed = this.clone();
            let confirmed = this.clone();
            let dialog = dialog
                .w(ui_px(WIDTH))
                // ↵ confirms. The dialog closes once crewd says it's done.
                .on_ok(move |_, window, cx| {
                    confirmed
                        .update(cx, |ws, cx| ws.confirm_dialog(window, cx))
                        .ok();
                    false
                })
                .on_close(move |_, _, cx| {
                    closed.update(cx, |ws, _| ws.dialog = None).ok();
                });
            let Some(ws) = this.upgrade() else {
                return dialog;
            };
            ws.read(cx).build_dialog(dialog, this.clone())
        });
        cx.notify();
    }

    /// The open dialog, drawn from what [`OpenDialog`] holds now.
    fn build_dialog(&self, dialog: Dialog, this: WeakEntity<Self>) -> Dialog {
        let Some(open) = &self.dialog else {
            return dialog;
        };
        let body = match &open.kind {
            DialogKind::Rename { thread, title, .. } => self.rename_body(*thread, title),
            DialogKind::Archive { thread } => self.archive_dialog_body(*thread),
            DialogKind::Discard {
                thread,
                diff,
                delete_branch,
            } => self.discard_body(*thread, diff.as_ref(), *delete_branch, this.clone()),
            DialogKind::ChannelSettings {
                name, description, ..
            } => self.channel_settings_body(name, description),
        };
        let (confirm, danger) = match &open.kind {
            DialogKind::Rename { .. } => ("Rename".to_string(), false),
            DialogKind::Archive { .. } => ("Archive".to_string(), false),
            DialogKind::Discard { thread, diff, .. } => (
                discard_label(self.discard_count(*thread, diff.as_ref())),
                true,
            ),
            DialogKind::ChannelSettings { .. } => ("Save".to_string(), false),
        };
        let busy = open.busy;
        let cancel = this.clone();
        dialog
            .when(danger, |d| d.border_color(color("danger-border")))
            .child(
                div()
                    .id("dialog-body")
                    .flex()
                    .flex_col()
                    .gap(ui_px(14.))
                    .child(body)
                    .children(open.error.clone().map(|error| {
                        div()
                            .id("dialog-error")
                            .text_size(scale::text(12.))
                            .text_color(color("danger"))
                            .aria_label(error.clone())
                            .child(error)
                            .test_support()
                    }))
                    .test_support(),
            )
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("dialog-cancel")
                            .ghost()
                            .label("Cancel")
                            .on_click(move |_, window, cx| {
                                cancel.update(cx, |ws, _| ws.dialog = None).ok();
                                window.close_dialog(cx);
                            }),
                    )
                    .child(
                        Button::new("dialog-confirm")
                            .map(|b| if danger { b.danger() } else { b.primary() })
                            .label(confirm)
                            .disabled(busy)
                            .on_click(move |_, window, cx| {
                                this.update(cx, |ws, cx| ws.confirm_dialog(window, cx)).ok();
                            }),
                    ),
            )
    }

    /// Does what the open dialog asks for.
    pub(super) fn confirm_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = &self.dialog else {
            return;
        };
        if open.busy {
            return;
        }
        let checked = match &open.kind {
            DialogKind::Rename { thread, title, .. } => {
                let title = title.read(cx).value().trim().to_string();
                if title.is_empty() {
                    Err("Give the thread a title.".to_string())
                } else {
                    Ok(Request::RenameThread {
                        thread: *thread,
                        title,
                    })
                }
            }
            DialogKind::Archive { thread } => Ok(Request::ArchiveThread {
                thread: *thread,
                archived: true,
            }),
            DialogKind::Discard {
                thread,
                delete_branch,
                ..
            } => Ok(Request::Discard {
                thread: *thread,
                repos: None,
                delete_branch: *delete_branch,
            }),
            DialogKind::ChannelSettings {
                channel,
                name,
                description,
                ..
            } => checked_channel_name(
                &name.read(cx).value(),
                self.state().channel(*channel).map(|c| c.name.as_str()),
            )
            .map(|name| Request::UpdateChannel {
                channel: *channel,
                name,
                description: description.read(cx).value().trim().to_string(),
            }),
        };
        let request = match checked {
            Ok(request) => request,
            Err(error) => {
                if let Some(open) = &mut self.dialog {
                    open.error = Some(error);
                }
                self.focus_dialog_field(window, cx);
                cx.notify();
                return;
            }
        };
        let answer = self.client.request(request);
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = answer.await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(_) => {
                        this.dialog = None;
                        window.close_dialog(cx);
                    }
                    Err(error) => {
                        if let Some(open) = &mut this.dialog {
                            open.busy = false;
                            open.error = Some(error);
                            open.task = None;
                        }
                        this.focus_dialog_field(window, cx);
                    }
                }
                cx.notify();
            })
            .ok();
        });
        if let Some(open) = &mut self.dialog {
            open.busy = true;
            open.error = None;
            open.task = Some(task);
        }
        cx.notify();
    }

    /// Puts the cursor in the dialog's first field, to fix what was
    /// wrong or start typing.
    fn focus_dialog_field(&self, window: &mut Window, cx: &mut Context<Self>) {
        let field = match self.dialog.as_ref().map(|d| &d.kind) {
            Some(DialogKind::Rename { title, .. }) => title.clone(),
            Some(DialogKind::ChannelSettings { name, .. }) => name.clone(),
            _ => return,
        };
        field.update(cx, |input, cx| input.focus(window, cx));
    }

    /// The card the shortcuts act on: the selected one, else the one
    /// under the pointer, while the channel's cards are shown.
    fn shortcut_card(&self, window: &mut Window, cx: &mut App) -> Option<ThreadId> {
        self.shown_channel()?;
        if self.composer_mode() != super::composer::ComposerMode::Reply
            || window.has_active_dialog(cx)
        {
            return None;
        }
        let view = &self.channel_view;
        [view.selected, view.hovered]
            .into_iter()
            .flatten()
            .find(|t| view.card_in_view(*t))
    }

    /// ⌘R: renames the selected card's thread.
    pub(super) fn rename_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(thread) = self.shortcut_card(window, cx) {
            self.open_rename_dialog(thread, window, cx);
        }
    }

    /// ⌘⌫: archives the selected card's thread, or brings back an
    /// archived one.
    pub(super) fn archive_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self.shortcut_card(window, cx) else {
            return;
        };
        let archived = self
            .state()
            .thread(thread)
            .is_some_and(|t| t.archived_at.is_some());
        if archived {
            self.unarchive_thread(thread, window, cx);
        } else {
            self.open_archive_dialog(thread, window, cx);
        }
    }
}

/// The name a channel is saved with: the one it has when you left it
/// as it is, even one from before the rules, as crewd allows, else a
/// new name by the rules.
fn checked_channel_name(typed: &str, stored: Option<&str>) -> Result<String, String> {
    let trimmed = typed.trim();
    let bare = trimmed.strip_prefix('#').unwrap_or(trimmed);
    match stored {
        Some(stored) if bare == stored => Ok(stored.to_string()),
        _ => crew_core::slug::channel_name(typed).map_err(str::to_string),
    }
}

/// A field's label above it.
fn field_label(text: &'static str) -> Div {
    div()
        .text_size(scale::text(12.))
        .font_weight(weight(FontWeight::MEDIUM))
        .text_color(color("text-secondary"))
        .child(text)
}

/// Small grey text under a field.
fn hint() -> Div {
    div()
        .text_size(scale::text(12.))
        .line_height(scale::line(18.))
        .text_color(color("text-tertiary"))
}

/// The dialog's question and what it means.
fn heading(id: &'static str, title: String, body: Option<AnyElement>, label: String) -> AnyElement {
    div()
        .id(id)
        .flex()
        .flex_col()
        .gap(ui_px(4.))
        .aria_label(label)
        .child(
            div()
                .text_size(scale::text(15.))
                .line_height(scale::line(22.))
                .font_weight(weight(FontWeight::SEMIBOLD))
                .child(title),
        )
        .children(body.map(|body| {
            div()
                .line_height(scale::line(20.))
                .text_color(color("text-secondary"))
                .child(body)
        }))
        .test_support()
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::archive::{archive_body, uncommitted_note};
    use super::checked_channel_name;
    use super::discard::{discard_label, more_files};

    #[test]
    fn an_unchanged_old_name_skips_the_rules() {
        assert_eq!(
            checked_channel_name(" #Bugs ", Some("Bugs")).unwrap(),
            "Bugs"
        );
        assert_eq!(
            checked_channel_name("#bugs-2", Some("Bugs")).unwrap(),
            "bugs-2"
        );
        assert_eq!(
            checked_channel_name("Bugs Two", Some("Bugs")).unwrap_err(),
            "Use lowercase letters, numbers and dashes."
        );
    }

    #[test]
    fn archive_copy_names_the_thread_and_its_files() {
        assert_eq!(
            archive_body("Fix the login", "general"),
            "“Fix the login” moves to Archived in #general. You can bring it back any time."
        );
        assert_eq!(
            uncommitted_note(6),
            "The worktree and its 6 uncommitted files stay on disk. Discard it separately if you don't need them."
        );
        assert_eq!(
            uncommitted_note(1),
            "The worktree and its 1 uncommitted file stay on disk. Discard it separately if you don't need them."
        );
    }

    #[test]
    fn discard_counts_its_files() {
        assert_eq!(discard_label(0), "Discard worktree");
        assert_eq!(discard_label(1), "Discard 1 file");
        assert_eq!(discard_label(6), "Discard 6 files");
        assert_eq!(more_files(1), "and 1 more file");
        assert_eq!(more_files(4), "and 4 more files");
    }
}
