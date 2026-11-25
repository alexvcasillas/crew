//! The inspector's Thread tab: who is in the thread, how handoffs run,
//! the working copy and a way to stop it all.

use crew_core::agent::Runtime;
use crew_core::event::RunStatus;
use crew_core::ids::{AgentId, ProjectId, ThreadId};
use crew_core::policy::{self, HandoffPolicy, PolicyInputs};
use crew_core::store::{StoredRun, StoredThread};
use crew_proto::Request;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::{elapsed, monogram, now_ms, runtime_badge_for, runtime_chip};
use super::folder::{
    BRANCH_ICON, FolderMode, IN_PLACE_WARNING, folder_mode, ordinal, small_icon, warning_note,
    write_queue, writers,
};
use super::title_bar::home_relative;
use super::workspace::Workspace;
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};

const NO_WORKING_COPY: &str = "No working copy yet. One is made when an agent first edits files.";

/// The policies in the order the segmented control shows them.
const POLICIES: [(HandoffPolicy, &str, &str); 3] = [
    (HandoffPolicy::AskFirst, "policy-ask-first", "Ask first"),
    (HandoffPolicy::Budget, "policy-budget", "Run with budget"),
    (HandoffPolicy::PerAgent, "policy-per-agent", "Per agent"),
];

pub fn policy_name(policy: HandoffPolicy) -> &'static str {
    POLICIES
        .iter()
        .find(|(p, ..)| *p == policy)
        .map_or("Ask first", |(_, _, name)| name)
}

/// The thread's handoff policy and budget as they apply, without an
/// agent's own settings: thread, then channel, then the defaults.
pub fn effective(
    thread: &StoredThread,
    channel: (Option<HandoffPolicy>, Option<u32>),
) -> (HandoffPolicy, u32) {
    let inputs = PolicyInputs {
        thread: (thread.handoff_policy, thread.handoff_budget),
        agent: (None, None),
        channel,
        used: thread.handoffs_used,
    };
    let policy = thread
        .handoff_policy
        .or(channel.0)
        .unwrap_or(HandoffPolicy::AskFirst);
    (policy, policy::budget(&inputs))
}

/// Whether the bar shows the budget as used up: only when the budget
/// is what decides handoffs.
pub fn used_up(policy: HandoffPolicy, used: u32, budget: u32) -> bool {
    policy == HandoffPolicy::Budget && budget > 0 && used >= budget
}

/// One agent in the thread and where it stands.
struct Participant {
    agent: AgentId,
    name: String,
    chip: Option<(Runtime, String)>,
    status: String,
    token: &'static str,
}

impl Workspace {
    fn participants(&self, thread: ThreadId) -> Vec<Participant> {
        let state = self.state();
        let runs = state.thread_runs(thread);
        let pending = state.pending_approvals(thread);
        let mut agents: Vec<AgentId> = Vec::new();
        for run in &runs {
            if !agents.contains(&run.agent) {
                agents.push(run.agent);
            }
        }
        agents
            .into_iter()
            .map(|agent| {
                let spec = state.agent(agent).map(|a| &a.spec);
                let mine: Vec<_> = runs.iter().filter(|r| r.agent == agent).collect();
                let waits = pending.iter().any(|a| mine.iter().any(|r| r.id == a.run));
                let working = mine.iter().find(|r| r.status == RunStatus::Working);
                let queued = mine.iter().any(|r| r.status == RunStatus::Queued);
                let (status, token) = if waits {
                    ("Waiting for you".to_string(), "warning")
                } else if let Some(run) = working {
                    (
                        format!("Working · {}", elapsed(now_ms() - run.started_at)),
                        "accent-blue",
                    )
                } else if queued {
                    ("Queued".to_string(), "text-tertiary")
                } else {
                    let ms: i64 = mine
                        .iter()
                        .filter_map(|r| r.ended_at.map(|end| end - r.started_at))
                        .sum();
                    (elapsed(ms), "text-secondary")
                };
                Participant {
                    agent,
                    name: spec.map_or_else(|| "Deleted agent".to_string(), |s| s.name.clone()),
                    chip: spec.map(|s| (s.runtime.clone(), runtime_chip(s))),
                    status,
                    token,
                }
            })
            .collect()
    }

    /// The thread's policy and budget as they apply, counting settings
    /// sent but not yet reported back.
    fn shown_handoff(&self, thread: ThreadId) -> Option<(HandoffPolicy, u32)> {
        let state = self.state();
        let t = state.thread(thread)?;
        let (handoff_policy, handoff_budget) = state.thread_handoff(thread);
        let shown = StoredThread {
            handoff_policy,
            handoff_budget,
            ..t.clone()
        };
        Some(effective(&shown, state.channel_handoff(t.channel)))
    }

