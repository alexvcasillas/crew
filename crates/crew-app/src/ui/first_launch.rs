//! The first launch: no projects yet. Add one, check the agent CLIs and
//! make a starter agent, then land in `#general`.

use std::path::PathBuf;

use crew_core::agent::{AgentSpec, PermissionMode, Runtime};
use crew_core::ids::{AgentId, ChannelId, ProjectId};
use crew_proto::{Request, RuntimeInfo};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Disableable as _, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::provider_icon;
use super::settings::Page;
use super::settings::agents::{INSTALL_CLAUDE, blank_spec, default_codex_model, models_of};
use super::settings::projects::FIRST_CHANNEL;
use super::settings::runtimes::{runtime_name, runtime_state};
use super::title_bar::home_relative;
use super::workspace::{Place, Workspace};
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};

pub const STARTER_NAME: &str = "Forge";

/// The starter agent: Claude Code, Sonnet, edits files.
pub fn starter_spec() -> AgentSpec {
    AgentSpec {
        name: STARTER_NAME.into(),
        role: "Engineer".into(),
        model: "sonnet".into(),
        permission_mode: PermissionMode::EditFiles,
        ..blank_spec()
    }
}

/// The starter for the CLIs crewd found: Claude Code unless it isn't
/// installed and Codex is, then Codex on its default model.
pub fn starter_for(runtimes: &[RuntimeInfo]) -> AgentSpec {
    match only_codex(runtimes) {
        Some(true) => AgentSpec {
            runtime: Runtime::Codex,
            model: default_codex_model(models_of(runtimes, &Runtime::Codex)),
            ..starter_spec()
        },
        _ => starter_spec(),
    }
}

pub const CODEX_LOGIN_FIRST: &str = "Run `codex login` in a terminal, then come back.";

/// With Claude Code known to be missing and Codex installed, whether
/// Codex can run the starter (it isn't known to be signed out). `None`
/// otherwise.
fn only_codex(runtimes: &[RuntimeInfo]) -> Option<bool> {
    let find = |runtime: Runtime| runtimes.iter().find(|r| r.runtime == runtime);
    match (find(Runtime::ClaudeCode), find(Runtime::Codex)) {
        (Some(claude), Some(codex)) if !claude.installed && codex.installed => {
            Some(codex.signed_in != Some(false))
        }
        _ => None,
    }
}

/// What to do first when the only CLI is a signed-out Codex.
pub fn starter_note(runtimes: &[RuntimeInfo]) -> Option<&'static str> {
    (only_codex(runtimes) == Some(false)).then_some(CODEX_LOGIN_FIRST)
}

/// Step 3's line about the starter.
fn starter_about(spec: &AgentSpec, runtimes: &[RuntimeInfo]) -> String {
    let model = match spec.runtime {
        Runtime::ClaudeCode => "Sonnet".to_string(),
        Runtime::Codex => models_of(runtimes, &Runtime::Codex)
            .iter()
            .find(|m| m.id == spec.model)
            .map_or_else(|| "its default model".to_string(), |m| m.label.clone()),
    };
    let runtime = super::settings::runtimes::runtime_name(&spec.runtime);
    format!("{STARTER_NAME}: {runtime} · {model} · edits files. You can change it in Settings.")
}

/// Where the first launch is.
#[derive(Default)]
pub struct FirstLaunch {
    /// The project you added, its `#general` and its folder. Set once:
    /// step 1 is then done.
    pub project: Option<ProjectId>,
    pub channel: Option<ChannelId>,
    pub added: Option<AddedFolder>,
    pub error: Option<String>,
    /// Waiting on crewd.
    pub busy: bool,
}

/// The folder added in step 1, as step 1 shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct AddedFolder {
    pub name: String,
    pub root: PathBuf,
    /// The checked out branch, for a git repo.
    pub branch: Option<String>,
}

