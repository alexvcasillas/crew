//! The project overview: its folder, how its channels did this week, its
//! recent threads, the agents that ran there, terminal sessions, setup
//! files and removing it. Also what the overview and the agent profile
//! share: the open page, asking crewd for it, and their pieces.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use crew_core::agent::{AgentSpec, Runtime};
use crew_core::ids::{AgentId, ChannelId, ProjectId, ThreadId};
use crew_core::project_kind::{ProjectKind, WorkMode};
use crew_proto::{AgentProfile, ProjectOverview, Request, ThreadState};
use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::channel_view::{CardStatus, pill_tokens, retry_after};
use super::common::{chip, codex_short, monogram, now_ms, provider_icon, short_model};
use super::dialogs::{repos_line, short_remote, size_label};
use super::running_view::usage_time;
use super::title_bar::home_relative;
use super::workspace::{Place, Workspace};
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};

/// How long a page waits after a change before asking crewd again.
/// At most one answer per this while agents work.
const REFETCH_AFTER: Duration = Duration::from_secs(3);
/// How long the overview keeps the folder's git state and setup files
/// before a refresh looks at the folder again.
const FOLDER_FRESH: Duration = Duration::from_secs(60);
/// Below this width of the main area the side column goes under the
/// main one.
const TWO_COLUMNS_FROM: f32 = 880.;
/// The overview's side column.
const SIDE_WIDTH: f32 = 360.;
/// The danger box's and Remove's outline, as the board has it.
const DANGER_BORDER: u32 = 0x3f1c1e;
/// The setup files help, as the board has it.
pub const SETUP_HELP: &str =
    "Copied into every new worktree. Git ignores them, so agents would not see them otherwise.";

/// The branch mark of the overview's git line.
const BRANCH_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><circle cx="4.5" cy="3.5" r="1.5"/><circle cx="4.5" cy="12.5" r="1.5"/><circle cx="11.5" cy="5.5" r="1.5"/><path d="M4.5 5v6M11.5 7c0 3-7 2-7 4"/></svg>"#;
/// The terminal mark of a terminal session row.
const TERMINAL_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><rect x="2" y="3" width="12" height="10" rx="1.5"/><path d="M4.5 6.5l2 1.5-2 1.5"/></svg>"#;

/// Which page shows in the main area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PageKind {
    Project(ProjectId),
    Agent(AgentId),
}

/// The project overview or agent profile on screen, and what crewd said
/// about it.
pub(super) struct OpenPage {
    pub(super) kind: PageKind,
    pub(super) overview: Option<ProjectOverview>,
    pub(super) profile: Option<AgentProfile>,
    /// Why the last answer failed, shown until one loads.
    pub(super) error: Option<String>,
    failures: u32,
    fetch: Option<Task<()>>,
    /// Asks again a little after a change, or after a failure.
    refetch: Option<Task<()>>,
    /// Something it shows changed since the last answer.
    dirty: bool,
    pub(super) scroll: ScrollHandle,
    /// Scroll to the setup files once they are drawn.
    pub(super) to_setup: Rc<Cell<bool>>,
    /// The agent's threads are all listed.
    pub(super) all_threads: bool,
    /// When the overview last looked at the folder.
    folder_seen: Option<std::time::Instant>,
    /// The next answer looks at the folder whatever its age.
    folder_stale: bool,
    /// The project's worktree folders and their branches, for the
    /// terminal sessions, as of `branches_at`.
    pub(super) branches: std::collections::HashMap<PathBuf, String>,
    branches_at: Option<u64>,
}

impl OpenPage {
    fn new(kind: PageKind) -> Self {
        Self {
            kind,
            overview: None,
            profile: None,
            error: None,
            failures: 0,
            fetch: None,
            refetch: None,
            dirty: false,
            scroll: ScrollHandle::new(),
            to_setup: Rc::new(Cell::new(false)),
            all_threads: false,
            folder_seen: None,
            folder_stale: false,
            branches: std::collections::HashMap::new(),
            branches_at: None,
        }
    }
}

/// How a page button looks, as the board's `.btn` variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Look {
    /// The view's one filled button.
    Primary,
    Outline,
    Danger,
    /// 24 px high, outlined.
    Small,
    /// 24 px high, ghost.
    SmallGhost,
}

/// A button of a page header or section.
pub(super) fn page_button(id: impl Into<ElementId>, label: String, look: Look) -> Stateful<Div> {
    let small = matches!(look, Look::Small | Look::SmallGhost);
    let ghost = look == Look::SmallGhost;
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .h(ui_px(if small { 24. } else { 32. }))
        .px(ui_px(if small { 8. } else { 12. }))
        .rounded(ui_px(6.))
        .border_1()
        .cursor_pointer()
        .whitespace_nowrap()
        .text_size(scale::text(if small { 12. } else { 13. }))
        .font_weight(weight(FontWeight::MEDIUM))
        .map(|b| match look {
            _ if ghost => b
                .border_color(transparent_black())
                .text_color(color("text-secondary")),
            Look::Danger => b
                .border_color(rgb(DANGER_BORDER))
                .bg(color("bg-100"))
                .text_color(color("danger")),
            Look::Primary => b
                .border_color(color("primary"))
                .bg(color("primary"))
                .text_color(color("on-primary")),
            _ => b
                .border_color(color("border-200"))
                .bg(color("bg-100"))
                .text_color(color("text-primary")),
        })
        .map(|b| {
            if look == Look::Primary {
                b.hover(|b| b.bg(color("primary-hover")))
            } else {
                b.hover(|b| b.bg(color("surface-300")))
            }
        })
        .role(Role::Button)
        .aria_label(label.clone())
        .child(label)
}

