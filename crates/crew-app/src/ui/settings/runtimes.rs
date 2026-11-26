//! Settings › Runtimes: the agent CLIs crewd found.

use crew_core::agent::Runtime;
use crew_proto::{Request, RuntimeInfo};
use gpui_kit::component::button::Button;
use gpui_kit::component::{Disableable as _, IconName, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{help, page_scroll, page_title};
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};
use crate::ui::common::provider_icon;
use crate::ui::workspace::Workspace;

pub fn runtime_name(runtime: &Runtime) -> &'static str {
    match runtime {
        Runtime::ClaudeCode => "Claude Code",
        Runtime::Codex => "Codex",
    }
}

/// "Signed in", "Signed out", "Installed" or "Not installed", with its
/// colour token.
pub fn runtime_state(info: &RuntimeInfo) -> (&'static str, &'static str) {
    match (info.installed, info.signed_in) {
        (false, _) => ("Not installed", "text-tertiary"),
        (true, Some(true)) => ("Signed in", "success"),
        (true, Some(false)) => ("Signed out", "warning"),
        (true, None) => ("Installed", "text-secondary"),
    }
}

impl Workspace {
    /// Asks crewd which CLIs it finds now.
    pub(super) fn refresh_runtimes(&mut self, cx: &mut Context<Self>) {
        if self.runtimes_checking {
            return;
        }
        self.runtimes_checking = true;
        let answer = self.client.request(Request::Runtimes);
        self.runtimes_refresh = Some(cx.spawn(async move |this, cx| {
            let runtimes = answer.await.and_then(|v| {
                serde_json::from_value::<Vec<RuntimeInfo>>(v).map_err(|e| e.to_string())
            });
            this.update(cx, |this, cx| {
                this.runtimes_checking = false;
                match runtimes {
                    Ok(runtimes) => this.runtimes = runtimes,
                    Err(e) => eprintln!("crew: couldn't ask which agent CLIs are ready: {e}"),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    pub(super) fn render_runtimes_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let jump = self.jump(cx);
        let rows: Vec<AnyElement> = self
            .runtimes
            .iter()
            .map(|info| {
                let name = runtime_name(&info.runtime);
                let (word, token) = runtime_state(info);
                let detail = [
                    info.version.as_ref().map(|v| format!("version {v}")),
                    info.path.clone(),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
                let key = match info.runtime {
                    Runtime::ClaudeCode => "claude",
                    Runtime::Codex => "codex",
                };
                let id = format!("field-runtimes-{key}");
                div()
                    .relative()
                    .children(jump.mark(&id))
                    .id(SharedString::from(id))
                    .test_support()
                    .child(
                        div()
                            .id(SharedString::from(format!("runtime-row-{key}")))
                            .flex()
                            .items_center()
                            .gap(ui_px(12.))
                            .min_h(scale::row(52.))
                            .border_b_1()
                            .border_color(color("border-100"))
                            .aria_label(format!("{name} {word}"))
                            .child(provider_icon(&info.runtime, 16.))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        div().font_weight(weight(FontWeight::MEDIUM)).child(name),
                                    )
                                    .child(
                                        div()
                                            .truncate()
                                            .text_size(scale::mono(12.))
                                            .font_family(code_family())
                                            .text_color(color("text-tertiary"))
                                            .child(detail),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(scale::text(12.))
                                    .text_color(color(token))
                                    .child(word),
                            )
                            .test_support(),
                    )
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        page_scroll("settings-runtimes", &jump)
            .child(
                div()
                    .flex()
                    .items_center()
                    .max_w(ui_px(760.))
                    .child(div().flex_1().child(page_title("Runtimes")))
                    .child(
                        Button::new("runtimes-refresh")
                            .outline()
                            .small()
                            .icon(IconName::Redo)
                            .label(if self.runtimes_checking {
                                "Checking…"
                            } else {
                                "Refresh"
                            })
                            .disabled(self.runtimes_checking)
                            .on_click(cx.listener(|this, _, _, cx| this.refresh_runtimes(cx))),
                    ),
            )
            .child(help(
                "Crew runs the agent CLIs you installed. Sign in with each CLI's own login.",
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .max_w(ui_px(760.))
                    .children(rows)
                    .when(empty, |col| {
                        col.child(help("Checking which CLIs are installed…"))
                    }),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{runtime_name, runtime_state};
    use crew_core::agent::Runtime;
    use crew_proto::RuntimeInfo;

    #[test]
    fn runtimes_read_as_their_state() {
        let mut info = RuntimeInfo {
            runtime: Runtime::ClaudeCode,
            installed: true,
            version: Some("2.1.0".into()),
            signed_in: Some(true),
            path: Some("/usr/local/bin/claude".into()),
            models: vec![],
        };
        assert_eq!(runtime_state(&info), ("Signed in", "success"));
        info.signed_in = None;
        assert_eq!(runtime_state(&info).0, "Installed");
        info.installed = false;
        assert_eq!(runtime_state(&info).0, "Not installed");
        assert_eq!(runtime_name(&Runtime::Codex), "Codex");
        let codex = RuntimeInfo {
            runtime: Runtime::Codex,
            installed: true,
            ..info
        };
        assert_eq!(
            runtime_state(&codex).0,
            "Installed",
            "no longer coming soon"
        );
    }
}
