//! Settings › Skills: the skills each CLI can run in a project, by
//! runtime and source, with a "Show in /" switch for each.

use std::path::Path;

use crew_core::agent::Runtime;
use crew_core::ids::ProjectId;
use crew_proto::{Request, SkillInfo, SkillSource, skill_key};
use gpui_kit::component::IndexPath;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{control, page_scroll, segment, segmented};
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};
use crate::ui::common::{chip, provider_icon, switch};
use crate::ui::workspace::Workspace;

/// How wide the page gets.
const PAGE_WIDTH: f32 = 860.;
/// The search field.
const SEARCH_WIDTH: f32 = 240.;
/// The column of the "Show in /" switch.
const SWITCH_COLUMN: f32 = 92.;

pub const INTRO: &str = "Crew reads skills from your CLIs' folders. It never edits them. \
Turn one off to hide it from the / menu.";

/// Which runtime's skills the page lists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RuntimeFilter {
    #[default]
    All,
    Claude,
    Codex,
}

impl RuntimeFilter {
    fn keeps(self, runtime: &Runtime) -> bool {
        match self {
            RuntimeFilter::All => true,
            RuntimeFilter::Claude => *runtime == Runtime::ClaudeCode,
            RuntimeFilter::Codex => *runtime == Runtime::Codex,
        }
    }
}

pub type ProjectSelect = SelectState<Vec<SharedString>>;

/// The page's controls, made when it first opens.
pub struct SkillsPage {
    /// The project whose own skills are listed.
    pub project: Option<ProjectId>,
    pub runtime: RuntimeFilter,
    pub search: Entity<InputState>,
    pub projects: Entity<ProjectSelect>,
    /// The projects in the select's order.
    project_ids: Vec<ProjectId>,
    _events: [Subscription; 2],
}

/// One row of the page: a skill, or a plugin with all its skills.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillRow {
    pub runtime: Runtime,
    pub source: SkillSource,
    /// The skill shown: a plugin's first one, or its first that matches
    /// the search.
    pub name: String,
    pub description: String,
    /// A Claude Code command rather than a skill.
    pub command: bool,
    /// `also in Codex`, or the plugin's name.
    pub chip: Option<String>,
    /// Where the file is, or which plugin it comes with.
    pub place: String,
    /// On while every skill of the row shows in the "/" menu.
    pub shown: bool,
    /// The row's skills by name: one, or each of a plugin's.
    pub skills: Vec<String>,
    /// The project whose own skills the page lists.
    pub project: ProjectId,
}

impl SkillRow {
    /// The key of the skill shown, which names the row.
    pub fn key(&self) -> String {
        skill_key(&self.runtime, &self.source, &self.name, self.project)
    }
}

/// The skills of one source.
#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub title: String,
    pub rows: Vec<SkillRow>,
}

/// The skills of one runtime.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeGroup {
    pub runtime: Runtime,
    /// Its rows before the search: one per skill, and one per plugin.
    pub found: usize,
    pub sections: Vec<Section>,
}

/// The project whose skills the page lists.
pub struct Listed<'a> {
    pub id: ProjectId,
    pub name: &'a str,
    pub root: &'a Path,
}