    /// Moves the thread's budget one step, from where it is when clicked.
    fn step_budget(
        &mut self,
        thread: ThreadId,
        up: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((_, budget)) = self.shown_handoff(thread) else {
            return;
        };
        let next = if up {
            (budget + 1).min(99)
        } else {
            budget.saturating_sub(1)
        };
        let (policy, _) = self.state().thread_handoff(thread);
        self.send_thread_handoff(thread, (policy, Some(next)), window, cx);
    }

    pub(super) fn render_thread_tab(
        &self,
        thread: ThreadId,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let state = self.state();
        let Some(t) = state.thread(thread) else {
            return div().into_any_element();
        };
        let channel_layer = state.channel_handoff(t.channel);
        let channel_default = policy_name(channel_layer.0.unwrap_or(HandoffPolicy::AskFirst));
        let (policy, budget) = self
            .shown_handoff(thread)
            .unwrap_or_else(|| effective(t, channel_layer));
        let used = t.handoffs_used;
        let participants = self.participants(thread);
        let limits: Vec<String> = participants
            .iter()
            .filter_map(|p| {
                state.agent(p.agent).map(|a| {
                    format!(
                        "{}: {}",
                        a.spec.name,
                        super::settings::agents::run_limit_label(a.spec.max_run_minutes)
                    )
                })
            })
            .collect();

        let segments: Vec<_> = POLICIES
            .iter()
            .map(|(p, id, name)| {
                let selected = *p == policy;
                let p = *p;
                div()
                    .id(*id)
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(scale::row(26.))
                    .rounded(ui_px(4.))
                    .when(selected, |s| s.bg(color("surface-300")))
                    .text_color(color(if selected {
                        "text-primary"
                    } else {
                        "text-secondary"
                    }))
                    .font_weight(weight(if selected {
                        FontWeight::MEDIUM
                    } else {
                        FontWeight::NORMAL
                    }))
                    .cursor_pointer()
                    .role(Role::Button)
                    .aria_label(if selected {
                        format!("{name} (selected)")
                    } else {
                        name.to_string()
                    })
                    .child(*name)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let (_, budget) = this.state().thread_handoff(thread);
                        this.send_thread_handoff(thread, (Some(p), budget), window, cx);
                    }))
                    .test_support()
            })
            .collect();
        let down = cx.listener(move |this, _, window, cx| {
            this.step_budget(thread, false, window, cx);
        });
        let up = cx.listener(move |this, _, window, cx| {
            this.step_budget(thread, true, window, cx);
        });
        let fill = if budget == 0 {
            1.
        } else {
            (used as f32 / budget as f32).min(1.)
        };

        let wt = t.worktree.clone();
        let folder = self.folder_working_copy(thread, cx);
        let working_copy = match &wt {
            _ if folder.is_some() => Vec::new(),
            None => vec![
                div()
                    .id("working-none")
                    .text_color(color("text-tertiary"))
                    .aria_label(NO_WORKING_COPY)
                    .child(NO_WORKING_COPY)
                    .test_support()
                    .into_any_element(),
            ],
            Some(wt) => {
                let uncommitted = match &self.inspector.diff {
                    None => "Checking…".to_string(),
                    Some(d) if d.files.is_empty() => "Nothing uncommitted".to_string(),
                    Some(d) => {
                        let added: u32 = d.files.iter().map(|f| f.added).sum();
                        let removed: u32 = d.files.iter().map(|f| f.removed).sum();
                        let n = d.files.len();
                        format!(
                            "{n} {} · +{added} −{removed}",
                            if n == 1 { "file" } else { "files" }
                        )
                    }
                };
                vec![
                    value_row("Branch", "working-branch", wt.branch.clone(), true)
                        .into_any_element(),
                    value_row("Folder", "working-folder", home_relative(&wt.path), true)
                        .into_any_element(),
                    value_row("Uncommitted", "working-uncommitted", uncommitted, false)
                        .into_any_element(),
                ]
            }
        };

        div()
            .id("thread-settings")
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .px(ui_px(16.))
            .py(scale::row(12.))
            .gap(ui_px(20.))
            .text_size(scale::text(12.))
            .child(
                section("In this thread").children(if participants.is_empty() {
                    vec![help("No agent has run here yet.").into_any_element()]
                } else {
                    participants
                        .into_iter()
                        .map(|p| {
                            div()
                                .id(format!("participant-{}", p.agent))
                                .flex()
                                .items_center()
                                .gap(ui_px(8.))
                                .h(scale::row(28.))
                                .aria_label(format!("{} {}", p.name, p.status))
                                .child(monogram(&p.name, 20.))
                                .child(
                                    div()
                                        .font_weight(weight(FontWeight::MEDIUM))
                                        .text_size(scale::text(13.))
                                        .child(p.name),
                                )
                                .children(
                                    p.chip.map(|(runtime, text)| runtime_badge_for(&runtime, text)),
                                )
                                .child(div().flex_1())
                                .child(div().text_color(color(p.token)).child(p.status))
                                .test_support()
                                .into_any_element()
                        })
                        .collect()
                }),
            )
            .child(
                section("When an agent mentions another agent")
                    .child(help(format!(
                        "Applies to this thread. The channel default is {channel_default}."
                    )))
                    .child(
                        div()
                            .flex()
                            .gap(ui_px(2.))
                            .p(ui_px(2.))
                            .border_1()
                            .border_color(color("border-200"))
                            .rounded(ui_px(6.))
                            .children(segments),
                    ),
            )
            .child(
                section("Handoffs per thread").child(
                    div()
                        .flex()
                        .items_center()
                        .gap(ui_px(4.))
                        .child(
                            Button::new("budget-down")
                                .outline()
                                .small()
                                .icon(IconName::Minus)
                                .on_click(down),
                        )
                        .child(
                            div()
                                .id("budget-value")
                                .min_w(ui_px(32.))
                                .text_center()
                                .font_family(code_family())
                                .text_size(scale::mono(13.))
                                .aria_label(budget.to_string())
                                .child(budget.to_string())
                                .test_support(),
                        )
                        .child(
                            Button::new("budget-up")
                                .outline()
                                .small()
                                .icon(IconName::Plus)
                                .on_click(up),
                        ),
                ),
            )
            .child(
                section("Longest single run")
                    .child({
                        let text = if limits.is_empty() {
                            "Set per agent".to_string()
                        } else {
                            limits.join(" · ")
                        };
                        div()
                            .id("thread-run-limits")
                            .text_color(color("text-primary"))
                            .aria_label(text.clone())
                            .child(text)
                            .test_support()
                    })
                    .child(help("Set per agent in Settings.")),
            )
            .child(
                section("Handoffs used")
                    .child(
                        div()
                            .id("handoffs-used")
                            .font_family(code_family())
                            .text_color(color("text-primary"))
                            .aria_label(format!("{used} / {budget}"))
                            .child(format!("{used} / {budget}"))
                            .test_support(),
                    )
                    .child(
                        div()
                            .h(ui_px(4.))
                            .w_full()
                            .rounded_full()
                            .bg(color("surface-300"))
                            .child(div().h_full().w(relative(fill)).rounded_full().bg(color(
                                if used_up(policy, used, budget) {
                                    "warning"
                                } else {
                                    "text-secondary"
                                },
                            ))),
                    )
                    .when(policy != HandoffPolicy::Budget, |s| {
                        s.child(help(
                            "Every handoff that ran counts. The limit applies only with Run with budget.",
                        ))
                    }),
            )
            .map(|tab| match folder {
                Some(sections) => tab.children(sections),
                None => tab.child(section("Working copy").children(working_copy)),
            })
            .child(
                Button::new("stop-thread")
                    .outline()
                    .w_full()
                    .text_color(color("danger"))
                    .icon(IconName::Square)
                    .label("Stop everything in this thread")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.request(Request::StopThread { thread }, window, cx);
                    })),
            )
            .test_support()
            .into_any_element()
    }
}

