//! The sidebar's bar: the filter field and its keys, the filter menu,
//! Add project, New thread and the switch between the two sidebars.

use crew_core::ids::{ChannelId, ProjectId};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::super::palette::match_range;
use super::super::workspace::{Place, Workspace};
use crate::prefs::{ShowProjects, SidebarMode, SortProjects};
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};

actions!(
    sidebar,
    [
        /// The next filter result.
        SidebarDown,
        /// The filter result above.
        SidebarUp,
        /// Opens the highlighted filter result.
        SidebarOpen,
        /// Empties the filter.
        SidebarClear,
        /// Closes the filter menu.
        SidebarMenuClose,
    ]
);

/// Bindings that beat the filter and search fields' own, and the
/// menus'.
pub(crate) fn bindings() -> Vec<KeyBinding> {
    use super::switcher::{SwitcherClose, SwitcherDown, SwitcherPick, SwitcherUp};
    use super::threads::{ThreadsBack, ThreadsClear, ThreadsDown, ThreadsOpen, ThreadsUp};
    let field = Some("SidebarFilter > Input");
    let search = Some("ThreadsSearch > Input");
    let find = Some("ProjectSwitcher > Input");
    vec![
        KeyBinding::new("down", SidebarDown, field),
        KeyBinding::new("up", SidebarUp, field),
        KeyBinding::new("enter", SidebarOpen, field),
        KeyBinding::new("escape", SidebarClear, field),
        KeyBinding::new("escape", SidebarMenuClose, Some("SidebarMenu")),
        KeyBinding::new("down", ThreadsDown, search),
        KeyBinding::new("up", ThreadsUp, search),
        KeyBinding::new("enter", ThreadsOpen, search),
        KeyBinding::new("escape", ThreadsClear, search),
        KeyBinding::new("down", ThreadsDown, Some("ThreadsList")),
        KeyBinding::new("up", ThreadsUp, Some("ThreadsList")),
        KeyBinding::new("enter", ThreadsOpen, Some("ThreadsList")),
        KeyBinding::new("escape", ThreadsBack, Some("ThreadsList")),
        KeyBinding::new("down", SwitcherDown, find),
        KeyBinding::new("up", SwitcherUp, find),
        KeyBinding::new("enter", SwitcherPick, find),
        KeyBinding::new("escape", SwitcherClose, find),
        KeyBinding::new("escape", SwitcherClose, Some("ProjectSwitcher")),
    ]
}

/// A filter result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hit {
    Project(ProjectId),
    Channel(ChannelId),
}

/// The projects and channels whose names hold `query`, ignoring case,
/// in the order given.
pub(crate) fn filter_hits(
    query: &str,
    projects: &[(ProjectId, &str)],
    channels: &[(ChannelId, &str)],
) -> (Vec<ProjectId>, Vec<ChannelId>) {
    let query = query.trim();
    if query.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let found = |name: &str| match_range(name, query).is_some();
    (
        projects
            .iter()
            .filter(|(_, n)| found(n))
            .map(|(id, _)| *id)
            .collect(),
        channels
            .iter()
            .filter(|(_, n)| found(n))
            .map(|(id, _)| *id)
            .collect(),
    )
}

/// The filter icon, from the board.
pub(super) const FUNNEL_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"><path d="M2 3.5h12L9.5 9v4l-3 1.5V9z"/></svg>"#;
/// Add project: a folder with a plus.
pub(super) const FOLDER_PLUS_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"><path d="M1.5 3.5h4.5l1.5 1.5h7v8.5h-13z"/><path d="M8 7.5v4M6 9.5h4" stroke-linecap="round"/></svg>"#;
/// New thread: a pencil on a page.
pub(super) const COMPOSE_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"><path d="M13.5 9v4.5h-11v-11H7"/><path d="M11.5 1.8l2.7 2.7-6 6H5.5V7.8z"/></svg>"#;
/// Show threads: a list.
const LIST_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><path d="M2 4h.5M2 8h.5M2 12h.5M5.5 4h8.5M5.5 8h8.5M5.5 12h8.5"/></svg>"#;
/// The menu's check, from the board.
pub(super) const CHECK_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><path d="M3 8.5l3 3 7-7"/></svg>"#;

