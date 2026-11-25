//! The threads sidebar's project switcher: All projects, or one project,
//! whose channels then show as chips above the list.

use crew_core::ids::ProjectId;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::super::common::{dot, now_ms};
use super::super::workspace::Workspace;
use super::filter::{CHECK_SVG, FOLDER_PLUS_SVG};
use super::projects::{group_projects, sort_projects};
use super::threads::project_badge;
use crate::model::ThreadStatus;
use crate::prefs::{ShowProjects, SortProjects};
use crate::theme::color;
use crate::theme::scale::{self, ui_px, weight};

actions!(
    project_switcher,
    [
        /// The next choice.
        SwitcherDown,
        /// The choice above.
        SwitcherUp,
        /// Takes the highlighted choice.
        SwitcherPick,
        /// Closes the switcher.
        SwitcherClose,
    ]
);

/// Where the switcher opens in the sidebar, as the board has it.
const SWITCHER_TOP: f32 = 82.;

/// The switcher's state, kept in memory.
pub(crate) struct Switcher {
    pub(crate) open: bool,
    /// The projects past Pinned and Active are listed.
    more: bool,
    /// The highlighted choice.
    pub(crate) cursor: usize,
    pub(super) find: Entity<InputState>,
    query: String,
    /// Where the press that closed it was, so the button under it
    /// doesn't open it again.
    closed_at: Option<Point<Pixels>>,
    _find_events: Subscription,
}

impl Switcher {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let find = cx.new(|cx| InputState::new(window, cx).placeholder("Find a project"));
        let events = cx.subscribe(&find, |this: &mut Workspace, input, event, cx| {
            if !matches!(event, InputEvent::Change) {
                return;
            }
            let query = input.read(cx).value().to_string();
            let s = &mut this.sidebar.threads.switcher;
            if query != s.query {
                s.query = query;
                s.cursor = 0;
                cx.notify();
            }
        });
        Self {
            open: false,
            more: false,
            cursor: 0,
            find,
            query: String::new(),
            closed_at: None,
            _find_events: events,
        }
    }
}

/// A choice of the switcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    All,
    Project(ProjectId),
    /// "{n} more": lists the rest.
    More(usize),
    Add,
}

/// A line of the switcher: a heading or a choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Line {
    Heading(&'static str),
    Choice(Choice),
    Rule,
}

impl Workspace {
    /// The switcher's lines: All projects, Pinned, Active, the rest under
    /// "{n} more", then Add project. Finding lists every project whose
    /// name holds the text.
    fn switcher_lines(&self) -> Vec<Line> {
        let state = self.state();
        let s = &self.sidebar.threads.switcher;
        let facts = state.project_facts();
        let query = s.query.trim().to_lowercase();
        let infos = state
            .projects()
            .into_iter()
            .filter(|p| query.is_empty() || p.name.to_lowercase().contains(&query))
            .map(|p| super::project_info(p, facts.get(&p.id)))
            .collect::<Vec<_>>();
        let mut lines = vec![Line::Choice(Choice::All)];
        if !query.is_empty() {
            let mut infos = infos;
            sort_projects(&mut infos, SortProjects::Recent);
            lines.extend(infos.iter().map(|p| Line::Choice(Choice::Project(p.id))));
        } else {
            let groups = group_projects(
                infos,
                ShowProjects::All,
                SortProjects::Recent,
                now_ms(),
                None,
            );
            if !groups.pinned.is_empty() {
                lines.push(Line::Heading("Pinned"));
                lines.extend(
                    groups
                        .pinned
                        .iter()
                        .map(|p| Line::Choice(Choice::Project(*p))),
                );
            }
            if !groups.active.is_empty() {
                lines.push(Line::Heading("Active"));
                lines.extend(
                    groups
                        .active
                        .iter()
                        .map(|p| Line::Choice(Choice::Project(*p))),
                );
            }
            if !groups.other.is_empty() {
                if s.more {
                    lines.extend(
                        groups
                            .other
                            .iter()
                            .map(|p| Line::Choice(Choice::Project(*p))),
                    );
                } else {
                    lines.push(Line::Choice(Choice::More(groups.other.len())));
                }
            }
        }
        lines.push(Line::Rule);
        lines.push(Line::Choice(Choice::Add));
        lines
    }

    fn switcher_choices(&self) -> Vec<Choice> {
        self.switcher_lines()
            .into_iter()
            .filter_map(|l| match l {
                Line::Choice(c) => Some(c),
                _ => None,
            })
            .collect()
    }

