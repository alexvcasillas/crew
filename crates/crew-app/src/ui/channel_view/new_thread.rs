//! The new thread composer at the top of the channel.

use gpui_kit::*;

use super::super::workspace::Workspace;

impl Workspace {
    pub(super) fn render_new_thread_composer(&self) -> impl IntoElement {
        div()
            .id("new-thread-composer")
            .flex_none()
            .mx(px(24.))
            .role(Role::Group)
            .aria_label("New thread")
            .child(self.composer.clone())
            .test_support()
    }
}
