//! The channel's header, its stats strip and the toolbar above the cards.

use crew_core::ids::ChannelId;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::super::common::dot;
use super::super::running_view::usage_time;
use super::super::workspace::Workspace;
use super::{Filter, Sort, count, lines_changed, thousands};
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};

const SEARCH_WIDTH: f32 = 240.;

impl Workspace {
    /// `# name` in its project, its description, and Channel settings.
    pub(super) fn render_channel_header(
        &self,
        channel: ChannelId,
        ruled: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let state = self.state();
        let (name, description, project) = state
            .channel(channel)
            .map(|c| {
                let project = state
                    .project(c.project)
                    .map(|p| p.name.clone())
                    .unwrap_or_default();
                (c.name.clone(), c.description.trim().to_string(), project)
            })
            .unwrap_or_default();
        div()
            .flex()
            .flex_none()
            .items_start()
            .gap(ui_px(12.))
            .px(scale::chrome(24.))
            .pt(ui_px(20.))
            .pb(scale::row(16.))
            .when(ruled, |header| {
                header.border_b_1().border_color(color("border-100"))
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(ui_px(4.))
                    .child(
                        div()
                            .flex()
                            .items_end()
                            .gap(ui_px(10.))
                            .child(
                                div()
                                    .id("channel-title")
                                    .flex()
                                    .min_w_0()
                                    .text_size(scale::text(20.))
                                    .line_height(scale::line(28.))
                                    .font_weight(weight(FontWeight::SEMIBOLD))
                                    .aria_label(format!("#{name}"))
                                    .child(
                                        div()
                                            .font_family(code_family())
                                            .font_weight(weight(FontWeight::NORMAL))
                                            .text_color(color("text-tertiary"))
                                            .child("#"),
                                    )
                                    .child(div().truncate().child(name))
                                    .test_support(),
                            )
                            .child(
                                div()
                                    .id("channel-project")
                                    .flex_none()
                                    .pb(scale::row(4.))
                                    .text_size(scale::text(12.))
                                    .text_color(color("text-tertiary"))
                                    .aria_label(format!("in {project}"))
                                    .child(format!("in {project}"))
                                    .test_support(),
                            ),
                    )
                    .child(if description.is_empty() {
                        div()
                            .id("channel-add-description")
                            .flex_none()
                            .text_color(color("accent-blue"))
                            .cursor_pointer()
                            .role(Role::Link)
                            .aria_label("Add a description")
                            .child("Add a description")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_channel_settings_dialog(channel, window, cx);
                            }))
                            .test_support()
                            .into_any_element()
                    } else {
                        div()
                            .id("channel-description")
                            .text_color(color("text-secondary"))
                            .aria_label(description.clone())
                            .child(description)
                            .test_support()
                            .into_any_element()
                    }),
            )
            .child(
                Button::new("channel-settings")
                    .ghost()
                    .label("Channel settings")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_channel_settings_dialog(channel, window, cx);
                    })),
            )
    }

    /// Threads, Active now, Needs you, Agent time and Files changed.
    pub(super) fn render_channel_stats(&self) -> impl IntoElement {
        let stats = self.channel_view.stats();
        let note = |text: String| {
            div()
                .text_size(scale::text(12.))
                .font_weight(weight(FontWeight::NORMAL))
                .text_color(color("text-tertiary"))
                .child(text)
        };
        let cell = |id: &'static str, key: &'static str, label: String, value: Div, last: bool| {
            div()
                .id(id)
                .flex()
                .flex_col()
                .flex_1()
                .flex_basis(ui_px(0.))
                .min_w_0()
                .gap(ui_px(2.))
                .px(ui_px(16.))
                .py(scale::row(12.))
                .when(!last, |cell| {
                    cell.border_r_1().border_color(color("border-100"))
                })
                .aria_label(format!("{key} {label}"))
                .child(
                    div()
                        .text_size(scale::text(12.))
                        .text_color(color("text-tertiary"))
                        .child(key),
                )
                .child(
                    value
                        .flex()
                        .items_center()
                        .gap(ui_px(6.))
                        .text_size(scale::text(16.))
                        .line_height(scale::line(24.))
                        .font_weight(weight(FontWeight::SEMIBOLD))
                        .whitespace_nowrap()
                        .overflow_hidden(),
                )
                .test_support()
        };
        let threads_week = format!("· {} this week", thousands(stats.threads_week));
        let runs_week = format!("· {}", count(stats.runs_week, "run", "runs"));
        let lines = lines_changed(stats.added_week, stats.removed_week);
        let needs = stats.needs_you > 0;
        div()
            .id("channel-stats")
            .flex()
            .flex_none()
            .mx(ui_px(24.))
            .border_1()
            .border_color(color("border-100"))
            .rounded(ui_px(8.))
            .child(cell(
                "channel-stat-threads",
                "Threads",
                format!("{} {threads_week}", thousands(stats.threads)),
                div()
                    .child(thousands(stats.threads))
                    .child(note(threads_week.clone())),
                false,
            ))
            .child(cell(
                "channel-stat-active",
                "Active now",
                thousands(stats.active),
                div()
                    .when(stats.active > 0, |v| v.child(dot("accent-blue")))
                    .child(thousands(stats.active)),
                false,
            ))
            .child(cell(
                "channel-stat-needs",
                "Needs you",
                thousands(stats.needs_you),
                div()
                    .when(needs, |v| {
                        v.text_color(color("warning")).child(dot("warning"))
                    })
                    .child(thousands(stats.needs_you)),
                false,
            ))
            .child(cell(
                "channel-stat-time",
                "Agent time this week",
                format!("{} {runs_week}", usage_time(stats.agent_ms_week)),
                div()
                    .child(usage_time(stats.agent_ms_week))
                    .child(note(runs_week.clone())),
                false,
            ))
            .child(cell(
                "channel-stat-files",
                "Files changed this week",
                format!("{} {lines}", thousands(stats.files_week)),
                div().child(thousands(stats.files_week)).child(
                    div()
                        .flex()
                        .gap(ui_px(4.))
                        .font_family(code_family())
                        .text_size(scale::mono(12.))
                        .font_weight(weight(FontWeight::NORMAL))
                        .child(
                            div()
                                .text_color(color("success"))
                                .child(format!("+{}", thousands(stats.added_week))),
                        )
                        .child(
                            div()
                                .text_color(color("danger"))
                                .child(format!("−{}", thousands(stats.removed_week))),
                        ),
                ),
                true,
            ))
    }

    /// The filter, the sort, the search and New thread.
    pub(super) fn render_channel_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = &self.channel_view;
        let filters: Vec<AnyElement> = Filter::EVERY
            .iter()
            .zip(view.counts)
            .map(|(&filter, n)| {
                let on = view.filter == filter;
                let amber = filter == Filter::NeedsYou && n > 0;
                div()
                    .id(SharedString::from(format!(
                        "channel-filter-{}",
                        filter.key()
                    )))
                    .flex()
                    .items_center()
                    .gap(ui_px(6.))
                    .h(scale::row(26.))
                    .px(ui_px(10.))
                    .rounded(ui_px(4.))
                    .cursor_pointer()
                    .text_size(scale::text(12.))
                    .font_weight(weight(FontWeight::MEDIUM))
                    .when(on, |b| b.bg(color("surface-300")))
                    .text_color(color(if on { "text-primary" } else { "text-secondary" }))
                    .hover(|b| b.text_color(color("text-primary")))
                    .role(Role::Button)
                    .aria_label(format!("{} {n}", filter.label()))
                    .child(filter.label())
                    .child(
                        div()
                            .text_size(scale::text(11.))
                            .text_color(color(if amber { "warning" } else { "text-tertiary" }))
                            .child(n.to_string()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_channel_filter(filter, cx);
                    }))
                    .test_support()
                    .into_any_element()
            })
            .collect();
        let this = cx.entity().downgrade();
        let sort = view.sort;
        let sort_button = Button::new("channel-sort")
            .outline()
            .label(sort.label())
            .icon(IconName::ChevronDown)
            .accessibility_label("Sort")
            .dropdown_menu(move |menu, _, _| {
                Sort::EVERY.iter().fold(menu, |menu, &option| {
                    let this = this.clone();
                    menu.item(
                        PopupMenuItem::new(option.label())
                            .checked(option == sort)
                            .on_click(move |_, _, cx| {
                                this.update(cx, |this, cx| this.set_channel_sort(option, cx))
                                    .ok();
                            }),
                    )
                })
            });
        div()
            .flex()
            .flex_none()
            .flex_wrap()
            .items_center()
            .gap(ui_px(8.))
            .px(scale::chrome(24.))
            .pt(scale::row(16.))
            .pb(scale::row(12.))
            .child(
                div()
                    .id("channel-filters")
                    .flex()
                    .gap(ui_px(2.))
                    .p(ui_px(2.))
                    .border_1()
                    .border_color(color("border-200"))
                    .rounded(ui_px(6.))
                    .role(Role::Group)
                    .aria_label("Filter threads")
                    .children(filters)
                    .test_support(),
            )
            .child(
                div()
                    .id("channel-sort-value")
                    .flex()
                    .aria_label(sort.label())
                    .child(sort_button)
                    .test_support(),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("channel-search")
                    .w(ui_px(SEARCH_WIDTH))
                    .child(
                        Input::new(&view.search)
                            .aria_label("Search threads")
                            .prefix(
                                Icon::new(IconName::Search)
                                    .size(ui_px(14.))
                                    .text_color(color("text-tertiary")),
                            ),
                    )
                    .test_support(),
            )
            .child(
                Button::new("new-thread")
                    .primary()
                    .icon(IconName::Plus)
                    .label("New thread")
                    .child(
                        div()
                            .font_family(code_family())
                            .text_size(scale::mono(11.))
                            .opacity(0.6)
                            .child("⌘N"),
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.new_thread(window, cx))),
            )
    }
}
