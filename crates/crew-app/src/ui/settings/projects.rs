//! Settings › Projects: add a git folder, its channels and what a new
//! worktree gets.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

use crew_core::ids::{ChannelId, ProjectId};
use crew_core::policy::HandoffPolicy;
use crew_core::store::StoredChannel;
use crew_proto::Request;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::{Disableable as _, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{control, field, form_error, help, segment, segmented};
use crate::client::ClientHandle;
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};
use crate::ui::common::chip;
use crate::ui::thread_settings::policy_name;
use crate::ui::title_bar::home_relative;
use crate::ui::workspace::Workspace;

pub const NOT_GIT: &str = "Crew needs a git repository. Run `git init` there first.";
const NO_BRANCH: &str = "Crew couldn't tell the main branch. Check out a branch there first.";
const NO_GIT: &str = "Crew couldn't run git. Install the Xcode command line tools, then try again.";
/// Budget shown when no layer sets one.
const DEFAULT_BUDGET: u32 = 3;

const CHANNEL_POLICIES: [(HandoffPolicy, &str); 3] = [
    (HandoffPolicy::AskFirst, "ask"),
    (HandoffPolicy::Budget, "budget"),
    (HandoffPolicy::PerAgent, "per-agent"),
];

/// Opens a folder picker and resolves to the folder chosen, if any.
/// Tests replace the native one.
pub type PickFolder = Rc<dyn Fn(&mut App) -> Task<Option<PathBuf>>>;

/// The system's folder picker, directories only.
pub fn native_picker() -> PickFolder {
    Rc::new(|cx: &mut App| {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add project".into()),
        });
        cx.spawn(async move |_| {
            paths
                .await
                .ok()
                .and_then(Result::ok)
                .flatten()
                .and_then(|p| p.into_iter().next())
        })
    })
}

/// A git repository's top folder and its current branch, found by git.
#[derive(Debug, Clone, PartialEq)]
pub struct Repo {
    pub root: PathBuf,
    pub branch: String,
}

/// Asks git whether `dir` is in a repository, and which branch is out.
/// Blocks: run it off the UI thread.
pub fn check_folder(dir: &Path) -> Result<Repo, String> {
    let git = |args: &[&str]| -> Result<Option<String>, String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .map_err(|_| NO_GIT.to_string())?;
        Ok(out
            .status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|s| !s.is_empty()))
    };
    let root = git(&["rev-parse", "--show-toplevel"])?.ok_or_else(|| NOT_GIT.to_string())?;
    let branch = git(&["symbolic-ref", "--short", "HEAD"])?.ok_or_else(|| NO_BRANCH.to_string())?;
    Ok(Repo {
        root: PathBuf::from(root),
        branch,
    })
}

/// A project's name from its folder.
pub fn folder_name(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".into())
}

/// One path per line, blank lines dropped.
pub fn copy_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// A channel name: what you typed without a leading `#`, by the rules
/// crewd checks a renamed channel with, and new in `project`.
pub fn channel_name(text: &str, taken: &[&str], project: &str) -> Result<String, String> {
    let name = crew_core::slug::channel_name(text).map_err(str::to_string)?;
    if taken.iter().any(|t| t.eq_ignore_ascii_case(&name)) {
        return Err(format!("#{name} already exists in {project}."));
    }
    Ok(name)
}

/// The general channel every new project starts with.
pub const FIRST_CHANNEL: &str = "general";

/// Makes the project and its `#general`, the way both Settings and the
/// first launch add one.
pub(crate) async fn create_project(
    client: ClientHandle,
    name: String,
    repo: Repo,
) -> Result<(ProjectId, ChannelId), String> {
    let project = client
        .request(Request::CreateProject {
            name,
            root: repo.root.to_string_lossy().into_owned(),
            main_branch: repo.branch,
            work_mode: None,
            copy_files: vec![],
            setup_command: None,
        })
        .await?;
    let project: ProjectId = serde_json::from_value(project).map_err(|e| e.to_string())?;
    let channel = client
        .request(Request::CreateChannel {
            project,
            name: FIRST_CHANNEL.into(),
        })
        .await?;
    let channel: ChannelId = serde_json::from_value(channel).map_err(|e| e.to_string())?;
    Ok((project, channel))
}

