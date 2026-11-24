//! Settings › Appearance: text sizes, fonts, line height, bolder text,
//! density, the sidebar's mode and zoom. Each change applies at once to
//! the whole app, and the preview beside the form shows it.

use crew_core::agent::Runtime;
use gpui_kit::component::IndexPath;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::radio::Radio;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{control, field, page_scroll, segment, segmented};
use crate::markdown;
use crate::prefs::{
    Appearance, CODE_SIZES, DEFAULT_CODE_FONT, DEFAULT_UI_FONT, Density, LineHeight, SidebarMode,
    UI_SIZES,
};
use crate::theme::color;
use crate::theme::scale::{self, SF_MONO, SF_PRO, code_family, ui_px, weight};
use crate::ui::common::{dot, monogram, runtime_badge_for};
use crate::ui::markdown_view::{code_block, code_line, render_doc};
use crate::ui::sidebar::{channel_row, project_row};
use crate::ui::thread_view::message_row;
use crate::ui::workspace::Workspace;

/// The font select item that opens the list of every installed font.
pub const OTHER_FONT: &str = "Other installed font…";
const UI_PRESETS: [&str; 2] = [DEFAULT_UI_FONT, SF_PRO];
const CODE_PRESETS: [&str; 3] = [DEFAULT_CODE_FONT, SF_MONO, "Menlo"];

const BOLDER_HELP: &str = "Heavier strokes, easier to read on external displays.";
const COMING_SOON: &str = "Coming soon.";
/// The preview column.
const PREVIEW_WIDTH: f32 = 440.;
/// How wide the form gets.
const FORM_WIDTH: f32 = 720.;
/// The form column's width before the preview wraps under it.
const FORM_BASIS: f32 = 560.;
const SELECT_WIDTH: f32 = 200.;
const FONT_LIST_HEIGHT: f32 = 320.;
const FONT_PICKER_WIDTH: f32 = 420.;

/// A font select: its labels.
pub type FontSelect = SelectState<Vec<SharedString>>;

/// Which font a select or the font list sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontSlot {
    Interface,
    Code,
}

impl FontSlot {
    fn presets(self) -> &'static [&'static str] {
        match self {
            Self::Interface => &UI_PRESETS,
            Self::Code => &CODE_PRESETS,
        }
    }

    fn chosen(self, a: &Appearance) -> &str {
        match self {
            Self::Interface => &a.ui_font,
            Self::Code => &a.code_font,
        }
    }

    fn set(self, a: &mut Appearance, name: String) {
        match self {
            Self::Interface => a.ui_font = name,
            Self::Code => a.code_font = name,
        }
    }
}

/// The page's own controls, made when it first opens.
pub struct AppearancePage {
    pub ui_font: Entity<FontSelect>,
    pub code_font: Entity<FontSelect>,
    _events: [Subscription; 2],
}

/// A font select's labels: the presets, a font of your own (saying so
/// when it is missing), then Other. Also which one is shown.
pub fn font_items(
    slot: FontSlot,
    chosen: &str,
    installed: bool,
) -> (Vec<SharedString>, Option<IndexPath>) {
    let mut labels: Vec<SharedString> = slot
        .presets()
        .iter()
        .map(|p| SharedString::from(*p))
        .collect();
    let shown = if slot.presets().contains(&chosen) {
        chosen.to_string()
    } else {
        let own = if installed {
            chosen.to_string()
        } else {
            format!("{chosen} (not installed)")
        };
        labels.push(own.clone().into());
        own
    };
    labels.push(OTHER_FONT.into());
    let ix = labels.iter().position(|l| *l == shown).map(IndexPath::new);
    (labels, ix)
}

/// Applies `change` to the settings in use, if it changes anything.
fn change(cx: &mut App, change: impl FnOnce(&mut Appearance)) {
    let before = scale::appearance(cx);
    let mut after = before.clone();
    change(&mut after);
    if after != before {
        scale::set_appearance(after, cx);
    }
}

/// "{ui} · {code} mono · {standard|bolder}".
pub fn preview_caption(a: &Appearance) -> String {
    format!(
        "{} · {} mono · {}",
        a.ui_size,
        a.code_size,
        if a.bolder { "bolder" } else { "standard" }
    )
}

