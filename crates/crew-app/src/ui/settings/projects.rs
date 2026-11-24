//! Settings › Projects: each project's channels and what a new worktree
//! gets. Projects are added through the Add project dialog.

use std::path::PathBuf;
use std::rc::Rc;

use crew_core::ids::ProjectId;
use crew_core::policy::HandoffPolicy;
use crew_core::store::StoredChannel;
use crew_proto::Request;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::{Disableable as _, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{control, field, form_error, help, segment, segmented};
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};
use crate::ui::thread_settings::policy_name;
use crate::ui::title_bar::home_relative;
use crate::ui::workspace::Workspace;

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
    system_picker(false, "Add project")
}

/// The system's picker for a setup file or folder.
pub fn native_file_picker() -> PickFolder {
    system_picker(true, "Add file")
}

fn system_picker(files: bool, prompt: &'static str) -> PickFolder {
    Rc::new(move |cx: &mut App| {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files,
            directories: true,
            multiple: false,
            prompt: Some(prompt.into()),
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

/// Settings › Projects.
#[derive(Default)]
pub struct ProjectsPage {
    pub selected: Option<ProjectId>,
    pub form: Option<ProjectForm>,
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
        if page.is_some_and(|p| p.form.is_some()) {
            return;
        }
        if let Some(first) = self.state().projects().first().map(|p| p.id) {
            self.select_project(first, window, cx);
        }
    }

    fn projects_page(&mut self) -> Option<&mut ProjectsPage> {
        self.settings.as_mut().map(|s| &mut s.projects)
    }

    pub(crate) fn select_project(
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
                form: Some(form), ..
            }) => self.render_project_form(form, cx).into_any_element(),
            _ => div()
                .flex_1()
                .p(ui_px(32.))
                .child(help(
                    "Add a project to give your agents a folder to work in.",
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
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_add_project(window, cx);
                            })),
                    ),
            )
            .children(rows)
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
                                    .child(help(if project.main_branch.is_empty() {
                                        home_relative(&project.root)
                                    } else {
                                        format!(
                                            "{} · {}",
                                            home_relative(&project.root),
                                            project.main_branch
                                        )
                                    })),
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
    use super::{channel_name, copy_lines};

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
