//! The status bar: the connection when it isn't up, who is working, what
//! waits on you, live terminal sessions, which agent CLIs are ready, and
//! Stop all. On Running now it explains Stop and links to the run cap.

use crew_core::agent::Runtime;
use crew_proto::RuntimeInfo;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Disableable as _, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::{dot, spinner};
use super::running_view::{STOP_HINT, max_runs_line};
use super::settings::Page;
use super::workspace::Workspace;
use crate::connection::ConnectionState;
use crate::theme::color;

pub const STATUS_BAR_HEIGHT: f32 = 28.;
/// Names the status bar lists before "+N more".
const NAMES_SHOWN: usize = 3;

/// The connection item while not connected: its dot token and words.
fn connection_line(state: Option<&ConnectionState>) -> Option<(&'static str, &'static str)> {
    match state {
        None => Some(("text-tertiary", "Connecting…")),
        Some(ConnectionState::Connected) => None,
        Some(ConnectionState::Reconnecting) => Some(("text-tertiary", "Reconnecting…")),
        Some(ConnectionState::VersionMismatch { .. }) => Some(("warning", "Service out of date")),
        Some(ConnectionState::NotRunning) => Some(("text-tertiary", "Not connected")),
    }
}

/// "Forge, Scout and Prism working", at most three names, then "+N more".
pub fn working_line(names: &[String]) -> Option<String> {
    let list = match names {
        [] => return None,
        [one] => one.clone(),
        _ if names.len() > NAMES_SHOWN => format!(
            "{} +{} more",
            names[..NAMES_SHOWN].join(", "),
            names.len() - NAMES_SHOWN
        ),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    };
    Some(format!("{list} working"))
}

/// Each installed CLI as words and their colour token: "Claude Code
/// signed in", "Codex installed". CLIs that aren't installed are left out.
pub fn runtime_words(runtimes: &[RuntimeInfo]) -> Vec<(String, &'static str)> {
    runtimes
        .iter()
        .filter(|r| r.installed)
        .map(|r| {
            let name = match r.runtime {
                Runtime::ClaudeCode => "Claude Code",
                Runtime::Codex => "Codex",
            };
            match r.signed_in {
                Some(true) => (format!("{name} signed in"), "success"),
                Some(false) => (format!("{name} signed out"), "warning"),
                None => (format!("{name} installed"), "text-secondary"),
            }
        })
        .collect()
}