impl Workspace {
    /// Makes the font selects the first time the page opens.
    pub(super) fn appearance_page_opened(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(settings) = self.settings.as_ref() else {
            return;
        };
        if settings.appearance.is_some() {
            return;
        }
        let a = scale::appearance(cx);
        let make = |slot: FontSlot, window: &mut Window, cx: &mut Context<Self>| {
            let chosen = slot.chosen(&a);
            let (labels, ix) = font_items(slot, chosen, scale::font_installed(chosen, cx));
            let select = cx.new(|cx| SelectState::new(labels, ix, window, cx));
            let events = cx.subscribe_in(
                &select,
                window,
                move |this, _, event: &SelectEvent<Vec<SharedString>>, window, cx| {
                    if let SelectEvent::Confirm(Some(label)) = event {
                        this.pick_font(slot, label, window, cx);
                    }
                },
            );
            (select, events)
        };
        let (ui_font, ui_events) = make(FontSlot::Interface, window, cx);
        let (code_font, code_events) = make(FontSlot::Code, window, cx);
        if let Some(settings) = self.settings.as_mut() {
            settings.appearance = Some(AppearancePage {
                ui_font,
                code_font,
                _events: [ui_events, code_events],
            });
        }
    }

    /// The appearance page's controls, once it has opened.
    pub(crate) fn appearance_page(&self) -> Option<&AppearancePage> {
        self.settings.as_ref()?.appearance.as_ref()
    }

