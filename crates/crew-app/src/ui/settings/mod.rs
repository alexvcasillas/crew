//! Settings, in place of the body: General, Appearance, Skills, Agents,
//! Runtimes, Handoffs and limits, Projects and Shortcuts.

pub mod agents;
pub mod appearance;
pub mod general;
pub mod limits;
pub mod projects;
pub mod runtimes;
pub mod skills;

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::select::{Select, SelectEvent};
use gpui_kit::component::{IndexPath, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::palette::catalog::{Item, Scope, settings_entries};
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

/// How long a field jumped to from ⌘K stays tinted, fading at the end.
pub const JUMP_HIGHLIGHT: Duration = Duration::from_millis(1500);
/// The last part of [`JUMP_HIGHLIGHT`], when the tint fades out.
const JUMP_FADE: Duration = Duration::from_millis(500);
/// How often the fading tint is redrawn.
const FADE_FRAME: Duration = Duration::from_millis(16);
/// Space left above a field scrolled to.
const JUMP_MARGIN: f32 = 24.;

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

/// What the settings sidebar calls `page`.
pub fn page_name(page: Page) -> &'static str {
    PAGES
        .iter()
        .find(|(p, _, _)| *p == page)
        .map_or("", |(_, _, title)| title)
}

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
    /// The field a jump from ⌘K landed on, and when.
    pub highlight: Option<(&'static str, Instant)>,
    /// The open page's scrolling column.
    scroll: ScrollHandle,
    /// Set by a jump until its field is scrolled into view.
    scroll_to_highlight: Rc<Cell<bool>>,
    /// Fades the highlight out, then drops it.
    fade: Option<Task<()>>,
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
            highlight: None,
            scroll: ScrollHandle::new(),
            scroll_to_highlight: Rc::default(),
            fade: None,
        }
    }
}

impl SettingsView {
    /// Stops a jump's highlight at once.
    fn drop_highlight(&mut self) {
        self.highlight = None;
        self.fade = None;
        self.scroll_to_highlight.set(false);
    }
}

/// What a page needs to draw a jump from ⌘K: the field to tint and the
/// column to scroll.
#[derive(Clone, Default)]
pub(crate) struct Jump {
    /// The field and its tint's opacity, from 1 down to 0.
    target: Option<(&'static str, f32)>,
    scroll: ScrollHandle,
    scroll_pending: Rc<Cell<bool>>,
}

impl Jump {
    /// The scroller a page's column tracks.
    pub(super) fn scroll(&self) -> &ScrollHandle {
        &self.scroll
    }

    /// The tint behind the row `id` while a jump has it highlighted. Laid
    /// out absolutely, as the row's first child, so it paints under the
    /// row; it also scrolls the row into view once.
    pub(super) fn mark(&self, id: &str) -> Option<AnyElement> {
        let (_, alpha) = self.target.filter(|(field, _)| *field == id)?;
        let scroll = self.scroll.clone();
        let pending = self.scroll_pending.clone();
        let tint = canvas(
            move |bounds, window, _| {
                if pending.take() {
                    scroll_into_view(&scroll, bounds);
                    window.refresh();
                }
            },
            move |bounds, (), window, _| {
                window.paint_quad(quad(
                    bounds,
                    ui_px(8.),
                    color("info-subtle").opacity(alpha),
                    ui_px(1.),
                    color("info-border").opacity(alpha),
                    BorderStyle::Solid,
                ));
            },
        )
        .absolute()
        .top_0()
        .bottom_0()
        .left(ui_px(-12.))
        .right(ui_px(-12.));
        Some(tint.into_any_element())
    }
}

/// Scrolls `scroll` so a row drawn at `row` shows, with a margin above it.
fn scroll_into_view(scroll: &ScrollHandle, row: Bounds<Pixels>) {
    let view = scroll.bounds();
    if row.top() >= view.top() && row.bottom() <= view.bottom() {
        return;
    }
    let offset = scroll.offset();
    let max = scroll.max_offset();
    let y = (row.top() - view.top() - offset.y - ui_px(JUMP_MARGIN)).min(max.y);
    scroll.set_offset(point(offset.x, -y.max(px(0.))));
}

/// The tint's opacity `elapsed` into a jump: full, then fading to none.
fn jump_alpha(elapsed: Duration) -> f32 {
    let hold = JUMP_HIGHLIGHT - JUMP_FADE;
    let fading = elapsed.saturating_sub(hold).as_secs_f32() / JUMP_FADE.as_secs_f32();
    (1. - fading).clamp(0., 1.)
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
                "-" => "−",
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
        if view.page != page {
            view.scroll.set_offset(Point::default());
        }
        view.page = page;
        view.focus.focus(window, cx);
        self.limits_error = None;
        self.settings = Some(view);
        self.page_opened(window, cx);
        self.sync(cx);
        cx.notify();
    }