/// Who asked for a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderFor {
    Settings,
    FirstLaunch,
}

/// Settings › Projects.
#[derive(Default)]
pub struct ProjectsPage {
    pub selected: Option<ProjectId>,
    pub form: Option<ProjectForm>,
    /// A folder chosen to add, waiting for its name.
    pub adding: Option<Adding>,
    /// What went wrong with the folder you chose.
    pub folder_error: Option<String>,
}

pub struct Adding {
    pub repo: Repo,
    pub name: Entity<InputState>,
    pub error: Option<String>,
    pub saving: bool,
}

pub struct ProjectForm {
    pub project: ProjectId,
    pub setup: Entity<InputState>,
    pub copy_files: Entity<TextareaState>,
    pub new_channel: Entity<InputState>,
    pub error: Option<String>,
    pub channel_error: Option<String>,
    pub saving: bool,
    pub saved: bool,
    _edits: [Subscription; 2],
}

impl Workspace {
    pub(super) fn projects_page_opened(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let page = self.settings.as_ref().map(|s| &s.projects);
        if page.is_some_and(|p| p.form.is_some() || p.adding.is_some()) {
            return;
        }
        if let Some(first) = self.state().projects().first().map(|p| p.id) {
            self.select_project(first, window, cx);
        }
    }

    fn projects_page(&mut self) -> Option<&mut ProjectsPage> {
        self.settings.as_mut().map(|s| &mut s.projects)
    }

