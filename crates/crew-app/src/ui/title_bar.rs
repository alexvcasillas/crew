//! The title bar: the app name, the search button and (later) run counts.

use gpui_kit::component::TitleBar;
use gpui_kit::*;

use std::path::{Path, PathBuf};

use super::common::chip;
use super::workspace::Workspace;
use crate::fonts::mono_font;
use crate::theme::color;

pub const TITLE_BAR_HEIGHT: f32 = 48.;
/// The Kit title bar's left padding for the traffic lights on macOS.
const TRAFFIC_LIGHTS_INSET: f32 = 80.;
/// The breadcrumb separators, dimmer than any text token.
const SLASH: u32 = 0x3a3a3a;
const SEARCH_WIDTH: f32 = 320.;
const SEARCH_PLACEHOLDER: &str = "Search threads, agents, files…";

impl Workspace {
    pub(super) fn render_title_bar(&self) -> impl IntoElement + use<> {
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
                    .children(self.render_breadcrumb()),
            )
            .child(render_search_button())
            .child(
                div()
                    .flex_1()
                    .flex_basis(px(0.))
                    .flex()
                    .justify_end()
                    // Mirrors the traffic-light inset on the left.
                    .pl(px(TRAFFIC_LIGHTS_INSET))
                    .pr(px(12.)),
            )
    }
}

impl Workspace {
    /// `/ project / #channel` and the project's folder, for the selected
    /// channel.
    fn render_breadcrumb(&self) -> Vec<AnyElement> {
        let state = self.state();
        let Some(channel) = self.nav.channel.and_then(|c| state.channel(c)) else {
            return Vec::new();
        };
        let Some(project) = state.project(channel.project) else {
            return Vec::new();
        };
        let path = home_relative(&project.root);
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
            chip(path.clone())
                .id("title-path")
                .min_w_0()
                .truncate()
                .aria_label(path)
                .test_support()
                .into_any_element(),
        ]
    }
}

/// `path` with the home folder written as `~`.
fn home_relative(path: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) if !home.as_os_str().is_empty() => match path.strip_prefix(&home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        },
        _ => path.display().to_string(),
    }
}

/// "Search threads, agents, files… ⌘K". Search itself comes later.
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
        .text_color(color("text-secondary"))
        .cursor_pointer()
        .hover(|s| s.bg(color("surface-100")))
        .role(Role::Button)
        .aria_label(SEARCH_PLACEHOLDER)
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
