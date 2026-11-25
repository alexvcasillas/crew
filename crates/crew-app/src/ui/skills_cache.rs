//! The skills crewd found per project, asked for when a composer's `/`
//! menu first opens there and again when it opens after a while.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crew_core::agent::Runtime;
use crew_core::ids::{ProjectId, RunId};
use crew_proto::{Request, SkillInfo, SkillSource};
use gpui_kit::*;

use super::activity::ActivityRow;
use super::runs_with::{model_name, runtime_word};
use super::workspace::Workspace;

/// Opening the menu after this long asks crewd to look again, since it
/// doesn't watch the skill folders.
pub const SKILLS_STALE: Duration = Duration::from_secs(30);

/// Whether opening the menu asks crewd for the list, and if so whether
/// crewd looks again: `known` is whether a list is on its way and how
/// old the last one is.
pub fn fetch_plan(known: Option<(bool, Option<Duration>)>) -> Option<bool> {
    match known {
        None | Some((false, None)) => Some(false),
        Some((true, _)) => None,
        Some((false, Some(age))) => (age >= SKILLS_STALE).then_some(true),
    }
}

/// A project's skills as crewd last listed them.
#[derive(Default)]
pub struct ProjectSkills {
    pub list: Vec<SkillInfo>,
    /// Each skill file's size in bytes, by path, for the Activity tab.
    pub sizes: HashMap<String, u64>,
    /// Goes up with every new list, so the composer knows to take it.
    pub rev: u64,
    /// When the list came, if it has.
    pub fetched: Option<Instant>,
    pub loading: bool,
    task: Option<Task<()>>,
}

#[derive(Default)]
pub struct SkillsCache {
    pub projects: HashMap<ProjectId, ProjectSkills>,
    next_rev: u64,
}

impl SkillsCache {
    /// The skill a run of `runtime` named `name` ran in `project`, as
    /// crewd resolves it: the project's own over a global one.
    pub fn find(&self, project: ProjectId, runtime: &Runtime, name: &str) -> Option<&SkillInfo> {
        let list = &self.projects.get(&project)?.list;
        let of = |source: fn(&SkillSource) -> bool| {
            list.iter()
                .find(|s| s.runtime == *runtime && s.name == name && source(&s.source))
        };
        of(|s| *s == SkillSource::Project)
            .or_else(|| of(|s| *s == SkillSource::Global))
            .or_else(|| of(|s| matches!(s, SkillSource::Plugin(_))))
    }

    /// The size of the skill file at `path`, if known.
    pub fn size(&self, project: ProjectId, path: &str) -> Option<u64> {
        self.projects.get(&project)?.sizes.get(path).copied()
    }
}

impl Workspace {
    /// The project of the selected channel.
    pub(super) fn current_project(&self) -> Option<ProjectId> {
        self.nav
            .channel
            .and_then(|c| self.state().channel(c))
            .map(|c| c.project)
    }

    /// The `/` menu opened: asks for the project's skills the first time,
    /// and asks crewd to look again once the list is [`SKILLS_STALE`].
    pub(super) fn skills_wanted(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.current_project() else {
            return;
        };
        let known = self
            .skills
            .projects
            .get(&project)
            .map(|p| (p.loading, p.fetched.map(|at| at.elapsed())));
        if let Some(force) = fetch_plan(known) {
            self.fetch_skills(project, force, cx);
        }
    }

    /// Asks crewd for `project`'s skills unless asked already.
    pub(super) fn ensure_skills(&mut self, project: ProjectId, cx: &mut Context<Self>) {
        if !self.skills.projects.contains_key(&project) {
            self.fetch_skills(project, false, cx);
        }
    }