    /// Shows the fonts in use in the selects, after any change.
    pub(crate) fn sync_font_selects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(page) = self.appearance_page() else {
            return;
        };
        let selects = [
            (FontSlot::Interface, page.ui_font.clone()),
            (FontSlot::Code, page.code_font.clone()),
        ];
        let a = scale::appearance(cx);
        for (slot, select) in selects {
            let chosen = slot.chosen(&a);
            let (labels, ix) = font_items(slot, chosen, scale::font_installed(chosen, cx));
            select.update(cx, |select, cx| {
                select.set_items(labels, window, cx);
                select.set_selected_index(ix, window, cx);
            });
        }
    }

    /// A font select's item was picked: a preset applies, Other opens the
    /// list of installed fonts.
    pub(crate) fn pick_font(
        &mut self,
        slot: FontSlot,
        label: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if label == OTHER_FONT {
            // The select keeps showing the font in use until one is picked.
            cx.defer_in(window, |this, window, cx| {
                this.sync_font_selects(window, cx)
            });
            self.open_font_picker(slot, window, cx);
        } else if slot.presets().contains(&label) {
            let name = label.to_string();
            change(cx, |a| slot.set(a, name));
        }
    }

    fn open_font_picker(&mut self, slot: FontSlot, window: &mut Window, cx: &mut Context<Self>) {
        let picker = cx.new(|cx| FontPicker::new(slot, window, cx));
        let first_build = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first_build.replace(false) {
                let search = picker.read(cx).search.clone();
                search.update(cx, |search, cx| search.focus(window, cx));
            }
            dialog
                .w(ui_px(FONT_PICKER_WIDTH))
                .title(match slot {
                    FontSlot::Interface => "Interface font",
                    FontSlot::Code => "Code font",
                })
                .child(picker.clone())
        });
    }

    pub(super) fn render_appearance_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let a = scale::appearance(cx);
        let form = div()
            .flex()
            .flex_col()
            .max_w(ui_px(FORM_WIDTH))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(ui_px(12.))
                    .mb(ui_px(16.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(scale::text(24.))
                            .line_height(scale::line(32.))
                            .font_weight(weight(FontWeight::SEMIBOLD))
                            .child("Appearance"),
                    )
                    .child(
                        Button::new("appearance-reset")
                            .ghost()
                            .label("Reset to defaults")
                            .on_click(|_, _, cx| {
                                change(cx, |a| {
                                    *a = Appearance {
                                        extra: std::mem::take(&mut a.extra),
                                        ..Appearance::default()
                                    }
                                })
                            }),
                    ),
            )
            .child(row(
                "ui-size",
                "Interface text size",
                Some("Messages, sidebar, buttons."),
                sizes("appearance-ui-size", &UI_SIZES, a.ui_size, 13, |a, n| {
                    a.ui_size = n
                }),
            ))
            .child(row(
                "code-size",
                "Code text size",
                Some("Paths, commands, diffs, terminal."),
                sizes(
                    "appearance-code-size",
                    &CODE_SIZES,
                    a.code_size,
                    12,
                    |a, n| a.code_size = n,
                ),
            ))
            .child(row(
                "fonts",
                "Fonts",
                Some("Any font installed on this Mac."),
                self.render_font_selects(),
            ))
            .child(row(
                "line-height",
                "Line height",
                None,
                choices(
                    "appearance-line",
                    &[
                        (LineHeight::Compact, "compact", "Compact"),
                        (LineHeight::Comfortable, "comfortable", "Comfortable"),
                        (LineHeight::Roomy, "roomy", "Roomy"),
                    ],
                    a.line_height,
                    |a, l| a.line_height = l,
                ),
            ))
            .child(row(
                "rendering",
                "Text rendering",
                Some("Crew already uses grayscale smoothing."),
                div()
                    .flex()
                    .flex_col()
                    .gap(ui_px(6.))
                    .child(rendering_card(false, "Standard", None, !a.bolder))
                    .child(rendering_card(
                        true,
                        "Bolder text",
                        Some(BOLDER_HELP),
                        a.bolder,
                    )),
            ))
            .child(row(
                "density",
                "Density",
                Some("Row heights and padding."),
                choices(
                    "appearance-density",
                    &[
                        (Density::Compact, "compact", "Compact"),
                        (Density::Comfortable, "comfortable", "Comfortable"),
                    ],
                    a.density,
                    |a, d| a.density = d,
                ),
            ))
            .child(row(
                "sidebar",
                "Sidebar",
                Some("Also switches from the button in the sidebar header."),
                sidebar_modes(a.sidebar_mode),
            ))
            .child(row(
                "zoom",
                "Zoom",
                Some("Scales the whole window."),
                zoom(a.zoom),
            ));
        // As on the board, the preview drops under the form once the form
        // would be narrower than its basis. Both are at least as tall as
        // the page and grow with what they hold; the page scrolls them.
        div()
            .id("appearance-layout")
            .flex()
            .flex_wrap()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .child(
                page_scroll("settings-appearance")
                    .flex_grow(999.)
                    .flex_shrink(1.)
                    .flex_basis(ui_px(FORM_BASIS))
                    .h_auto()
                    .min_h_full()
                    .pb(ui_px(32.))
                    .child(form)
                    .test_support(),
            )
            .child(preview(&a))
            .test_support()
            .into_any_element()
    }

    fn render_font_selects(&self) -> impl IntoElement {
        let Some(page) = self.appearance_page() else {
            return div();
        };
        div()
            .flex()
            .flex_wrap()
            .gap(ui_px(16.))
            .child(font_select(
                "appearance-ui-font",
                "Interface",
                &page.ui_font,
            ))
            .child(font_select("appearance-code-font", "Code", &page.code_font))
    }
}

/// A form row of the page, labelled so tests can find it. The board's
/// rows have a rule above each one.
fn row(
    key: &'static str,
    label: &'static str,
    help: Option<&'static str>,
    content: impl IntoElement,
) -> impl IntoElement {
    div()
        .id(SharedString::from(format!("appearance-row-{key}")))
        .aria_label(label)
        .child(
            field(label, help, content)
                .py(scale::row(16.))
                .border_b_0()
                .border_t_1(),
        )
        .test_support()
}

/// One option of the page's segmented controls: 12 px, at least 36 wide.
fn option(id: String, label: &str, selected: bool) -> Stateful<Div> {
    segment(id, label, selected)
        .flex_none()
        .min_w(ui_px(36.))
        .text_size(scale::text(12.))
        .font_weight(weight(FontWeight::MEDIUM))
}