    /// Opens the switcher on the choice in use, or closes it.
    fn toggle_switcher(&mut self, click: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let pressed_at = match click {
            ClickEvent::Mouse(m) => Some(m.down.position),
            _ => None,
        };
        let s = &mut self.sidebar.threads.switcher;
        if pressed_at.is_some() && s.closed_at.take() == pressed_at {
            return;
        }
        if s.open {
            self.close_switcher(window, cx);
            return;
        }
        self.sidebar.menu_open = false;
        self.sidebar.threads.hovered = None;
        let s = &mut self.sidebar.threads.switcher;
        s.open = true;
        s.more = false;
        s.query.clear();
        let find = s.find.clone();
        find.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.focus(window, cx);
        });
        let picked = match self.sidebar.threads.project {
            Some(p) => Choice::Project(p),
            None => Choice::All,
        };
        let cursor = self
            .switcher_choices()
            .iter()
            .position(|c| *c == picked)
            .unwrap_or(0);
        self.sidebar.threads.switcher.cursor = cursor;
        cx.notify();
    }

    /// Closes the switcher; typing goes back to the search.
    fn close_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar.threads.switcher.open = false;
        self.sidebar
            .threads
            .search
            .update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn move_switcher(&mut self, by: isize, cx: &mut Context<Self>) {
        let count = self.switcher_choices().len();
        let s = &mut self.sidebar.threads.switcher;
        s.cursor = s
            .cursor
            .saturating_add_signed(by)
            .min(count.saturating_sub(1));
        cx.notify();
    }

    fn pick_switcher_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.sidebar.threads.switcher.cursor;
        if let Some(choice) = self.switcher_choices().get(cursor).copied() {
            self.take_choice(choice, window, cx);
        }
    }

    fn take_choice(&mut self, choice: Choice, window: &mut Window, cx: &mut Context<Self>) {
        match choice {
            Choice::More(_) => {
                self.sidebar.threads.switcher.more = true;
                cx.notify();
            }
            Choice::Add => {
                self.close_switcher(window, cx);
                self.open_add_project(window, cx);
            }
            Choice::All | Choice::Project(_) => {
                let project = match choice {
                    Choice::Project(p) => Some(p),
                    _ => None,
                };
                let t = &mut self.sidebar.threads;
                if t.project != project {
                    t.project = project;
                    t.channel = None;
                    t.cursor = None;
                    t.list.scroll_to(ListOffset::default());
                }
                self.close_switcher(window, cx);
            }
        }
    }

    /// The button that opens the switcher: "All projects" or the picked
    /// project's name.
    pub(super) fn render_switcher_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.sidebar.threads.switcher.open;
        let name = self
            .sidebar
            .threads
            .project
            .and_then(|p| self.state().project(p))
            .map_or_else(|| "All projects".to_string(), |p| p.name.clone());
        div()
            .id("threads-switcher")
            .flex()
            .flex_none()
            .items_center()
            .gap(ui_px(6.))
            .h(scale::row(28.))
            .mx(ui_px(8.))
            .mb(scale::row(6.))
            .px(ui_px(8.))
            .border_1()
            .border_color(color(if open { "accent-blue" } else { "border-200" }))
            .rounded(ui_px(6.))
            .bg(color("bg-100"))
            .cursor_pointer()
            .text_size(scale::text(12.))
            .font_weight(weight(FontWeight::MEDIUM))
            .text_color(color("text-primary"))
            .aria_label(name.clone())
            .aria_expanded(open)
            .child(div().flex_1().min_w_0().truncate().child(name))
            .child(
                Icon::new(if open {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronDown
                })
                .size(ui_px(12.))
                .text_color(color("text-secondary")),
            )
            .on_click(cx.listener(|this, click, window, cx| {
                this.toggle_switcher(click, window, cx);
            }))
            .test_support()
    }

    /// The switcher, open under its button.
    pub(super) fn render_switcher(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state();
        let s = &self.sidebar.threads.switcher;
        let picked = self.sidebar.threads.project;
        // As many as All projects lists, Show counted.
        let shown = self.shown_projects(self.threads_now());
        let threads: usize = state
            .projects()
            .into_iter()
            .filter(|p| shown.contains(&p.id))
            .flat_map(|p| state.channels(p.id))
            .map(|c| state.threads(c.id).len())
            .sum();
        let mut ix = 0;
        let mut children: Vec<AnyElement> = Vec::new();
        for line in self.switcher_lines() {
            let el = match line {
                Line::Heading(text) => div()
                    .px(ui_px(8.))
                    .pt(scale::row(6.))
                    .pb(scale::row(2.))
                    .text_size(scale::text(11.))
                    .font_weight(weight(FontWeight::MEDIUM))
                    .text_color(color("text-tertiary"))
                    .child(text.to_uppercase())
                    .into_any_element(),
                Line::Rule => div()
                    .my(scale::row(4.))
                    .h(ui_px(1.))
                    .bg(color("border-100"))
                    .into_any_element(),
                Line::Choice(choice) => {
                    let highlighted = ix == s.cursor;
                    ix += 1;
                    let row = option(choice, highlighted);
                    let row = match choice {
                        Choice::All => row
                            .aria_label("All projects")
                            .aria_selected(picked.is_none())
                            .child(
                                div()
                                    .flex()
                                    .flex_none()
                                    .justify_center()
                                    .w(ui_px(12.))
                                    .when(picked.is_none(), |ck| {
                                        ck.child(Icon::default().data(CHECK_SVG).size(ui_px(12.)))
                                    }),
                            )
                            .child(div().flex_1().child("All projects"))
                            .child({
                                let text = format!(
                                    "{threads} {}",
                                    if threads == 1 { "thread" } else { "threads" }
                                );
                                div()
                                    .id("threads-switcher-count")
                                    .text_size(scale::text(11.))
                                    .text_color(color("text-tertiary"))
                                    .aria_label(text.clone())
                                    .child(text)
                                    .test_support()
                            }),
                        Choice::Project(p) => {
                            let name = state.project(p).map(|p| p.name.clone()).unwrap_or_default();
                            let badge = self.sidebar.threads.badges.get(&p);
                            let status = match state.project_status(p) {
                                ThreadStatus::NeedsYou => Some(("warning", "Needs you")),
                                ThreadStatus::Working => Some(("accent-blue", "Agent working")),
                                _ => None,
                            };
                            row.aria_label(name.clone())
                                .aria_selected(picked == Some(p))
                                .children(badge.map(project_badge))
                                .child(div().flex_1().min_w_0().truncate().child(name))
                                .children(status.map(|(token, word)| {
                                    div()
                                        .id(SharedString::from(format!("switcher-status-{p}")))
                                        .aria_label(word)
                                        .child(dot(token))
                                        .test_support()
                                }))
                        }
                        Choice::More(n) => {
                            let text = format!("{n} more");
                            row.aria_label(text.clone())
                                .text_color(color("text-tertiary"))
                                .child(div().flex_none().w(ui_px(16.)))
                                .child(text)
                        }
                        Choice::Add => row
                            .aria_label("Add project…")
                            .text_color(color("text-secondary"))
                            .child(Icon::default().data(FOLDER_PLUS_SVG).size(ui_px(14.)))
                            .child("Add project…"),
                    };
                    row.on_click(cx.listener(move |this, _, window, cx| {
                        this.take_choice(choice, window, cx);
                    }))
                    .test_support()
                    .into_any_element()
                }
            };
            children.push(el);
        }
        div()
            .id("threads-switcher-menu")
            .key_context("ProjectSwitcher")
            .absolute()
            .left(ui_px(8.))
            .right(ui_px(8.))
            .top(scale::row(SWITCHER_TOP))
            .flex()
            .flex_col()
            .p(ui_px(4.))
            .bg(color("surface-raised"))
            .border_1()
            .border_color(color("border-200"))
            .rounded(ui_px(8.))
            .shadow_lg()
            .occlude()
            .aria_label("Projects")
            .on_action(cx.listener(|this, _: &SwitcherDown, _, cx| this.move_switcher(1, cx)))
            .on_action(cx.listener(|this, _: &SwitcherUp, _, cx| this.move_switcher(-1, cx)))
            .on_action(cx.listener(|this, _: &SwitcherPick, window, cx| {
                this.pick_switcher_cursor(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &SwitcherClose, window, cx| this.close_switcher(window, cx)),
            )
            .on_mouse_down_out(cx.listener(|this, e: &MouseDownEvent, window, cx| {
                let s = &mut this.sidebar.threads.switcher;
                s.open = false;
                s.closed_at = Some(e.position);
                this.focus_workspace(window, cx);
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(ui_px(6.))
                    .h(scale::row(32.))
                    .px(ui_px(8.))
                    .mb(scale::row(4.))
                    .border_b_1()
                    .border_color(color("border-100"))
                    .text_color(color("text-tertiary"))
                    .child(Icon::new(IconName::Search).size(ui_px(13.)))
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&s.find)
                                .appearance(false)
                                .aria_label("Find a project")
                                .text_size(scale::text(12.)),
                        ),
                    ),
            )
            .children(children)
            .child(
                div()
                    .px(ui_px(8.))
                    .py(scale::row(6.))
                    .text_size(scale::text(11.))
                    .text_color(color("text-tertiary"))
                    .child("Picking a project shows its channels as chips above the list."),
            )
            .test_support()
    }
}

/// A 32 px choice, highlighted when the keys are on it.
fn option(choice: Choice, highlighted: bool) -> Stateful<Div> {
    let id = match choice {
        Choice::All => "switcher-all".to_string(),
        Choice::Project(p) => format!("switcher-project-{p}"),
        Choice::More(_) => "switcher-more".to_string(),
        Choice::Add => "switcher-add".to_string(),
    };
    div()
        .id(SharedString::from(id))
        .flex()
        .items_center()
        .gap(ui_px(8.))
        .h(scale::row(32.))
        .px(ui_px(8.))
        .rounded(ui_px(4.))
        .cursor_pointer()
        .text_size(scale::text(13.))
        .text_color(color("text-primary"))
        .when(highlighted, |row| row.bg(color("surface-300")))
        .hover(|row| row.bg(color("surface-300")))
}
