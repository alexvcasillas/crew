//! A channel's threads, newest first.

use crew_core::ids::ChannelId;
use crew_core::store::StoredThread;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::status_word;
use super::workspace::Workspace;
use crate::model::ThreadStatus;
use crate::theme::color;

impl Workspace {
    pub(super) fn render_thread_list(
        &self,
        channel: ChannelId,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let state = self.state();
        let name = state
            .channel(channel)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let mut threads = state.threads(channel);
        threads.reverse();
        let count = threads.len();
        let rows: Vec<AnyElement> = threads
            .into_iter()
            .map(|t| self.render_thread_row(t, cx).into_any_element())
            .collect();
        div()
            .id("thread-list")
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(8.))
                    .h(px(44.))
                    .px(px(24.))
                    .border_b_1()
                    .border_color(color("border-100"))
                    .child(
                        div()
                            .id("thread-list-title")
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .aria_label(format!("#{name}"))
                            .child(format!("#{name}"))
                            .test_support(),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.))
                            .text_color(color("text-tertiary"))
                            .child(match count {
                                1 => "1 thread".to_string(),
                                n => format!("{n} threads"),
                            }),
                    )
                    .child(
                        Button::new("new-thread")
                            .primary()
                            .small()
                            .icon(IconName::Plus)
                            .label("New thread")
                            .tooltip("New thread (⌘N)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.new_thread(window, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .id("thread-list-rows")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p(px(8.))
                    .when(count == 0, |list| {
                        list.child(
                            div()
                                .id("thread-list-empty")
                                .p(px(16.))
                                .text_color(color("text-tertiary"))
                                .aria_label("No threads yet")
                                .child("No threads yet. Start one to give your agents a task.")
                                .test_support(),
                        )
                    })
                    .children(rows),
            )
            .test_support()
    }

    fn render_thread_row(&self, thread: &StoredThread, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state();
        let id = thread.id;
        let status = state.thread_status(id);
        let preview = state
            .last_message(id)
            .and_then(|m| m.body.lines().next().map(str::to_string))
            .unwrap_or_default();
        let (word, token) = status_word(status);
        div()
            .id(format!("thread-{id}"))
            .flex()
            .items_center()
            .gap(px(12.))
            .min_h(px(56.))
            .px(px(16.))
            .py(px(8.))
            .rounded(px(8.))
            .cursor_pointer()
            .hover(|row| row.bg(color("surface-100")))
            .aria_label(thread.title.clone())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .child(thread.title.clone()),
                    )
                    .when(!preview.is_empty(), |col| {
                        col.child(
                            div()
                                .truncate()
                                .text_size(px(12.))
                                .text_color(color("text-secondary"))
                                .child(preview),
                        )
                    }),
            )
            .when(status != ThreadStatus::Idle, |row| {
                row.child(
                    div()
                        .flex_none()
                        .text_size(px(11.))
                        .text_color(color(token))
                        .child(word),
                )
            })
            .on_click(cx.listener(move |this, _, window, cx| this.open_thread(id, window, cx)))
            .test_support()
    }
}
