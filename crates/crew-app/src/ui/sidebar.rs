//! The sidebar: projects with their channels and terminal sessions, then
//! the agents.

use crew_core::ids::{ChannelId, ProjectId};
use crew_core::store::{StoredAgent, StoredChannel, StoredProject};
use crew_proto::TerminalSession;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Disableable as _, Icon, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::{ago, dot, monogram, now_ms, status_word};
use super::settings::Page;
use super::settings::projects::FolderFor;
use super::workspace::{SIDEBAR_WIDTH, Workspace};
use crate::fonts::mono_font;
use crate::model::ThreadStatus;
use crate::theme::color;

/// What a channel row shows on its right: a status, unread messages or
/// nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelBadge {
    Status(ThreadStatus),
    Unread(usize),
    None,
}

impl ChannelBadge {
    pub fn label(&self) -> String {
        match self {
            Self::Status(status) => status_word(*status).0.to_string(),
            Self::Unread(n) => format!("{n} unread"),
            Self::None => String::new(),
        }
    }
}

impl Workspace {
    pub(super) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let state = self.state();
        let projects: Vec<AnyElement> = state
            .projects()
            .into_iter()
            .map(|p| self.render_project(p, cx).into_any_element())
            .collect();
        let agents: Vec<AnyElement> = state
            .agents()
            .into_iter()
            .map(|a| self.render_agent(a).into_any_element())
            .collect();
        let running = state.working_runs().len();
        div()
            .id("sidebar")
            .flex()
            .flex_col()
            .flex_none()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .border_r_1()
            .border_color(color("border-100"))
            .child(
                div()
                    .id("sidebar-scroll")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(8.))
                    .py(px(12.))
                    .child(section_header(
                        "sidebar-projects",
                        "Projects",
                        "Add project",
                        cx.listener(|this, _, window, cx| {
                            this.open_settings(Page::Projects, window, cx);
                            this.choose_folder(FolderFor::Settings, window, cx);
                        }),
                    ))
                    .children(projects)
                    .child(
                        div()
                            .my(px(12.))
                            .mx(px(8.))
                            .h(px(1.))
                            .bg(color("border-100")),
                    )
                    .child(section_header(
                        "sidebar-agents",
                        "Agents",
                        "New agent",
                        cx.listener(|this, _, window, cx| {
                            this.open_settings(Page::Agents, window, cx);
                            this.new_agent(window, cx);
                        }),
                    ))
                    .children(agents),
            )
            .when(running > 0, |sidebar| {
                sidebar.child(render_footer(running, self.stopping_all, cx))
            })
            .test_support()
    }

    fn render_project(&self, project: &StoredProject, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state();
        let id = project.id;
        let open = !self.nav.collapsed.contains(&id);
        let status = state.project_status(id);
        let mut channels: Vec<AnyElement> = if open {
            state
                .channels(id)
                .into_iter()
                .map(|c| self.render_channel(c, cx).into_any_element())
                .collect()
        } else {
            Vec::new()
        };
        if open {
            channels.extend(self.render_terminal_group(id, cx));
        }
        div()
            .flex()
            .flex_col()
            .child(
                row(format!("project-{id}"))
                    .gap(px(6.))
                    .aria_label(project.name.clone())
                    .child(
                        Icon::new(if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(px(14.))
                        .text_color(color("text-tertiary")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .child(project.name.clone()),
                    )
                    .when_some(status_dot(status), |row, dot| row.child(dot))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_project(id, cx)))
                    .test_support(),
            )
            .children(channels)
    }

    fn render_channel(&self, channel: &StoredChannel, cx: &mut Context<Self>) -> impl IntoElement {
        let id = channel.id;
        let selected = self.nav.channel == Some(id);
        let badge = self.channel_badge(id);
        let name = format!("#{}", channel.name);
        row(format!("channel-{id}"))
            .pl(px(28.))
            .gap(px(4.))
            .when(selected, |row| row.bg(color("surface-300")))
            .text_color(color(if selected {
                "text-primary"
            } else {
                "text-secondary"
            }))
            .aria_label(name)
            .child(
                div()
                    .font_family(mono_font())
                    .text_color(color("text-tertiary"))
                    .child("#"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(channel.name.clone()),
            )
            .child(render_badge(id, &badge))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select_channel(id, window, cx);
            }))
            .test_support()
    }

    /// "In terminal · N" under a project with terminal sessions, and the
    /// sessions once you open it.
    fn render_terminal_group(&self, project: ProjectId, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let sessions = self.state().terminal_sessions(project);
        if sessions.is_empty() {
            return Vec::new();
        }
        let open = self.nav.terminal_groups.contains(&project);
        let label = format!("In terminal · {}", sessions.len());
        let mut rows = vec![
            row(format!("terminal-group-{project}"))
                .pl(px(28.))
                .gap(px(4.))
                .text_color(color("text-secondary"))
                .aria_label(label.clone())
                .child(
                    Icon::new(if open {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .size(px(12.))
                    .text_color(color("text-tertiary")),
                )
                .child(div().flex_1().min_w_0().truncate().child(label))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_terminal_group(project, cx);
                }))
                .test_support()
                .into_any_element(),
        ];
        if open {
            let now = now_ms();
            rows.extend(
                sessions
                    .into_iter()
                    .map(|s| self.render_terminal_session(s, now, cx).into_any_element()),
            );
        }
        rows
    }

    fn render_terminal_session(
        &self,
        session: &TerminalSession,
        now: i64,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = session.id.clone();
        let selected = self.terminal.as_ref().is_some_and(|t| t.session == id);
        let title = if session.title.is_empty() {
            "Untitled session".to_string()
        } else {
            session.title.clone()
        };
        let open = id.clone();
        row(format!("terminal-session-{id}"))
            .pl(px(44.))
            .gap(px(6.))
            .when(selected, |row| row.bg(color("surface-300")))
            .text_color(color(if selected {
                "text-primary"
            } else {
                "text-secondary"
            }))
            .aria_label(title.clone())
            .child(div().flex_1().min_w_0().truncate().child(title))
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.))
                    .text_color(color("text-tertiary"))
                    .child(ago(session.last_active_at, now)),
            )
            .when(session.live, |row| {
                row.child(
                    div()
                        .id(SharedString::from(format!("terminal-live-{id}")))
                        .flex_none()
                        .p(px(2.))
                        .aria_label("live")
                        .tooltip(|window, cx| {
                            gpui_kit::component::tooltip::Tooltip::new("live").build(window, cx)
                        })
                        .child(dot("accent-blue"))
                        .test_support(),
                )
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_terminal(open.clone(), window, cx);
            }))
            .test_support()
    }

    fn toggle_terminal_group(&mut self, project: ProjectId, cx: &mut Context<Self>) {
        if !self.nav.terminal_groups.remove(&project) {
            self.nav.terminal_groups.insert(project);
        }
        cx.notify();
    }

    /// The badge of `channel`'s row: urgent states first, then unread
    /// messages. The open channel has none unread.
    pub(super) fn channel_badge(&self, channel: ChannelId) -> ChannelBadge {
        let state = self.state();
        match state.channel_status(channel) {
            ThreadStatus::Idle | ThreadStatus::Queued => {}
            status => return ChannelBadge::Status(status),
        }
        if self.nav.channel == Some(channel) {
            return ChannelBadge::None;
        }
        let since = self.nav.viewed.get(&channel).copied().unwrap_or(0);
        match state.unread(channel, since) {
            0 => ChannelBadge::None,
            n => ChannelBadge::Unread(n),
        }
    }

    fn render_agent(&self, agent: &StoredAgent) -> impl IntoElement {
        let id = agent.id;
        let status = self.state().agent_status(id);
        let (word, token) = status_word(status);
        row(format!("agent-{id}"))
            .gap(px(8.))
            .aria_label(agent.spec.name.clone())
            .child(monogram(&agent.spec.name, 20.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(color(if status == ThreadStatus::Idle {
                        "text-secondary"
                    } else {
                        "text-primary"
                    }))
                    .child(agent.spec.name.clone()),
            )
            .child(
                div()
                    .id(format!("agent-status-{id}"))
                    .flex_none()
                    .text_size(px(11.))
                    .text_color(color(token))
                    .aria_label(word)
                    .child(word)
                    .test_support(),
            )
            .test_support()
    }

    pub(super) fn toggle_project(&mut self, project: ProjectId, cx: &mut Context<Self>) {
        if !self.nav.collapsed.remove(&project) {
            self.nav.collapsed.insert(project);
        }
        cx.notify();
    }
}

