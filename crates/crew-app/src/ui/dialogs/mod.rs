//! The dialogs: add project, rename, archive, discard the worktree,
//! channel settings and the scheduled job form. One is open at a time; what it shows lives in
//! [`OpenDialog`] so an answer from crewd can land in it.

mod add_project;
mod archive;
mod channel_settings;
mod discard;
mod job;
mod project;
mod rename;

use crew_core::ids::{ChannelId, ProjectId, ThreadId};
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
#[cfg(test)]
pub(crate) use add_project::NOT_A_FOLDER;
pub(crate) use add_project::{repos_line, setup_file_in, short_remote, size_label};
use discard::discard_label;
pub(crate) use job::JobForm;
#[cfg(test)]
pub(crate) use job::next_times;
pub(crate) use project::REMOVE_BODY;

/// The dialogs' width.
const WIDTH: f32 = 420.;
/// Space at the dialogs' sides, as the board has it.
const PADDING_X: f32 = 20.;
/// Space above and below the footer's buttons.
const FOOTER_Y: f32 = 12.;

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
    AddProject(Box<add_project::AddProject>),
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
    NewChannel {
        project: ProjectId,
        name: Entity<InputState>,
        description: Entity<InputState>,
    },
    RenameProject {
        project: ProjectId,
        name: Entity<InputState>,
    },
    RemoveProject {
        project: ProjectId,
    },
    Job(Box<JobForm>),
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
        let (width, top) = match self.dialog.as_ref().map(|d| &d.kind) {
            Some(DialogKind::AddProject(_)) => (add_project::WIDTH, None),
            Some(DialogKind::Job(_)) => (job::WIDTH, Some(job::TOP)),
            _ => (WIDTH, None),
        };
        let this = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let closed = this.clone();
            let confirmed = this.clone();
            let dialog = dialog
                .w(ui_px(width))
                .px(ui_px(PADDING_X))
                .pb(ui_px(FOOTER_Y))
                .when_some(top, |d, top| d.margin_top(ui_px(top)))
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
            ws.read(cx).build_dialog(dialog, this.clone(), cx)
        });
        cx.notify();
    }

    /// The open dialog, drawn from what [`OpenDialog`] holds now.
    fn build_dialog(&self, dialog: Dialog, this: WeakEntity<Self>, cx: &App) -> Dialog {
        let Some(open) = &self.dialog else {
            return dialog;
        };
        let body = match &open.kind {
            DialogKind::AddProject(add) => self.add_project_body(add, this.clone()),
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
            DialogKind::NewChannel {
                project,
                name,
                description,
            } => self.new_channel_body(*project, name, description),
            DialogKind::RenameProject { name, .. } => self.rename_project_body(name),
            DialogKind::RemoveProject { project } => self.remove_project_body(*project),
            DialogKind::Job(form) => self.job_form_body(form, this.clone(), cx),
        };
        let (confirm, danger) = match &open.kind {
            DialogKind::AddProject(_) => ("Add project".to_string(), false),
            DialogKind::Rename { .. } => ("Rename".to_string(), false),
            DialogKind::Archive { .. } => ("Archive".to_string(), false),
            DialogKind::Discard { thread, diff, .. } => (
                discard_label(self.discard_count(*thread, diff.as_ref())),
                true,
            ),
            DialogKind::ChannelSettings { .. } => ("Save".to_string(), false),
            DialogKind::NewChannel { .. } => ("Create channel".to_string(), false),
            DialogKind::RenameProject { .. } => ("Rename".to_string(), false),
            DialogKind::RemoveProject { .. } => ("Remove".to_string(), true),
            DialogKind::Job(form) if form.editing().is_some() => ("Save".to_string(), false),
            DialogKind::Job(_) => ("Create job".to_string(), false),
        };
        let busy = open.busy;
        // The job form: whether it can be sent, and whether it is new.
        let job = self.job_form_ready(cx);
        let off = busy
            || (matches!(open.kind, DialogKind::AddProject(_)) && !self.add_project_ready())
            || job.is_some_and(|(ready, _)| !ready);
        let cancel = this.clone();
        let test_run = this.clone();
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
                // Full width with a line above it, as the board has it.
                DialogFooter::new()
                    .mx(ui_px(-PADDING_X))
                    .px(ui_px(PADDING_X))
                    .pt(ui_px(FOOTER_Y))
                    .border_t_1()
                    .border_color(color("border-100"))
                    // A new job can be tried first, on the left.
                    .when_some(job.filter(|(_, new)| *new), |footer, (ready, _)| {
                        footer
                            .child(
                                Button::new("job-test-run")
                                    .ghost()
                                    .label("Test run now")
                                    .disabled(busy || !ready)
                                    .on_click(move |_, window, cx| {
                                        test_run
                                            .update(cx, |ws, cx| ws.test_run_job(window, cx))
                                            .ok();
                                    }),
                            )
                            .child(div().flex_1())
                    })
                    .child(
                        Button::new("dialog-cancel")
                            .map(|b| {
                                if job.is_some() {
                                    b.outline()
                                } else {
                                    b.ghost()
                                }
                            })
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
                            .disabled(off)
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
        // A new channel's name and description, set once it is made.
        let mut new_channel: Option<(String, String)> = None;
        let checked = match &open.kind {
            DialogKind::AddProject(_) => return self.confirm_add_project(window, cx),
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
            DialogKind::NewChannel {
                project,
                name,
                description,
            } => {
                let state = self.state();
                let taken: Vec<&str> = state
                    .channels(*project)
                    .into_iter()
                    .map(|c| c.name.as_str())
                    .collect();
                let project_name = state
                    .project(*project)
                    .map(|p| p.name.as_str())
                    .unwrap_or_default();
                super::settings::projects::channel_name(
                    &name.read(cx).value(),
                    &taken,
                    project_name,
                )
                .map(|name| {
                    let description = description.read(cx).value().trim().to_string();
                    new_channel = Some((name.clone(), description));
                    Request::CreateChannel {
                        project: *project,
                        name,
                    }
                })
            }
            DialogKind::RenameProject { project, name } => {
                let name = name.read(cx).value().trim().to_string();
                if name.is_empty() {
                    Err("Give the project a name.".to_string())
                } else {
                    Ok(Request::RenameProject {
                        project: *project,
                        name,
                    })
                }
            }
            DialogKind::RemoveProject { project } => {
                Ok(Request::RemoveProject { project: *project })
            }
            DialogKind::Job(form) => {
                if !form.ready(cx) {
                    return;
                }
                form.request(cx)
            }
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
        let client = self.client.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let mut result = answer.await;
            // A new channel: its description, then it opens.
            let mut made = None;
            let mut described = Ok(serde_json::Value::Null);
            if let (Ok(value), Some((name, description))) = (&result, new_channel) {
                match serde_json::from_value::<ChannelId>(value.clone()) {
                    Ok(channel) => {
                        made = Some(channel);
                        if !description.is_empty() {
                            described = client
                                .request(Request::UpdateChannel {
                                    channel,
                                    name,
                                    description,
                                })
                                .await;
                        }
                    }
                    Err(e) => result = Err(e.to_string()),
                }
            }
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(_) => {
                        this.dialog = None;
                        window.close_dialog(cx);
                        if let Some(channel) = made {
                            this.leave_pages(cx);
                            this.select_channel(channel, window, cx);
                        }
                        if let Err(error) = described {
                            this.show_error(error, window, cx);
                        }
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
            Some(DialogKind::NewChannel { name, .. }) => name.clone(),
            Some(DialogKind::RenameProject { name, .. }) => name.clone(),
            Some(DialogKind::Job(form)) => form.name.clone(),
            _ => return,
        };
        field.update(cx, |input, cx| input.focus(window, cx));
    }

    /// The card the shortcuts act on: the selected one, else the one
    /// under the pointer, while the channel's cards are shown and no
    /// dialog is open.
    fn shortcut_card(&self, window: &mut Window, cx: &mut App) -> Option<ThreadId> {
        if window.has_active_dialog(cx) {
            return None;
        }
        self.card_for_shortcuts()
    }

    /// [`Self::shortcut_card`], dialogs aside: what ⌘R and ⌘⌫ from the
    /// palette will act on once it closes.
    pub(super) fn card_for_shortcuts(&self) -> Option<ThreadId> {
        self.shown_channel()?;
        if self.composer_mode() != super::composer::ComposerMode::Reply {
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
