//! An agent's profile: what it is doing, its week in numbers, its recent
//! runs, its permissions and limits, and the threads it is in.

use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ChannelId, ProjectId, ThreadId};
use crew_proto::{AgentProfile, AgentStats, RunRow};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::channel_view::CardStatus;
use super::common::{dot, monogram, now_ms, runtime_badge, runtime_chip};
use super::project_view::{
    Look, agent_name, caption, link, page_button, page_time, section, status_pill,
};
use super::settings::Page;
use super::settings::agents::{permission_label, run_limit_label};
use super::thread_settings::policy_name;
use super::workspace::{Place, Workspace};
use crate::model::ThreadStatus;
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};

/// The profile's side column.
const SIDE_WIDTH: f32 = 340.;
/// Below this width of the main area the side column goes under the
/// runs.
const TWO_COLUMNS_FROM: f32 = 880.;
/// Threads listed before "Show {n} more".
const THREADS_SHOWN: usize = 4;
/// The run strip's width.
const STRIP_WIDTH: f32 = 220.;

/// `+88 −51` as one mono run of text, each part in its colour.
fn diff_text(added: u32, removed: u32) -> StyledText {
    let plus = format!("+{added}");
    let text = format!("{plus} −{removed}");
    let colour = |token: &str| HighlightStyle {
        color: Some(color(token)),
        ..HighlightStyle::default()
    };
    let highlights = vec![
        (0..plus.len(), colour("success")),
        (plus.len() + 1..text.len(), colour("danger")),
    ];
    StyledText::new(text).with_highlights(highlights)
}

/// How long a run took: `48s`, `5m 02s`, `1h 04m`.
pub fn run_time(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// What a run's pill says, and its colours.
pub fn run_pill(row: &RunRow) -> (CardStatus, &'static str) {
    if row.waiting {
        return (CardStatus::NeedsYou, "Waiting");
    }
    match row.run.status {
        RunStatus::Queued => (CardStatus::Queued, "Queued"),
        RunStatus::Working => (CardStatus::Working, "Working"),
        RunStatus::Done => (CardStatus::Done, "Done"),
        RunStatus::Failed => (CardStatus::Failed, "Failed"),
        RunStatus::Stopped | RunStatus::Interrupted => (CardStatus::Stopped, "Stopped"),
    }
}

/// The run strip's colour for a run.
fn strip_token(row: &RunRow) -> &'static str {
    match run_pill(row).0 {
        CardStatus::Done => "success",
        CardStatus::Failed => "danger",
        CardStatus::NeedsYou => "warning",
        CardStatus::Working => "accent-blue",
        _ => "text-tertiary",
    }
}

/// What the run strip reads as: `Last 18 runs: 15 done, 1 stopped`.
pub fn strip_label(runs: &[RunRow]) -> String {
    let mut parts = Vec::new();
    for (word, status) in [
        ("working", CardStatus::Working),
        ("done", CardStatus::Done),
        ("stopped", CardStatus::Stopped),
        ("failed", CardStatus::Failed),
        ("waiting", CardStatus::NeedsYou),
    ] {
        let n = runs.iter().filter(|r| run_pill(r).0 == status).count();
        if n > 0 {
            parts.push(format!("{n} {word}"));
        }
    }
    let what = if runs.len() == 1 { "run" } else { "runs" };
    format!("Last {} {what}: {}", runs.len(), parts.join(", "))
}