/// A segmented control of sizes, with the default beside it.
fn sizes(
    id: &'static str,
    offered: &[u8],
    current: u8,
    default: u8,
    set: fn(&mut Appearance, u8),
) -> impl IntoElement {
    let options = offered
        .iter()
        .map(|&n| {
            option(format!("{id}-{n}"), &n.to_string(), n == current)
                .on_click(move |_, _, cx| change(cx, |a| set(a, n)))
                .test_support()
                .into_any_element()
        })
        .collect();
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(ui_px(10.))
        .child(segmented(options))
        .child(
            div()
                .text_size(scale::text(12.))
                .text_color(color("text-tertiary"))
                .child(format!("Default {default}")),
        )
}

/// A segmented control of named choices.
fn choices<T: Copy + PartialEq + 'static>(
    id: &'static str,
    offered: &[(T, &'static str, &'static str)],
    current: T,
    set: fn(&mut Appearance, T),
) -> impl IntoElement {
    let options = offered
        .iter()
        .map(|&(value, key, label)| {
            option(format!("{id}-{key}"), label, value == current)
                .on_click(move |_, _, cx| change(cx, |a| set(a, value)))
                .test_support()
                .into_any_element()
        })
        .collect();
    div().flex().child(segmented(options))
}

/// Projects, and Threads, which is coming.
fn sidebar_modes(current: SidebarMode) -> impl IntoElement {
    let projects = option(
        "appearance-sidebar-projects".into(),
        "Projects",
        current == SidebarMode::Projects,
    )
    .on_click(|_, _, cx| change(cx, |a| a.sidebar_mode = SidebarMode::Projects))
    .test_support()
    .into_any_element();
    let threads = option("appearance-sidebar-threads".into(), "Threads", false)
        .text_color(color("text-disabled"))
        .cursor_default()
        .aria_label("Threads, coming soon")
        .tooltip(|window, cx| Tooltip::new(COMING_SOON).build(window, cx))
        .test_support()
        .into_any_element();
    div().flex().child(segmented(vec![projects, threads]))
}

/// Standard or Bolder text, as a card with a radio.
fn rendering_card(
    bolder: bool,
    title: &'static str,
    help: Option<&'static str>,
    selected: bool,
) -> impl IntoElement {
    let key = if bolder { "bolder" } else { "standard" };
    let pick = move |_: &_, _: &mut Window, cx: &mut App| change(cx, |a| a.bolder = bolder);
    div()
        .id(SharedString::from(format!("appearance-render-{key}")))
        .flex()
        .items_start()
        .gap(ui_px(10.))
        .px(ui_px(12.))
        .py(scale::row(10.))
        .border_1()
        .border_color(color(if selected { "border-200" } else { "border-100" }))
        .rounded(ui_px(6.))
        .when(selected, |card| card.bg(color("surface-100")))
        .cursor_pointer()
        .aria_label(if selected {
            format!("{title} (selected)")
        } else {
            title.to_string()
        })
        .child(
            Radio::new(SharedString::from(format!("appearance-radio-{key}")))
                .checked(selected)
                .on_click(move |_, _, cx| change(cx, |a| a.bolder = bolder)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .font_weight(weight(if selected {
                            FontWeight::SEMIBOLD
                        } else {
                            FontWeight::MEDIUM
                        }))
                        .child(title),
                )
                .when_some(help, |col, help| {
                    col.child(
                        div()
                            .id("appearance-bolder-help")
                            .text_size(scale::text(12.))
                            .text_color(color("text-tertiary"))
                            .aria_label(help)
                            .child(help)
                            .test_support(),
                    )
                }),
        )
        .on_click(pick)
        .test_support()
}

/// A font select under its small label.
fn font_select(id: &'static str, label: &'static str, state: &Entity<FontSelect>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(ui_px(6.))
        .w(ui_px(SELECT_WIDTH))
        .child(
            div()
                .text_size(scale::text(12.))
                .text_color(color("text-secondary"))
                .child(label),
        )
        .child(control(
            id,
            Select::new(state).accessibility_label(format!("{label} font")),
        ))
}