/// A 28 px sidebar row.
fn row(id: String) -> Stateful<Div> {
    div()
        .id(SharedString::from(id))
        .flex()
        .items_center()
        .h(px(28.))
        .px(px(8.))
        .rounded(px(6.))
        .cursor_pointer()
        .hover(|row| row.bg(color("surface-200")))
}

/// "Projects" or "Agents", with a plus button.
fn section_header(
    id: &'static str,
    title: &'static str,
    add: &'static str,
    on_add: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .h(px(28.))
        .pl(px(8.))
        .child(
            div()
                .id(id)
                .flex_1()
                .text_size(px(12.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(color("text-secondary"))
                .aria_label(title)
                .child(title)
                .test_support(),
        )
        .child(
            Button::new(SharedString::from(format!("{id}-add")))
                .ghost()
                .xsmall()
                .icon(IconName::Plus)
                .accessibility_label(add)
                .tooltip(add)
                .on_click(on_add),
        )
}

/// A dot for urgent states only: queued runs get none.
fn status_dot(status: ThreadStatus) -> Option<Div> {
    match status {
        ThreadStatus::Idle | ThreadStatus::Queued => None,
        status => Some(dot(status_word(status).1)),
    }
}

fn render_badge(channel: ChannelId, badge: &ChannelBadge) -> impl IntoElement {
    let el = div()
        .id(format!("channel-status-{channel}"))
        .flex_none()
        .aria_label(badge.label());
    match badge {
        ChannelBadge::Status(status) => el.when_some(status_dot(*status), |el, dot| el.child(dot)),
        ChannelBadge::Unread(n) => el
            .px(px(6.))
            .rounded(px(9.))
            .bg(color("surface-300"))
            .text_size(px(11.))
            .font_weight(FontWeight::MEDIUM)
            .text_color(color("text-primary"))
            .child(n.to_string()),
        ChannelBadge::None => el,
    }
    .test_support()
}

/// "N agents running" and Stop all.
fn render_footer(running: usize, stopping: bool, cx: &mut Context<Workspace>) -> impl IntoElement {
    let agents = if running == 1 { "agent" } else { "agents" };
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .h(px(44.))
        .px(px(16.))
        .border_t_1()
        .border_color(color("border-100"))
        .text_size(px(12.))
        .text_color(color("text-secondary"))
        .child(dot("accent-blue"))
        .child({
            let text = format!("{running} {agents} running");
            div()
                .id("sidebar-running")
                .flex_1()
                .aria_label(text.clone())
                .child(text)
                .test_support()
        })
        .child(
            Button::new("sidebar-stop-all")
                .outline()
                .xsmall()
                .text_color(color(super::running_view::stop_all_look(true, stopping).1))
                .icon(IconName::Square)
                .label("Stop all")
                .disabled(stopping)
                .on_click(cx.listener(|this, _, window, cx| this.stop_all(window, cx))),
        )
}