impl Workspace {
    pub(super) fn render_status_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let state = self.state();
        let connection = connection_line(state.connection());
        let connected = connection.is_none();
        if self.running.is_some() {
            return self.render_running_status(connection, cx);
        }
        let names: Vec<String> = state
            .working_agents()
            .into_iter()
            .filter_map(|a| state.agent(a).map(|a| a.spec.name.clone()))
            .collect();
        let working = working_line(&names).filter(|_| connected);
        let needs = state.needs_you().len();
        let needs = (connected && needs > 0).then(|| format!("{needs} need you"));
        // Not Crew runs, so counted on their own.
        let terminal = state.live_terminal_sessions().len();
        let terminal = (connected && terminal > 0).then(|| format!("{terminal} in terminal"));
        let runtimes = if connected {
            runtime_words(&self.runtimes)
        } else {
            Vec::new()
        };
        let runtimes_label = runtimes
            .iter()
            .map(|(w, _)| w.as_str())
            .collect::<Vec<_>>()
            .join(" · ");
        let runtime_items: Vec<AnyElement> = runtimes
            .into_iter()
            .enumerate()
            .flat_map(|(i, (words, token))| {
                let sep = (i > 0).then(|| {
                    div()
                        .text_color(color("text-tertiary"))
                        .child("·")
                        .into_any_element()
                });
                sep.into_iter().chain(Some(
                    div()
                        .text_color(color(token))
                        .child(words)
                        .into_any_element(),
                ))
            })
            .collect();
        status_frame()
            .when_some(connection, |bar, (token, text)| {
                bar.child(item("status-connection", token, text.to_string()))
            })
            .when_some(working, |bar, text| {
                bar.child(
                    div()
                        .id("status-working")
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(px(6.))
                        .aria_label(text.clone())
                        .child(spinner("status-working-spinner", 12., "accent-blue"))
                        .child(text)
                        .test_support(),
                )
            })
            .when_some(needs, |bar, text| {
                bar.child(item("status-needs", "warning", text))
            })
            .when_some(terminal, |bar, text| {
                bar.child(item("status-terminal", "accent-blue", text))
            })
            .child(div().flex_1())
            .when(!runtime_items.is_empty(), |bar| {
                bar.child(
                    div()
                        .id("status-runtimes")
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .aria_label(runtimes_label)
                        .children(runtime_items)
                        .test_support(),
                )
            })
            .when(connected, |bar| {
                let (enabled, token) = self.stop_all_look();
                bar.child(
                    Button::new("status-stop-all")
                        .ghost()
                        .xsmall()
                        .text_color(color(token))
                        .label("Stop all ⌘⇧.")
                        .disabled(!enabled)
                        .on_click(cx.listener(|this, _, window, cx| this.stop_all(window, cx))),
                )
            })
            .test_support()
            .into_any_element()
    }

    /// Running now's status bar: what Stop does, and the run cap with a
    /// link to change it.
    fn render_running_status(
        &self,
        connection: Option<(&'static str, &'static str)>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cap = max_runs_line(self.state().limits().max_runs);
        status_frame()
            .when_some(connection, |bar, (token, text)| {
                bar.child(item("status-connection", token, text.to_string()))
            })
            .child(
                div()
                    .id("status-running-hint")
                    .flex_none()
                    .aria_label(STOP_HINT)
                    .child(STOP_HINT)
                    .test_support(),
            )
            .child(div().flex_1())
            .when(connection.is_none(), |bar| {
                bar.child(
                    div()
                        .id("status-max-runs")
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(px(4.))
                        .aria_label(format!("{cap} · change"))
                        .child(cap)
                        .child(div().text_color(color("text-tertiary")).child("·"))
                        .child(
                            div()
                                .id("status-limits-change")
                                .cursor_pointer()
                                .text_color(color("accent-blue"))
                                .hover(|link| link.underline())
                                .aria_label("change")
                                .child("change")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_settings(Page::Limits, window, cx);
                                }))
                                .test_support(),
                        )
                        .test_support(),
                )
            })
            .test_support()
            .into_any_element()
    }
}

/// The bar itself, before its items.
fn status_frame() -> Stateful<Div> {
    div()
        .id("status-bar")
        .flex()
        .flex_none()
        .items_center()
        .gap(px(16.))
        .h(px(STATUS_BAR_HEIGHT))
        .px(px(16.))
        .border_t_1()
        .border_color(color("border-100"))
        .text_size(px(12.))
        .text_color(color("text-secondary"))
}

/// A dot and words, labelled with the words.
fn item(id: &'static str, token: &str, text: String) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.))
        .aria_label(text.clone())
        .child(dot(token))
        .child(text)
        .test_support()
}

#[cfg(test)]
mod tests {
    use super::{runtime_words, working_line};
    use crew_core::agent::Runtime;
    use crew_proto::RuntimeInfo;

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_working_line_lists_up_to_three_names() {
        assert_eq!(working_line(&[]), None);
        assert_eq!(
            working_line(&names(&["Forge"])).as_deref(),
            Some("Forge working")
        );
        assert_eq!(
            working_line(&names(&["Forge", "Scout"])).as_deref(),
            Some("Forge and Scout working")
        );
        assert_eq!(
            working_line(&names(&["Forge", "Scout", "Prism"])).as_deref(),
            Some("Forge, Scout and Prism working")
        );
        assert_eq!(
            working_line(&names(&["Forge", "Scout", "Prism", "Atlas", "Echo"])).as_deref(),
            Some("Forge, Scout, Prism +2 more working")
        );
    }

    #[test]
    fn runtimes_read_as_signed_in_or_installed() {
        let info = |runtime, installed, signed_in| RuntimeInfo {
            runtime,
            installed,
            version: None,
            signed_in,
            path: None,
            models: vec![],
        };
        let words = runtime_words(&[
            info(Runtime::ClaudeCode, true, Some(true)),
            info(Runtime::Codex, true, None),
        ]);
        assert_eq!(
            words,
            [
                ("Claude Code signed in".to_string(), "success"),
                ("Codex installed".to_string(), "text-secondary"),
            ]
        );
        let words = runtime_words(&[
            info(Runtime::ClaudeCode, true, Some(false)),
            info(Runtime::Codex, false, None),
        ]);
        assert_eq!(words, [("Claude Code signed out".to_string(), "warning")]);
    }
}
