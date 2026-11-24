//! The threads sidebar's footer: Settings, Running now with its count,
//! Agents and the palette's shortcut.

use gpui_kit::component::Icon;
use gpui_kit::*;

use super::super::settings::Page;
use super::super::workspace::Workspace;
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};

/// The footer's Settings.
const GEAR_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><circle cx="8" cy="8" r="2"/><path d="M8 1.5v2M8 12.5v2M1.5 8h2M12.5 8h2M3.4 3.4l1.4 1.4M11.2 11.2l1.4 1.4M3.4 12.6l1.4-1.4M11.2 4.8l1.4-1.4"/></svg>"#;
/// The footer's Running now.
const PULSE_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><path d="M1.5 8h3l2-5 3 10 2-5h3"/></svg>"#;
/// The footer's Agents.
const PEOPLE_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><circle cx="6" cy="6" r="2.5"/><path d="M1.5 14c0-2.5 2-4 4.5-4s4.5 1.5 4.5 4"/><path d="M11 4a2 2 0 1 1 0 4M12.5 10.5c1.3.5 2 1.6 2 3.5" stroke-linecap="round"/></svg>"#;

impl Workspace {
    /// Settings, Running now with its count, Agents and the palette's
    /// shortcut.
    pub(super) fn render_threads_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.state().working_runs().len();
        let label = format!("Running now, {running} running");
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(ui_px(2.))
            .p(ui_px(8.))
            .border_t_1()
            .border_color(color("border-100"))
            .child(
                footer_button("threads-footer-settings", GEAR_SVG, "Settings")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_settings(Page::General, window, cx)
                    }))
                    .test_support(),
            )
            .child(
                div()
                    .id("threads-footer-running")
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(ui_px(6.))
                    .h(ui_px(28.))
                    .px(ui_px(8.))
                    .rounded(ui_px(6.))
                    .cursor_pointer()
                    .text_size(scale::text(12.))
                    .font_weight(weight(FontWeight::MEDIUM))
                    .text_color(color("text-secondary"))
                    .hover(|b| b.bg(color("surface-300")))
                    .aria_label(label)
                    .tooltip(|window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new("Running now").build(window, cx)
                    })
                    .child(Icon::default().data(PULSE_SVG).size(ui_px(14.)))
                    .child(
                        div()
                            .text_color(color(if running > 0 {
                                "accent-blue"
                            } else {
                                "text-tertiary"
                            }))
                            .child(running.to_string()),
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.show_running(window, cx)))
                    .test_support(),
            )
            .child(
                footer_button("threads-footer-agents", PEOPLE_SVG, "Agents")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_settings(Page::Agents, window, cx)
                    }))
                    .test_support(),
            )
            .child(
                div()
                    .ml_auto()
                    .min_w_0()
                    .overflow_hidden()
                    .pr(ui_px(4.))
                    .flex()
                    .gap(ui_px(4.))
                    .whitespace_nowrap()
                    .text_size(scale::text(11.))
                    .text_color(color("text-tertiary"))
                    .child(div().flex_none().font_family(code_family()).child("⌘K"))
                    .child(div().min_w_0().truncate().child("projects, channels")),
            )
    }
}

/// A 28 px icon button of the footer.
fn footer_button(id: &'static str, svg: &'static [u8], label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(ui_px(28.))
        .rounded(ui_px(6.))
        .cursor_pointer()
        .text_color(color("text-secondary"))
        .hover(|b| b.bg(color("surface-300")).text_color(color("text-primary")))
        .aria_label(label)
        .tooltip(move |window, cx| {
            gpui_kit::component::tooltip::Tooltip::new(label).build(window, cx)
        })
        .child(Icon::default().data(svg).size(ui_px(15.)))
}