/// What the page lists for `list`, crewd's answer for `project`: one
/// group per runtime that `filter` keeps and that has skills matching
/// `query` (by name or description, ignoring case), sections in the order
/// This project, Global, Plugins. A plugin is one row.
pub fn groups(
    list: &[SkillInfo],
    project: &Listed,
    filter: RuntimeFilter,
    query: &str,
) -> Vec<RuntimeGroup> {
    let query = query.trim().to_lowercase();
    let matches = |s: &SkillInfo| {
        query.is_empty()
            || s.name.to_lowercase().contains(&query)
            || s.description.to_lowercase().contains(&query)
    };
    [Runtime::ClaudeCode, Runtime::Codex]
        .into_iter()
        .filter(|runtime| filter.keeps(runtime))
        .filter_map(|runtime| {
            let mine: Vec<&SkillInfo> = list.iter().filter(|s| s.runtime == runtime).collect();
            // Each row's skills, in the order crewd listed them.
            let mut rows: Vec<Vec<&SkillInfo>> = Vec::new();
            for s in &mine {
                let plugin_row = match &s.source {
                    SkillSource::Plugin(_) => rows.iter_mut().find(|r| r[0].source == s.source),
                    _ => None,
                };
                match plugin_row {
                    Some(row) => row.push(s),
                    None => rows.push(vec![s]),
                }
            }
            let found = rows.len();
            let row = |skills: &[&SkillInfo]| -> Option<SkillRow> {
                let shown_skill = skills.iter().find(|s| matches(s))?;
                let first = skills[0];
                let s = if query.is_empty() { first } else { shown_skill };
                let (chip, place_line) = match &s.source {
                    SkillSource::Plugin(plugin) => {
                        let n = skills.len();
                        let word = if n == 1 { "skill" } else { "skills" };
                        (
                            Some(plugin.clone()),
                            format!("plugin {plugin} · {n} {word}"),
                        )
                    }
                    _ => (
                        list.iter()
                            .any(|o| o.runtime != s.runtime && o.name == s.name)
                            .then(|| format!("also in {}", other_name(&s.runtime))),
                        place(s, project.name, project.root),
                    ),
                };
                Some(SkillRow {
                    runtime: s.runtime.clone(),
                    source: s.source.clone(),
                    name: s.name.clone(),
                    description: s.description.clone(),
                    command: is_command(s),
                    chip,
                    place: place_line,
                    shown: skills.iter().all(|s| s.shown),
                    skills: skills.iter().map(|s| s.name.clone()).collect(),
                    project: project.id,
                })
            };
            let section = |title: String, of: &dyn Fn(&SkillSource) -> bool| Section {
                title,
                rows: rows
                    .iter()
                    .filter(|r| of(&r[0].source))
                    .filter_map(|r| row(r))
                    .collect(),
            };
            let sections: Vec<Section> = [
                section(format!("This project · {}", project.name), &|s| {
                    *s == SkillSource::Project
                }),
                section("Global".into(), &|s| *s == SkillSource::Global),
                section("Plugins".into(), &|s| matches!(s, SkillSource::Plugin(_))),
            ]
            .into_iter()
            .filter(|s| !s.rows.is_empty())
            .collect();
            (!sections.is_empty()).then_some(RuntimeGroup {
                runtime,
                found,
                sections,
            })
        })
        .collect()
}

fn other_name(runtime: &Runtime) -> &'static str {
    match runtime {
        Runtime::ClaudeCode => "Codex",
        Runtime::Codex => "Claude",
    }
}

/// A Claude Code command: a file in a `commands` folder, not a skill's
/// `SKILL.md`.
fn is_command(s: &SkillInfo) -> bool {
    s.runtime == Runtime::ClaudeCode
        && !s.path.ends_with("SKILL.md")
        && Path::new(&s.path)
            .components()
            .any(|c| c.as_os_str() == "commands")
}

/// The line under a skill: `curl-runner/.claude/skills/bench/SKILL.md`
/// for a project's and `~/.claude/skills/…` for a global one.
fn place(s: &SkillInfo, project: &str, root: &Path) -> String {
    let path = Path::new(&s.path);
    match path.strip_prefix(root) {
        Ok(rest) => format!("{project}/{}", rest.display()),
        Err(_) => crate::ui::title_bar::home_relative(path),
    }
}

