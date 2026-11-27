//! The title bar: the app name, the search button, run counts and
//! settings.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{IconName, Sizable as _, TitleBar};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use std::path::{Path, PathBuf};

use super::common::{chip, dot};
use super::settings::Page;
use super::workspace::Workspace;
use crate::app;
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_family, ui_px, weight};

pub const TITLE_BAR_HEIGHT: f32 = 48.;
/// The Kit title bar's left padding for the traffic lights on macOS.
const TRAFFIC_LIGHTS_INSET: f32 = 80.;
/// The breadcrumb separators, dimmer than any text token.
const SLASH: u32 = 0x3a3a3a;
const SEARCH_WIDTH: f32 = 320.;
/// Narrowest the search button gets in a crowded title bar.
const SEARCH_MIN_WIDTH: f32 = 120.;
/// Widest the project path chip gets before its start is cut.
const PATH_MAX_WIDTH: f32 = 220.;
/// The path chip's padding and border, left and right.
const CHIP_INSET: f32 = 14.;
const PATH_FONT_SIZE: f32 = 11.;
const SEARCH_PLACEHOLDER: &str = "Search threads, agents, files…";

impl Workspace {
    /// The title bar of a window `width` wide.
    pub(super) fn render_title_bar(
        &self,
        width: Pixels,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let special = self
            .render_settings_title(cx)
            .or_else(|| self.render_running_title(cx))
            .or_else(|| self.render_review_title(cx));
        if let Some((left, right)) = special {
            return TitleBar::new()
                .h(scale::chrome(TITLE_BAR_HEIGHT))
                .bg(color("bg-100"))
                .border_color(color("border-100"))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .px(ui_px(8.))
                        .child(left),
                )
                .child(div().flex_none().pr(ui_px(12.)).child(right));
        }
        let search = search_width(width);
        let crowded = search < ui_px(SEARCH_WIDTH);
        TitleBar::new()
            .h(scale::chrome(TITLE_BAR_HEIGHT))
            .bg(color("bg-100"))
            .border_color(color("border-100"))
            .child(
                // Equal flexible sides keep the search button centred.
                // "Crew" always shows; the breadcrumb after it is cut, and
                // never reaches under the search button.
                div()
                    .flex()
                    .items_center()
                    .gap(ui_px(8.))
                    .flex_1()
                    .flex_basis(ui_px(0.))
                    .min_w(app_name_width(cx) + ui_px(16.))
                    .overflow_hidden()
                    .mr(ui_px(12.))
                    .px(ui_px(8.))
                    .child(
                        div()
                            .id("title-app")
                            .flex_none()
                            .font_weight(weight(FontWeight::SEMIBOLD))
                            .aria_label("Crew")
                            .child("Crew")
                            .test_support(),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(ui_px(8.))
                            .min_w_0()
                            .overflow_hidden()
                            .children(self.render_breadcrumb(cx)),
                    ),
            )
            .child(render_search_button(search, cx))
            .child(
                div()
                    .flex_1()
                    .flex_basis(ui_px(0.))
                    .flex()
                    .justify_end()
                    // Mirrors the traffic-light inset on the left, which
                    // doesn't zoom, unless the bar is too crowded for it.
                    .items_center()
                    .gap(ui_px(8.))
                    .pl(if crowded {
                        ui_px(12.)
                    } else {
                        px(TRAFFIC_LIGHTS_INSET)
                    })
                    .pr(ui_px(12.))
                    .children(self.render_title_actions(cx)),
            )
    }

    /// "● N running · ● M need you", or "Nothing running", and the
    /// settings button. Every view's title bar ends with them. The first
    /// opens Running now.
    pub(super) fn render_title_actions(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let (running, needs) = self.run_counts();
        let live = running + needs > 0;
        let label = if live {
            format!("{running} running · {needs} need you")
        } else {
            "Nothing running".to_string()
        };
        let mut actions = vec![
            div()
                .id("title-running")
                .flex()
                .flex_none()
                .items_center()
                .gap(ui_px(6.))
                .h(scale::row(28.))
                .px(ui_px(10.))
                .border_1()
                .border_color(color("border-200"))
                .rounded(ui_px(6.))
                .text_size(scale::text(12.))
                .text_color(color("text-secondary"))
                .cursor_pointer()
                .hover(|s| s.bg(color("surface-100")))
                .role(Role::Button)
                .aria_label(label)
                .map(|button| {
                    if live {
                        button
                            .child(dot("accent-blue"))
                            .child(format!("{running} running"))
                            .child(div().text_color(color("text-tertiary")).child("·"))
                            .child(dot("warning"))
                            .child(format!("{needs} need you"))
                    } else {
                        button.child("Nothing running")
                    }
                })
                .on_click(cx.listener(|this, _, window, cx| this.show_running(window, cx)))
                .test_support()
                .into_any_element(),
        ];
        actions.push(
            Button::new("title-settings")
                .ghost()
                .small()
                .icon(IconName::Settings)
                .accessibility_label("Settings")
                .tooltip("Settings (⌘,)")
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_settings(Page::General, window, cx);
                }))
                .into_any_element(),
        );
        actions
    }
}