/// The − {n}% + stepper and the shortcut hints.
fn zoom(current: u16) -> impl IntoElement {
    let step = |id: &'static str, label: &'static str, sign: &'static str, direction: i8| {
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .w(ui_px(32.))
            .h(scale::row(30.))
            .cursor_pointer()
            .hover(|b| b.bg(color("surface-200")))
            .role(Role::Button)
            .aria_label(label)
            .child(sign)
            .on_click(move |_, _, cx| scale::step_zoom(direction, cx))
            .test_support()
    };
    let value = format!("{current}%");
    let hint = |keys: &'static str, word: &'static str| {
        div()
            .flex()
            .items_center()
            .gap(ui_px(4.))
            .child(
                div()
                    .px(ui_px(5.))
                    .border_1()
                    .border_color(color("border-200"))
                    .rounded(ui_px(4.))
                    .font_family(code_family())
                    .text_size(scale::mono(11.))
                    .text_color(color("text-secondary"))
                    .child(keys),
            )
            .child(word)
    };
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(ui_px(12.))
        .child(
            div()
                .flex()
                .items_center()
                .border_1()
                .border_color(color("border-200"))
                .rounded(ui_px(6.))
                .overflow_hidden()
                .child(step("appearance-zoom-out", "Zoom out", "−", -1))
                .child(
                    div()
                        .id("appearance-zoom-value")
                        .flex()
                        .items_center()
                        .justify_center()
                        .w(ui_px(52.))
                        .h(scale::row(30.))
                        .border_l_1()
                        .border_r_1()
                        .border_color(color("border-200"))
                        .font_family(code_family())
                        .text_size(scale::mono(12.))
                        .aria_label(value.clone())
                        .child(value)
                        .test_support(),
                )
                .child(step("appearance-zoom-in", "Zoom in", "+", 1)),
        )
        .child(
            div()
                .flex()
                .gap(ui_px(12.))
                .text_size(scale::text(12.))
                .text_color(color("text-tertiary"))
                .child(hint("⌘+", "bigger"))
                .child(hint("⌘−", "smaller"))
                .child(hint("⌘0", "reset")),
        )
}

const PREVIEW_MESSAGE: &str = "Done. `load_config()` now reads the same keys as the CLI.";
const PREVIEW_CODE: [&str; 4] = [
    "    if let Some(port) = cfg.get(\"port\") {",
    "-       check_port(port)?;",
    "+       validate_port(port, &path)?;",
    "    }",
];

/// The Preview column: a bit of the sidebar, a message and a code block,
/// drawn by the app's own views with the settings in use.
fn preview(a: &Appearance) -> impl IntoElement {
    let caption = preview_caption(a);
    let chip = "claude · opus".to_string();
    let doc = markdown::parse(PREVIEW_MESSAGE);
    div()
        .id("appearance-preview")
        .flex()
        .flex_col()
        .flex_grow(1.)
        .flex_shrink(0.)
        .flex_basis(ui_px(PREVIEW_WIDTH))
        .min_h_full()
        .border_l_1()
        .border_color(color("border-100"))
        .aria_label("Preview")
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(ui_px(8.))
                .h(scale::row(44.))
                .px(ui_px(16.))
                .border_b_1()
                .border_color(color("border-100"))
                .child(
                    div()
                        .flex_1()
                        .text_size(scale::text(14.))
                        .font_weight(weight(FontWeight::SEMIBOLD))
                        .child("Preview"),
                )
                .child(
                    div()
                        .id("appearance-preview-caption")
                        .text_size(scale::text(12.))
                        .text_color(color("text-tertiary"))
                        .aria_label(caption.clone())
                        .child(caption)
                        .test_support(),
                ),
        )
        .child(
            div()
                .id("appearance-preview-body")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .p(ui_px(16.))
                .gap(ui_px(16.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(ui_px(2.))
                        .p(ui_px(8.))
                        .border_1()
                        .border_color(color("border-100"))
                        .rounded(ui_px(8.))
                        .child(project_row(
                            "preview-project".into(),
                            "demo-app".into(),
                            true,
                        ))
                        .child(
                            channel_row(
                                "preview-channel-general".into(),
                                "general".into(),
                                true,
                                true,
                                "preview-channel-general".into(),
                            )
                            .child(dot("warning")),
                        )
                        .child(
                            channel_row(
                                "preview-channel-bugs".into(),
                                "bugs".into(),
                                false,
                                false,
                                "preview-channel-bugs".into(),
                            )
                            .child(dot("accent-blue")),
                        ),
                )
                .child(
                    message_row(
                        "preview".into(),
                        monogram("Forge", 32.),
                        "Forge".into(),
                        Some((runtime_badge_for(&Runtime::ClaudeCode, chip.clone()), chip)),
                        "10:11".into(),
                        PREVIEW_MESSAGE.into(),
                        render_doc(&doc, &[], "preview-md").into_any_element(),
                    )
                    .px_0()
                    .py_0(),
                )
                .child(code_block().children(PREVIEW_CODE.iter().map(|line| {
                    let tint = match line.chars().next() {
                        Some('-') => Some(("danger-subtle", "danger")),
                        Some('+') => Some(("success-subtle", "success")),
                        _ => None,
                    };
                    code_line(line)
                        .px(ui_px(12.))
                        .when_some(tint, |l, (bg, fg)| l.bg(color(bg)).text_color(color(fg)))
                }))),
        )
        .test_support()
}