impl Workspace {
    /// The Working copy of a folder project's thread and what follows
    /// it, or `None` for a single repo with a worktree per thread.
    fn folder_working_copy(
        &self,
        thread: ThreadId,
        cx: &mut Context<Self>,
    ) -> Option<Vec<AnyElement>> {
        let state = self.state();
        let mode = folder_mode(state, thread)?;
        let project_id = state.thread_project(thread)?;
        let project = state.project(project_id)?;
        let sections = match mode {
            FolderMode::OwnCopy => self.own_copy_sections(thread, project_id),
            FolderMode::InPlace => {
                let mut working = section("Working copy").child(path_hint(
                    "working-in-place",
                    "Edits go straight to",
                    home_relative(&project.root),
                ));
                let name = |run: &StoredRun| {
                    state
                        .agent(run.agent)
                        .map_or_else(|| "Agent".to_string(), |a| a.spec.name.clone())
                };
                for (ix, run) in writers(state, project_id).into_iter().enumerate() {
                    let who = name(run);
                    working = working.child(
                        kv_row(("working-writer", ix))
                            .aria_label(format!("{who} is writing"))
                            .child(ringed(monogram(&who, 18.)))
                            .child(div().text_color(color("text-primary")).child(who))
                            .child(div().flex_1().child("is writing"))
                            .child(
                                div()
                                    .font_family(code_family())
                                    .text_color(color("accent-blue"))
                                    .child(elapsed(now_ms() - run.started_at)),
                            )
                            .test_support(),
                    );
                }
                for (ix, run) in write_queue(state, project_id).into_iter().enumerate() {
                    let who = name(run);
                    let id = run.id;
                    let place = format!("waits to write · {} in queue", ordinal(ix + 1));
                    working = working.child(
                        kv_row(("working-waiting", ix))
                            .aria_label(format!("{who} {place}"))
                            .child(monogram(&who, 18.))
                            .child(div().text_color(color("text-primary")).child(who))
                            .child(div().flex_1().min_w_0().truncate().child(place))
                            .child(
                                Button::new(("working-cancel", ix))
                                    .ghost()
                                    .xsmall()
                                    .label("Cancel")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.stop_run(id, window, cx);
                                    })),
                            )
                            .test_support(),
                    );
                }
                let mut sections = vec![working.into_any_element()];
                let changes = &self.inspector.changes;
                if !changes.is_empty() {
                    sections.push(
                        section("Changed so far")
                            .children(changes.iter().enumerate().map(|(ix, f)| {
                                let mut label = f.path.clone();
                                if f.added > 0 {
                                    label.push_str(&format!(" +{}", f.added));
                                }
                                if f.removed > 0 {
                                    label.push_str(&format!(" −{}", f.removed));
                                }
                                kv_row(("working-changed", ix))
                                    .aria_label(label)
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .font_family(code_family())
                                            .text_color(color("text-primary"))
                                            .child(f.path.clone()),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .gap(ui_px(4.))
                                            .font_family(code_family())
                                            .when(f.added > 0, |d| {
                                                d.child(
                                                    div()
                                                        .text_color(color("success"))
                                                        .child(format!("+{}", f.added)),
                                                )
                                            })
                                            .when(f.removed > 0, |d| {
                                                d.child(
                                                    div()
                                                        .text_color(color("danger"))
                                                        .child(format!("−{}", f.removed)),
                                                )
                                            }),
                                    )
                                    .test_support()
                            }))
                            .into_any_element(),
                    );
                }
                sections.push(warning_note("working-warning", IN_PLACE_WARNING).into_any_element());
                sections
            }
        };
        Some(sections)
    }

    /// A thread folder of a folder of repos: each repo's worktree, then
    /// the plain files it leaves out.
    fn own_copy_sections(&self, thread: ThreadId, project_id: ProjectId) -> Vec<AnyElement> {
        let state = self.state();
        let Some(project) = state.project(project_id) else {
            return Vec::new();
        };
        let Some(wt) = state.thread(thread).and_then(|t| t.worktree.as_ref()) else {
            return vec![
                section("Working copy")
                    .child(
                        div()
                            .id("working-none")
                            .text_color(color("text-tertiary"))
                            .aria_label(NO_WORKING_COPY)
                            .child(NO_WORKING_COPY)
                            .test_support(),
                    )
                    .into_any_element(),
            ];
        };
        let mut working = section("Working copy").child(path_hint(
            "working-folder-hint",
            "Thread folder",
            format!("{} · {}", project.name, wt.branch),
        ));
        for (ix, repo) in wt.repos.iter().enumerate() {
            let changed = self.inspector.diff.as_ref().map(|d| {
                d.repos
                    .iter()
                    .find(|r| r.repo == repo.repo)
                    .map_or(0, |r| r.diff.files.len())
            });
            let changed = match changed {
                None => "Checking…".to_string(),
                Some(0) => "clean".to_string(),
                Some(n) => format!("{n} changed"),
            };
            working = working.child(
                kv_row(("working-repo", ix))
                    .aria_label(format!("{} {} {changed}", repo.repo, wt.branch))
                    .child(small_icon(BRANCH_ICON, "text-secondary"))
                    .child(
                        div()
                            .flex_none()
                            .w(scale::mono(48.))
                            .truncate()
                            .font_family(code_family())
                            .text_color(color("text-primary"))
                            .child(repo.repo.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(code_family())
                            .child(wt.branch.clone()),
                    )
                    .child(div().flex_none().child(changed))
                    .test_support(),
            );
        }
        let mut sections = vec![working.into_any_element()];
        let top = self
            .inspector
            .top_files
            .as_ref()
            .filter(|(p, _)| *p == project_id)
            .map(|(_, files)| files.clone())
            .unwrap_or_default();
        if !top.is_empty() {
            let folder = project
                .root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| project.name.clone());
            let text = format!(
                "Plain files at the top of {folder}/ stay out of the thread folder: {}. Agents can read them from the original folder but not change them.",
                top.join(", ")
            );
            sections.push(
                section("Not copied")
                    .child(
                        div()
                            .id("working-not-copied")
                            .line_height(scale::line(18.))
                            .text_color(color("text-tertiary"))
                            .aria_label(text.clone())
                            .child(text)
                            .test_support(),
                    )
                    .into_any_element(),
            );
        }
        sections
    }
}