impl Workspace {
    /// `/ project / #channel` and the project's folder, for the selected
    /// channel.
    fn render_breadcrumb(&self, cx: &App) -> Vec<AnyElement> {
        let state = self.state();
        let slash = || div().flex_none().text_color(rgb(SLASH)).child("/");
        let crumb = |id: &'static str, text: String| {
            div()
                .id(id)
                .min_w_0()
                .truncate()
                .font_weight(weight(FontWeight::MEDIUM))
                .aria_label(text.clone())
                .child(text)
                .test_support()
                .into_any_element()
        };
        if self.scheduled.is_some() {
            return vec![
                slash().into_any_element(),
                crumb("title-scheduled", "Scheduled".into()),
            ];
        }
        // A page shows where it is: `/ project` or `/ Agents / Name`.
        if let Some(project) = self.page_project().and_then(|p| state.project(p)) {
            return vec![
                slash().into_any_element(),
                crumb("title-project", project.name.clone()),
            ];
        }
        if let Some(agent) = self.page_agent().filter(|a| state.agent(*a).is_some()) {
            return vec![
                slash().into_any_element(),
                crumb("title-agents", "Agents".into()),
                slash().into_any_element(),
                crumb("title-agent", super::project_view::agent_name(self, agent)),
            ];
        }
        let Some(channel) = self.nav.channel.and_then(|c| state.channel(c)) else {
            return Vec::new();
        };
        let Some(project) = state.project(channel.project) else {
            return Vec::new();
        };
        let full = home_relative(&project.root);
        let text = cx.text_system();
        let font_id = text.resolve_font(&font(code_family()));
        let width = |s: &str| -> f32 {
            s.chars()
                .filter_map(|c| text.advance(font_id, scale::mono(PATH_FONT_SIZE), c).ok())
                .map(|size| size.width.as_f32())
                .sum()
        };
        let path = cut_start(&full, f32::from(ui_px(PATH_MAX_WIDTH - CHIP_INSET)), width);
        vec![
            slash().into_any_element(),
            crumb("title-project", project.name.clone()),
            slash().into_any_element(),
            div()
                .id("title-channel")
                .flex()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .gap(ui_px(2.))
                .font_weight(weight(FontWeight::MEDIUM))
                .aria_label(format!("#{}", channel.name))
                .child(
                    div()
                        .font_family(code_family())
                        .text_size(scale::mono(13.))
                        .text_color(color("text-tertiary"))
                        .child("#"),
                )
                .child(channel.name.clone())
                .test_support()
                .into_any_element(),
            // When even the cut path doesn't fit, its start is clipped.
            chip(div().flex_none().child(path))
                .id("title-path")
                .flex()
                .justify_end()
                .flex_shrink(1.)
                .min_w_0()
                .max_w(ui_px(PATH_MAX_WIDTH))
                .overflow_hidden()
                .whitespace_nowrap()
                .aria_label(full)
                .test_support()
                .into_any_element(),
        ]
    }
}