actions!(
    font_picker,
    [
        /// The font above.
        FontUp,
        /// The font below.
        FontDown,
        /// Uses the highlighted font.
        FontPick,
    ]
);

/// The font list's key context.
const PICKER_CONTEXT: &str = "FontPicker";
const LOADING_FONTS: &str = "Loading fonts…";

/// ↑ ↓ and ↵ in the font list's search field.
pub fn bindings() -> Vec<KeyBinding> {
    let field = Some("FontPicker > Input");
    vec![
        KeyBinding::new("up", FontUp, field),
        KeyBinding::new("down", FontDown, field),
        KeyBinding::new("enter", FontPick, field),
    ]
}

/// The list of every installed font, searchable.
pub struct FontPicker {
    slot: FontSlot,
    search: Entity<InputState>,
    query: String,
    /// The row ↵ picks.
    highlighted: usize,
    scroll: UniformListScrollHandle,
    _search_events: Subscription,
}

impl FontPicker {
    fn new(slot: FontSlot, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search fonts"));
        let events = cx.subscribe(&search, |this: &mut Self, search, event, cx| {
            if matches!(event, InputEvent::Change) {
                this.query = search.read(cx).value().to_lowercase();
                this.highlighted = 0;
                this.scroll.scroll_to_item(0, ScrollStrategy::Top);
                cx.notify();
            }
        });
        Self {
            slot,
            search,
            query: String::new(),
            highlighted: 0,
            scroll: UniformListScrollHandle::new(),
            _search_events: events,
        }
    }

    /// The fonts shown, or `None` while the list is still loading.
    fn names(&self, cx: &App) -> Option<Vec<String>> {
        scale::installed_fonts(cx).map(|n| matching_fonts(&n, &self.query))
    }