impl Workspace {
    /// The Add project dialog added `project`, or opened one that has the
    /// folder, while the first launch shows: step 1 is done. If step 1
    /// was done already, the first launch ends and `project` opens.
    pub(super) fn first_launch_added(
        &mut self,
        project: ProjectId,
        channel: Option<ChannelId>,
        added: AddedFolder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.first_launch.as_mut() {
            Some(first) if first.project.is_none() => {
                first.project = Some(project);
                first.channel = channel;
                first.added = Some(added);
                first.error = None;
            }
            _ => {
                self.first_launch = None;
                let place = match channel {
                    Some(channel) => Place::Channel(channel),
                    None => Place::Project(project),
                };
                self.open_place(place, window, cx);
            }
        }
        cx.notify();
    }

    /// Makes Forge unless an agent has that name, then opens a new thread
    /// in the project's `#general` (made with the project, or made now if
    /// it has no channel).
    fn create_starter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((project, made_channel)) = self
            .first_launch
            .as_ref()
            .filter(|f| !f.busy)
            .and_then(|f| Some((f.project?, f.channel)))
        else {
            return;
        };
        if let Some(first) = self.first_launch.as_mut() {
            first.busy = true;
            first.error = None;
        }
        let state = self.state();
        let has_starter = state
            .agents()
            .iter()
            .any(|a| a.spec.name.eq_ignore_ascii_case(STARTER_NAME));
        let channels = state.channels(project);
        let existing = made_channel.or_else(|| {
            channels
                .iter()
                .find(|c| c.name == FIRST_CHANNEL)
                .or(channels.first())
                .map(|c| c.id)
        });
        let client = self.client.clone();
        let spec = starter_for(&self.runtimes);
        self.spawn_request(window, cx, async move |this, cx| {
            let made = async {
                if !has_starter {
                    let agent = client.request(Request::CreateAgent { spec }).await?;
                    serde_json::from_value::<AgentId>(agent).map_err(|e| e.to_string())?;
                }
                match existing {
                    Some(channel) => Ok(channel),
                    None => {
                        let channel = client
                            .request(Request::CreateChannel {
                                project,
                                name: FIRST_CHANNEL.into(),
                            })
                            .await?;
                        serde_json::from_value::<ChannelId>(channel).map_err(|e| e.to_string())
                    }
                }
            }
            .await;
            this.update_in(cx, |this, window, cx| match made {
                Ok(channel) => {
                    this.first_launch = None;
                    this.select_channel(channel, window, cx);
                    this.new_thread(window, cx);
                }
                Err(e) => {
                    if let Some(first) = this.first_launch.as_mut() {
                        first.busy = false;
                        first.error = Some(e);
                    }
                    cx.notify();
                }
            })
            .ok();
            Ok(())
        });
        cx.notify();
    }

    /// "Set up in Settings instead": Settings › Projects.
    fn skip_first_launch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.first_launch = None;
        self.open_settings(Page::Projects, window, cx);
    }

    pub(super) fn render_first_launch(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(first) = &self.first_launch else {
            return div().into_any_element();
        };
        // Done once the project is added, whether or not crewd's notice of
        // it arrived yet.
        let added = first.added.as_ref().filter(|_| first.project.is_some());
        let step1 = match added {
            Some(added) => {
                let name = added.name.clone();
                let place = match &added.branch {
                    Some(branch) => format!("{} · {branch}", home_relative(&added.root)),
                    None => home_relative(&added.root),
                };
                div()
                    .id("first-launch-project")
                    .flex()
                    .items_center()
                    .gap(ui_px(8.))
                    .aria_label(format!("{name} added"))
                    .child(div().text_color(color("success")).child("✓"))
                    .child(div().font_weight(weight(FontWeight::MEDIUM)).child(name))
                    .child(
                        div()
                            .font_family(code_family())
                            .text_size(scale::mono(12.))
                            .text_color(color("text-tertiary"))
                            .child(place),
                    )
                    .test_support()
                    .into_any_element()
            }
            None => div()
                .flex()
                .child(
                    Button::new("first-launch-pick")
                        .outline()
                        .small()
                        .label("Choose a folder…")
                        .disabled(first.busy)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_add_project_and_pick(window, cx);
                        })),
                )
                .into_any_element(),
        };
        let runtimes: Vec<AnyElement> = if self.runtimes.is_empty() {
            vec![muted_line("Checking which agent CLIs are installed…").into_any_element()]
        } else {
            self.runtimes
                .iter()
                .map(|info| {
                    let name = runtime_name(&info.runtime);
                    let (word, token) = runtime_state(info);
                    let version = info
                        .version
                        .as_ref()
                        .map(|v| format!(" {v}"))
                        .unwrap_or_default();
                    div()
                        .id(SharedString::from(format!("first-launch-runtime-{name}")))
                        .flex()
                        .items_center()
                        .gap(ui_px(6.))
                        .aria_label(format!("{name}{version} {word}"))
                        .child(provider_icon(&info.runtime, 16.))
                        .child(div().child(format!("{name}{version}")))
                        .child(div().text_color(color(token)).child(word))
                        .test_support()
                        .into_any_element()
                })
                .collect()
        };
        let starter = starter_for(&self.runtimes);
        // No word on installing Claude Code when Codex makes the starter.
        let claude_missing = starter.runtime == Runtime::ClaudeCode
            && self
                .runtimes
                .iter()
                .any(|r| r.runtime == Runtime::ClaudeCode && !r.installed);
        let about = starter_about(&starter, &self.runtimes);
        let note = starter_note(&self.runtimes);

        div()
            .id("first-launch")
            .flex()
            .flex_1()
            .min_h_0()
            .justify_center()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .max_w(ui_px(560.))
                    .py(ui_px(48.))
                    .px(ui_px(16.))
                    .gap(ui_px(24.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(ui_px(4.))
                            .child(
                                div()
                                    .id("first-launch-title")
                                    .text_size(scale::text(24.))
                                    .line_height(scale::line(32.))
                                    .font_weight(weight(FontWeight::SEMIBOLD))
                                    .aria_label("Welcome to Crew")
                                    .child("Welcome to Crew")
                                    .test_support(),
                            )
                            .child(muted_line(
                                "Your coding agents work as members of a chat. Three steps and you can give them a task.",
                            )),
                    )
                    .child(step(
                        1,
                        "Add your first project",
                        "A folder on this Mac: a git repo, a folder of repos or a plain folder.",
                        div()
                            .flex()
                            .flex_col()
                            .gap(ui_px(6.))
                            .child(step1)
                            .when_some(first.error.clone(), |col, e| {
                                col.child(super::settings::form_error("first-launch-error", e))
                            }),
                    ))
                    .child(step(
                        2,
                        "Check your agent CLIs",
                        "Crew runs the CLIs you installed and signed in to.",
                        div()
                            .id("first-launch-runtimes")
                            .flex()
                            .flex_col()
                            .gap(ui_px(4.))
                            .children(runtimes)
                            .when(claude_missing, |col| col.child(muted_line(INSTALL_CLAUDE)))
                            .test_support(),
                    ))
                    .child(step(
                        3,
                        "Create a starter agent",
                        about,
                        div()
                            .flex()
                            .flex_col()
                            .gap(ui_px(6.))
                            .when_some(note, |col, note| {
                                col.child(
                                    div()
                                        .id("first-launch-starter-note")
                                        .text_color(color("warning"))
                                        .aria_label(note)
                                        .child(note)
                                        .test_support(),
                                )
                            })
                            .child(
                                div().flex().child(
                                    Button::new("first-launch-starter")
                                        .primary()
                                        .small()
                                        .label("Create Forge and start")
                                        .disabled(first.project.is_none() || first.busy)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.create_starter(window, cx);
                                        })),
                                ),
                            ),
                    ))
                    .child(
                        div()
                            .id("first-launch-skip")
                            .text_color(color("accent-blue"))
                            .cursor_pointer()
                            .role(Role::Link)
                            .aria_label("Set up in Settings instead")
                            .child("Set up in Settings instead")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.skip_first_launch(window, cx);
                            }))
                            .test_support(),
                    ),
            )
            .test_support()
            .into_any_element()
    }
}