/// Where the filter menu sits in the sidebar, as the board has it.
const MENU_LEFT: f32 = 96.;
const MENU_TOP: f32 = 46.;
const MENU_WIDTH: f32 = 208.;

impl Workspace {
    /// The filter field changed: the list shows what it finds, from the
    /// top.
    pub(super) fn filter_changed(
        &mut self,
        input: &Entity<InputState>,
        event: &InputEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(event, InputEvent::Change) {
            return;
        }
        let query = input.read(cx).value().to_string();
        if query == self.sidebar.query {
            return;
        }
        self.sidebar.query = query;
        self.sidebar.selected = 0;
        self.sidebar.list.scroll_to(ListOffset::default());
        cx.notify();
    }

    /// ↓ or ↑ in the filter: the next or previous result.
    fn move_hit(&mut self, by: isize, cx: &mut Context<Self>) {
        let count = self.sidebar.hits.len();
        if count == 0 {
            return;
        }
        let next = self
            .sidebar
            .selected
            .saturating_add_signed(by)
            .min(count - 1);
        self.sidebar.selected = next;
        let row = self.sidebar.rows.iter().position(|r| match r {
            super::Row::Project(look) => look.hit == Some(next),
            super::Row::ChannelHit { hit, .. } => *hit == next,
            _ => false,
        });
        if let Some(row) = row {
            self.sidebar.list.scroll_to_reveal_item(row);
        }
        cx.notify();
    }