/// The five numbers of the week: each one's label, value and what
/// follows it.
pub fn stat_texts(stats: &AgentStats) -> [(&'static str, String, String); 5] {
    let clean = if stats.finished_week == 0 {
        ("None yet".to_string(), String::new())
    } else {
        let pct = (f64::from(stats.clean_week) * 100. / f64::from(stats.finished_week)).round();
        (
            format!("{pct}%"),
            format!("· {} of {}", stats.clean_week, stats.finished_week),
        )
    };
    let avg = if stats.finished_week == 0 {
        "None yet".to_string()
    } else {
        run_time(stats.avg_run_ms)
    };
    [
        (
            "Runs this week",
            stats.runs_week.to_string(),
            format!("· {} last week", stats.runs_last_week),
        ),
        ("Finished cleanly", clean.0, clean.1),
        ("Avg run time", avg, String::new()),
        (
            "Files changed",
            stats.files_week.to_string(),
            format!("+{} −{}", stats.added_week, stats.removed_week),
        ),
        (
            "Handoffs",
            format!("{} sent", stats.handoffs_sent_week),
            format!("· {} received", stats.handoffs_received_week),
        ),
    ]
}

impl Workspace {
    /// What the agent is doing: waiting on you or working, and where.
    fn agent_doing(&self, agent: AgentId) -> (CardStatus, String) {
        let state = self.state();
        let channel_of = |thread: ThreadId| {
            state
                .thread(thread)
                .and_then(|t| state.channel(t.channel))
                .map(|c| format!(" in #{}", c.name))
                .unwrap_or_default()
        };
        let asking = state
            .needs_you()
            .into_iter()
            .find(|n| state.run(n.approval.run).is_some_and(|r| r.agent == agent));
        if let Some(ask) = asking {
            return (
                CardStatus::NeedsYou,
                format!("Needs you{}", channel_of(ask.approval.thread)),
            );
        }
        let working = state
            .working_runs()
            .into_iter()
            .find(|r| r.agent == agent && r.status == RunStatus::Working);
        if let Some(run) = working {
            return (
                CardStatus::Working,
                format!("Working{}", channel_of(run.thread)),
            );
        }
        (CardStatus::Idle, "Idle".into())
    }

    /// The channel of the agent's latest run that is still around.
    fn agent_last_channel(&self, profile: Option<&AgentProfile>) -> Option<ChannelId> {
        let state = self.state();
        profile
            .into_iter()
            .flat_map(|p| &p.recent_runs)
            .find_map(|r| state.thread(r.run.thread).map(|t| t.channel))
    }

    /// The new thread composer in the agent's last channel, with
    /// `@Name ` written.
    pub(super) fn start_thread_with(
        &mut self,
        agent: AgentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = self.state();
        let Some(name) = state
            .agent(agent)
            .filter(|a| a.deleted_at.is_none())
            .map(|a| a.spec.name.clone())
        else {
            return;
        };
        let profile = self.page.as_ref().and_then(|p| p.profile.as_ref());
        let first = || {
            state
                .projects()
                .into_iter()
                .find_map(|p| state.channels(p.id).first().map(|c| c.id))
        };
        let Some(channel) = self.agent_last_channel(profile).or_else(first) else {
            return;
        };
        self.leave_pages(cx);
        self.select_channel(channel, window, cx);
        self.new_thread(window, cx);
        self.composer.update(cx, |composer, cx| {
            composer.set_text(&format!("@{name} "), window, cx)
        });
    }

    /// Settings › Agents on this agent.
    fn edit_agent_settings(&mut self, agent: AgentId, window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings(Page::Agents, window, cx);
        self.edit_agent(agent, window, cx);
    }

    pub(super) fn render_agent_page(&self, agent: AgentId, cx: &mut Context<Self>) -> AnyElement {
        let Some(page) = &self.page else {
            return div().into_any_element();
        };
        let two = self.main_width() >= scale::chrome(TWO_COLUMNS_FROM);
        let body = page.profile.as_ref().map(|profile| {
            div()
                .flex()
                .flex_col()
                .child(render_stats(&profile.stats))
                .child(
                    div()
                        .flex()
                        .items_start()
                        .gap(ui_px(16.))
                        .when(!two, |cols| cols.flex_col().items_stretch())
                        .child(self.render_recent_runs(profile, cx))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_none()
                                .min_w_0()
                                .gap(ui_px(16.))
                                .map(|side| {
                                    if two {
                                        side.w(ui_px(SIDE_WIDTH))
                                    } else {
                                        side.w_full()
                                    }
                                })
                                .child(self.render_limits(agent, cx))
                                .child(self.render_agent_threads(profile, page.all_threads, cx)),
                        ),
                )
        });
        div()
            .id("agent-page")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&page.scroll)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .px(scale::chrome(24.))
                    .py(ui_px(24.))
                    .child(self.render_agent_header(agent, page.profile.as_ref(), cx))
                    .children(
                        page.error
                            .as_ref()
                            .filter(|_| page.profile.is_none())
                            .map(|error| {
                                let text = format!("Couldn't load this agent. {error}");
                                div()
                                    .id("agent-error")
                                    .text_size(scale::text(12.))
                                    .text_color(color("danger"))
                                    .aria_label(text.clone())
                                    .child(text)
                                    .test_support()
                            }),
                    )
                    .children(body),
            )
            .test_support()
            .into_any_element()
    }

    /// Monogram, name, runtime, what it is doing, its role, Start a thread
    /// and Edit agent.
    fn render_agent_header(
        &self,
        agent: AgentId,
        profile: Option<&AgentProfile>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let state = self.state();
        let Some(stored) = state.agent(agent) else {
            return div().into_any_element();
        };
        let deleted = stored.deleted_at.is_some();
        let name = agent_name(self, agent);
        let (status, doing) = self.agent_doing(agent);
        let role = match profile {
            Some(p) => {
                let projects = if p.projects == 1 {
                    "in 1 project".to_string()
                } else {
                    format!("in {} projects", p.projects)
                };
                if stored.spec.role.trim().is_empty() {
                    projects
                } else {
                    format!("{} · {projects}", stored.spec.role.trim())
                }
            }
            None => stored.spec.role.trim().to_string(),
        };
        let first = stored.spec.name.clone();
        div()
            .flex()
            .items_center()
            .gap(ui_px(14.))
            .mb(ui_px(20.))
            .child(
                monogram(&stored.spec.name, 48.)
                    .rounded(ui_px(10.))
                    .text_size(scale::text(18.)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(ui_px(10.))
                            .min_w_0()
                            .child(
                                div()
                                    .id("agent-name")
                                    .min_w_0()
                                    .truncate()
                                    .text_size(scale::text(24.))
                                    .line_height(scale::line(32.))
                                    .font_weight(weight(FontWeight::SEMIBOLD))
                                    .aria_label(name.clone())
                                    .child(name)
                                    .test_support(),
                            )
                            .child(
                                div()
                                    .id("agent-runtime")
                                    .flex_none()
                                    .aria_label(runtime_chip(&stored.spec))
                                    .child(runtime_badge(&stored.spec))
                                    .test_support(),
                            )
                            .child(status_pill("agent-status", status, &doing).test_support()),
                    )
                    .child(
                        div()
                            .id("agent-role")
                            .truncate()
                            .text_color(color("text-secondary"))
                            .aria_label(role.clone())
                            .child(role)
                            .test_support(),
                    ),
            )
            .when(!deleted, |header| {
                header
                    .child(
                        page_button(
                            "agent-start-thread",
                            format!("Start a thread with {first}"),
                            Look::Primary,
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.start_thread_with(agent, window, cx)
                        }))
                        .test_support(),
                    )
                    .child(
                        page_button("agent-edit", "Edit agent".into(), Look::Outline)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.edit_agent_settings(agent, window, cx)
                            }))
                            .test_support(),
                    )
            })
            .into_any_element()
    }

    /// The runs table: status, thread, where, time, files and when.
    fn render_recent_runs(
        &self,
        profile: &AgentProfile,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let state = self.state();
        let now = now_ms();
        let runs = &profile.recent_runs;
        let strip = div()
            .id("agent-runs-strip")
            .flex()
            .flex_none()
            .gap(ui_px(2.))
            .w(ui_px(STRIP_WIDTH))
            .role(Role::Image)
            .aria_label(strip_label(runs))
            .children(runs.iter().rev().map(|r| {
                div()
                    .flex_1()
                    .h(ui_px(12.))
                    .rounded(ui_px(1.))
                    .bg(color(strip_token(r)))
            }))
            .test_support()
            .into_any_element();
        let grid = |row: Stateful<Div>| {
            row.flex()
                .items_center()
                .gap(ui_px(12.))
                .px(ui_px(16.))
                .text_size(scale::text(12.))
                .text_color(color("text-secondary"))
        };
        let col = |width: f32, right: bool| {
            div()
                .flex()
                .flex_none()
                .min_w_0()
                .w(scale::text(width))
                .when(right, |c| c.justify_end())
        };
        let header = grid(div().id("agent-runs-head"))
            .h(scale::row(32.))
            .border_b_1()
            .border_color(color("surface-200"))
            .text_color(color("text-tertiary"))
            .child(col(96., false).child("Status"))
            .child(div().flex_1().min_w_0().child("Thread"))
            .child(col(150., false).child("Where"))
            .child(col(70., true).child("Time"))
            .child(col(90., true).child("Files"))
            .child(col(72., true).child("When"));
        let count = runs.len();
        let rows = runs.iter().enumerate().map(|(ix, row)| {
            let id = row.run.id;
            let thread = row.run.thread;
            let known = state.thread(thread);
            let (title, place) = match (&row.removed_project, known) {
                (Some(gone), _) => (
                    gone.thread_title.clone(),
                    format!("{} (removed) · #{}", gone.project, gone.channel),
                ),
                (None, Some(t)) => {
                    let channel = state.channel(t.channel);
                    let project = channel.and_then(|c| state.project(c.project));
                    (
                        t.title.clone(),
                        format!(
                            "{} · #{}",
                            project.map(|p| p.name.as_str()).unwrap_or_default(),
                            channel.map(|c| c.name.as_str()).unwrap_or_default()
                        ),
                    )
                }
                (None, None) => ("A thread you can't open".into(), String::new()),
            };
            let (status, word) = run_pill(row);
            let cell = |suffix: &str, text: String| {
                div()
                    .id(SharedString::from(format!("agent-run-{id}-{suffix}")))
                    .min_w_0()
                    .truncate()
                    .aria_label(text.clone())
                    .child(text)
                    .test_support()
            };
            let answer_only = row.files == 0 && row.added == 0 && row.removed == 0;
            let files = if answer_only {
                cell("files", "answer only".into()).into_any_element()
            } else {
                div()
                    .id(SharedString::from(format!("agent-run-{id}-files")))
                    .whitespace_nowrap()
                    .font_family(code_family())
                    .text_size(scale::mono(12.))
                    .aria_label(format!("+{} −{}", row.added, row.removed))
                    .child(diff_text(row.added, row.removed))
                    .test_support()
                    .into_any_element()
            };
            let at = row.run.ended_at.unwrap_or(row.run.started_at);
            grid(div().id(SharedString::from(format!("agent-run-{id}"))))
                .h(scale::row(40.))
                .when(ix + 1 < count, |r| {
                    r.border_b_1().border_color(color("surface-200"))
                })
                .when(known.is_some(), |r| {
                    r.cursor_pointer()
                        .hover(|r| r.bg(color("surface-100")))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_place(Place::Thread(thread), window, cx)
                        }))
                })
                .aria_label(title.clone())
                .child(
                    col(96., false).child(
                        status_pill(
                            SharedString::from(format!("agent-run-{id}-status")),
                            status,
                            word,
                        )
                        .test_support(),
                    ),
                )
                .child(
                    div().flex_1().min_w_0().child(
                        cell("thread", title)
                            .text_size(scale::text(13.))
                            .text_color(color("text-primary")),
                    ),
                )
                .child(col(150., false).child(cell("where", place)))
                .child(
                    col(70., true).child(
                        cell("time", run_time(row.ms))
                            .font_family(code_family())
                            .text_size(scale::mono(12.)),
                    ),
                )
                .child(col(90., true).child(files))
                .child(col(72., true).child(cell("when", page_time(at, now))))
                .test_support()
        });
        let empty = runs.is_empty().then(|| {
            div()
                .id("agent-runs-empty")
                .px(ui_px(16.))
                .py(scale::row(12.))
                .text_size(scale::text(12.))
                .text_color(color("text-tertiary"))
                .aria_label("No runs yet.")
                .child("No runs yet.")
                .test_support()
        });
        section(
            "agent-runs",
            "Recent runs",
            (!runs.is_empty()).then_some(strip),
        )
        .flex_1()
        .when(!runs.is_empty(), |s| s.child(header))
        .children(rows)
        .children(empty)
        .test_support()
    }

    /// What it can do, its longest run and how it brings in others, with
    /// Change.
    fn render_limits(&self, agent: AgentId, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state();
        let Some(stored) = state.agent(agent) else {
            return div().into_any_element();
        };
        let spec = &stored.spec;
        let policy = spec.handoff_policy.map_or("Channel default", policy_name);
        let budget = spec.handoff_budget.unwrap_or(state.limits().handoff_budget);
        let handoffs = if budget == 1 {
            "1 handoff per thread".to_string()
        } else {
            format!("{budget} handoffs per thread")
        };
        let row = |id: &'static str, key: &'static str, value: String| {
            div()
                .flex()
                .items_start()
                .gap(ui_px(12.))
                .px(ui_px(16.))
                .py(scale::row(8.))
                .text_size(scale::text(12.))
                .line_height(scale::line(18.))
                .child(
                    div()
                        .flex_none()
                        .w(scale::text(104.))
                        .text_color(color("text-tertiary"))
                        .child(key),
                )
                .child(
                    div()
                        .id(id)
                        .flex_1()
                        .min_w_0()
                        .text_color(color("text-primary"))
                        .aria_label(value.clone())
                        .child(value)
                        .test_support(),
                )
        };
        let change = (stored.deleted_at.is_none()).then(|| {
            link("agent-limits-change", "Change")
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.edit_agent_settings(agent, window, cx)
                }))
                .test_support()
                .into_any_element()
        });
        section("agent-limits", "Permissions and limits", change)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .py(ui_px(6.))
                    .child(row(
                        "agent-limit-mode",
                        "What it can do",
                        permission_label(spec.permission_mode.clone()).to_string(),
                    ))
                    .child(row(
                        "agent-limit-longest",
                        "Longest run",
                        run_limit_label(spec.max_run_minutes),
                    ))
                    .child(row(
                        "agent-limit-handoffs",
                        "Bringing in others",
                        format!("{policy} · {handoffs}"),
                    )),
            )
            .test_support()
            .into_any_element()
    }

    /// The open threads it has runs in, four of them until you ask for
    /// the rest.
    fn render_agent_threads(
        &self,
        profile: &AgentProfile,
        all: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let state = self.state();
        let threads: Vec<ThreadId> = profile
            .threads
            .iter()
            .copied()
            .filter(|t| state.thread(*t).is_some())
            .collect();
        // Its latest project's threads say their channel; others their
        // project.
        let home: Option<ProjectId> = threads.first().and_then(|t| state.thread_project(*t));
        let shown = if all {
            threads.len()
        } else {
            threads.len().min(THREADS_SHOWN)
        };
        let rows = threads[..shown].iter().map(|&id| {
            let Some(thread) = state.thread(id) else {
                return div().into_any_element();
            };
            let channel = state.channel(thread.channel);
            let project = channel.and_then(|c| state.project(c.project));
            let place = match (channel, project) {
                (Some(c), Some(p)) if Some(p.id) == home => format!("#{}", c.name),
                (_, Some(p)) => p.name.clone(),
                _ => String::new(),
            };
            let token = match state.thread_status(id) {
                ThreadStatus::NeedsYou => "warning",
                ThreadStatus::Working => "accent-blue",
                _ => match state.thread_runs(id).last().map(|r| r.status.clone()) {
                    Some(RunStatus::Done) => "success",
                    Some(RunStatus::Failed) => "danger",
                    _ => "text-tertiary",
                },
            };
            div()
                .id(SharedString::from(format!("agent-thread-{id}")))
                .flex()
                .items_center()
                .gap(ui_px(8.))
                .h(scale::row(32.))
                .px(ui_px(16.))
                .cursor_pointer()
                .hover(|r| r.bg(color("surface-100")))
                .text_size(scale::text(12.))
                .text_color(color("text-secondary"))
                .aria_label(thread.title.clone())
                .child(dot(token))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(color("text-primary"))
                        .child(thread.title.clone()),
                )
                .child(div().flex_none().child(place))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_place(Place::Thread(id), window, cx)
                }))
                .test_support()
                .into_any_element()
        });
        let more = threads.len() - shown;
        section(
            "agent-threads",
            "Threads it's in",
            Some(caption(
                "agent-threads-count",
                format!("{} open", threads.len()),
            )),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .py(ui_px(6.))
                .children(rows)
                .when(more > 0, |list| {
                    list.child(
                        link("agent-threads-more", "")
                            .h(scale::row(32.))
                            .flex()
                            .items_center()
                            .px(ui_px(16.))
                            .aria_label(format!("Show {more} more"))
                            .child(format!("Show {more} more"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(page) = &mut this.page {
                                    page.all_threads = true;
                                }
                                cx.notify();
                            }))
                            .test_support(),
                    )
                }),
        )
        .test_support()
    }
}