fn muted_line(text: &'static str) -> impl IntoElement {
    div().text_color(color("text-secondary")).child(text)
}

fn step(
    n: u8,
    title: &'static str,
    about: impl Into<SharedString>,
    body: impl IntoElement,
) -> impl IntoElement {
    let about: SharedString = about.into();
    div()
        .flex()
        .gap(ui_px(12.))
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(ui_px(24.))
                .rounded_full()
                .border_1()
                .border_color(color("border-200"))
                .text_size(scale::text(12.))
                .font_weight(weight(FontWeight::MEDIUM))
                .text_color(color("text-secondary"))
                .child(n.to_string()),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .gap(ui_px(8.))
                .child(div().font_weight(weight(FontWeight::SEMIBOLD)).child(title))
                .child(
                    div()
                        .text_size(scale::text(12.))
                        .text_color(color("text-tertiary"))
                        .id(SharedString::from(format!("first-launch-step-{n}")))
                        .aria_label(about.clone())
                        .child(about)
                        .test_support(),
                )
                .child(body),
        )
}

#[cfg(test)]
mod tests {
    use super::{CODEX_LOGIN_FIRST, starter_for, starter_note, starter_spec};
    use crew_core::agent::{PermissionMode, Runtime};
    use crew_proto::{ModelInfo, RuntimeInfo};