    /// ↵ in the filter: opens the highlighted result.
    fn open_hit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hit) = self.sidebar.hits.get(self.sidebar.selected).copied() else {
            return;
        };
        let place = match hit {
            // As a click on its row: a project's one channel, or its
            // overview.
            Hit::Project(project) => match self.single_channel(project) {
                Some(channel) => Place::Channel(channel),
                None => Place::Project(project),
            },
            Hit::Channel(channel) => Place::Channel(channel),
        };
        self.open_place(place, window, cx);
        // Typing goes on in the filter.
        self.sidebar
            .filter
            .update(cx, |input, cx| input.focus(window, cx));
    }

    /// Esc in the filter: empties it, or leaves it when it is empty.
    pub(super) fn clear_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sidebar.query.is_empty() {
            self.focus_workspace(window, cx);
            return;
        }
        self.sidebar
            .filter
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.sidebar.query.clear();
        self.sidebar.selected = 0;
        cx.notify();
    }

    pub(super) fn toggle_filter_menu(
        &mut self,
        click: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pressed_at = match click {
            ClickEvent::Mouse(m) => Some(m.down.position),
            _ => None,
        };
        // The press that closed the menu ends on this button: it stays
        // closed.
        if pressed_at.is_some() && self.sidebar.menu_closed_at.take() == pressed_at {
            return;
        }
        if self.sidebar.menu_open {
            self.close_filter_menu(window, cx);
        } else {
            self.sidebar.menu_open = true;
            self.sidebar.menu_focus.focus(window, cx);
            cx.notify();
        }
    }

    fn close_filter_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar.menu_open = false;
        let field = match super::threads::sidebar_mode(cx) {
            SidebarMode::Projects => self.sidebar.filter.clone(),
            SidebarMode::Threads => self.sidebar.threads.search.clone(),
        };
        field.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn set_sidebar_prefs(
        &mut self,
        change: impl FnOnce(&mut crate::prefs::SidebarPrefs),
        cx: &mut Context<Self>,
    ) {
        let before = self.sidebar.prefs;
        change(&mut self.sidebar.prefs);
        if self.sidebar.prefs != before {
            self.save_sidebar_prefs(cx);
        }
        cx.notify();
    }

    /// New thread from the bar: in the open channel, else the first one
    /// listed.
    pub(super) fn new_thread_from_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.nav.channel.is_none() {
            let first = match super::threads::sidebar_mode(cx) {
                SidebarMode::Threads => self.threads_new_thread_channel(),
                SidebarMode::Projects => self.sidebar.rows.iter().find_map(|row| match row {
                    super::Row::Project(look) => {
                        self.state().channels(look.project).first().map(|c| c.id)
                    }
                    _ => None,
                }),
            };
            let Some(channel) = first else {
                return;
            };
            self.select_channel(channel, window, cx);
        }
        self.leave_pages(cx);
        self.new_thread(window, cx);
    }

    /// The filter field, the filter menu button, Add project, New thread
    /// and the switch to the threads sidebar.
    pub(super) fn render_sidebar_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let filtering = !self.sidebar.query.is_empty();
        let menu_open = self.sidebar.menu_open;
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(ui_px(4.))
            .pt(scale::row(12.))
            .px(ui_px(8.))
            .pb(scale::row(8.))
            .child(
                div()
                    .id("sidebar-filter")
                    .key_context("SidebarFilter")
                    .flex_1()
                    .min_w_0()
                    .on_action(cx.listener(|this, _: &SidebarDown, _, cx| this.move_hit(1, cx)))
                    .on_action(cx.listener(|this, _: &SidebarUp, _, cx| this.move_hit(-1, cx)))
                    .on_action(
                        cx.listener(|this, _: &SidebarOpen, window, cx| this.open_hit(window, cx)),
                    )
                    .on_action(cx.listener(|this, _: &SidebarClear, window, cx| {
                        this.clear_filter(window, cx)
                    }))
                    .child(
                        Input::new(&self.sidebar.filter)
                            .aria_label("Filter projects and channels")
                            .h(scale::row(28.))
                            .text_size(scale::text(12.))
                            .prefix(
                                Icon::new(IconName::Search)
                                    .size(ui_px(13.))
                                    .text_color(color("text-tertiary")),
                            )
                            .when(filtering, |input| {
                                input.suffix(
                                    div()
                                        .id("sidebar-filter-clear")
                                        .flex()
                                        .flex_none()
                                        .items_center()
                                        .justify_center()
                                        .size(ui_px(16.))
                                        .rounded(ui_px(4.))
                                        .cursor_pointer()
                                        .text_color(color("text-tertiary"))
                                        .hover(|b| b.text_color(color("text-primary")))
                                        .aria_label("Clear filter")
                                        .child(Icon::new(IconName::Close).size(ui_px(10.)))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.clear_filter(window, cx);
                                        }))
                                        .test_support(),
                                )
                            }),
                    )
                    .test_support(),
            )
            .child(
                icon_button(
                    "sidebar-filter-options",
                    FUNNEL_SVG,
                    "Filter options",
                    menu_open,
                )
                .aria_expanded(menu_open)
                .on_click(cx.listener(|this, click, window, cx| {
                    this.toggle_filter_menu(click, window, cx);
                }))
                .test_support(),
            )
            .child(
                icon_button(
                    "sidebar-projects-add",
                    FOLDER_PLUS_SVG,
                    "Add project",
                    false,
                )
                .on_click(cx.listener(|this, _, window, cx| this.open_add_project(window, cx)))
                .test_support(),
            )
            .child(
                icon_button("sidebar-new-thread", COMPOSE_SVG, "New thread", false)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.new_thread_from_sidebar(window, cx)),
                    )
                    .test_support(),
            )
            .child(switch_button("Show threads", LIST_SVG, cx))
    }

    /// The filter menu: which projects show, their order, and whether the
    /// agents are listed. The threads sidebar has only which projects'
    /// threads show.
    pub(super) fn render_filter_menu(
        &self,
        threads: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let prefs = self.sidebar.prefs;
        let show = |id: &'static str, label: &'static str, value: ShowProjects| {
            radio(id, label, prefs.show == value)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_sidebar_prefs(|p| p.show = value, cx);
                }))
                .test_support()
        };
        let shows = [
            show("sidebar-show-all", "All projects", ShowProjects::All),
            show("sidebar-show-active", "Active only", ShowProjects::Active),
            show("sidebar-show-pinned", "Pinned only", ShowProjects::Pinned),
        ];
        let sort = |id: &'static str, label: &'static str, value: SortProjects| {
            radio(id, label, prefs.sort == value)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_sidebar_prefs(|p| p.sort = value, cx);
                }))
                .test_support()
        };
        let sorts = [
            sort(
                "sidebar-sort-recent",
                "Recent activity",
                SortProjects::Recent,
            ),
            sort("sidebar-sort-name", "Name", SortProjects::Name),
        ];
        div()
            .id("sidebar-filter-menu")
            .key_context("SidebarMenu")
            .track_focus(&self.sidebar.menu_focus)
            .absolute()
            .left(ui_px(MENU_LEFT))
            .top(scale::row(MENU_TOP))
            .w(ui_px(MENU_WIDTH))
            .flex()
            .flex_col()
            .p(ui_px(4.))
            .bg(color("surface-raised"))
            .border_1()
            .border_color(color("border-200"))
            .rounded(ui_px(8.))
            .shadow_lg()
            .occlude()
            .aria_label("Filter options")
            .on_action(cx.listener(|this, _: &SidebarMenuClose, window, cx| {
                this.close_filter_menu(window, cx);
            }))
            .on_mouse_down_out(cx.listener(|this, e: &MouseDownEvent, window, cx| {
                this.sidebar.menu_open = false;
                this.sidebar.menu_closed_at = Some(e.position);
                // The menu's focus goes with it; what was pressed may
                // take it next.
                this.focus_workspace(window, cx);
                cx.notify();
            }))
            .child(menu_heading("Show"))
            .children(shows)
            .when(!threads, |menu| {
                menu.child(menu_rule())
                    .child(menu_heading("Sort"))
                    .children(sorts)
                    .child(menu_rule())
                    .child(
                        menu_item("sidebar-show-agents", "Show agents")
                            .aria_selected(prefs.agents)
                            .child(div().w(ui_px(12.)))
                            .child(div().flex_1().child("Show agents"))
                            .child(crate::ui::common::switch(prefs.agents))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_sidebar_prefs(|p| p.agents = !p.agents, cx);
                            }))
                            .test_support(),
                    )
            })
            .test_support()
    }
}