/// "{words} {path}", the path in mono, as the Working copy's first line.
fn path_hint(id: &'static str, words: &'static str, path: String) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .gap(ui_px(4.))
        .line_height(scale::line(18.))
        .text_color(color("text-tertiary"))
        .aria_label(format!("{words} {path}"))
        .child(div().flex_none().child(words))
        .child(
            div()
                .min_w_0()
                .truncate()
                .font_family(code_family())
                .text_color(color("text-secondary"))
                .child(path),
        )
        .test_support()
}

/// A 24 px row of the Working copy: things side by side, 12 px.
fn kv_row(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(ui_px(8.))
        .h(scale::row(24.))
        .whitespace_nowrap()
        .text_color(color("text-secondary"))
}

/// An avatar with the blue ring of an agent at work.
fn ringed(avatar: impl IntoElement) -> impl IntoElement {
    div()
        .flex_none()
        .p(ui_px(1.))
        .rounded(ui_px(7.))
        .border_1()
        .border_color(color("accent-blue"))
        .child(avatar)
}

/// A titled group of the Thread tab.
fn section(title: &str) -> Div {
    div().flex().flex_col().gap(ui_px(6.)).child(
        div()
            .text_color(color("text-secondary"))
            .font_weight(weight(FontWeight::MEDIUM))
            .child(title.to_string()),
    )
}

