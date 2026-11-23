//! The screen shown while crewd isn't reachable: start it, or restart an
//! out-of-date one.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::*;

use super::workspace::Workspace;
use crate::connection::ConnectionState;
use crate::theme::color;

/// What the screen says and offers for `state`.
struct Wording {
    title: &'static str,
    body: &'static str,
    button_id: &'static str,
    button: &'static str,
    restart: bool,
}

fn wording(state: &ConnectionState) -> Wording {
    match state {
        ConnectionState::VersionMismatch { .. } => Wording {
            title: "Crew's service is out of date.",
            body: "It belongs to another version of Crew. Restart it to use this one.",
            button_id: "connect-restart",
            button: "Restart service",
            restart: true,
        },
        _ => Wording {
            title: "Crew's background service isn't running.",
            body: "Your agents run inside it. Start it to continue.",
            button_id: "connect-start",
            button: "Start Crew service",
            restart: false,
        },
    }
}

impl Workspace {
    pub(super) fn render_connect(
        &self,
        state: &ConnectionState,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let wording = wording(state);
        let restart = wording.restart;
        div()
            .id("connect")
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(px(400.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .id("connect-title")
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .aria_label(wording.title)
                            .child(wording.title)
                            .test_support(),
                    )
                    .child(
                        div()
                            .text_color(color("text-secondary"))
                            .text_center()
                            .child(wording.body),
                    )
                    .child(
                        div().pt(px(8.)).child(
                            Button::new(wording.button_id)
                                .primary()
                                .label(wording.button)
                                .loading(self.starting())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.start_service(restart, window, cx);
                                })),
                        ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::wording;
    use crate::connection::ConnectionState;

    #[test]
    fn out_of_date_service_offers_a_restart() {
        let mismatch = ConnectionState::VersionMismatch { theirs: 2, ours: 3 };
        assert_eq!(wording(&mismatch).button, "Restart service");
        assert!(wording(&mismatch).restart);
        assert_eq!(
            wording(&ConnectionState::NotRunning).button,
            "Start Crew service"
        );
    }
}