    pub(super) fn select_project(
        &mut self,
        project: ProjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(stored) = self.state().project(project).cloned() else {
            return;
        };
        let setup = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("e.g. npm ci")
                .default_value(stored.setup_command.clone().unwrap_or_default())
        });
        let copy_files = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(4)
                .placeholder(".env")
                .default_value(stored.copy_files.join("\n"))
        });
        let new_channel = cx.new(|cx| InputState::new(window, cx).placeholder("bugs"));
        let edited = |this: &mut Self, event: &InputEvent, cx: &mut Context<Self>| {
            if matches!(event, InputEvent::Change) {
                if let Some(form) = this.projects_page().and_then(|p| p.form.as_mut()) {
                    form.saved = false;
                }
                cx.notify();
            }
        };
        let edits = [
            cx.subscribe(&setup, move |this, _, event, cx| edited(this, event, cx)),
            cx.subscribe(&copy_files, move |this, _, event, cx| {
                edited(this, event, cx)
            }),
        ];
        if let Some(page) = self.projects_page() {
            page.selected = Some(project);
            page.adding = None;
            page.folder_error = None;
            page.form = Some(ProjectForm {
                project,
                setup,
                copy_files,
                new_channel,
                error: None,
                channel_error: None,
                saving: false,
                saved: false,
                _edits: edits,
            });
        }
        cx.notify();
    }

    /// Opens the folder picker. The folder must be a git repository.
    pub(crate) fn choose_folder(
        &mut self,
        target: FolderFor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // One picker at a time.
        if self.folder_task.is_some() {
            return;
        }
        let picked = (self.pick_folder)(cx);
        let background = cx.background_executor().clone();
        self.folder_task = Some(cx.spawn_in(window, async move |this, cx| {
            let checked = match picked.await {
                Some(dir) => Some(background.spawn(async move { check_folder(&dir) }).await),
                None => None,
            };
            this.update_in(cx, |this, window, cx| {
                this.folder_task = None;
                if let Some(checked) = checked {
                    this.folder_checked(target, checked, window, cx);
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// A picker is open or git is being asked about its folder.
    pub(crate) fn picking_folder(&self) -> bool {
        self.folder_task.is_some()
    }

    fn folder_checked(
        &mut self,
        target: FolderFor,
        checked: Result<Repo, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let checked = checked.and_then(|repo| {
            let known = self.state().projects().iter().any(|p| p.root == repo.root);
            if known {
                Err(format!(
                    "{} is already a project. Open it from the sidebar.",
                    folder_name(&repo.root)
                ))
            } else {
                Ok(repo)
            }
        });
        match target {
            FolderFor::FirstLaunch => self.first_launch_folder(checked, window, cx),
            FolderFor::Settings => {
                let adding = checked.map(|repo| {
                    let name = folder_name(&repo.root);
                    Adding {
                        repo,
                        name: cx.new(|cx| InputState::new(window, cx).default_value(name)),
                        error: None,
                        saving: false,
                    }
                });
                if let Some(page) = self.projects_page() {
                    match adding {
                        Ok(adding) => {
                            page.adding = Some(adding);
                            page.folder_error = None;
                            page.form = None;
                            page.selected = None;
                        }
                        Err(e) => page.folder_error = Some(e),
                    }
                }
            }
        }
        cx.notify();
    }

    /// Makes the project from the folder being added.
    fn add_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(adding) = self.projects_page().and_then(|p| p.adding.as_mut()) else {
            return;
        };
        if adding.saving {
            return;
        }
        let name = adding.name.read(cx).value().trim().to_string();
        if name.is_empty() {
            adding.error = Some("Give the project a name.".into());
            cx.notify();
            return;
        }
        adding.saving = true;
        adding.error = None;
        let id = adding.name.entity_id();
        let repo = adding.repo.clone();
        let made = create_project(self.client.clone(), name, repo);
        self.spawn_request(window, cx, async move |this, cx| {
            let made = made.await;
            this.update_in(cx, |this, window, cx| match made {
                Ok((project, _)) => this.project_added(project, window, cx),
                Err(e) => {
                    if let Some(adding) = this
                        .projects_page()
                        .and_then(|p| p.adding.as_mut())
                        .filter(|a| a.name.entity_id() == id)
                    {
                        adding.saving = false;
                        adding.error = Some(e);
                    }
                    cx.notify();
                }
            })
            .ok();
            Ok(())
        });
        cx.notify();
    }

    /// Selects a project once crewd has told the app about it.
    fn project_added(&mut self, project: ProjectId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(page) = self.projects_page() {
            page.adding = None;
            page.selected = Some(project);
        }
        self.select_project(project, window, cx);
    }

    /// Fills the form of a project that was selected before crewd told
    /// the app about it.
    pub(crate) fn sync_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(page) = self.settings.as_ref().map(|s| &s.projects) else {
            return;
        };
        if let (Some(project), None) = (page.selected, &page.form)
            && self.state().project(project).is_some()
        {
            self.select_project(project, window, cx);
        }
    }

    fn cancel_adding(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(page) = self.projects_page() {
            page.adding = None;
        }
        self.projects_page_opened(window, cx);
        cx.notify();
    }

    fn save_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.projects_page().and_then(|p| p.form.as_mut()) else {
            return;
        };
        if form.saving {
            return;
        }
        let setup = form.setup.read(cx).value().trim().to_string();
        let copy_files = copy_lines(&form.copy_files.read(cx).value());
        form.saving = true;
        form.error = None;
        let id = form.setup.entity_id();
        let req = Request::UpdateProject {
            project: form.project,
            setup_command: (!setup.is_empty()).then_some(setup),
            copy_files,
        };
        let answer = self.client.request(req);
        self.spawn_request(window, cx, async move |this, cx| {
            let result = answer.await;
            this.update(cx, |this, cx| {
                if let Some(form) = this
                    .projects_page()
                    .and_then(|p| p.form.as_mut())
                    .filter(|f| f.setup.entity_id() == id)
                {
                    form.saving = false;
                    match result {
                        Ok(_) => form.saved = true,
                        Err(e) => form.error = Some(e),
                    }
                }
                cx.notify();
            })
            .ok();
            Ok(())
        });
        cx.notify();
    }

    fn add_channel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self
            .settings
            .as_ref()
            .and_then(|s| s.projects.form.as_ref())
        else {
            return;
        };
        let project = form.project;
        let text = form.new_channel.read(cx).value().to_string();
        let taken: Vec<&str> = self
            .state()
            .channels(project)
            .into_iter()
            .map(|c| c.name.as_str())
            .collect();
        let project_name = self
            .state()
            .project(project)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let checked = channel_name(&text, &taken, &project_name);
        let input = form.new_channel.clone();
        let name = match checked {
            Ok(name) => name,
            Err(e) => {
                if let Some(form) = self.projects_page().and_then(|p| p.form.as_mut()) {
                    form.channel_error = Some(e);
                }
                cx.notify();
                return;
            }
        };
        if let Some(form) = self.projects_page().and_then(|p| p.form.as_mut()) {
            form.channel_error = None;
        }
        input.update(cx, |input, cx| input.set_value("", window, cx));
        self.request(Request::CreateChannel { project, name }, window, cx);
        cx.notify();
    }

    pub(super) fn render_projects_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let page = self.settings.as_ref().map(|s| &s.projects);
        let main = match page {
            Some(ProjectsPage {
                adding: Some(adding),
                ..
            }) => self.render_adding(adding, cx).into_any_element(),
            Some(ProjectsPage {
                form: Some(form), ..
            }) => self.render_project_form(form, cx).into_any_element(),
            _ => div()
                .flex_1()
                .p(ui_px(32.))
                .child(help(
                    "Add a project to give your agents a repository to work in.",
                ))
                .into_any_element(),
        };
        div()
            .flex()
            .size_full()
            .child(self.render_project_list(page, cx))
            .child(main)
            .into_any_element()
    }

    fn render_project_list(
        &self,
        page: Option<&ProjectsPage>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = page.and_then(|p| p.selected);
        let folder_error = page.and_then(|p| p.folder_error.clone());
        let projects = self.state().projects();
        let count = match projects.len() {
            1 => "1 project".to_string(),
            n => format!("{n} projects"),
        };
        let rows: Vec<AnyElement> = projects
            .into_iter()
            .map(|p| {
                let id = p.id;
                div()
                    .id(SharedString::from(format!("settings-project-{id}")))
                    .flex()
                    .flex_col()
                    .justify_center()
                    .h(scale::row(44.))
                    .px(ui_px(8.))
                    .rounded(ui_px(6.))
                    .cursor_pointer()
                    .when(selected == Some(id), |row| row.bg(color("surface-300")))
                    .hover(|row| row.bg(color("surface-200")))
                    .aria_label(p.name.clone())
                    .child(
                        div()
                            .truncate()
                            .font_weight(weight(FontWeight::MEDIUM))
                            .child(p.name.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(scale::mono(11.))
                            .font_family(code_family())
                            .text_color(color("text-tertiary"))
                            .child(home_relative(&p.root)),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_project(id, window, cx);
                    }))
                    .test_support()
                    .into_any_element()
            })
            .collect();
        div()
            .id("settings-project-list")
            .flex()
            .flex_col()
            .flex_none()
            .w(self.sidebar_width())
            .h_full()
            .px(ui_px(8.))
            .py(scale::row(12.))
            .border_r_1()
            .border_color(color("border-100"))
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(scale::row(28.))
                    .pl(ui_px(8.))
                    .mb(ui_px(4.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(scale::text(12.))
                            .font_weight(weight(FontWeight::MEDIUM))
                            .text_color(color("text-secondary"))
                            .child(count),
                    )
                    .child(
                        Button::new("settings-add-project")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .label("Add project")
                            .disabled(self.picking_folder())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.choose_folder(FolderFor::Settings, window, cx);
                            })),
                    ),
            )
            .when_some(folder_error, |list, error| {
                list.child(
                    div()
                        .px(ui_px(8.))
                        .pb(scale::row(8.))
                        .child(form_error("project-folder-error", error)),
                )
            })
            .children(rows)
            .test_support()
    }

    fn render_adding(&self, adding: &Adding, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("project-adding")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .px(ui_px(32.))
            .py(ui_px(24.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .max_w(ui_px(760.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(ui_px(8.))
                            .pb(scale::row(16.))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(scale::text(20.))
                                    .font_weight(weight(FontWeight::SEMIBOLD))
                                    .child("Add project"),
                            )
                            .child(
                                Button::new("project-add-cancel")
                                    .ghost()
                                    .small()
                                    .label("Cancel")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.cancel_adding(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("project-add-confirm")
                                    .primary()
                                    .small()
                                    .label("Add project")
                                    .disabled(adding.saving)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.add_project(window, cx);
                                    })),
                            ),
                    )
                    .when_some(adding.error.clone(), |col, e| {
                        col.child(form_error("project-add-error", e))
                    })
                    .child(field(
                        "Folder",
                        None,
                        chip(home_relative(&adding.repo.root)),
                    ))
                    .child(field(
                        "Main branch",
                        Some("New threads branch from here."),
                        div()
                            .id("project-add-branch")
                            .font_family(code_family())
                            .text_size(scale::mono(12.))
                            .aria_label(adding.repo.branch.clone())
                            .child(adding.repo.branch.clone())
                            .test_support(),
                    ))
                    .child(field(
                        "Name",
                        None,
                        control("project-add-name", Input::new(&adding.name)),
                    )),
            )
            .test_support()
    }

    fn render_project_form(&self, form: &ProjectForm, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state();
        let Some(project) = state.project(form.project) else {
            return div().into_any_element();
        };
        let channels: Vec<AnyElement> = state
            .channels(form.project)
            .into_iter()
            .map(|c| self.render_channel_settings(c, cx).into_any_element())
            .collect();
        div()
            .id("project-form")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .px(ui_px(32.))
            .py(ui_px(24.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .max_w(ui_px(760.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(ui_px(12.))
                            .pb(scale::row(16.))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .id("project-form-title")
                                            .text_size(scale::text(24.))
                                            .line_height(scale::line(32.))
                                            .font_weight(weight(FontWeight::SEMIBOLD))
                                            .aria_label(project.name.clone())
                                            .child(project.name.clone())
                                            .test_support(),
                                    )
                                    .child(help(format!(
                                        "{} · {}",
                                        home_relative(&project.root),
                                        project.main_branch
                                    ))),
                            )
                            .when(form.saved, |row| {
                                row.child(
                                    div()
                                        .id("project-saved")
                                        .text_size(scale::text(12.))
                                        .text_color(color("success"))
                                        .aria_label("Saved")
                                        .child("Saved")
                                        .test_support(),
                                )
                            })
                            .child(
                                Button::new("project-save")
                                    .primary()
                                    .small()
                                    .label("Save project")
                                    .disabled(form.saving)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.save_project(window, cx);
                                    })),
                            ),
                    )
                    .when_some(form.error.clone(), |col, e| {
                        col.child(form_error("project-error", e))
                    })
                    .child(field(
                        "Channels",
                        Some("How handoffs run by default in each channel."),
                        div()
                            .flex()
                            .flex_col()
                            .gap(ui_px(8.))
                            .children(channels)
                            .child(
                                div()
                                    .flex()
                                    .gap(ui_px(8.))
                                    .child(div().flex_1().child(control(
                                        "project-new-channel",
                                        Input::new(&form.new_channel),
                                    )))
                                    .child(
                                        Button::new("project-add-channel")
                                            .outline()
                                            .small()
                                            .icon(IconName::Plus)
                                            .label("Add channel")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.add_channel(window, cx);
                                            })),
                                    ),
                            )
                            .when_some(form.channel_error.clone(), |col, e| {
                                col.child(form_error("project-channel-error", e))
                            }),
                    ))
                    .child(field(
                        "Setup command",
                        Some("Runs in a new worktree before its first run."),
                        control("project-setup", Input::new(&form.setup)),
                    ))
                    .child(field(
                        "Files to copy",
                        Some("One path per line, relative to the project. Copied into each new worktree, such as .env."),
                        div()
                            .font_family(code_family())
                            .child(control("project-copy-files", Textarea::new(&form.copy_files))),
                    )),
            )
            .test_support()
            .into_any_element()
    }

    /// `#name`, its handoff policy and its budget. Changes are sent at
    /// once.
    fn render_channel_settings(
        &self,
        channel: &StoredChannel,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = channel.id;
        let (own_policy, own_budget) = self.state().channel_handoff(id);
        let policy = own_policy.unwrap_or(HandoffPolicy::AskFirst);
        let budget = own_budget.unwrap_or(DEFAULT_BUDGET);
        // Clicks build on the setting as it is when clicked, which may be
        // ahead of this drawing.
        let step = move |this: &mut Self, up: bool, window: &mut Window, cx: &mut Context<Self>| {
            let (policy, budget) = this.state().channel_handoff(id);
            let budget = budget.unwrap_or(DEFAULT_BUDGET);
            let next = if up {
                (budget + 1).min(99)
            } else {
                budget.saturating_sub(1)
            };
            this.send_channel_handoff(id, (policy, Some(next)), window, cx);
        };
        let segments: Vec<AnyElement> = CHANNEL_POLICIES
            .iter()
            .map(|(p, key)| {
                let p = *p;
                segment(
                    format!("channel-policy-{id}-{key}"),
                    policy_name(p),
                    p == policy,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    let (_, budget) = this.state().channel_handoff(id);
                    this.send_channel_handoff(id, (Some(p), budget), window, cx);
                }))
                .test_support()
                .into_any_element()
            })
            .collect();
        div()
            .id(SharedString::from(format!("channel-settings-{id}")))
            .flex()
            .items_center()
            .gap(ui_px(8.))
            .aria_label(format!("#{}", channel.name))
            .child(
                div()
                    .w(ui_px(96.))
                    .flex_none()
                    .truncate()
                    .font_family(code_family())
                    .child(format!("#{}", channel.name)),
            )
            .child(div().flex_1().child(segmented(segments)))
            .child(
                Button::new(SharedString::from(format!("channel-budget-down-{id}")))
                    .outline()
                    .xsmall()
                    .icon(IconName::Minus)
                    .disabled(budget == 0)
                    .on_click(
                        cx.listener(move |this, _, window, cx| step(this, false, window, cx)),
                    ),
            )
            .child(
                div()
                    .id(SharedString::from(format!("channel-budget-{id}")))
                    .min_w(ui_px(24.))
                    .text_center()
                    .font_family(code_family())
                    .aria_label(budget.to_string())
                    .child(budget.to_string())
                    .test_support(),
            )
            .child(
                Button::new(SharedString::from(format!("channel-budget-up-{id}")))
                    .outline()
                    .xsmall()
                    .icon(IconName::Plus)
                    .on_click(cx.listener(move |this, _, window, cx| step(this, true, window, cx))),
            )
            .test_support()
    }
}