    fn move_by(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.names(cx).map_or(0, |n| n.len());
        if count == 0 {
            return;
        }
        self.highlighted = self.highlighted.saturating_add_signed(delta).min(count - 1);
        self.scroll
            .scroll_to_item(self.highlighted, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn pick_highlighted(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self
            .names(cx)
            .and_then(|n| n.get(self.highlighted).cloned())
        else {
            return;
        };
        use_font(self.slot, name, window, cx);
    }
}

/// Applies the font picked from the list and closes it.
fn use_font(slot: FontSlot, name: String, window: &mut Window, cx: &mut App) {
    change(cx, |a| slot.set(a, name));
    window.close_dialog(cx);
}

/// The fonts in `names` whose name holds `query`, any case.
pub fn matching_fonts(names: &[String], query: &str) -> Vec<String> {
    let query = query.trim().to_lowercase();
    names
        .iter()
        .filter(|n| !n.starts_with('.') && n.to_lowercase().contains(&query))
        .cloned()
        .collect()
}

impl Render for FontPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let names = self.names(cx);
        let loading = names.is_none();
        let names = std::rc::Rc::new(names.unwrap_or_default());
        let slot = self.slot;
        let highlighted = self.highlighted;
        let count = names.len();
        div()
            .id("font-picker")
            .key_context(PICKER_CONTEXT)
            .on_action(cx.listener(|this, _: &FontUp, _, cx| this.move_by(-1, cx)))
            .on_action(cx.listener(|this, _: &FontDown, _, cx| this.move_by(1, cx)))
            .on_action(
                cx.listener(|this, _: &FontPick, window, cx| this.pick_highlighted(window, cx)),
            )
            .flex()
            .flex_col()
            .gap(ui_px(8.))
            .child(Input::new(&self.search).aria_label("Search fonts"))
            .when(loading, |picker| {
                picker.child(
                    div()
                        .id("font-loading")
                        .flex()
                        .items_center()
                        .h(scale::row(28.))
                        .px(ui_px(8.))
                        .text_color(color("text-tertiary"))
                        .aria_label(LOADING_FONTS)
                        .child(LOADING_FONTS)
                        .test_support(),
                )
            })
            .child(
                uniform_list("font-list", count, move |range, _, _| {
                    range
                        .map(|ix| {
                            let name = names[ix].clone();
                            let picked = name.clone();
                            let on = ix == highlighted;
                            div()
                                .id(SharedString::from(format!("font-option-{ix}")))
                                .flex()
                                .items_center()
                                .h(scale::row(28.))
                                .px(ui_px(8.))
                                .rounded(ui_px(6.))
                                .cursor_pointer()
                                .when(on, |r| r.bg(color("surface-300")))
                                .hover(|r| r.bg(color("surface-200")))
                                .aria_label(if on {
                                    format!("{name} (highlighted)")
                                } else {
                                    name.clone()
                                })
                                .child(name)
                                .on_click(move |_, window, cx| {
                                    use_font(slot, picked.clone(), window, cx);
                                })
                                .test_support()
                        })
                        .collect()
                })
                .track_scroll(&self.scroll)
                .h(ui_px(FONT_LIST_HEIGHT)),
            )
            .test_support()
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::SharedString;
    use gpui_kit::component::IndexPath;

    use super::{FontSlot, OTHER_FONT, font_items, matching_fonts, preview_caption};
    use crate::prefs::Appearance;

    fn labels(items: &[SharedString]) -> Vec<&str> {
        items.iter().map(|s| s.as_ref()).collect()
    }

    #[test]
    fn font_selects_offer_the_presets_and_other() {
        let (items, ix) = font_items(FontSlot::Interface, "Geist", true);
        assert_eq!(labels(&items), ["Geist", "SF Pro", OTHER_FONT]);
        assert_eq!(ix, Some(IndexPath::new(0)));
        let (items, ix) = font_items(FontSlot::Code, "Menlo", true);
        assert_eq!(
            labels(&items),
            ["Geist Mono", "SF Mono", "Menlo", OTHER_FONT]
        );
        assert_eq!(ix, Some(IndexPath::new(2)));
    }

    #[test]
    fn a_font_of_your_own_shows_and_says_when_it_is_missing() {
        let (items, ix) = font_items(FontSlot::Interface, "Helvetica", true);
        assert_eq!(labels(&items), ["Geist", "SF Pro", "Helvetica", OTHER_FONT]);
        assert_eq!(ix, Some(IndexPath::new(2)));
        let (items, ix) = font_items(FontSlot::Interface, "Gone Sans", false);
        assert_eq!(items[2], "Gone Sans (not installed)");
        assert_eq!(ix, Some(IndexPath::new(2)));
    }

    #[test]
    fn the_font_list_filters_by_name_and_hides_system_names() {
        let names = [
            ".AppleSystemUIFont".to_string(),
            "Avenir Next".to_string(),
            "Helvetica".to_string(),
            "Helvetica Neue".to_string(),
        ];
        assert_eq!(
            matching_fonts(&names, "HELV"),
            ["Helvetica", "Helvetica Neue"]
        );
        assert_eq!(matching_fonts(&names, "").len(), 3);
    }

    #[test]
    fn the_caption_reads_sizes_and_rendering() {
        assert_eq!(
            preview_caption(&Appearance::default()),
            "13 · 12 mono · standard"
        );
        let a = Appearance {
            ui_size: 14,
            code_size: 13,
            bolder: true,
            ..Appearance::default()
        };
        assert_eq!(preview_caption(&a), "14 · 13 mono · bolder");
    }
}
