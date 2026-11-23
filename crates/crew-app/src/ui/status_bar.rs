//! The status bar: whether the app is connected to crewd.

use gpui_kit::*;

use crate::connection::ConnectionState;
use crate::theme::color;

pub const STATUS_BAR_HEIGHT: f32 = 28.;

#[derive(Default)]
pub struct StatusBar {
    /// `None` while the first check runs.
    connection: Option<ConnectionState>,
}

impl StatusBar {
    pub fn set_connection(&mut self, state: Option<ConnectionState>, cx: &mut Context<Self>) {
        if self.connection != state {
            self.connection = state;
            cx.notify();
        }
    }
}

/// The connection item: its dot token and words.
fn connection_line(state: Option<&ConnectionState>) -> (&'static str, &'static str) {
    match state {
        None => ("text-tertiary", "Connecting…"),
        Some(ConnectionState::Connected) => ("success", "Connected to crewd"),
        Some(_) => ("text-tertiary", "Not connected"),
    }
}

impl Render for StatusBar {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let (dot, text) = connection_line(self.connection.as_ref());
        div()
            .id("status-bar")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(16.))
            .h(px(STATUS_BAR_HEIGHT))
            .px(px(16.))
            .border_t_1()
            .border_color(color("border-100"))
            .text_size(px(12.))
            .text_color(color("text-secondary"))
            .child(
                div()
                    .id("status-connection")
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(6.))
                    .aria_label(text)
                    .child(div().flex_none().size(px(6.)).rounded_full().bg(color(dot)))
                    .child(text)
                    .test_support(),
            )
            .test_support()
    }
}
