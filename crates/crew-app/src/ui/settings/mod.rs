//! Settings, in place of the body: General, Appearance, Skills, Agents,
//! Runtimes, Handoffs and limits, Projects and Shortcuts.

pub mod agents;
pub mod appearance;
pub mod general;
pub mod limits;
pub mod projects;
pub mod runtimes;
pub mod skills;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::select::{Select, SelectEvent};
use gpui_kit::component::{IndexPath, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::title_bar::home_relative;
use super::workspace::{EditorSelect, Workspace};
use crate::app::bindings as app_bindings;
use crate::editor::{self, FINDER};
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};

pub use agents::AgentsPage;
pub use appearance::AppearancePage;
pub use general::GeneralPage;
pub use projects::ProjectsPage;
pub use skills::SkillsPage;

actions!(
    settings,
    [
        /// Leave settings.
        CloseSettings,
    ]
);

const SETTINGS_CONTEXT: &str = "Settings";
/// The settings nav column.
const NAV_WIDTH: f32 = 220.;
/// The label column of a form row.
pub(super) const LABEL_WIDTH: f32 = 200.;

/// Esc leaves settings; ↑ ↓ ↵ pick from the font list.
pub fn bindings() -> Vec<KeyBinding> {
    let mut bindings = vec![KeyBinding::new(
        "escape",
        CloseSettings,
        Some(SETTINGS_CONTEXT),
    )];
    bindings.extend(appearance::bindings());
    bindings
}

/// A settings page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    General,
    Appearance,
    Skills,
    Agents,
    Runtimes,
    Limits,
    Projects,
    Shortcuts,
}

const PAGES: [(Page, &str, &str); 8] = [
    (Page::General, "settings-nav-general", "General"),
    (Page::Appearance, "settings-nav-appearance", "Appearance"),
    (Page::Skills, "settings-nav-skills", "Skills"),
    (Page::Agents, "settings-nav-agents", "Agents"),
    (Page::Runtimes, "settings-nav-runtimes", "Runtimes"),
    (Page::Limits, "settings-nav-limits", "Handoffs and limits"),
    (Page::Projects, "settings-nav-projects", "Projects"),
    (Page::Shortcuts, "settings-nav-shortcuts", "Shortcuts"),
];

/// What settings shows.
pub struct SettingsView {
    pub page: Page,
    pub(super) focus: FocusHandle,
    pub agents: AgentsPage,
    pub projects: ProjectsPage,
    /// The appearance page's controls, made when it first opens.
    pub appearance: Option<AppearancePage>,
    /// The skills page's controls, made when it first opens.
    pub skills: Option<SkillsPage>,
    /// The general page's controls, made when it first opens.
    pub general: Option<GeneralPage>,
}

impl SettingsView {
    pub(super) fn new(page: Page, focus: FocusHandle) -> Self {
        Self {
            page,
            focus,
            agents: AgentsPage::default(),
            projects: ProjectsPage::default(),
            appearance: None,
            skills: None,
            general: None,
        }
    }
}