/// "↓ results", "↵ open" and "esc clear", under the results.
pub(super) fn render_hints() -> impl IntoElement {
    let hint = |key: &'static str, text: &'static str| {
        div()
            .flex()
            .gap(ui_px(4.))
            .child(div().font_family(code_family()).child(key))
            .child(text)
    };
    div()
        .id("sidebar-filter-hints")
        .flex()
        .flex_none()
        .gap(ui_px(12.))
        .px(ui_px(16.))
        .py(scale::row(12.))
        .text_size(scale::text(12.))
        .text_color(color("text-tertiary"))
        .aria_label("↓ results, ↵ open, esc clear")
        .child(hint("↓", "results"))
        .child(hint("↵", "open"))
        .child(hint("esc", "clear"))
        .test_support()
}

/// A 24 px icon button of the bar, with its name as tooltip.
pub(super) fn icon_button(
    id: &'static str,
    svg: &'static [u8],
    label: &'static str,
    on: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(ui_px(24.))
        .rounded(ui_px(6.))
        .cursor_pointer()
        .text_color(color(if on { "text-primary" } else { "text-secondary" }))
        .when(on, |b| b.bg(color("surface-300")))
        .hover(|b| b.bg(color("surface-300")).text_color(color("text-primary")))
        .aria_label(label)
        .tooltip(move |window, cx| Tooltip::new(label).build(window, cx))
        .child(Icon::default().data(svg).size(ui_px(14.)))
}