    /// Asks crewd for `project`'s skills, off the UI thread, and sizes up
    /// their files.
    pub(super) fn fetch_skills(&mut self, project: ProjectId, force: bool, cx: &mut Context<Self>) {
        let answer = self.client.request(Request::Skills {
            project,
            force_reload: force,
        });
        let task = cx.spawn(async move |this, cx| {
            let list = answer.await.and_then(|v| {
                serde_json::from_value::<Vec<SkillInfo>>(v).map_err(|e| e.to_string())
            });
            let paths: Vec<String> = list
                .as_ref()
                .map(|l| l.iter().map(|s| s.path.clone()).collect())
                .unwrap_or_default();
            let sizes = cx
                .background_executor()
                .spawn(async move {
                    paths
                        .into_iter()
                        .filter_map(|p| std::fs::metadata(&p).ok().map(|m| (p, m.len())))
                        .collect::<HashMap<_, _>>()
                })
                .await;
            this.update(cx, |this, cx| {
                this.skills.next_rev += 1;
                let rev = this.skills.next_rev;
                let entry = this.skills.projects.entry(project).or_default();
                entry.loading = false;
                match list {
                    Ok(list) => {
                        entry.list = list;
                        entry.sizes = sizes;
                        entry.rev = rev;
                        entry.fetched = Some(Instant::now());
                    }
                    Err(e) => {
                        eprintln!("crew: couldn't list the skills: {e}");
                        // The menu says none match; the next open asks
                        // again.
                        if entry.fetched.is_none() {
                            entry.rev = rev;
                        }
                        entry.fetched = Some(
                            Instant::now()
                                .checked_sub(SKILLS_STALE)
                                .unwrap_or_else(Instant::now),
                        );
                    }
                }
                this.inspector.rows_from = None;
                this.sync(cx);
                cx.notify();
            })
            .ok();
        });
        let entry = self.skills.projects.entry(project).or_default();
        entry.loading = true;
        entry.task = Some(task);
    }

    /// The runtime a run's agent uses, which the run takes once it starts.
    pub(super) fn run_runtime(&self, run: &crew_core::store::StoredRun) -> Runtime {
        match self.state().agent(run.agent) {
            Some(agent) if run.session_id.is_empty() => agent.spec.runtime.clone(),
            _ => run.runtime.clone(),
        }
    }

    /// `claude · sonnet`: the runtime and the model a run uses, its
    /// message's model if it had one.
    pub(super) fn run_chip(&self, run: &crew_core::store::StoredRun) -> Option<String> {
        let agent = self.state().agent(run.agent)?;
        let runtime = self.run_runtime(run);
        let models = super::settings::agents::models_of(&self.runtimes, &runtime);
        let model = run.model_override.as_deref().unwrap_or(&agent.spec.model);
        Some(format!(
            "{} · {}",
            runtime_word(&runtime),
            model_name(&runtime, model, models)
        ))
    }

    /// The skill's source as its tag reads, `global` or `project`, if
    /// known.
    pub(super) fn run_skill_source(&self, run: &crew_core::store::StoredRun) -> Option<String> {
        let name = run.skill.as_deref()?;
        let project = self.state().thread_project(run.thread)?;
        self.skills
            .find(project, &self.run_runtime(run), name)
            .map(|s| super::slash_menu::source_tag(&s.source))
    }

    /// The rows before what `run` did: the skill it was asked to run and
    /// the model it used for its message alone.
    pub(super) fn request_rows(&self, run: RunId) -> Vec<ActivityRow> {
        let state = self.state();
        let Some(r) = state.run(run) else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        let runtime = self.run_runtime(r);
        if let Some(name) = &r.skill {
            let project = state.thread_project(r.thread);
            let path = project
                .and_then(|p| self.skills.find(p, &runtime, name))
                .map(|s| s.path.clone());
            let size = project
                .zip(path.as_deref())
                .and_then(|(p, path)| self.skills.size(p, path));
            rows.push(ActivityRow::Skill {
                name: name.clone(),
                path,
                size,
            });
        }
        if let (Some(model), Some(agent)) = (&r.model_override, state.agent(r.agent)) {
            let models = super::settings::agents::models_of(&self.runtimes, &runtime);
            rows.push(ActivityRow::Model {
                model: model_name(&runtime, model, models),
                agent: agent.spec.name.clone(),
                default: model_name(&runtime, &agent.spec.model, models),
            });
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{SKILLS_STALE, fetch_plan};

    #[test]
    fn the_list_is_asked_for_once_then_again_when_stale() {
        assert_eq!(fetch_plan(None), Some(false), "first open");
        assert_eq!(fetch_plan(Some((true, None))), None, "on its way");
        let fresh = Some((false, Some(Duration::from_secs(5))));
        assert_eq!(fetch_plan(fresh), None, "kept");
        let stale = Some((false, Some(SKILLS_STALE)));
        assert_eq!(fetch_plan(stale), Some(true), "crewd looks again");
    }
}