/// `cmd-shift-.` as `⌘⇧.`.
pub fn pretty_keys(keys: &str) -> String {
    keys.split(' ')
        .map(|stroke| {
            let parts: Vec<&str> = stroke.split('-').collect();
            // A key of `-` itself splits into empty parts.
            let (mods, key) = match parts.as_slice() {
                [mods @ .., "", ""] => (mods, "-"),
                [mods @ .., key] => (mods, *key),
                [] => (&[][..], ""),
            };
            let mut out: String = mods
                .iter()
                .map(|m| match *m {
                    "cmd" => "⌘",
                    "shift" => "⇧",
                    "alt" => "⌥",
                    "ctrl" => "⌃",
                    other => other,
                })
                .collect();
            out.push_str(match key {
                "enter" => "↵",
                "escape" => "esc",
                "up" => "↑",
                "down" => "↓",
                other => other,
            });
            if key.len() == 1 {
                out = out.to_uppercase();
            }
            out
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl Workspace {
    /// Opens settings on `page`, in place of the body.
    pub(super) fn open_settings(
        &mut self,
        page: Page,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drop_review(cx);
        self.running = None;
        let mut view = self
            .settings
            .take()
            .unwrap_or_else(|| SettingsView::new(page, cx.focus_handle()));
        view.page = page;
        view.focus.focus(window, cx);
        self.limits_error = None;
        self.settings = Some(view);
        self.page_opened(window, cx);
        self.sync(cx);
        cx.notify();
    }

    /// Goes back to what was open before.
    pub(super) fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.flush_agent_save(cx);
        self.flush_project_save(cx);
        if self.settings.take().is_none() {
            return;
        }
        if self.open_thread_id().is_some() || self.nav.drafting {
            self.composer
                .update(cx, |composer, cx| composer.focus(window, cx));
        } else {
            self.focus_workspace(window, cx);
        }
        self.sync(cx);
        cx.notify();
    }

    pub(super) fn show_page(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.settings.as_mut() {
            view.page = page;
            self.limits_error = None;
            self.page_opened(window, cx);
            cx.notify();
        }
    }

    /// Fills a page that has nothing picked with its first item.
    pub(super) fn page_opened(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(page) = self.settings.as_ref().map(|s| s.page) else {
            return;
        };
        match page {
            Page::Agents => self.agents_page_opened(window, cx),
            Page::Projects => self.projects_page_opened(window, cx),
            Page::Appearance => self.appearance_page_opened(window, cx),
            Page::Skills => self.skills_page_opened(window, cx),
            Page::General => self.general_page_opened(window, cx),
            _ => {}
        }
    }

    /// The title bar's content in settings.
    pub(super) fn render_settings_title(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<(AnyElement, AnyElement)> {
        self.settings.as_ref()?;
        let left = div()
            .flex()
            .items_center()
            .gap(ui_px(8.))
            .child(
                Button::new("settings-back")
                    .ghost()
                    .small()
                    .label("‹ Back")
                    .on_click(cx.listener(|this, _, window, cx| this.close_settings(window, cx))),
            )
            .child(
                div()
                    .id("settings-title")
                    .flex_none()
                    .font_weight(weight(FontWeight::SEMIBOLD))
                    .aria_label("Settings")
                    .child("Settings")
                    .test_support(),
            )
            .into_any_element();
        let right = div()
            .flex()
            .items_center()
            .gap(ui_px(8.))
            .children(self.render_title_actions(cx))
            .into_any_element();
        Some((left, right))
    }

    /// Settings in place of the body row.
    pub(super) fn render_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(view) = &self.settings else {
            return div().into_any_element();
        };
        let page = match view.page {
            Page::General => self.render_general(cx).into_any_element(),
            Page::Appearance => self.render_appearance_page(cx),
            Page::Skills => self.render_skills_page(cx),
            Page::Agents => self.render_agents_page(cx),
            Page::Projects => self.render_projects_page(cx),
            Page::Runtimes => self.render_runtimes_page(cx),
            Page::Limits => self.render_limits_page(cx),
            Page::Shortcuts => render_shortcuts().into_any_element(),
        };
        div()
            .id("settings")
            .key_context(SETTINGS_CONTEXT)
            .track_focus(&view.focus)
            .on_action(cx.listener(|this, _: &CloseSettings, window, cx| {
                this.close_settings(window, cx);
            }))
            .flex()
            .flex_1()
            .min_h_0()
            .child(self.render_settings_nav(view.page, cx))
            .child(div().flex().flex_1().min_w_0().h_full().child(page))
            .test_support()
            .into_any_element()
    }

    fn render_settings_nav(&self, page: Page, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("settings-nav")
            .flex()
            .flex_col()
            .flex_none()
            .w(scale::chrome(NAV_WIDTH))
            .h_full()
            .px(ui_px(8.))
            .py(scale::row(12.))
            .gap(ui_px(2.))
            .border_r_1()
            .border_color(color("border-100"))
            .children(PAGES.iter().map(|(p, id, name)| {
                let p = *p;
                nav_row(id, page == p)
                    .aria_label(*name)
                    .child(*name)
                    .on_click(cx.listener(move |this, _, window, cx| this.show_page(p, window, cx)))
                    .test_support()
            }))
    }

    fn render_general(&self, cx: &mut Context<Self>) -> impl IntoElement {
        page_scroll("settings-general")
            .gap_0()
            .pb(ui_px(32.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .max_w(ui_px(general::PAGE_WIDTH))
                    .child(
                        div()
                            .mb(ui_px(16.))
                            .text_size(scale::text(24.))
                            .line_height(scale::line(32.))
                            .font_weight(weight(FontWeight::SEMIBOLD))
                            .child("General"),
                    )
                    .child(self.render_title_fields(cx))
                    .child(general::board_row(field(
                        "field-general-home",
                        "Crew home",
                        Some("Where crewd keeps its database, socket and worktrees."),
                        value_text("general-home", home_relative(&self.conn.crew_home), true),
                    )))
                    .child(general::board_row(field(
                        "field-general-editor",
                        "Editor",
                        Some("Open in editor uses this app."),
                        div().w(ui_px(280.)).child(control(
                            "general-editor",
                            Select::new(&self.editor_select)
                                .small()
                                .accessibility_label("Editor"),
                        )),
                    ))),
            )
    }
}

/// The Editor select's labels (Finder first) and the one shown for
/// `chosen`.
pub(super) fn editor_items(
    chosen: Option<&str>,
    detected: &[String],
) -> (Vec<SharedString>, Option<IndexPath>) {
    let labels: Vec<SharedString> = std::iter::once(FINDER.to_string())
        .chain(detected.iter().cloned())
        .map(SharedString::from)
        .collect();
    let shown = editor::resolve(chosen, detected).unwrap_or_else(|| FINDER.to_string());
    let ix = labels.iter().position(|l| *l == shown).map(IndexPath::new);
    (labels, ix)
}

impl Workspace {
    /// The app Open in editor launches, or `None` for Finder.
    pub(super) fn editor_app(&self) -> Option<String> {
        editor::resolve(self.editor_choice.as_deref(), &self.editors)
    }

    pub(super) fn editor_picked(
        &mut self,
        _: &Entity<EditorSelect>,
        event: &SelectEvent<Vec<SharedString>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let SelectEvent::Confirm(Some(label)) = event {
            self.choose_editor(label.to_string(), cx);
        }
    }

    /// Remembers `choice` (an app name or Finder) for next time too.
    pub(super) fn choose_editor(&mut self, choice: String, cx: &mut Context<Self>) {
        if let Err(e) = editor::save_choice(&self.conn.crew_home, &choice) {
            self.last_error = Some(format!("Your editor choice wasn't saved: {e}.").into());
        }
        self.editor_choice = Some(choice);
        cx.notify();
    }

    /// Replaces what was detected and the launcher, as tests do.
    #[cfg(test)]
    pub(super) fn set_editor_env(
        &mut self,
        detected: Vec<String>,
        opener: editor::Opener,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editors = detected;
        self.opener = opener;
        let (labels, ix) = editor_items(self.editor_choice.as_deref(), &self.editors);
        self.editor_select.update(cx, |select, cx| {
            select.set_items(labels, window, cx);
            if let Some(ix) = ix {
                select.set_selected_index(Some(ix), window, cx);
            }
        });
    }
}

fn render_shortcuts() -> impl IntoElement {
    let rows = app_bindings().into_iter().map(|s| {
        let keys = s
            .binding
            .keystrokes()
            .iter()
            .map(|k| k.unparse(OperatingSystem::Mac))
            .collect::<Vec<_>>()
            .join(" ");
        let pretty = pretty_keys(&keys);
        div()
            .id(SharedString::from(format!("shortcut-{}", s.label)))
            .flex()
            .items_center()
            .h(scale::row(36.))
            .border_b_1()
            .border_color(color("border-100"))
            .aria_label(format!("{} {pretty}", s.label))
            .child(div().flex_1().child(s.label))
            .child(
                div()
                    .px(ui_px(6.))
                    .border_1()
                    .border_color(color("border-200"))
                    .rounded(ui_px(4.))
                    .bg(color("surface-300"))
                    .font_family(code_family())
                    .text_size(scale::mono(11.))
                    .child(pretty),
            )
            .test_support()
    });
    page_scroll("settings-shortcuts")
        .child(page_title("Shortcuts"))
        .child(div().flex().flex_col().max_w(ui_px(560.)).children(rows))
}

/// A nav row of settings.
pub(super) fn nav_row(id: &'static str, selected: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(ui_px(8.))
        .h(scale::row(28.))
        .px(ui_px(8.))
        .rounded(ui_px(6.))
        .cursor_pointer()
        .when(selected, |row| row.bg(color("surface-300")))
        .text_color(color(if selected {
            "text-primary"
        } else {
            "text-secondary"
        }))
        .hover(|row| row.bg(color("surface-200")))
}

/// A page's scrolling column.
pub(super) fn page_scroll(id: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .h_full()
        .overflow_y_scroll()
        .px(ui_px(32.))
        .py(ui_px(24.))
        .gap(ui_px(16.))
}

pub(super) fn page_title(text: &'static str) -> impl IntoElement {
    div()
        .text_size(scale::text(20.))
        .line_height(scale::line(28.))
        .font_weight(weight(FontWeight::SEMIBOLD))
        .child(text)
}

/// A form row: the label and its help on the left, the control on the
/// right. Its `id` starts with `field-`, so the palette can find it.
pub(crate) fn field(
    id: &'static str,
    label: &'static str,
    help: Option<&'static str>,
    content: impl IntoElement,
) -> impl IntoElement + Styled {
    div()
        .id(id)
        .flex()
        .gap(ui_px(24.))
        .py(scale::row(12.))
        .border_b_1()
        .border_color(color("border-100"))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_none()
                .gap(ui_px(2.))
                .w(ui_px(LABEL_WIDTH))
                .child(div().font_weight(weight(FontWeight::MEDIUM)).child(label))
                .when_some(help, |col, help| {
                    col.child(
                        div()
                            .text_size(scale::text(12.))
                            .text_color(color("text-tertiary"))
                            .child(help),
                    )
                }),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap(ui_px(6.))
                .child(content),
        )
        .test_support()
}

/// Read-only text, mono for paths.
pub(super) fn value_text(id: &'static str, text: String, mono: bool) -> impl IntoElement {
    div()
        .id(id)
        .text_color(color("text-secondary"))
        .when(mono, |el| {
            el.font_family(code_family()).text_size(scale::mono(12.))
        })
        .aria_label(text.clone())
        .child(text)
        .test_support()
}

/// A field's control wrapped so tests can find and click it.
pub(super) fn control(id: impl Into<SharedString>, child: impl IntoElement) -> impl IntoElement {
    div()
        .id(ElementId::Name(id.into()))
        .w_full()
        .child(child)
        .test_support()
}

/// One option of a segmented control.
pub(super) fn segment(id: impl Into<SharedString>, label: &str, selected: bool) -> Stateful<Div> {
    div()
        .id(ElementId::Name(id.into()))
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .h(scale::row(26.))
        .px(ui_px(10.))
        .rounded(ui_px(4.))
        .when(selected, |s| s.bg(color("surface-300")))
        .text_color(color(if selected {
            "text-primary"
        } else {
            "text-secondary"
        }))
        .font_weight(weight(if selected {
            FontWeight::MEDIUM
        } else {
            FontWeight::NORMAL
        }))
        .cursor_pointer()
        .role(Role::Button)
        .aria_label(if selected {
            format!("{label} (selected)")
        } else {
            label.to_string()
        })
        .child(label.to_string())
}

/// The frame of a segmented control.
pub(super) fn segmented(children: Vec<AnyElement>) -> Div {
    div()
        .flex()
        .gap(ui_px(2.))
        .p(ui_px(2.))
        .border_1()
        .border_color(color("border-200"))
        .rounded(ui_px(6.))
        .children(children)
}

/// A red sentence under a form: what is wrong and how to fix it.
pub(super) fn form_error(id: &'static str, text: String) -> impl IntoElement {
    div()
        .id(id)
        .text_size(scale::text(12.))
        .text_color(color("danger"))
        .aria_label(text.clone())
        .child(text)
        .test_support()
}

/// A small help line.
pub(super) fn help(text: impl Into<SharedString>) -> impl IntoElement {
    div()
        .text_size(scale::text(12.))
        .text_color(color("text-tertiary"))
        .child(text.into())
}

#[cfg(test)]
mod tests {
    use super::pretty_keys;

    #[test]
    fn shortcuts_read_as_mac_symbols() {
        assert_eq!(pretty_keys("cmd-shift-."), "⌘⇧.");
        assert_eq!(pretty_keys("cmd-k"), "⌘K");
        assert_eq!(pretty_keys("cmd-enter"), "⌘↵");
        assert_eq!(pretty_keys("cmd-,"), "⌘,");
        assert_eq!(pretty_keys("escape"), "esc");
    }
}