    #[test]
    fn the_starter_is_forge_on_sonnet_editing_files() {
        let spec = starter_spec();
        assert_eq!(spec.name, "Forge");
        assert_eq!(spec.role, "Engineer");
        assert_eq!(spec.model, "sonnet");
        assert_eq!(spec.permission_mode, PermissionMode::EditFiles);
        assert_eq!(spec.runtime, crew_core::agent::Runtime::ClaudeCode);
    }

    #[test]
    fn the_starter_runs_on_codex_only_without_claude() {
        let info = |runtime, installed| RuntimeInfo {
            runtime,
            installed,
            version: None,
            signed_in: None,
            path: None,
            models: vec![
                ModelInfo {
                    id: "gpt-6.1-sol".into(),
                    label: "GPT-6.1-Sol".into(),
                },
                ModelInfo {
                    id: "gpt-6-luna".into(),
                    label: "GPT-6-Luna".into(),
                },
            ],
        };
        let both = [info(Runtime::ClaudeCode, true), info(Runtime::Codex, true)];
        assert_eq!(starter_for(&both), starter_spec());
        assert_eq!(starter_for(&[]), starter_spec(), "still checking");
        let codex_only = [info(Runtime::ClaudeCode, false), info(Runtime::Codex, true)];
        let spec = starter_for(&codex_only);
        assert_eq!(spec.runtime, Runtime::Codex);
        assert_eq!(spec.model, "gpt-6-luna");
        assert_eq!(spec.name, "Forge");
        assert_eq!(spec.permission_mode, PermissionMode::EditFiles);
        let neither = [
            info(Runtime::ClaudeCode, false),
            info(Runtime::Codex, false),
        ];
        assert_eq!(starter_for(&neither), starter_spec());
        assert_eq!(starter_note(&neither), None);
        // A signed-out Codex can't run the starter: say how to sign in.
        let mut signed_out = codex_only.clone();
        signed_out[1].signed_in = Some(false);
        assert_eq!(starter_for(&signed_out), starter_spec());
        assert_eq!(starter_note(&signed_out), Some(CODEX_LOGIN_FIRST));
        assert_eq!(starter_note(&codex_only), None);
        assert_eq!(starter_note(&both), None);
    }
}