/// `path` cut from its start until `width` says it fits in `max`,
/// keeping whole trailing folders where it can: `…/demo/repo`.
pub(super) fn cut_start(path: &str, max: f32, width: impl Fn(&str) -> f32) -> String {
    if width(path) <= max {
        return path.to_string();
    }
    let folders = path.match_indices('/').map(|(i, _)| i);
    let chars = path.char_indices().map(|(i, _)| i).skip(1);
    folders
        .chain(chars)
        .map(|i| format!("…{}", &path[i..]))
        .find(|cut| width(cut) <= max)
        .unwrap_or_else(|| "…".to_string())
}

/// `path` with the home folder written as `~`.
pub(super) fn home_relative(path: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) if !home.as_os_str().is_empty() => match path.strip_prefix(&home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        },
        _ => path.display().to_string(),
    }
}

/// How wide "Crew" is drawn in the title bar.
fn app_name_width(cx: &App) -> Pixels {
    let text = cx.text_system();
    let mut name = font(ui_family());
    name.weight = weight(FontWeight::SEMIBOLD);
    let id = text.resolve_font(&name);
    let size = scale::text(13.);
    "Crew"
        .chars()
        .filter_map(|c| text.advance(id, size, c).ok())
        .map(|a| a.width)
        .fold(px(0.), |sum, w| sum + w)
        .ceil()
}

/// The search button's width in a window `window` wide: its full width
/// unless big text or zoom leave too little room beside it.
fn search_width(window: Pixels) -> Pixels {
    // The room the rest of the title bar gets in the smallest window at
    // the default look, grown with the zoom and with bigger text.
    let text_growth = (scale::text(1.) / ui_px(1.)).max(1.);
    let rest = ui_px(app::WINDOW_MIN_WIDTH - SEARCH_WIDTH) * text_growth;
    (window - rest).clamp(ui_px(SEARCH_MIN_WIDTH), ui_px(SEARCH_WIDTH))
}

/// "Search threads, agents, files… ⌘K": opens the palette.
fn render_search_button(width: Pixels, cx: &mut Context<Workspace>) -> impl IntoElement {
    div()
        .id("title-search")
        .flex()
        .flex_none()
        .w(width)
        .items_center()
        .gap(ui_px(8.))
        .h(ui_px(32.))
        .pl(ui_px(10.))
        .pr(ui_px(8.))
        .border_1()
        .border_color(color("border-200"))
        .rounded(ui_px(6.))
        .bg(color("bg-100"))
        .text_color(color("text-tertiary"))
        .cursor_pointer()
        .hover(|s| s.bg(color("surface-100")))
        .role(Role::Button)
        .aria_label(SEARCH_PLACEHOLDER)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(SEARCH_PLACEHOLDER),
        )
        .child(
            div()
                .flex_none()
                .px(ui_px(5.))
                .border_1()
                .border_color(color("border-200"))
                .rounded(ui_px(4.))
                .bg(color("surface-300"))
                .font_family(code_family())
                .text_size(scale::mono(11.))
                .child("⌘K"),
        )
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(cx.listener(|this, _, window, cx| this.open_palette(window, cx)))
        .test_support()
}

#[cfg(test)]
mod tests {
    use super::cut_start;

    #[test]
    fn long_paths_lose_their_start() {
        // Every character 7 px wide, as in a mono font.
        let width = |s: &str| s.chars().count() as f32 * 7.;
        assert_eq!(cut_start("~/demo/repo", 200., width), "~/demo/repo");
        assert_eq!(
            cut_start("/private/var/folders/xy/work/demo/repo", 84., width),
            "…/demo/repo"
        );
        assert_eq!(
            cut_start("/a/averyveryverylongfoldername", 70., width),
            "…oldername"
        );
        assert_eq!(cut_start("/abc", 3., width), "…");
    }
}