/// A status pill reading `word`, coloured as `status`.
pub(super) fn status_pill(
    id: impl Into<ElementId>,
    status: CardStatus,
    word: &str,
) -> Stateful<Div> {
    let (text, fill, border) = pill_tokens(status);
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .h(scale::row(20.))
        .px(ui_px(7.))
        .rounded_full()
        .border_1()
        .border_color(color(border))
        .when_some(fill, |pill, fill| pill.bg(color(fill)))
        .text_size(scale::text(11.))
        .font_weight(weight(FontWeight::MEDIUM))
        .text_color(color(text))
        .whitespace_nowrap()
        .aria_label(word.to_string())
        .child(word.to_string())
}

/// A bordered section with a 40 px header: its title and what goes on
/// the right.
pub(super) fn section(
    id: impl Into<ElementId>,
    title: &'static str,
    right: Option<AnyElement>,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .min_w_0()
        .border_1()
        .border_color(color("border-100"))
        .rounded(ui_px(8.))
        .aria_label(title)
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(ui_px(8.))
                .h(scale::row(40.))
                .pl(ui_px(16.))
                .pr(ui_px(12.))
                .border_b_1()
                .border_color(color("border-100"))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_weight(weight(FontWeight::SEMIBOLD))
                        .child(title),
                )
                .children(right),
        )
}

/// A section header's caption on the right.
pub(super) fn caption(id: impl Into<ElementId>, text: String) -> AnyElement {
    div()
        .id(id)
        .flex_none()
        .text_size(scale::text(12.))
        .text_color(color("text-tertiary"))
        .aria_label(text.clone())
        .child(text)
        .test_support()
        .into_any_element()
}

/// A section header's link on the right.
pub(super) fn link(id: impl Into<ElementId>, text: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .cursor_pointer()
        .text_size(scale::text(12.))
        .text_color(color("accent-blue"))
        .role(Role::Link)
        .aria_label(text)
        .child(text)
}

/// An agent's name, with "(removed)" once it was deleted.
pub(super) fn agent_name(ws: &Workspace, agent: AgentId) -> String {
    match ws.state().agent(agent) {
        Some(a) if a.deleted_at.is_some() => format!("{} (removed)", a.spec.name),
        Some(a) => a.spec.name.clone(),
        None => "Unknown agent".into(),
    }
}

/// The model's short name a chip shows: `opus`, `luna`.
pub(super) fn model_short(spec: &AgentSpec) -> String {
    match spec.runtime {
        Runtime::ClaudeCode => short_model(&spec.model).to_string(),
        Runtime::Codex => codex_short(&spec.model, None),
    }
}

/// A runtime chip with its provider's mark and `text`.
pub(super) fn runtime_chip_with(runtime: &Runtime, text: String) -> Div {
    chip(
        div()
            .flex()
            .items_center()
            .gap(ui_px(4.))
            .child(provider_icon(runtime, 12.))
            .child(text),
    )
    .flex()
    .items_center()
}

/// When something happened, at `now` (both Unix ms), as the pages say
/// it: `now`, `2m ago`, `3h ago`, `Today`, `Yesterday`, then a date.
pub fn page_time(then: i64, now: i64) -> String {
    use chrono::{Datelike as _, Local, TimeZone as _};
    let mins = (now - then).max(0) / 60_000;
    if mins < 1 {
        return "now".into();
    }
    if mins < 60 {
        return format!("{mins}m ago");
    }
    if mins < 6 * 60 {
        return format!("{}h ago", mins / 60);
    }
    let (Some(then), Some(now)) = (
        Local.timestamp_millis_opt(then).single(),
        Local.timestamp_millis_opt(now).single(),
    ) else {
        return String::new();
    };
    if then.date_naive() == now.date_naive() {
        return "Today".into();
    }
    if now.date_naive().pred_opt() == Some(then.date_naive()) {
        return "Yesterday".into();
    }
    if then.year() == now.year() {
        then.format("%b %-d").to_string()
    } else {
        then.format("%b %-d, %Y").to_string()
    }
}

/// Agent time in the channels table: `6h 40m`, `2h 05m`, `48m`, `0m`.
pub fn agent_time(ms: i64) -> String {
    let mins = ms.max(0) / 60_000;
    let (h, m) = (mins / 60, mins % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m}m")
    }
}

/// `1 worktree`, `6 worktrees`.
fn plural(n: u32, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Where agents write in a project that isn't one repo.
fn mode_words(mode: WorkMode) -> &'static str {
    match mode {
        WorkMode::OwnCopy => "Own copy per thread",
        WorkMode::InPlace => "In the folder itself",
    }
}

/// The git line of a repo project: `main · clean`, `main · 3 uncommitted`,
/// `detached · clean`.
pub fn git_line(branch: Option<&str>, uncommitted: u32) -> String {
    let branch = branch.unwrap_or("detached");
    if uncommitted == 0 {
        format!("{branch} · clean")
    } else {
        format!("{branch} · {uncommitted} uncommitted")
    }
}

