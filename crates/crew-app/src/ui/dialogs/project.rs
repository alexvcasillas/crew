//! The project menu's dialogs: a new channel, a new name, and removing
//! the project from Crew.

use crew_core::ids::ProjectId;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::*;

use super::super::workspace::Workspace;
use super::{DialogKind, field_label, heading, hint};
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px};

/// The help under a new channel's name.
pub const CHANNEL_HELP: &str = "Lowercase, no spaces. Agents see the channel name in their brief.";

/// What removing a project does, and doesn't.
pub const REMOVE_BODY: &str =
    "Removes it from Crew. Your folder, branches and worktrees stay on disk.";

impl Workspace {
    /// Asks for a new channel's name and description in `project`.
    pub(crate) fn open_new_channel_dialog(
        &mut self,
        project: ProjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state().project(project).is_none() {
            return;
        }
        let name = cx.new(|cx| InputState::new(window, cx));
        let description = cx.new(|cx| InputState::new(window, cx).placeholder("What goes here"));
        self.show_dialog(
            DialogKind::NewChannel {
                project,
                name: name.clone(),
                description,
            },
            window,
            cx,
        );
        cx.defer_in(window, move |_, window, cx| {
            name.update(cx, |input, cx| input.focus(window, cx));
        });
    }

    /// Asks for the project's new name.
    pub(crate) fn open_rename_project_dialog(
        &mut self,
        project: ProjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.state().project(project).map(|p| p.name.clone()) else {
            return;
        };
        let name = cx.new(|cx| InputState::new(window, cx).default_value(current.clone()));
        self.show_dialog(
            DialogKind::RenameProject {
                project,
                name: name.clone(),
            },
            window,
            cx,
        );
        cx.defer_in(window, move |_, window, cx| {
            name.update(cx, |input, cx| {
                input.set_selected_range(0..current.len(), cx);
                input.focus(window, cx);
            });
        });
    }

    /// Asks before removing the project from Crew.
    pub(crate) fn open_remove_project_dialog(
        &mut self,
        project: ProjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state().project(project).is_none() {
            return;
        }
        self.show_dialog(DialogKind::RemoveProject { project }, window, cx);
    }

    pub(super) fn new_channel_body(
        &self,
        project: ProjectId,
        name: &Entity<InputState>,
        description: &Entity<InputState>,
    ) -> AnyElement {
        let title = format!(
            "New channel in {}",
            self.state()
                .project(project)
                .map(|p| p.name.as_str())
                .unwrap_or_default()
        );
        div()
            .flex()
            .flex_col()
            .gap(ui_px(14.))
            .child(heading("new-channel-heading", title.clone(), None, title))
            .child(
                div()
                    .id("new-channel-name")
                    .flex()
                    .flex_col()
                    .gap(ui_px(6.))
                    .child(field_label("Name"))
                    .child(
                        Input::new(name).aria_label("Name").prefix(
                            div()
                                .font_family(code_family())
                                .text_color(color("text-tertiary"))
                                .child("#"),
                        ),
                    )
                    .test_support(),
            )
            .child(
                div()
                    .id("new-channel-description")
                    .flex()
                    .flex_col()
                    .gap(ui_px(6.))
                    .child(
                        div()
                            .flex()
                            .gap(ui_px(4.))
                            .child(field_label("Description"))
                            .child(
                                div()
                                    .text_size(scale::text(12.))
                                    .text_color(color("text-tertiary"))
                                    .child("· optional"),
                            ),
                    )
                    .child(Input::new(description).aria_label("Description"))
                    .test_support(),
            )
            .child(
                hint()
                    .id("new-channel-help")
                    .aria_label(CHANNEL_HELP)
                    .child(CHANNEL_HELP)
                    .test_support(),
            )
            .into_any_element()
    }

    pub(super) fn rename_project_body(&self, name: &Entity<InputState>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(ui_px(14.))
            .child(heading(
                "rename-project-heading",
                "Rename project".into(),
                None,
                "Rename project".into(),
            ))
            .child(
                div()
                    .id("rename-project-name")
                    .flex()
                    .flex_col()
                    .gap(ui_px(6.))
                    .child(field_label("Name"))
                    .child(Input::new(name).aria_label("Name"))
                    .test_support(),
            )
            .into_any_element()
    }

    pub(super) fn remove_project_body(&self, project: ProjectId) -> AnyElement {
        let name = self
            .state()
            .project(project)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let title = format!("Remove {name} from Crew?");
        div()
            .flex()
            .flex_col()
            .gap(ui_px(14.))
            .child(heading(
                "remove-project-heading",
                title.clone(),
                Some(
                    div()
                        .id("remove-project-body")
                        .aria_label(REMOVE_BODY)
                        .child(REMOVE_BODY)
                        .test_support()
                        .into_any_element(),
                ),
                title,
            ))
            .into_any_element()
    }
}