/// The week in five numbers.
fn render_stats(stats: &AgentStats) -> impl IntoElement {
    let ids = [
        "agent-stat-runs",
        "agent-stat-clean",
        "agent-stat-avg",
        "agent-stat-files",
        "agent-stat-handoffs",
    ];
    let texts = stat_texts(stats);
    div()
        .id("agent-stats")
        .flex()
        .mb(ui_px(16.))
        .border_1()
        .border_color(color("border-100"))
        .rounded(ui_px(8.))
        .children(
            texts
                .into_iter()
                .zip(ids)
                .enumerate()
                .map(|(ix, ((key, value, after), id))| {
                    let label = if after.is_empty() {
                        format!("{key} {value}")
                    } else {
                        format!("{key} {value} {after}")
                    };
                    // The files' lines read in their colours.
                    let after_el = if ix == 3 {
                        div()
                            .font_family(code_family())
                            .text_size(scale::mono(12.))
                            .child(diff_text(stats.added_week, stats.removed_week))
                            .into_any_element()
                    } else {
                        div().child(after).into_any_element()
                    };
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
                        .when(ix < 4, |cell| {
                            cell.border_r_1().border_color(color("border-100"))
                        })
                        .aria_label(label)
                        .child(
                            div()
                                .text_size(scale::text(12.))
                                .text_color(color("text-tertiary"))
                                .child(key),
                        )
                        .child(
                            // The suffix follows after a space's width.
                            div()
                                .flex()
                                .items_baseline()
                                .gap(ui_px(4.))
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .text_size(scale::text(16.))
                                .line_height(scale::line(24.))
                                .font_weight(weight(FontWeight::SEMIBOLD))
                                .child(value)
                                .child(
                                    div()
                                        .text_size(scale::text(12.))
                                        .font_weight(weight(FontWeight::NORMAL))
                                        .text_color(color("text-tertiary"))
                                        .child(after_el),
                                ),
                        )
                        .test_support()
                }),
        )
}

