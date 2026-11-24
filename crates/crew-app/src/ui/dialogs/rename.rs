//! Rename thread: a new title. The branch keeps its name.

use crew_core::ids::ThreadId;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::*;

use super::super::workspace::Workspace;
use super::{DialogKind, field_label, heading, hint};
use crate::fonts::mono_font;
use crate::theme::color;

impl Workspace {
    /// Asks for a new title for `thread`.
    pub(crate) fn open_rename_dialog(
        &mut self,
        thread: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.state().thread(thread).map(|t| t.title.clone()) else {
            return;
        };
        let title = cx.new(|cx| InputState::new(window, cx).default_value(current.clone()));
        self.show_dialog(
            DialogKind::Rename {
                thread,
                title: title.clone(),
            },
            window,
            cx,
        );
        // After the dialog took focus.
        title.update(cx, |input, cx| {
            input.set_selected_range(0..current.len(), cx);
            input.focus(window, cx);
        });
    }

    pub(super) fn rename_body(&self, thread: ThreadId, title: &Entity<InputState>) -> AnyElement {
        let branch = self
            .state()
            .thread(thread)
            .and_then(|t| t.worktree.as_ref())
            .map(|wt| wt.branch.clone());
        div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(heading(
                "rename-heading",
                "Rename thread".into(),
                None,
                "Rename thread".into(),
            ))
            .child(
                div()
                    .id("rename-title")
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(field_label("Title"))
                    .child(Input::new(title).aria_label("Title"))
                    .test_support(),
            )
            .children(branch.map(|branch| {
                hint()
                    .id("rename-branch")
                    .flex()
                    .gap(px(4.))
                    .aria_label(format!("The branch keeps its name: {branch}"))
                    .child("The branch keeps its name:")
                    .child(
                        div()
                            .font_family(mono_font())
                            .text_color(color("text-secondary"))
                            .child(branch),
                    )
                    .test_support()
            }))
            .into_any_element()
    }
}