/// The status a thread's pill shows.
pub(super) fn thread_pill(state: ThreadState) -> CardStatus {
    match state {
        ThreadState::Idle => CardStatus::Idle,
        ThreadState::NeedsYou => CardStatus::NeedsYou,
        ThreadState::Working { .. } => CardStatus::Working,
        ThreadState::Queued { .. } => CardStatus::Queued,
        ThreadState::Done => CardStatus::Done,
        ThreadState::Stopped => CardStatus::Stopped,
        ThreadState::Failed => CardStatus::Failed,
    }
}

impl Workspace {
    /// The project whose overview is open, if one is.
    pub(super) fn page_project(&self) -> Option<ProjectId> {
        match self.page.as_ref()?.kind {
            PageKind::Project(p) => Some(p),
            PageKind::Agent(_) => None,
        }
    }

    /// The agent whose profile is open, if one is.
    pub(super) fn page_agent(&self) -> Option<AgentId> {
        match self.page.as_ref()?.kind {
            PageKind::Agent(a) => Some(a),
            PageKind::Project(_) => None,
        }
    }

    /// Opens `kind`'s page in the main area, in place of a thread,
    /// channel or terminal session.
    pub(super) fn open_page(
        &mut self,
        kind: PageKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The same page again keeps what it shows while it asks again.
        let kept = self.page.take().filter(|p| p.kind == kind).map(|mut p| {
            p.dirty = true;
            p
        });
        self.leave_for_terminal(cx);
        self.page = Some(kept.unwrap_or_else(|| OpenPage::new(kind)));
        self.focus_workspace(window, cx);
        self.sync_page(cx);
        cx.notify();
    }

    /// Opens the project's overview, its channels shown in the sidebar.
    /// With `setup`, it scrolls to the setup files.
    pub(super) fn open_project_page(
        &mut self,
        project: ProjectId,
        setup: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state().project(project).is_none() {
            return;
        }
        self.leave_pages(cx);
        if self.recents.visited_project(project) {
            self.save_recents(cx);
        }
        self.nav.collapsed.remove(&project);
        self.open_page(PageKind::Project(project), window, cx);
        if let Some(page) = &self.page {
            page.to_setup.set(setup);
            if !setup {
                page.scroll.set_offset(point(px(0.), px(0.)));
            }
        }
    }

    /// A click on a project's row: its overview, or, while that is open,
    /// folding the project.
    pub(super) fn click_project(
        &mut self,
        project: ProjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shown = self.page_project() == Some(project)
            && self.settings.is_none()
            && self.running.is_none()
            && self.review.is_none();
        if shown {
            self.toggle_project(project, cx);
        } else {
            self.open_place(Place::Project(project), window, cx);
        }
    }

    /// Notes changes crewd reported, so the open page is asked for again.
    pub(super) fn page_touched(
        &mut self,
        threads: &[ThreadId],
        stale: &[ChannelId],
        snapshot: bool,
    ) {
        let Some(kind) = self.page.as_ref().map(|p| p.kind) else {
            return;
        };
        let state = self.state();
        let touched = snapshot
            || match kind {
                PageKind::Project(project) => {
                    threads
                        .iter()
                        .any(|t| state.thread_project(*t) == Some(project))
                        || stale
                            .iter()
                            .any(|c| state.channel(*c).is_some_and(|c| c.project == project))
                }
                PageKind::Agent(agent) => threads
                    .iter()
                    .any(|t| state.thread_runs(*t).iter().any(|r| r.agent == agent)),
            };
        if touched && let Some(page) = &mut self.page {
            page.dirty = true;
        }
    }