impl Workspace {
    /// Makes the page's controls the first time, picks the project of the
    /// channel you were in, and asks for its skills.
    pub(super) fn skills_page_opened(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let projects: Vec<(ProjectId, String)> = self
            .state()
            .projects()
            .iter()
            .map(|p| (p.id, p.name.clone()))
            .collect();
        let current = self
            .current_project()
            .or_else(|| projects.first().map(|(id, _)| *id));
        let labels: Vec<SharedString> = projects.iter().map(|(_, n)| n.clone().into()).collect();
        let ids: Vec<ProjectId> = projects.iter().map(|(id, _)| *id).collect();
        let ix = current
            .and_then(|c| ids.iter().position(|id| *id == c))
            .map(IndexPath::new);
        let made = self.settings.as_ref().is_some_and(|s| s.skills.is_some());
        if made {
            let select = self
                .settings
                .as_ref()
                .and_then(|s| s.skills.as_ref())
                .map(|p| p.projects.clone());
            if let Some(select) = select {
                select.update(cx, |select, cx| {
                    select.set_items(labels, window, cx);
                    select.set_selected_index(ix, window, cx);
                });
            }
            if let Some(page) = self.settings.as_mut().and_then(|s| s.skills.as_mut()) {
                page.project = current;
                page.project_ids = ids;
            }
        } else {
            let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search skills"));
            let searched = cx.subscribe(&search, |_: &mut Workspace, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            });
            let select = cx.new(|cx| SelectState::new(labels, ix, window, cx));
            let picked = cx.subscribe_in(
                &select,
                window,
                |this, _, event: &SelectEvent<Vec<SharedString>>, _, cx| {
                    if let SelectEvent::Confirm(Some(label)) = event {
                        let id = this
                            .state()
                            .projects()
                            .iter()
                            .find_map(|p| (p.name.as_str() == label.as_ref()).then_some(p.id));
                        if let Some(id) = id {
                            this.pick_skills_project(id, cx);
                        }
                    }
                },
            );
            if let Some(settings) = self.settings.as_mut() {
                settings.skills = Some(SkillsPage {
                    project: current,
                    runtime: RuntimeFilter::All,
                    search,
                    projects: select,
                    project_ids: ids,
                    _events: [searched, picked],
                });
            }
        }
        if let Some(project) = current {
            self.ensure_skills(project, cx);
        }
    }

    fn skills_page(&self) -> Option<&SkillsPage> {
        self.settings.as_ref()?.skills.as_ref()
    }

    /// Lists `project`'s own skills.
    pub(crate) fn pick_skills_project(&mut self, project: ProjectId, cx: &mut Context<Self>) {
        if let Some(page) = self.settings.as_mut().and_then(|s| s.skills.as_mut()) {
            page.project = Some(project);
        }
        self.ensure_skills(project, cx);
        cx.notify();
    }

    fn pick_skills_runtime(&mut self, runtime: RuntimeFilter, cx: &mut Context<Self>) {
        if let Some(page) = self.settings.as_mut().and_then(|s| s.skills.as_mut()) {
            page.runtime = runtime;
        }
        cx.notify();
    }

    /// Asks crewd to look at the skill folders again.
    fn rescan_skills(&mut self, cx: &mut Context<Self>) {
        if let Some(project) = self.skills_page().and_then(|p| p.project) {
            self.fetch_skills(project, true, cx);
            cx.notify();
        }
    }

    /// Shows or hides the row's skills in the "/" menu: one skill, or
    /// every skill of a plugin. The lists they are in change at once, so
    /// open menus follow; crewd's refusal puts them back.
    pub(crate) fn set_skill_shown(
        &mut self,
        row: &SkillRow,
        shown: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let project = row.project;
        let keys: Vec<String> = row
            .skills
            .iter()
            .map(|name| skill_key(&row.runtime, &row.source, name, project))
            .collect();
        for key in &keys {
            self.skills.mark_shown(project, key, shown);
        }
        let answers: Vec<_> = row
            .skills
            .iter()
            .map(|name| {
                self.client.request(Request::SetSkillShown {
                    runtime: row.runtime.clone(),
                    source: row.source.clone(),
                    name: name.clone(),
                    shown,
                    project: Some(project),
                })
            })
            .collect();
        self.spawn_request(window, cx, async move |this, cx| {
            let mut failed = None;
            for answer in answers {
                if let Err(error) = answer.await {
                    failed.get_or_insert(error);
                }
            }
            let Some(error) = failed else {
                return Ok(());
            };
            this.update(cx, |this, cx| {
                for key in &keys {
                    this.skills.mark_shown(project, key, !shown);
                }
                this.sync(cx);
                cx.notify();
            })
            .ok();
            Err((error, None))
        });
        self.sync(cx);
        cx.notify();
    }

    pub(super) fn render_skills_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(page) = self.skills_page() else {
            return page_scroll("settings-skills").into_any_element();
        };
        let project = page.project.and_then(|p| self.state().project(p));
        let query = page.search.read(cx).value().to_string();
        let listed = page.project.and_then(|p| self.skills.projects.get(&p));
        let groups = match (project, listed) {
            (Some(project), Some(listed)) => groups(
                &listed.list,
                &Listed {
                    id: project.id,
                    name: &project.name,
                    root: &project.root,
                },
                page.runtime,
                &query,
            ),
            _ => vec![],
        };
        let loading = listed.is_none_or(|l| l.fetched.is_none());

        let header = div()
            .flex()
            .items_center()
            .gap(ui_px(12.))
            .child(
                div()
                    .flex_1()
                    .text_size(scale::text(24.))
                    .line_height(scale::line(32.))
                    .font_weight(weight(FontWeight::SEMIBOLD))
                    .child("Skills"),
            )
            .child(
                Button::new("skills-rescan")
                    .ghost()
                    .label("Rescan")
                    .on_click(cx.listener(|this, _, _, cx| this.rescan_skills(cx))),
            );
        let intro = div()
            .id("skills-intro")
            .aria_label(INTRO)
            .mt(ui_px(4.))
            .mb(ui_px(16.))
            .text_color(color("text-secondary"))
            .child(INTRO)
            .test_support();

        let runtimes: Vec<AnyElement> = [
            (RuntimeFilter::All, "all", "All"),
            (RuntimeFilter::Claude, "claude", "Claude"),
            (RuntimeFilter::Codex, "codex", "Codex"),
        ]
        .into_iter()
        .map(|(filter, key, label)| {
            segment(
                format!("skills-runtime-{key}"),
                label,
                page.runtime == filter,
            )
            .flex_none()
            .text_size(scale::text(12.))
            .font_weight(weight(FontWeight::MEDIUM))
            .on_click(cx.listener(move |this, _, _, cx| this.pick_skills_runtime(filter, cx)))
            .test_support()
            .into_any_element()
        })
        .collect();
        let filters = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(ui_px(8.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(ui_px(8.))
                    .text_size(scale::text(12.))
                    .text_color(color("text-secondary"))
                    .child("Project skills from")
                    .child(div().min_w(ui_px(160.)).child(control(
                        "skills-project",
                        Select::new(&page.projects).accessibility_label("Project skills from"),
                    ))),
            )
            .child(segmented(runtimes))
            .child(div().flex_1())
            .child(
                div()
                    .id("skills-search")
                    .w(ui_px(SEARCH_WIDTH))
                    .child(
                        Input::new(&page.search).aria_label("Search skills").prefix(
                            Icon::new(IconName::Search)
                                .size(ui_px(13.))
                                .text_color(color("text-tertiary")),
                        ),
                    )
                    .test_support(),
            );

        let mut list = div().flex().flex_col();
        for group in &groups {
            list = list.child(group_header(group));
            for section in &group.sections {
                list = list.child(section_header(&group.runtime, &section.title));
                for row in &section.rows {
                    list = list.child(self.skill_row(row, cx));
                }
            }
        }
        if groups.is_empty() && !loading {
            list = list.child(
                div()
                    .id("skills-none")
                    .mt(ui_px(20.))
                    .text_color(color("text-tertiary"))
                    .aria_label("No skills match.")
                    .child("No skills match.")
                    .test_support(),
            );
        }

        page_scroll("settings-skills")
            .gap_0()
            .pb(ui_px(32.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .max_w(ui_px(PAGE_WIDTH))
                    .child(header)
                    .child(intro)
                    .child(filters)
                    .child(list),
            )
            .test_support()
            .into_any_element()
    }

    fn skill_row(&self, row: &SkillRow, cx: &mut Context<Self>) -> impl IntoElement {
        let key = row.key();
        let name = format!("/{}", row.name);
        let label = if row.shown { "Show in /" } else { "Hidden" };
        let clicked = row.clone();
        div()
            .id(SharedString::from(format!("skill-row-{key}")))
            .flex()
            .items_center()
            .gap(ui_px(16.))
            .py(scale::row(8.))
            .border_b_1()
            .border_color(color("surface-200"))
            .aria_label(format!("{name} {}", row.description).trim_end().to_string())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap(ui_px(8.))
                            .min_w_0()
                            .child(
                                div()
                                    .flex_none()
                                    .font_family(code_family())
                                    .text_size(scale::mono(13.))
                                    .text_color(color(if row.shown {
                                        "text-primary"
                                    } else {
                                        "text-tertiary"
                                    }))
                                    .child(name.clone()),
                            )
                            .when(row.command, |line| {
                                line.child(
                                    div()
                                        .flex_none()
                                        .text_size(scale::text(11.))
                                        .text_color(color("text-tertiary"))
                                        .child("command"),
                                )
                            })
                            .when(!row.description.is_empty(), |line| {
                                line.child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(scale::text(12.))
                                        .text_color(color("text-secondary"))
                                        .child(row.description.clone()),
                                )
                            })
                            .when_some(row.chip.clone(), |line, text| {
                                line.child(
                                    div()
                                        .id(SharedString::from(format!("skill-chip-{key}")))
                                        .flex_none()
                                        .aria_label(text.clone())
                                        .child(chip(text))
                                        .test_support(),
                                )
                            }),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("skill-place-{key}")))
                            .mt(ui_px(2.))
                            .min_w_0()
                            .truncate()
                            .font_family(code_family())
                            .text_size(scale::mono(11.))
                            .text_color(color("text-tertiary"))
                            .aria_label(row.place.clone())
                            .child(row.place.clone())
                            .test_support(),
                    ),
            )
            .child(
                div()
                    .id(SharedString::from(format!("skill-toggle-{key}")))
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_end()
                    .gap(ui_px(8.))
                    .w(ui_px(SWITCH_COLUMN))
                    .cursor_pointer()
                    .text_size(scale::text(12.))
                    .text_color(color("text-tertiary"))
                    .role(Role::Switch)
                    .aria_label(format!("{label} {name}"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_skill_shown(&clicked, !clicked.shown, window, cx);
                    }))
                    .child(label)
                    .child(switch(row.shown))
                    .test_support(),
            )
            .test_support()
    }
}

