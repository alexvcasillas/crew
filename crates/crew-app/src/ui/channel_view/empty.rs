//! A channel with no threads: how threads start, the composer, and the
//! agents you can bring in.

use gpui_kit::*;

use super::super::common::{monogram, runtime_badge};
use super::super::workspace::Workspace;
use crate::theme::color;
use crate::theme::scale::{self, ui_px, weight};

/// The empty state's column.
const COLUMN_MAX: f32 = 680.;

impl Workspace {
    pub(super) fn render_channel_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let agents = self.state().agents();
        let chips: Vec<AnyElement> = agents
            .iter()
            .map(|agent| {
                let name = agent.spec.name.clone();
                let label = format!("@{name}");
                div()
                    .id(SharedString::from(format!("bring-in-{}", agent.id)))
                    .flex()
                    .items_center()
                    .gap(ui_px(10.))
                    .min_w_0()
                    .px(ui_px(12.))
                    .py(scale::row(10.))
                    .border_1()
                    .border_color(color("border-100"))
                    .rounded(ui_px(8.))
                    .bg(color("bg-100"))
                    .cursor_pointer()
                    .hover(|chip| {
                        chip.bg(color("surface-100"))
                            .border_color(color("border-200"))
                    })
                    .role(Role::Button)
                    .aria_label(label.clone())
                    .child(monogram(&name, 28.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(div().font_weight(weight(FontWeight::MEDIUM)).child(label))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(scale::text(12.))
                                    .text_color(color("text-secondary"))
                                    .child(agent.spec.role.clone()),
                            ),
                    )
                    .child(runtime_badge(&agent.spec))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.composer
                            .update(cx, |composer, cx| composer.add_mention(&name, window, cx));
                    }))
                    .test_support()
                    .into_any_element()
            })
            .collect();
        let mut chips = chips.into_iter();
        let mut rows: Vec<AnyElement> = Vec::new();
        while let Some(first) = chips.next() {
            let cell = |chip: Option<AnyElement>| {
                div()
                    .flex_1()
                    .flex_basis(ui_px(0.))
                    .min_w_0()
                    .children(chip)
            };
            rows.push(
                div()
                    .flex()
                    .gap(ui_px(8.))
                    .child(cell(Some(first)))
                    .child(cell(chips.next()))
                    .into_any_element(),
            );
        }
        let has_agents = !rows.is_empty();
        div()
            .id("channel-empty")
            .flex_1()
            .min_h_0()
            .flex()
            .items_center()
            .justify_center()
            .px(ui_px(24.))
            .py(ui_px(32.))
            .child(
                div()
                    .w_full()
                    .max_w(ui_px(COLUMN_MAX))
                    .flex()
                    .flex_col()
                    .gap(ui_px(20.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(ui_px(4.))
                            .child(
                                div()
                                    .id("channel-empty-title")
                                    .text_size(scale::text(20.))
                                    .line_height(scale::line(28.))
                                    .font_weight(weight(FontWeight::SEMIBOLD))
                                    .aria_label("No threads yet")
                                    .child("No threads yet")
                                    .test_support(),
                            )
                            .child(div().text_color(color("text-secondary")).child(
                                "Each message you send here starts a thread. Agents you mention work in their own copy of the repo.",
                            )),
                    )
                    .child(self.composer.clone())
                    .children(has_agents.then(|| {
                        div()
                            .id("bring-in")
                            .flex()
                            .flex_col()
                            .gap(ui_px(8.))
                            .child(
                                div()
                                    .text_size(scale::text(12.))
                                    .font_weight(weight(FontWeight::MEDIUM))
                                    .text_color(color("text-secondary"))
                                    .child("Bring in"),
                            )
                            .children(rows)
                            .child(
                                div()
                                    .text_size(scale::text(12.))
                                    .text_color(color("text-tertiary"))
                                    .child("Picking one adds the @mention to your message. You can mention more than one."),
                            )
                            .test_support()
                    })),
            )
            .test_support()
    }
}
