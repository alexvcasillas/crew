//! The title bar: the app name, the search button, run counts and
//! settings.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{IconName, Sizable as _, TitleBar};
use gpui_kit::*;

use std::path::{Path, PathBuf};

use super::common::{chip, dot};
use super::settings::Page;
use super::workspace::Workspace;
use crate::fonts::mono_font;
use crate::theme::color;

pub const TITLE_BAR_HEIGHT: f32 = 48.;
/// The Kit title bar's left padding for the traffic lights on macOS.
const TRAFFIC_LIGHTS_INSET: f32 = 80.;
/// The breadcrumb separators, dimmer than any text token.
const SLASH: u32 = 0x3a3a3a;
const SEARCH_WIDTH: f32 = 320.;
/// Widest the project path chip gets before its start is cut.
const PATH_MAX_WIDTH: f32 = 220.;
/// The path chip's padding and border, left and right.
const CHIP_INSET: f32 = 14.;
const PATH_FONT_SIZE: f32 = 11.;
const SEARCH_PLACEHOLDER: &str = "Search threads, agents, files…";

impl Workspace {
    pub(super) fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let special = self
            .render_settings_title(cx)
            .or_else(|| self.render_running_title(cx))
            .or_else(|| self.render_review_title(cx));
        if let Some((left, right)) = special {
            return TitleBar::new()
                .h(px(TITLE_BAR_HEIGHT))
                .bg(color("bg-100"))
                .border_color(color("border-100"))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .px(px(8.))
                        .child(left),
                )
                .child(div().flex_none().pr(px(12.)).child(right));
        }
        TitleBar::new()
            .h(px(TITLE_BAR_HEIGHT))
            .bg(color("bg-100"))
            .border_color(color("border-100"))
            .child(
                // Equal flexible sides keep the search button centred.
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .flex_1()
                    .flex_basis(px(0.))
                    .min_w_0()
                    // Never reaches under the search button.
                    .overflow_hidden()
                    .mr(px(12.))
                    .px(px(8.))
                    .child(
                        div()
                            .id("title-app")
                            .flex_none()
                            .font_weight(FontWeight::SEMIBOLD)
                            .aria_label("Crew")
                            .child("Crew")
                            .test_support(),
                    )
                    .children(self.render_breadcrumb(cx)),
            )
            .child(render_search_button())
            .child(
                div()
                    .flex_1()
                    .flex_basis(px(0.))
                    .flex()
                    .justify_end()
                    // Mirrors the traffic-light inset on the left.
                    .items_center()
                    .gap(px(8.))
                    .pl(px(TRAFFIC_LIGHTS_INSET))
                    .pr(px(12.))
                    .children(self.render_title_actions(cx)),
            )
    }

    /// "● N running · ● M need you", when anything runs or waits, and
    /// the settings button. Every view's title bar ends with them.
    pub(super) fn render_title_actions(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let (running, needs) = self.run_counts();
        let mut actions = Vec::new();
        if running + needs > 0 {
            let label = format!("{running} running · {needs} need you");
            actions.push(
                div()
                    .id("title-running")
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(6.))
                    .h(px(28.))
                    .px(px(10.))
                    .border_1()
                    .border_color(color("border-200"))
                    .rounded(px(6.))
                    .text_size(px(12.))
                    .text_color(color("text-secondary"))
                    .cursor_pointer()
                    .hover(|s| s.bg(color("surface-100")))
                    .role(Role::Button)
                    .aria_label(label)
                    .child(dot("accent-blue"))
                    .child(format!("{running} running"))
                    .child(div().text_color(color("text-tertiary")).child("·"))
                    .child(dot("warning"))
                    .child(format!("{needs} need you"))
                    .on_click(cx.listener(|this, _, window, cx| this.show_running(window, cx)))
                    .test_support()
                    .into_any_element(),
            );
        }
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
        let Some(channel) = self.nav.channel.and_then(|c| state.channel(c)) else {
            return Vec::new();
        };
        let Some(project) = state.project(channel.project) else {
            return Vec::new();
        };
        let full = home_relative(&project.root);
        let text = cx.text_system();
        let font_id = text.resolve_font(&font(mono_font()));
        let width = |s: &str| -> f32 {
            s.chars()
                .filter_map(|c| text.advance(font_id, px(PATH_FONT_SIZE), c).ok())
                .map(|size| size.width.as_f32())
                .sum()
        };
        let path = cut_start(&full, PATH_MAX_WIDTH - CHIP_INSET, width);
        let slash = || div().flex_none().text_color(rgb(SLASH)).child("/");
        vec![
            slash().into_any_element(),
            div()
                .id("title-project")
                .flex_none()
                .font_weight(FontWeight::MEDIUM)
                .aria_label(project.name.clone())
                .child(project.name.clone())
                .test_support()
                .into_any_element(),
            slash().into_any_element(),
            div()
                .id("title-channel")
                .flex()
                .flex_none()
                .gap(px(2.))
                .font_weight(FontWeight::MEDIUM)
                .aria_label(format!("#{}", channel.name))
                .child(
                    div()
                        .font_family(mono_font())
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
                .max_w(px(PATH_MAX_WIDTH))
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

/// Said by the search box and by ⌘K until search exists.
pub(super) const SEARCH_SOON: &str = "Search is coming soon.";

/// "Search threads, agents, files… ⌘K", looking off: search itself comes
/// later.
fn render_search_button() -> impl IntoElement {
    div()
        .id("title-search")
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.))
        .w(px(SEARCH_WIDTH))
        .h(px(32.))
        .pl(px(10.))
        .pr(px(8.))
        .border_1()
        .border_color(color("border-200"))
        .rounded(px(6.))
        .bg(color("bg-100"))
        .text_color(color("text-tertiary"))
        .aria_label(SEARCH_PLACEHOLDER)
        .aria_description(SEARCH_SOON)
        .tooltip(|window, cx| {
            gpui_kit::component::tooltip::Tooltip::new(SEARCH_SOON).build(window, cx)
        })
        .child(div().flex_1().child(SEARCH_PLACEHOLDER))
        .child(
            div()
                .flex_none()
                .px(px(5.))
                .border_1()
                .border_color(color("border-200"))
                .rounded(px(4.))
                .bg(color("surface-300"))
                .font_family(mono_font())
                .text_size(px(11.))
                .child("⌘K"),
        )
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
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