/// The switch to the other sidebar, last in the bar. Hovered, its tip
/// shows under it (see [`switch_tip`]).
pub(super) fn switch_button(
    label: &'static str,
    svg: &'static [u8],
    cx: &mut Context<Workspace>,
) -> impl IntoElement + use<> {
    div()
        .id("sidebar-switch")
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(ui_px(24.))
        .rounded(ui_px(6.))
        .cursor_pointer()
        .text_color(color("text-secondary"))
        .hover(|b| b.bg(color("surface-300")).text_color(color("text-primary")))
        .aria_label(label)
        .child(Icon::default().data(svg).size(ui_px(14.)))
        .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
            if this.sidebar.switch_hovered != *hovered {
                this.sidebar.switch_hovered = *hovered;
                cx.notify();
            }
        }))
        .on_click(cx.listener(|this, _, _, cx| this.switch_sidebar(cx)))
        .test_support()
}

/// The switch's tip, as the board has it: under the button, against
/// the sidebar's right edge, "Show threads · switch sidebar".
pub(super) fn switch_tip(label: &'static str) -> impl IntoElement {
    div()
        .id("sidebar-switch-tip")
        .absolute()
        .right(ui_px(4.))
        .top(scale::row(44.))
        .flex()
        .items_center()
        .gap(ui_px(8.))
        .h(scale::row(26.))
        .px(ui_px(8.))
        .bg(color("surface-300"))
        .border_1()
        .border_color(color("border-200"))
        .rounded(ui_px(6.))
        .shadow_md()
        .whitespace_nowrap()
        .text_size(scale::text(12.))
        .text_color(color("text-primary"))
        .aria_label(format!("{label} · switch sidebar"))
        .child(label)
        .child(
            div()
                .text_color(color("text-tertiary"))
                .child("· switch sidebar"),
        )
        .test_support()
}

/// "SHOW" or "SORT" over the menu's choices.
fn menu_heading(text: &'static str) -> impl IntoElement {
    div()
        .px(ui_px(8.))
        .pt(scale::row(6.))
        .pb(scale::row(2.))
        .text_size(scale::text(11.))
        .font_weight(weight(FontWeight::MEDIUM))
        .text_color(color("text-tertiary"))
        .child(text.to_uppercase())
}

fn menu_rule() -> impl IntoElement {
    div().my(ui_px(4.)).h(ui_px(1.)).bg(color("border-100"))
}

fn menu_item(id: impl Into<SharedString>, label: &'static str) -> Stateful<Div> {
    div()
        .id(ElementId::Name(id.into()))
        .flex()
        .items_center()
        .gap(ui_px(8.))
        .h(scale::row(28.))
        .px(ui_px(8.))
        .rounded(ui_px(4.))
        .cursor_pointer()
        .text_color(color("text-primary"))
        .hover(|row| row.bg(color("surface-300")))
        .aria_label(label)
}

/// A choice with a check when it is the one in use.
fn radio(id: &'static str, label: &'static str, on: bool) -> Stateful<Div> {
    menu_item(id, label)
        .aria_selected(on)
        .child(
            div()
                .flex()
                .flex_none()
                .justify_center()
                .w(ui_px(12.))
                .when(on, |ck| {
                    ck.child(Icon::default().data(CHECK_SVG).size(ui_px(12.)))
                }),
        )
        .child(label)
}

#[cfg(test)]
mod tests {
    use crew_core::ids::{ChannelId, ProjectId};

    use super::filter_hits;

    #[test]
    fn finds_projects_and_channels_by_name_ignoring_case() {
        let (a, b, c) = (ProjectId::new(), ProjectId::new(), ProjectId::new());
        let (x, y) = (ChannelId::new(), ChannelId::new());
        let projects = [(a, "supa-app"), (b, "api"), (c, "Super-cli")];
        let channels = [(x, "support"), (y, "general")];
        assert_eq!(
            filter_hits("SUP", &projects, &channels),
            (vec![a, c], vec![x])
        );
        assert_eq!(filter_hits("  ", &projects, &channels), (vec![], vec![]));
        assert_eq!(filter_hits("zzz", &projects, &channels), (vec![], vec![]));
    }
}
