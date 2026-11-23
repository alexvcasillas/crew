//! The first launch: no projects yet. Add one, check the agent CLIs and
//! make a starter agent, then land in `#general`.

use crew_core::agent::{AgentSpec, PermissionMode};
use crew_core::ids::{AgentId, ChannelId, ProjectId};
use crew_proto::Request;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Disableable as _, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::settings::Page;
use super::settings::agents::{INSTALL_CLAUDE, blank_spec};
use super::settings::projects::{FIRST_CHANNEL, FolderFor, Repo, create_project, folder_name};
use super::settings::runtimes::{listed_state, runtime_name};
use super::title_bar::home_relative;
use super::workspace::Workspace;
use crate::fonts::mono_font;
use crate::theme::color;

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

/// Where the first launch is.
#[derive(Default)]
pub struct FirstLaunch {
    /// The project you added, its `#general` and its folder. Set once:
    /// step 1 is then done.
    pub project: Option<ProjectId>,
    pub channel: Option<ChannelId>,
    pub repo: Option<Repo>,
    pub error: Option<String>,
    /// Waiting on crewd.
    pub busy: bool,
}

impl Workspace {
    /// Called with the folder you picked, checked.
    pub(super) fn first_launch_folder(
        &mut self,
        checked: Result<Repo, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A project was already added, or is being added.
        let Some(first) = self
            .first_launch
            .as_mut()
            .filter(|f| f.project.is_none() && !f.busy)
        else {
            return;
        };
        let repo = match checked {
            Ok(repo) => repo,
            Err(e) => {
                first.error = Some(e);
                cx.notify();
                return;
            }
        };
        first.error = None;
        first.busy = true;
        let made = create_project(self.client.clone(), folder_name(&repo.root), repo.clone());
        self.spawn_request(window, cx, async move |this, cx| {
            let made = made.await;
            this.update(cx, |this, cx| {
                if let Some(first) = this.first_launch.as_mut() {
                    first.busy = false;
                    match made {
                        Ok((project, channel)) => {
                            first.project = Some(project);
                            first.channel = Some(channel);
                            first.repo = Some(repo);
                        }
                        Err(e) => first.error = Some(e),
                    }
                }
                cx.notify();
            })
            .ok();
            Ok(())
        });
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
        self.spawn_request(window, cx, async move |this, cx| {
            let made = async {
                if !has_starter {
                    let agent = client
                        .request(Request::CreateAgent {
                            spec: starter_spec(),
                        })
                        .await?;
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
        let added = first.repo.as_ref().filter(|_| first.project.is_some());
        let step1 = match added {
            Some(repo) => {
                let name = folder_name(&repo.root);
                div()
                    .id("first-launch-project")
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .aria_label(format!("{name} added"))
                    .child(div().text_color(color("success")).child("✓"))
                    .child(div().font_weight(FontWeight::MEDIUM).child(name))
                    .child(
                        div()
                            .font_family(mono_font())
                            .text_size(px(12.))
                            .text_color(color("text-tertiary"))
                            .child(format!("{} · {}", home_relative(&repo.root), repo.branch)),
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
                        .disabled(first.busy || self.picking_folder())
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.choose_folder(FolderFor::FirstLaunch, window, cx);
                        })),
                )
                .into_any_element(),
        };
        let runtimes: Vec<AnyElement> = if self.runtimes.is_empty() {
            vec![line("Checking which agent CLIs are installed…").into_any_element()]
        } else {
            self.runtimes
                .iter()
                .map(|info| {
                    let name = runtime_name(&info.runtime);
                    let (word, token) = listed_state(info);
                    let version = info
                        .version
                        .as_ref()
                        .map(|v| format!(" {v}"))
                        .unwrap_or_default();
                    div()
                        .id(SharedString::from(format!("first-launch-runtime-{name}")))
                        .flex()
                        .gap(px(6.))
                        .aria_label(format!("{name}{version} {word}"))
                        .child(div().child(format!("{name}{version}")))
                        .child(div().text_color(color(token)).child(word))
                        .test_support()
                        .into_any_element()
                })
                .collect()
        };
        let claude_missing = self
            .runtimes
            .iter()
            .any(|r| r.runtime == crew_core::agent::Runtime::ClaudeCode && !r.installed);

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
                    .max_w(px(560.))
                    .py(px(48.))
                    .px(px(16.))
                    .gap(px(24.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .child(
                                div()
                                    .id("first-launch-title")
                                    .text_size(px(24.))
                                    .line_height(px(32.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .aria_label("Welcome to Crew")
                                    .child("Welcome to Crew")
                                    .test_support(),
                            )
                            .child(line(
                                "Your coding agents work as members of a chat. Three steps and you can give them a task.",
                            )),
                    )
                    .child(step(
                        1,
                        "Add your first project",
                        "A git repository on this Mac. Each thread gets its own worktree, so your checkout stays as it is.",
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
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
                            .gap(px(4.))
                            .children(runtimes)
                            .when(claude_missing, |col| col.child(line(INSTALL_CLAUDE)))
                            .test_support(),
                    ))
                    .child(step(
                        3,
                        "Create a starter agent",
                        "Forge: Claude Code · Sonnet · edits files. You can change it in Settings.",
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

fn line(text: &'static str) -> impl IntoElement {
    div().text_color(color("text-secondary")).child(text)
}

fn step(
    n: u8,
    title: &'static str,
    about: &'static str,
    body: impl IntoElement,
) -> impl IntoElement {
    div()
        .flex()
        .gap(px(12.))
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(px(24.))
                .rounded_full()
                .border_1()
                .border_color(color("border-200"))
                .text_size(px(12.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(color("text-secondary"))
                .child(n.to_string()),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .gap(px(8.))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(color("text-tertiary"))
                        .child(about),
                )
                .child(body),
        )
}

#[cfg(test)]
mod tests {
    use super::starter_spec;
    use crew_core::agent::PermissionMode;

    #[test]
    fn the_starter_is_forge_on_sonnet_editing_files() {
        let spec = starter_spec();
        assert_eq!(spec.name, "Forge");
        assert_eq!(spec.role, "Engineer");
        assert_eq!(spec.model, "sonnet");
        assert_eq!(spec.permission_mode, PermissionMode::EditFiles);
        assert_eq!(spec.runtime, crew_core::agent::Runtime::ClaudeCode);
    }
}