fn help(text: impl Into<SharedString>) -> Div {
    div().text_color(color("text-tertiary")).child(text.into())
}

fn value_row(label: &str, id: &'static str, value: String, mono: bool) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(ui_px(8.))
        .h(scale::row(24.))
        .child(
            div()
                .w(ui_px(96.))
                .flex_none()
                .text_color(color("text-tertiary"))
                .child(label.to_string()),
        )
        .child(
            div()
                .id(id)
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(color("text-primary"))
                .when(mono, |v| v.font_family(code_family()))
                .aria_label(value.clone())
                .child(value)
                .test_support(),
        )
}

#[cfg(test)]
mod tests {
    use super::{effective, used_up};
    use crew_core::ids::{ChannelId, ThreadId};
    use crew_core::policy::HandoffPolicy;
    use crew_core::store::StoredThread;

    #[test]
    fn the_bar_warns_only_when_the_budget_decides() {
        assert!(used_up(HandoffPolicy::Budget, 3, 3));
        assert!(!used_up(HandoffPolicy::Budget, 2, 3));
        assert!(!used_up(HandoffPolicy::AskFirst, 3, 3));
        assert!(!used_up(HandoffPolicy::Budget, 0, 0));
    }

    #[test]
    fn the_thread_falls_back_to_its_channel_then_the_defaults() {
        let mut t = StoredThread {
            id: ThreadId::new(),
            channel: ChannelId::new(),
            title: "t".into(),
            created_at: 0,
            worktree: None,
            handoff_policy: None,
            handoff_budget: None,
            handoffs_used: 1,
            archived_at: None,
            title_source: Default::default(),
        };
        assert_eq!(effective(&t, (None, None)), (HandoffPolicy::AskFirst, 3));
        assert_eq!(
            effective(&t, (Some(HandoffPolicy::Budget), Some(5))),
            (HandoffPolicy::Budget, 5)
        );
        t.handoff_policy = Some(HandoffPolicy::PerAgent);
        t.handoff_budget = Some(2);
        assert_eq!(
            effective(&t, (Some(HandoffPolicy::Budget), Some(5))),
            (HandoffPolicy::PerAgent, 2)
        );
    }
}