    /// Opens the settings page that holds `field` on `item`, the agent or
    /// project it belongs to, then highlights the field, scrolls to it and
    /// focuses its text control if it has one. An item that is gone opens
    /// just the page.
    pub(super) fn open_setting(
        &mut self,
        field: &'static str,
        item: Option<Item>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = settings_entries().iter().find(|e| e.field == field) else {
            return;
        };
        self.open_settings(entry.page, window, cx);
        let shown = match (entry.scope, item) {
            (Scope::Global, _) => true,
            (Scope::Agent, Some(Item::Agent(agent))) if self.state().agent(agent).is_some() => {
                // Switching agents saves what you typed first, as a click
                // in the list does.
                self.edit_agent(agent, window, cx);
                true
            }
            (Scope::Project, Some(Item::Project(project)))
                if self.state().project(project).is_some() =>
            {
                self.select_project(project, window, cx);
                true
            }
            _ => false,
        };
        let Some(view) = self.settings.as_mut() else {
            return;
        };
        view.drop_highlight();
        if !shown {
            cx.notify();
            return;
        }
        let start = cx.background_executor().now();
        view.highlight = Some((field, start));
        view.scroll_to_highlight.set(true);
        view.fade = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(JUMP_HIGHLIGHT - JUMP_FADE)
                .await;
            let end = start + JUMP_HIGHLIGHT;
            loop {
                let left = this
                    .update(cx, |this, cx| {
                        cx.notify();
                        let left = end.saturating_duration_since(cx.background_executor().now());
                        if left.is_zero()
                            && let Some(view) = this.settings.as_mut()
                        {
                            view.highlight = None;
                            view.scroll_to_highlight.set(false);
                        }
                        left
                    })
                    .unwrap_or_default();
                if left.is_zero() {
                    break;
                }
                cx.background_executor().timer(left.min(FADE_FRAME)).await;
            }
        }));
        match entry.page {
            Page::Agents => self.focus_agent_field(field, window, cx),
            Page::Projects => self.focus_project_field(field, window, cx),
            _ => {}
        }
        cx.notify();
    }

    /// Jumps to the field the debug startup hook names, once its agent or
    /// project is known.
    pub(super) fn jump_if_known(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((field, name)) = &self.setting_pending else {
            return;
        };
        let Some(entry) = settings_entries().iter().find(|e| e.field == field) else {
            eprintln!("crew: CREW_APP_SHOW=setting:{field} names no settings field, ignored");
            self.setting_pending = None;
            return;
        };
        let state = self.state();
        let item = match (entry.scope, name) {
            (Scope::Global, _) => None,
            (Scope::Agent, Some(name)) => {
                match state.agents().iter().find(|a| a.spec.name == *name) {
                    Some(a) => Some(Item::Agent(a.id)),
                    None => return,
                }
            }
            (Scope::Project, Some(name)) => match state.projects().iter().find(|p| p.name == *name)
            {
                Some(p) => Some(Item::Project(p.id)),
                None => return,
            },
            (_, None) => None,
        };
        self.setting_pending = None;
        self.open_setting(entry.field, item, window, cx);
    }

    /// What the open settings page needs to draw a jump.
    pub(super) fn jump(&self, cx: &App) -> Jump {
        let Some(view) = &self.settings else {
            return Jump::default();
        };
        let now = cx.background_executor().now();
        Jump {
            target: view
                .highlight
                .map(|(field, at)| (field, jump_alpha(now.saturating_duration_since(at)))),
            scroll: view.scroll.clone(),
            scroll_pending: view.scroll_to_highlight.clone(),
        }
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
            if view.page != page {
                view.scroll.set_offset(Point::default());
            }
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
            Page::Shortcuts => render_shortcuts(&self.jump(cx)).into_any_element(),
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
        let jump = self.jump(cx);
        page_scroll("settings-general", &jump)
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
                        &jump,
                        "field-general-home",
                        "Crew home",
                        Some("Where crewd keeps its database, socket and worktrees."),
                        value_text("general-home", home_relative(&self.conn.crew_home), true),
                    )))
                    .child(general::board_row(field(
                        &jump,
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

fn render_shortcuts(jump: &Jump) -> impl IntoElement {
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
    page_scroll("settings-shortcuts", jump)
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

/// A page's scrolling column, scrolled to a field jumped to.
pub(super) fn page_scroll(id: &'static str, jump: &Jump) -> Stateful<Div> {
    page_column(id)
        .overflow_y_scroll()
        .track_scroll(&jump.scroll)
}

/// A page's column, for a page that scrolls it inside something else.
pub(super) fn page_column(id: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .h_full()
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
/// right. Its `id` starts with `field-`, so the palette can find it, and
/// it is tinted while `jump` has it highlighted.
pub(crate) fn field(
    jump: &Jump,
    id: &'static str,
    label: &'static str,
    help: Option<&'static str>,
    content: impl IntoElement,
) -> impl IntoElement + Styled {
    div()
        .id(id)
        .relative()
        .children(jump.mark(id))
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
    use std::time::Duration;

    use super::{JUMP_HIGHLIGHT, jump_alpha, pretty_keys};

    #[test]
    fn a_jump_holds_its_tint_then_fades_it() {
        let ms = Duration::from_millis;
        assert_eq!(jump_alpha(ms(0)), 1.);
        assert_eq!(jump_alpha(ms(1000)), 1.);
        assert!((jump_alpha(ms(1250)) - 0.5).abs() < 0.01);
        assert_eq!(jump_alpha(JUMP_HIGHLIGHT), 0.);
        assert_eq!(jump_alpha(ms(4000)), 0.);
    }

    #[test]
    fn shortcuts_read_as_mac_symbols() {
        assert_eq!(pretty_keys("cmd-shift-."), "⌘⇧.");
        assert_eq!(pretty_keys("cmd-k"), "⌘K");
        assert_eq!(pretty_keys("cmd-enter"), "⌘↵");
        assert_eq!(pretty_keys("cmd-,"), "⌘,");
        assert_eq!(pretty_keys("escape"), "esc");
        assert_eq!(pretty_keys("cmd--"), "⌘−", "a minus sign");
    }
}