    /// Asks crewd for the open page when it opens, and again
    /// [`REFETCH_AFTER`] a change.
    pub(super) fn sync_page(&mut self, cx: &mut Context<Self>) {
        self.sync_branches();
        // Under settings, Running now or a review it waits to be seen.
        let hidden = self.settings.is_some() || self.running.is_some() || self.review.is_some();
        let Some(page) = &mut self.page else {
            return;
        };
        if hidden || page.fetch.is_some() || page.refetch.is_some() {
            return;
        }
        let first = page.overview.is_none() && page.profile.is_none() && page.error.is_none();
        if first {
            self.fetch_page(cx);
        } else if page.dirty {
            let kind = page.kind;
            page.refetch = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(REFETCH_AFTER).await;
                this.update(cx, |this, cx| {
                    let hidden =
                        this.settings.is_some() || this.running.is_some() || this.review.is_some();
                    if let Some(page) = this.page.as_mut().filter(|p| p.kind == kind) {
                        page.refetch = None;
                        // Hidden, it stays dirty and asks once shown.
                        if !hidden {
                            this.fetch_page(cx);
                        }
                    }
                })
                .ok();
            }));
        }
    }

    /// The open project's worktree folders and their branches, worked out
    /// again only when what crewd said changed.
    fn sync_branches(&mut self) {
        let revision = self.state().revision();
        let Some(PageKind::Project(project)) = self.page.as_ref().map(|p| p.kind) else {
            return;
        };
        if self.page.as_ref().and_then(|p| p.branches_at) == Some(revision) {
            return;
        }
        let state = self.state();
        let branches = state
            .channels(project)
            .into_iter()
            .flat_map(|c| state.threads(c.id))
            .filter_map(|t| t.worktree.as_ref())
            .map(|w| (w.path.clone(), w.branch.clone()))
            .collect();
        if let Some(page) = &mut self.page {
            page.branches = branches;
            page.branches_at = Some(revision);
        }
    }

    /// Looks at the project's folder again on the next answer: its setup
    /// files or git state changed.
    pub(super) fn folder_changed(&mut self) {
        if let Some(page) = &mut self.page {
            page.folder_stale = true;
        }
    }

    /// Asks crewd for the open page now.
    pub(super) fn fetch_page(&mut self, cx: &mut Context<Self>) {
        let Some(page) = &mut self.page else {
            return;
        };
        page.dirty = false;
        let kind = page.kind;
        // The folder's git state and setup files are looked at again only
        // when old, or known to have changed.
        let light = page.overview.is_some()
            && !page.folder_stale
            && page.folder_seen.is_some_and(|t| t.elapsed() < FOLDER_FRESH);
        if !light {
            page.folder_stale = false;
        }
        let request = match kind {
            PageKind::Project(project) => Request::ProjectOverview { project, light },
            PageKind::Agent(agent) => Request::AgentProfile { agent },
        };
        let answer = self.client.request(request);
        page.fetch = Some(cx.spawn(async move |this, cx| {
            let answer = answer.await;
            this.update(cx, |this, cx| {
                let Some(page) = this.page.as_mut().filter(|p| p.kind == kind) else {
                    return;
                };
                page.fetch = None;
                let read = answer.and_then(|v| match kind {
                    PageKind::Project(_) => serde_json::from_value::<ProjectOverview>(v)
                        .map(|mut o| {
                            match (light, page.overview.take()) {
                                (true, Some(old)) => {
                                    o.root = old.root;
                                    o.setup_files = old.setup_files;
                                }
                                _ => page.folder_seen = Some(std::time::Instant::now()),
                            }
                            page.overview = Some(o);
                        })
                        .map_err(|e| e.to_string()),
                    PageKind::Agent(_) => serde_json::from_value(v)
                        .map(|p| page.profile = Some(p))
                        .map_err(|e| e.to_string()),
                });
                match read {
                    Ok(()) => {
                        page.error = None;
                        page.failures = 0;
                    }
                    // What loaded last stays; ask again, backing off.
                    Err(e) => {
                        page.failures += 1;
                        let wait = retry_after(page.failures);
                        if page.error.as_deref() != Some(e.as_str()) {
                            eprintln!("crew: couldn't load the page: {e}");
                        }
                        page.error = Some(e);
                        page.refetch = Some(cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(wait).await;
                            this.update(cx, |this, cx| {
                                if let Some(page) = this.page.as_mut().filter(|p| p.kind == kind) {
                                    page.refetch = None;
                                    this.fetch_page(cx);
                                }
                            })
                            .ok();
                        }));
                    }
                }
                this.sync_page(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// The main area's width as last drawn.
    pub(super) fn main_width(&self) -> Pixels {
        self.window_width - self.sidebar_width()
    }

    /// The overview of `project`.
    pub(super) fn render_project_page(
        &self,
        project: ProjectId,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(page) = &self.page else {
            return div().into_any_element();
        };
        let two = self.main_width() >= scale::chrome(TWO_COLUMNS_FROM);
        let body = page.overview.as_ref().map(|overview| {
            let main = div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap(ui_px(16.))
                .child(self.render_channels_table(overview, cx))
                .child(self.render_recent_threads(project, overview, cx));
            let side = div()
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
                .child(self.render_agents_used(overview, cx))
                .children(self.render_in_terminal(project, cx))
                .child(self.render_setup_files(project, overview, page.to_setup.clone(), cx))
                .child(render_danger(project, cx));
            div()
                .flex()
                .items_start()
                .gap(ui_px(16.))
                .when(!two, |cols| cols.flex_col().items_stretch())
                .child(main)
                .child(side)
        });
        div()
            .id("overview-page")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&page.scroll)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .px(scale::chrome(24.))
                    .py(ui_px(24.))
                    .child(self.render_project_header(project, page.overview.as_ref(), cx))
                    .children(page.error.as_ref().filter(|_| page.overview.is_none()).map(
                        |error| {
                            let text = format!("Couldn't load this project. {error}");
                            div()
                                .id("overview-error")
                                .text_size(scale::text(12.))
                                .text_color(color("danger"))
                                .aria_label(text.clone())
                                .child(text)
                                .test_support()
                        },
                    ))
                    .children(body),
            )
            .test_support()
            .into_any_element()
    }

    /// The name, the folder and its git state, Reveal in Finder and New
    /// channel.
    fn render_project_header(
        &self,
        project: ProjectId,
        overview: Option<&ProjectOverview>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let state = self.state();
        let stored = state.project(project);
        let name = stored.map(|p| p.name.clone()).unwrap_or_default();
        let path = stored.map(|p| home_relative(&p.root)).unwrap_or_default();
        let small = |id: &'static str, text: String| {
            div()
                .id(id)
                .flex_none()
                .whitespace_nowrap()
                .aria_label(text.clone())
                .child(text)
                .test_support()
        };
        let mut meta: Vec<AnyElement> = vec![
            // On one line: the path and the remote give way first.
            chip(path.clone())
                .flex_shrink(1.)
                .min_w(ui_px(48.))
                .truncate()
                .id("overview-path")
                .aria_label(path)
                .test_support()
                .into_any_element(),
        ];
        match (overview, stored) {
            (Some(o), _) if !o.root.exists => {
                meta.push(
                    small("overview-missing", "Folder not found".into())
                        .text_color(color("warning"))
                        .into_any_element(),
                );
            }
            (Some(o), Some(p)) => {
                match &p.kind {
                    ProjectKind::Repo if o.root.is_repo => {
                        let branch = o.root.branch.clone().unwrap_or_else(|| "detached".into());
                        let line = git_line(Some(&branch), o.root.uncommitted);
                        let rest = line[branch.len()..].trim_start().to_string();
                        meta.push(
                            div()
                                .id("overview-git")
                                .flex()
                                .flex_none()
                                .items_center()
                                .gap(ui_px(5.))
                                .whitespace_nowrap()
                                .aria_label(line.clone())
                                .child(Icon::default().data(BRANCH_SVG).size(ui_px(12.)))
                                .child(
                                    div()
                                        .font_family(code_family())
                                        .text_size(scale::mono(12.))
                                        .text_color(color("text-primary"))
                                        .child(branch),
                                )
                                .child(rest)
                                .test_support()
                                .into_any_element(),
                        );
                        if let Some(url) = &o.root.remote_url {
                            let short = short_remote(url);
                            meta.push(
                                div()
                                    .id("overview-remote")
                                    .flex()
                                    .flex_shrink(1.)
                                    .min_w(ui_px(64.))
                                    .overflow_hidden()
                                    .gap(ui_px(4.))
                                    .whitespace_nowrap()
                                    .aria_label(format!("origin {short}"))
                                    .child(div().flex_none().child("origin"))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .font_family(code_family())
                                            .text_size(scale::mono(12.))
                                            .text_color(color("text-primary"))
                                            .child(short),
                                    )
                                    .test_support()
                                    .into_any_element(),
                            );
                        }
                    }
                    ProjectKind::MultiRepo { repos } => meta.push(
                        small(
                            "overview-kind",
                            format!("{} · {}", repos_line(repos), mode_words(p.work_mode)),
                        )
                        .into_any_element(),
                    ),
                    _ => meta.push(
                        small(
                            "overview-kind",
                            format!("Plain folder, no git · {}", mode_words(p.work_mode)),
                        )
                        .into_any_element(),
                    ),
                }
                if o.worktrees > 0 {
                    meta.push(
                        small(
                            "overview-worktrees",
                            format!("· {}", plural(o.worktrees, "worktree", "worktrees")),
                        )
                        .into_any_element(),
                    );
                }
            }
            _ => {}
        }
        div()
            .flex()
            .items_start()
            .gap(ui_px(12.))
            .mb(ui_px(20.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .id("overview-title")
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
                            .flex()
                            .items_center()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .gap(ui_px(8.))
                            .mt(ui_px(6.))
                            .text_size(scale::text(12.))
                            .text_color(color("text-secondary"))
                            .children(meta),
                    ),
            )
            .child(
                page_button("overview-reveal", "Reveal in Finder".into(), Look::Outline)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.reveal_project(project, window, cx)
                    }))
                    .test_support(),
            )
            .child(
                page_button("overview-new-channel", "New channel".into(), Look::Primary)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_new_channel_dialog(project, window, cx)
                    }))
                    .test_support(),
            )
    }

    /// Channel | Threads | Active | Needs you | Agent time | Files changed.
    fn render_channels_table(
        &self,
        overview: &ProjectOverview,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // Number columns, by their header's width at 12 px plus padding.
        const COLUMNS: [(&str, f32); 5] = [
            ("Threads", 80.),
            ("Active", 72.),
            ("Needs you", 92.),
            ("Agent time", 96.),
            ("Files changed", 112.),
        ];
        let state = self.state();
        let cell = |id: String, text: String, token: &str, width: f32| {
            div()
                .id(SharedString::from(id))
                .flex()
                .flex_none()
                .justify_end()
                .w(scale::text(width))
                .px(ui_px(16.))
                .text_color(color(token))
                .aria_label(text.clone())
                .child(text)
                .test_support()
        };
        let header = div()
            .flex()
            .items_center()
            .h(scale::row(32.))
            .border_b_1()
            .border_color(color("border-100"))
            .text_size(scale::text(12.))
            .font_weight(weight(FontWeight::MEDIUM))
            .text_color(color("text-tertiary"))
            .child(div().flex_1().min_w_0().px(ui_px(16.)).child("Channel"))
            .children(COLUMNS.map(|(title, width)| {
                div()
                    .flex()
                    .flex_none()
                    .justify_end()
                    .w(scale::text(width))
                    .px(ui_px(16.))
                    .whitespace_nowrap()
                    .child(title)
            }));
        let count = overview.channels.len();
        let rows = overview.channels.iter().enumerate().map(|(ix, row)| {
            let id = row.channel;
            let name = state
                .channel(id)
                .map(|c| c.name.clone())
                .unwrap_or_default();
            let s = &row.stats;
            let lit = |n: u32, token: &'static str| if n > 0 { token } else { "text-secondary" };
            div()
                .id(SharedString::from(format!("overview-channel-{id}")))
                .flex()
                .items_center()
                .h(scale::row(36.))
                .cursor_pointer()
                .when(ix + 1 < count, |r| {
                    r.border_b_1().border_color(color("surface-200"))
                })
                .hover(|r| r.bg(color("surface-100")))
                .text_color(color("text-secondary"))
                .aria_label(format!("#{name}"))
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .min_w_0()
                        .gap(ui_px(4.))
                        .px(ui_px(16.))
                        .text_color(color("text-primary"))
                        .child(
                            div()
                                .font_family(code_family())
                                .text_color(color("text-gutter"))
                                .child("#"),
                        )
                        .child(div().min_w_0().truncate().child(name)),
                )
                .child(cell(
                    format!("overview-channel-{id}-threads"),
                    s.threads.to_string(),
                    "text-secondary",
                    COLUMNS[0].1,
                ))
                .child(cell(
                    format!("overview-channel-{id}-active"),
                    s.active.to_string(),
                    lit(s.active, "accent-blue"),
                    COLUMNS[1].1,
                ))
                .child(cell(
                    format!("overview-channel-{id}-needs"),
                    s.needs_you.to_string(),
                    lit(s.needs_you, "warning"),
                    COLUMNS[2].1,
                ))
                .child(cell(
                    format!("overview-channel-{id}-time"),
                    agent_time(s.agent_ms_week),
                    "text-secondary",
                    COLUMNS[3].1,
                ))
                .child(cell(
                    format!("overview-channel-{id}-files"),
                    s.files_week.to_string(),
                    "text-secondary",
                    COLUMNS[4].1,
                ))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_place(Place::Channel(id), window, cx)
                }))
                .test_support()
        });
        section(
            "overview-channels",
            "Channels",
            Some(caption("overview-channels-caption", "This week".into())),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .overflow_x_hidden()
                .child(header)
                .children(rows),
        )
        .test_support()
    }

    /// The latest threads, each with its status, channel, agents and when.
    fn render_recent_threads(
        &self,
        project: ProjectId,
        overview: &ProjectOverview,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let state = self.state();
        let now = now_ms();
        let count = overview.recent_threads.len();
        let rows = overview.recent_threads.iter().enumerate().map(|(ix, t)| {
            let id = t.thread;
            let title = state
                .thread(id)
                .map(|t| t.title.clone())
                .unwrap_or_default();
            let channel = state
                .channel(t.channel)
                .map(|c| format!("#{}", c.name))
                .unwrap_or_default();
            let agents = agents_text(self, &t.agents);
            let status = thread_pill(t.state);
            let small = |suffix: &str, text: String, token: &str| {
                div()
                    .id(SharedString::from(format!("overview-thread-{id}-{suffix}")))
                    .flex_none()
                    .whitespace_nowrap()
                    .text_size(scale::text(12.))
                    .text_color(color(token))
                    .aria_label(text.clone())
                    .child(text)
                    .test_support()
            };
            div()
                .id(SharedString::from(format!("overview-thread-{id}")))
                .flex()
                .items_center()
                .gap(ui_px(10.))
                .h(scale::row(40.))
                .px(ui_px(16.))
                .cursor_pointer()
                .when(ix + 1 < count, |r| {
                    r.border_b_1().border_color(color("surface-200"))
                })
                .hover(|r| r.bg(color("surface-100")))
                .aria_label(title.clone())
                .child(
                    status_pill(
                        SharedString::from(format!("overview-thread-{id}-status")),
                        status,
                        status.word(),
                    )
                    .test_support(),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_weight(weight(FontWeight::MEDIUM))
                        .child(title),
                )
                .child(small("channel", channel, "text-tertiary"))
                .child(small("agents", agents, "text-secondary"))
                .child(
                    small("when", page_time(t.last_activity_at, now), "text-tertiary")
                        .flex()
                        .justify_end()
                        .w(scale::text(64.)),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_place(Place::Thread(id), window, cx)
                }))
                .test_support()
        });
        section(
            "overview-recent",
            "Recent threads",
            Some(
                link("overview-all-channels", "All channels")
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.show_project_threads(project, cx)),
                    )
                    .test_support()
                    .into_any_element(),
            ),
        )
        .children(rows)
        .test_support()
    }

    /// Agents with runs here: monogram, name, model and runs this week.
    fn render_agents_used(
        &self,
        overview: &ProjectOverview,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let state = self.state();
        let rows = overview.agents.iter().filter_map(|used| {
            let agent = state.agent(used.agent)?;
            let id = used.agent;
            let name = agent_name(self, id);
            Some(
                div()
                    .id(SharedString::from(format!("overview-agent-{id}")))
                    .flex()
                    .items_center()
                    .gap(ui_px(8.))
                    .h(scale::row(32.))
                    .px(ui_px(16.))
                    .cursor_pointer()
                    .hover(|r| r.bg(color("surface-100")))
                    .text_size(scale::text(12.))
                    .aria_label(name.clone())
                    .child(monogram(&agent.spec.name, 22.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(color("text-primary"))
                            .child(name),
                    )
                    .child(runtime_chip_with(
                        &agent.spec.runtime,
                        model_short(&agent.spec),
                    ))
                    .child(
                        div()
                            .id(SharedString::from(format!("overview-agent-{id}-runs")))
                            .flex()
                            .flex_none()
                            .justify_end()
                            .w(scale::mono(28.))
                            .font_family(code_family())
                            .text_size(scale::mono(12.))
                            .text_color(color("text-primary"))
                            .aria_label(used.runs_week.to_string())
                            .child(used.runs_week.to_string())
                            .test_support(),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_place(Place::Agent(id), window, cx)
                    }))
                    .test_support(),
            )
        });
        section(
            "overview-agents",
            "Agents used here",
            Some(caption("overview-agents-caption", "Runs this week".into())),
        )
        .child(div().flex().flex_col().py(ui_px(6.)).children(rows))
        .test_support()
    }

    /// Live terminal sessions in the project, each with Focus. `None`
    /// when none is live.
    fn render_in_terminal(&self, project: ProjectId, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.state();
        let sessions: Vec<_> = state
            .terminal_sessions(project)
            .into_iter()
            .filter(|s| s.live)
            .collect();
        if sessions.is_empty() {
            return None;
        }
        let branches = &self.page.as_ref()?.branches;
        let now = now_ms();
        let rows: Vec<_> = sessions
            .iter()
            .map(|s| {
                let id = s.id.clone();
                let cwd = PathBuf::from(&s.cwd);
                // A thread's worktree reads as its branch.
                let place = branches
                    .get(&cwd)
                    .cloned()
                    .unwrap_or_else(|| home_relative(&cwd));
                let age = usage_time(now - s.started_at);
                let focus = id.clone();
                div()
                    .id(SharedString::from(format!("overview-session-{id}")))
                    .flex()
                    .items_center()
                    .gap(ui_px(8.))
                    .h(scale::row(32.))
                    .px(ui_px(16.))
                    .text_size(scale::text(12.))
                    .text_color(color("text-secondary"))
                    .aria_label(place.clone())
                    .child(Icon::default().data(TERMINAL_SVG).size(ui_px(12.)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(code_family())
                            .text_size(scale::mono(12.))
                            .text_color(color("text-primary"))
                            .child(place),
                    )
                    .child(div().flex_none().child(format!("claude · {age}")))
                    .child(
                        page_button(
                            SharedString::from(format!("overview-session-{id}-focus")),
                            "Focus".into(),
                            Look::SmallGhost,
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_terminal(focus.clone(), window, cx)
                        }))
                        .test_support(),
                    )
                    .test_support()
            })
            .collect();
        let open = format!("{} open", rows.len());
        Some(
            section(
                "overview-terminal",
                "In terminal",
                Some(caption("overview-terminal-caption", open)),
            )
            .child(div().flex().flex_col().py(ui_px(6.)).children(rows))
            .test_support()
            .into_any_element(),
        )
    }

    /// The files copied into new worktrees, with Add and a remove button
    /// per row.
    fn render_setup_files(
        &self,
        project: ProjectId,
        overview: &ProjectOverview,
        to_setup: Rc<Cell<bool>>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let scroll = self.page.as_ref().map(|p| p.scroll.clone());
        // Scrolls the page so the section's top shows, once drawn.
        let marker = canvas(
            move |bounds, window, _| {
                let Some(scroll) = scroll.filter(|_| to_setup.take()) else {
                    return;
                };
                let view = scroll.bounds();
                let offset = scroll.offset();
                let max = scroll.max_offset();
                let y = (bounds.origin.y - view.origin.y - offset.y).min(max.y);
                scroll.set_offset(point(offset.x, -y.max(px(0.))));
                window.refresh();
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .size_0();
        let rows = overview.setup_files.iter().enumerate().map(|(ix, file)| {
            let path = file.path.clone();
            let group = SharedString::from(format!("overview-setup-row-{ix}"));
            div()
                .id(SharedString::from(format!("overview-setup-file-{ix}")))
                .group(group.clone())
                .flex()
                .items_center()
                .gap(ui_px(8.))
                .h(scale::row(32.))
                .px(ui_px(16.))
                .text_size(scale::text(12.))
                .text_color(color("text-secondary"))
                .aria_label(path.clone())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(code_family())
                        .text_size(scale::mono(12.))
                        .text_color(color("text-primary"))
                        .child(path.clone()),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("overview-setup-file-{ix}-size")))
                        .flex_none()
                        .aria_label(size_label(file.size))
                        .child(size_label(file.size))
                        .test_support(),
                )
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "overview-setup-file-{ix}-remove"
                        )))
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .size(ui_px(18.))
                        .rounded(ui_px(4.))
                        .cursor_pointer()
                        .opacity(0.)
                        .group_hover(group, |b| b.opacity(1.))
                        .hover(|b| b.bg(color("surface-300")))
                        .role(Role::Button)
                        .aria_label(format!("Remove {path}"))
                        .child(
                            Icon::new(IconName::Close)
                                .size(ui_px(12.))
                                .text_color(color("text-tertiary")),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.remove_setup_file(project, path.clone(), window, cx)
                        }))
                        .test_support(),
                )
                .test_support()
        });
        section(
            "overview-setup",
            "Setup files",
            Some(
                page_button("overview-setup-add", "Add".into(), Look::Small)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.add_setup_file(project, window, cx)
                    }))
                    .test_support()
                    .into_any_element(),
            ),
        )
        .relative()
        .child(marker)
        .child(
            div()
                .id("overview-setup-help")
                .mx(ui_px(16.))
                .mt(scale::row(10.))
                .mb(scale::row(4.))
                .text_size(scale::text(12.))
                .line_height(scale::line(18.))
                .text_color(color("text-tertiary"))
                .aria_label(SETUP_HELP)
                .child(SETUP_HELP)
                .test_support(),
        )
        .child(div().flex().flex_col().pb(ui_px(6.)).children(rows))
        .test_support()
    }

    /// Picks a file or folder inside the project folder and adds it to
    /// the setup files.
    fn add_setup_file(&mut self, project: ProjectId, window: &mut Window, cx: &mut Context<Self>) {
        if self.folder_task.is_some() {
            return;
        }
        let Some(stored) = self.state().project(project).cloned() else {
            return;
        };
        let picked = (self.pick_file)(cx);
        let background = cx.background_executor().clone();
        let root = stored.root.clone();
        self.folder_task = Some(cx.spawn_in(window, async move |this, cx| {
            let found = match picked.await {
                Some(file) => {
                    let root = root.clone();
                    Some(
                        background
                            .spawn(async move { super::dialogs::setup_file_in(&root, &file) })
                            .await,
                    )
                }
                None => None,
            };
            this.update_in(cx, |this, window, cx| {
                this.folder_task = None;
                match found {
                    Some(Some(file)) => {
                        let mut files = stored.copy_files.clone();
                        if !files.contains(&file.path) {
                            files.push(file.path);
                        }
                        this.set_setup_files(project, files, window, cx);
                    }
                    Some(None) => {
                        let folder = root
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| root.display().to_string());
                        this.show_error(format!("Pick a file inside {folder}."), window, cx);
                    }
                    None => {}
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn remove_setup_file(
        &mut self,
        project: ProjectId,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(stored) = self.state().project(project) else {
            return;
        };
        let files = stored
            .copy_files
            .iter()
            .filter(|f| **f != path)
            .cloned()
            .collect();
        self.set_setup_files(project, files, window, cx);
    }

    /// Saves the project's setup files, keeping its setup command, then
    /// asks for the overview again.
    fn set_setup_files(
        &mut self,
        project: ProjectId,
        files: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(stored) = self.state().project(project) else {
            return;
        };
        let answer = self.client.request(Request::UpdateProject {
            project,
            setup_command: stored.setup_command.clone(),
            copy_files: files,
        });
        self.spawn_request(window, cx, async move |this, cx| {
            let result = answer.await.map(drop);
            this.update(cx, |this, cx| {
                if this.page_project() == Some(project) {
                    this.folder_changed();
                    this.fetch_page(cx);
                }
            })
            .ok();
            result.map_err(|e| (e, None))
        });
    }
}

/// The names of a thread's agents, deleted ones marked.
fn agents_text(ws: &Workspace, agents: &[AgentId]) -> String {
    let names: Vec<String> = agents.iter().map(|a| agent_name(ws, *a)).collect();
    names_line(&names)
}

/// The first two names, then how many more: `Forge, Triage +1`.
pub fn names_line(names: &[String]) -> String {
    let shown = names[..names.len().min(2)].join(", ");
    match names.len() {
        0..=2 => shown,
        n => format!("{shown} +{}", n - 2),
    }
}

/// "Remove project", what it does, and Remove.
fn render_danger(project: ProjectId, cx: &mut Context<Workspace>) -> impl IntoElement {
    div()
        .id("overview-danger")
        .flex()
        .items_center()
        .gap(ui_px(12.))
        .px(ui_px(16.))
        .py(scale::row(12.))
        .border_1()
        .border_color(rgb(DANGER_BORDER))
        .rounded(ui_px(8.))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .font_weight(weight(FontWeight::SEMIBOLD))
                        .text_color(color("danger"))
                        .child("Remove project"),
                )
                .child(
                    div()
                        .id("overview-remove-body")
                        .text_size(scale::text(12.))
                        .line_height(scale::line(18.))
                        .text_color(color("text-tertiary"))
                        .aria_label(super::dialogs::REMOVE_BODY)
                        .child(super::dialogs::REMOVE_BODY)
                        .test_support(),
                ),
        )
        .child(
            page_button("overview-remove", "Remove".into(), Look::Danger)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_remove_project_dialog(project, window, cx)
                }))
                .test_support(),
        )
        .test_support()
}