#[cfg(test)]
mod tests {
    use crew_core::agent::Runtime;
    use crew_core::event::RunStatus;
    use crew_core::ids::{AgentId, RunId, ThreadId};
    use crew_core::store::StoredRun;
    use crew_proto::{AgentStats, RunRow};

    use super::{run_time, stat_texts, strip_label};

    fn row(status: RunStatus, waiting: bool) -> RunRow {
        RunRow {
            run: StoredRun {
                id: RunId::new(),
                thread: ThreadId::new(),
                agent: AgentId::new(),
                session_id: "s".into(),
                status,
                started_at: 1,
                ended_at: Some(2),
                runtime: Runtime::ClaudeCode,
                waiting_for: None,
                skill: None,
            },
            ms: 1,
            files: 0,
            added: 0,
            removed: 0,
            waiting,
            removed_project: None,
        }
    }

    #[test]
    fn run_times_pad_the_smaller_unit() {
        assert_eq!(run_time(48_000), "48s");
        assert_eq!(run_time(302_000), "5m 02s");
        assert_eq!(run_time(3_840_000), "1h 04m");
    }

    #[test]
    fn the_strip_counts_runs_by_how_they_ended() {
        let runs = [
            row(RunStatus::Working, true),
            row(RunStatus::Done, false),
            row(RunStatus::Done, false),
            row(RunStatus::Interrupted, false),
            row(RunStatus::Failed, false),
        ];
        assert_eq!(
            strip_label(&runs),
            "Last 5 runs: 2 done, 1 stopped, 1 failed, 1 waiting"
        );
        assert_eq!(strip_label(&runs[1..2]), "Last 1 run: 1 done");
    }

    #[test]
    fn stats_read_as_the_board_writes_them() {
        let stats = AgentStats {
            runs_week: 18,
            runs_last_week: 6,
            finished_week: 18,
            clean_week: 15,
            avg_run_ms: 290_000,
            files_week: 31,
            added_week: 904,
            removed_week: 288,
            handoffs_sent_week: 4,
            handoffs_received_week: 2,
        };
        let texts = stat_texts(&stats);
        assert_eq!(texts[0].1, "18");
        assert_eq!(texts[0].2, "· 6 last week");
        assert_eq!(
            (texts[1].1.as_str(), texts[1].2.as_str()),
            ("83%", "· 15 of 18")
        );
        assert_eq!(texts[2].1, "4m 50s");
        assert_eq!(
            (texts[3].1.as_str(), texts[3].2.as_str()),
            ("31", "+904 −288")
        );
        assert_eq!(
            (texts[4].1.as_str(), texts[4].2.as_str()),
            ("4 sent", "· 2 received")
        );
        let none = stat_texts(&AgentStats::default());
        assert_eq!(none[1].1, "None yet");
        assert_eq!(none[2].1, "None yet");
    }
}