#[cfg(test)]
mod tests {
    use super::{NOT_GIT, channel_name, check_folder, copy_lines, folder_name};
    use std::path::Path;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn a_git_folder_gives_its_root_and_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("demo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        git(&repo, &["init", "-q", "-b", "trunk"]);
        let found = check_folder(&repo.join("src")).unwrap();
        assert_eq!(found.branch, "trunk");
        assert_eq!(
            found.root.canonicalize().unwrap(),
            repo.canonicalize().unwrap()
        );
        assert_eq!(folder_name(&found.root), "demo");

        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(check_folder(&plain).unwrap_err(), NOT_GIT);
    }

    #[test]
    fn copy_files_are_one_per_line() {
        assert_eq!(
            copy_lines(" .env \n\nconfig/local.toml\n"),
            [".env", "config/local.toml"]
        );
    }

    #[test]
    fn channel_names_drop_the_hash_and_must_be_new() {
        let bad = "Use lowercase letters, numbers and dashes.";
        assert_eq!(channel_name(" #bugs ", &[], "demo").unwrap(), "bugs");
        assert_eq!(channel_name("", &[], "demo").unwrap_err(), bad);
        assert_eq!(channel_name("two words", &[], "demo").unwrap_err(), bad);
        assert_eq!(channel_name("General", &[], "demo").unwrap_err(), bad);
        assert_eq!(
            channel_name("bugs-2", &["Bugs-2"], "demo").unwrap_err(),
            "#bugs-2 already exists in demo."
        );
    }
}