#[cfg(test)]
mod tests {
    use super::{agent_time, git_line, names_line, page_time};

    const MIN: i64 = 60_000;

    #[test]
    fn agent_time_pads_minutes_after_hours() {
        assert_eq!(agent_time(0), "0m");
        assert_eq!(agent_time(48 * MIN + 5_000), "48m");
        assert_eq!(agent_time(125 * MIN), "2h 05m");
        assert_eq!(agent_time(400 * MIN), "6h 40m");
    }

    #[test]
    fn names_stop_after_two() {
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(names_line(&names(&["Forge"])), "Forge");
        assert_eq!(names_line(&names(&["Forge", "Quill"])), "Forge, Quill");
        assert_eq!(
            names_line(&names(&["Forge", "Triage", "Scout"])),
            "Forge, Triage +1"
        );
    }

    #[test]
    fn the_git_line_says_clean_or_counts_changes() {
        assert_eq!(git_line(Some("main"), 0), "main · clean");
        assert_eq!(git_line(Some("main"), 3), "main · 3 uncommitted");
        assert_eq!(git_line(None, 0), "detached · clean");
    }

    #[test]
    fn page_times_read_relative_then_by_day() {
        // Noon, so a few hours back stays on the same day.
        use chrono::{Local, TimeZone as _};
        let now = Local
            .with_ymd_and_hms(2026, 6, 10, 12, 0, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(page_time(now - 20_000, now), "now");
        assert_eq!(page_time(now - 2 * MIN, now), "2m ago");
        assert_eq!(page_time(now - 3 * 60 * MIN, now), "3h ago");
        assert_eq!(page_time(now - 7 * 60 * MIN, now), "Today");
        assert_eq!(page_time(now - 24 * 60 * MIN, now), "Yesterday");
        assert_eq!(page_time(now - 5 * 24 * 60 * MIN, now), "Jun 5");
    }
}
