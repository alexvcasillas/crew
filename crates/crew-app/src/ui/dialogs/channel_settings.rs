//! Channel settings: the channel's name and description.

use crew_core::ids::ChannelId;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::*;

use super::super::workspace::Workspace;
use super::{DialogKind, field_label, heading, hint};
use crate::fonts::mono_font;
use crate::theme::color;

impl Workspace {
    /// Asks for the channel's name and description.
    pub(crate) fn open_channel_settings_dialog(
        &mut self,
        channel: ChannelId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((name_now, description_now)) = self
            .state()
            .channel(channel)
            .map(|c| (c.name.clone(), c.description.clone()))
        else {
            return;
        };
        let name = cx.new(|cx| InputState::new(window, cx).default_value(name_now));
        let description = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("What goes here")
                .default_value(description_now)
        });
        let focus = if description.read(cx).value().is_empty() {
            description.clone()
        } else {
            name.clone()
        };
        self.show_dialog(
            DialogKind::ChannelSettings {
                channel,
                name,
                description,
            },
            window,
            cx,
        );
        // After the dialog, and the button that opened it, took focus.
        cx.defer_in(window, move |_, window, cx| {
            focus.update(cx, |input, cx| input.focus(window, cx));
        });
    }

    pub(super) fn channel_settings_body(
        &self,
        name: &Entity<InputState>,
        description: &Entity<InputState>,
    ) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(heading(
                "channel-settings-heading",
                "Channel settings".into(),
                None,
                "Channel settings".into(),
            ))
            .child(
                div()
                    .id("channel-settings-name")
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(field_label("Name"))
                    .child(
                        Input::new(name).aria_label("Name").prefix(
                            div()
                                .font_family(mono_font())
                                .text_color(color("text-tertiary"))
                                .child("#"),
                        ),
                    )
                    .child(
                        hint().child(
                            "Lowercase, no spaces. Agents see the channel name in their brief.",
                        ),
                    )
                    .test_support(),
            )
            .child(
                div()
                    .id("channel-settings-description")
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex()
                            .gap(px(4.))
                            .child(field_label("Description"))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(color("text-tertiary"))
                                    .child("· optional"),
                            ),
                    )
                    .child(Input::new(description).aria_label("Description"))
                    .test_support(),
            )
            .into_any_element()
    }
}