/// `Claude Code  9 found`, with the runtime's mark.
fn group_header(group: &RuntimeGroup) -> impl IntoElement {
    let name = match group.runtime {
        Runtime::ClaudeCode => "Claude Code",
        Runtime::Codex => "Codex",
    };
    let found = format!("{} found", group.found);
    div()
        .id(SharedString::from(format!("skills-group-{name}")))
        .flex()
        .items_center()
        .gap(ui_px(8.))
        .mt(ui_px(20.))
        .pb(scale::row(8.))
        .border_b_1()
        .border_color(color("border-100"))
        .text_size(scale::text(14.))
        .font_weight(weight(FontWeight::SEMIBOLD))
        .aria_label(format!("{name} {found}"))
        .child(provider_icon(&group.runtime, 14.))
        .child(name)
        .child(
            div()
                .text_size(scale::text(12.))
                .font_weight(weight(FontWeight::NORMAL))
                .text_color(color("text-tertiary"))
                .child(found),
        )
        .test_support()
}

/// `THIS PROJECT · CURL-RUNNER`, `GLOBAL` or `PLUGINS`.
fn section_header(runtime: &Runtime, title: &str) -> impl IntoElement {
    let runtime = match runtime {
        Runtime::ClaudeCode => "claude",
        Runtime::Codex => "codex",
    };
    div()
        .id(SharedString::from(format!(
            "skills-section-{runtime}-{title}"
        )))
        .flex()
        .items_center()
        .h(scale::row(32.))
        .mt(ui_px(4.))
        .text_size(scale::text(11.))
        .font_weight(weight(FontWeight::MEDIUM))
        .text_color(color("text-tertiary"))
        .aria_label(title.to_string())
        .child(title.to_uppercase())
        .test_support()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crew_core::agent::Runtime;
    use crew_proto::{SkillInfo, SkillSource};

    use crew_core::ids::ProjectId;

    use super::{Listed, RuntimeFilter, groups};

    fn demo(id: ProjectId) -> Listed<'static> {
        Listed {
            id,
            name: "demo",
            root: Path::new("/work/demo"),
        }
    }

    fn skill(runtime: Runtime, name: &str, source: SkillSource, path: &str) -> SkillInfo {
        SkillInfo {
            runtime,
            name: name.into(),
            description: format!("{name} things"),
            source,
            path: path.into(),
            shown: name != "standup",
        }
    }

    fn list() -> Vec<SkillInfo> {
        let home = std::env::var("HOME").unwrap();
        vec![
            skill(
                Runtime::ClaudeCode,
                "bench",
                SkillSource::Project,
                "/work/demo/.claude/skills/bench/SKILL.md",
            ),
            skill(
                Runtime::ClaudeCode,
                "fixture",
                SkillSource::Project,
                "/work/demo/.claude/commands/fixture.md",
            ),
            skill(
                Runtime::ClaudeCode,
                "unslop",
                SkillSource::Global,
                &format!("{home}/.claude/skills/unslop/SKILL.md"),
            ),
            skill(
                Runtime::ClaudeCode,
                "standup",
                SkillSource::Global,
                &format!("{home}/.claude/commands/standup.md"),
            ),
            skill(
                Runtime::ClaudeCode,
                "docs:write",
                SkillSource::Plugin("docs".into()),
                "/plugins/docs/skills/write/SKILL.md",
            ),
            skill(
                Runtime::ClaudeCode,
                "docs:check",
                SkillSource::Plugin("docs".into()),
                "/plugins/docs/skills/check/SKILL.md",
            ),
            skill(
                Runtime::Codex,
                "unslop",
                SkillSource::Global,
                &format!("{home}/.codex/skills/unslop/SKILL.md"),
            ),
        ]
    }

    #[test]
    fn skills_group_by_runtime_then_source_with_their_places() {
        let id = ProjectId::new();
        let all = groups(&list(), &demo(id), RuntimeFilter::All, "");
        assert_eq!(all.len(), 2);
        let claude = &all[0];
        assert_eq!(
            (claude.runtime.clone(), claude.found),
            (Runtime::ClaudeCode, 5)
        );
        let titles: Vec<&str> = claude.sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["This project · demo", "Global", "Plugins"]);

        let project = &claude.sections[0].rows;
        assert_eq!(project[0].place, "demo/.claude/skills/bench/SKILL.md");
        assert!(!project[0].command);
        assert!(project[1].command, "a commands file is a command");

        let global = &claude.sections[1].rows;
        assert_eq!(global[0].place, "~/.claude/skills/unslop/SKILL.md");
        assert_eq!(global[0].chip.as_deref(), Some("also in Codex"));
        assert!(!global[1].shown);

        // A plugin is one row: its first skill, all its skills under it.
        let plugins = &claude.sections[2].rows;
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "docs:write");
        assert_eq!(plugins[0].place, "plugin docs · 2 skills");
        assert_eq!(plugins[0].chip.as_deref(), Some("docs"));
        assert_eq!(plugins[0].skills, ["docs:write", "docs:check"]);
        assert_eq!(
            project[0].key(),
            format!("claude_code:project:{id}:bench"),
            "a project skill's key names its project"
        );
        assert_eq!(global[0].key(), "claude_code:global:unslop");

        let codex = &all[1];
        assert_eq!(codex.found, 1);
        assert_eq!(codex.sections[0].title, "Global");
        assert_eq!(
            codex.sections[0].rows[0].chip.as_deref(),
            Some("also in Claude")
        );
    }

    #[test]
    fn the_runtime_and_search_narrow_the_list() {
        let p = demo(ProjectId::new());
        let codex = groups(&list(), &p, RuntimeFilter::Codex, "");
        assert_eq!(codex.len(), 1);
        assert_eq!(codex[0].runtime, Runtime::Codex);

        let found = groups(&list(), &p, RuntimeFilter::All, "BENCH");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].found, 5, "the count is of all rows");
        assert_eq!(found[0].sections.len(), 1);
        assert_eq!(found[0].sections[0].rows[0].name, "bench");

        let by_description = groups(&list(), &p, RuntimeFilter::Claude, "write things");
        assert_eq!(by_description[0].sections[0].rows[0].name, "docs:write");
        // A search that finds a plugin's later skill shows that one.
        let later = groups(&list(), &p, RuntimeFilter::Claude, "check");
        assert_eq!(later[0].sections[0].rows[0].name, "docs:check");
        assert_eq!(later[0].sections[0].rows[0].place, "plugin docs · 2 skills");

        assert!(groups(&list(), &p, RuntimeFilter::All, "zzz").is_empty());
    }
}
