//! The ⌘K palette: recent places and actions at once, and as you type,
//! threads, channels, agents and worktree files. Channels, agents and
//! actions come from what the app knows; threads and files from crewd,
//! asked once typing pauses. Only the answer to the latest query shows.

mod rows;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use crew_core::ids::ProjectId;
use crew_proto::{FileHit, Request, SearchResults};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::dialog::DialogEntrance;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::monogram;
use super::settings::Page;
use super::settings::projects::FolderFor;
use super::workspace::{Place, Workspace};
use crate::client::ClientHandle;
use crate::fonts::mono_font;
use crate::theme::color;

use rows::{Inputs, Lead, Row, Section, sections};
pub(super) use rows::{Remote, SectionKind, Target};

actions!(
    palette,
    [
        /// The row above, wrapping to the last.
        PaletteUp,
        /// The row below, wrapping to the first.
        PaletteDown,
        /// Opens the highlighted row.
        PaletteOpen,
        PaletteClose,
        /// Searches the open project or all of them.
        PaletteScope,
        /// Copies the highlighted file's path.
        PaletteCopyPath,
    ]
);

/// The palette's key context.
const CONTEXT: &str = "Palette";
/// How long typing must pause before crewd is asked.
pub(super) const DEBOUNCE: Duration = Duration::from_millis(120);
/// Most results crewd sends per section.
const SEARCH_LIMIT: u32 = 200;
const WIDTH: f32 = 600.;
const TOP: f32 = 72.;
const ROW_H: f32 = 36.;
const LIST_MAX_H: f32 = 480.;
/// Alpha of the match tint.
const MATCH_ALPHA: f32 = 0.2;
/// Under the local rows when crewd couldn't search.
const SEARCH_DOWN: &str = "Search isn't available right now.";
const PLACEHOLDER: &str = "Search threads, channels, agents, files…";
/// The board's pulse mark for Running now.
const PULSE_SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><path d="M1.5 8h3l2-5 3 10 2-5h3"/></svg>"#;

/// Bindings that beat the search field's own.
pub fn bindings() -> Vec<KeyBinding> {
    let field = Some("Palette > Input");
    vec![
        KeyBinding::new("up", PaletteUp, field),
        KeyBinding::new("down", PaletteDown, field),
        KeyBinding::new("enter", PaletteOpen, field),
        KeyBinding::new("escape", PaletteClose, field),
        KeyBinding::new("tab", PaletteScope, field),
        KeyBinding::new("cmd-c", PaletteCopyPath, field),
    ]
}

pub(super) struct Palette {
    ws: WeakEntity<Workspace>,
    client: ClientHandle,
    input: Entity<InputState>,
    query: String,
    /// The project of what was open, searched until you press tab.
    home: Option<ProjectId>,
    all_projects: bool,
    /// The highlighted row, counted over every section.
    selected: usize,
    expanded: HashSet<SectionKind>,
    /// crewd's answer to the latest search that came back.
    remote: Option<Remote>,
    /// Numbers the searches, so only the latest one's answer lands.
    seq: u64,
    /// Waits out the debounce, then asks crewd.
    search: Option<Task<()>>,
    /// The latest search failed, so threads and files may be missing.
    failed: bool,
    /// Searches sent to crewd since the palette opened.
    sent: u64,
    scroll: ScrollHandle,
    /// Scroll the highlighted row into view on the next draw.
    reveal: bool,
    pointer: Option<Point<Pixels>>,
    _input_events: Subscription,
}

impl Palette {
    fn new(
        ws: WeakEntity<Workspace>,
        client: ClientHandle,
        home: Option<ProjectId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(PLACEHOLDER));
        let events = cx.subscribe(&input, |this: &mut Self, input, event, cx| {
            if matches!(event, InputEvent::Change) {
                let query = input.read(cx).value().to_string();
                this.set_query(query, cx);
            }
        });
        Palette {
            ws,
            client,
            input,
            query: String::new(),
            home,
            all_projects: home.is_none(),
            selected: 0,
            expanded: HashSet::new(),
            remote: None,
            seq: 0,
            search: None,
            failed: false,
            sent: 0,
            scroll: ScrollHandle::new(),
            reveal: false,
            pointer: None,
            _input_events: events,
        }
    }

    /// The project searched, `None` for all.
    pub(super) fn scope(&self) -> Option<ProjectId> {
        if self.all_projects { None } else { self.home }
    }

    /// The project searched was removed: search all of them instead.
    pub(super) fn project_removed(&mut self, project: ProjectId, cx: &mut Context<Self>) {
        if self.home != Some(project) {
            return;
        }
        self.home = None;
        self.all_projects = true;
        self.schedule_search(cx);
        cx.notify();
    }

    fn set_query(&mut self, query: String, cx: &mut Context<Self>) {
        if query == self.query {
            return;
        }
        self.query = query;
        self.selected = 0;
        self.expanded.clear();
        self.reveal = true;
        self.schedule_search(cx);
        cx.notify();
    }

    /// Asks crewd for threads and files once typing pauses for
    /// [`DEBOUNCE`]. A newer query drops the older one's wait and answer.
    fn schedule_search(&mut self, cx: &mut Context<Self>) {
        self.seq += 1;
        self.failed = false;
        let query = self.query.trim().to_string();
        if query.is_empty() {
            self.search = None;
            self.remote = None;
            return;
        }
        let seq = self.seq;
        let project = self.scope();
        let client = self.client.clone();
        self.search = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let sent = this.update(cx, |this, _| this.sent += 1);
            if sent.is_err() {
                return;
            }
            let answer = client
                .request(Request::Search {
                    query: query.clone(),
                    project,
                    limit: SEARCH_LIMIT,
                })
                .await
                .and_then(|v| {
                    serde_json::from_value::<SearchResults>(v).map_err(|e| e.to_string())
                });
            match answer {
                Ok(results) => {
                    let remote = Remote {
                        query,
                        project,
                        results,
                    };
                    this.update(cx, |this, cx| this.land(seq, remote, cx)).ok();
                }
                // The local rows stay, with a line saying search is down.
                Err(e) => {
                    eprintln!("crew: search failed: {e}");
                    this.update(cx, |this, cx| this.fail(seq, cx)).ok();
                }
            }
        }));
    }

    /// Takes in crewd's answer to search `seq`, unless a newer one was
    /// sent since.
    pub(super) fn land(&mut self, seq: u64, remote: Remote, cx: &mut Context<Self>) {
        if seq != self.seq {
            return;
        }
        self.remote = Some(remote);
        self.search = None;
        cx.notify();
    }

    /// Notes that search `seq` failed, unless a newer one was sent since.
    fn fail(&mut self, seq: u64, cx: &mut Context<Self>) {
        if seq != self.seq {
            return;
        }
        self.failed = true;
        self.search = None;
        cx.notify();
    }

    /// Whether the search field has focus.
    #[cfg(test)]
    pub(super) fn field_focused(&self, window: &Window, cx: &App) -> bool {
        self.input.read(cx).focus_handle(cx).is_focused(window)
    }

    /// How many searches went to crewd.
    #[cfg(test)]
    pub(super) fn sent(&self) -> u64 {
        self.sent
    }

    /// The number of the latest search.
    #[cfg(test)]
    pub(super) fn seq(&self) -> u64 {
        self.seq
    }

    #[cfg(test)]
    pub(super) fn selected(&self) -> usize {
        self.selected
    }

    #[cfg(test)]
    pub(super) fn remote_query(&self) -> Option<String> {
        self.remote.as_ref().map(|r| r.query.clone())
    }

    fn toggle_scope(&mut self, cx: &mut Context<Self>) {
        if self.home.is_none() {
            return;
        }
        self.all_projects = !self.all_projects;
        self.selected = 0;
        self.reveal = true;
        self.schedule_search(cx);
        cx.notify();
    }

    /// The sections as they are now. Empty once the workspace is gone.
    fn sections(&self, cx: &App) -> Vec<Section> {
        let Some(ws) = self.ws.upgrade() else {
            return Vec::new();
        };
        let ws = ws.read(cx);
        sections(&Inputs {
            state: ws.state(),
            query: &self.query,
            scope: self.scope(),
            recents: &ws.recents,
            channel: ws.palette_channel(),
            remote: self.remote.as_ref(),
            expanded: &self.expanded,
        })
    }

    fn rows(&self, cx: &App) -> Vec<Row> {
        self.sections(cx).into_iter().flat_map(|s| s.rows).collect()
    }

    fn select_by(&mut self, step: isize, cx: &mut Context<Self>) {
        let len = self.rows(cx).len();
        if len == 0 {
            return;
        }
        self.selected = (self.selected as isize + step).rem_euclid(len as isize) as usize;
        self.reveal = true;
        cx.notify();
    }

    fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.rows(cx).into_iter().nth(self.selected) {
            self.perform(row.target, window, cx);
        }
    }

    /// Does what the row is for. Everything but "Show all" closes the
    /// palette first, which gives focus back to where it was.
    fn perform(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        if let Target::ShowAll(kind) = target {
            self.expanded.insert(kind);
            // A click may have moved focus; the keys work in the field.
            self.input.update(cx, |input, cx| input.focus(window, cx));
            cx.notify();
            return;
        }
        window.close_dialog(cx);
        self.ws
            .update(cx, |ws, cx| {
                ws.palette = None;
                ws.palette_target(target, window, cx);
            })
            .ok();
    }

    /// ⌘C on a file row copies its full path; elsewhere the field copies.
    fn copy_path(&mut self, cx: &mut Context<Self>) {
        let row = self.rows(cx).into_iter().nth(self.selected);
        let path = match row.map(|r| r.target) {
            Some(Target::File(hit)) => self.ws.upgrade().and_then(|ws| ws.read(cx).file_path(&hit)),
            _ => None,
        };
        match path {
            Some(path) => cx.write_to_clipboard(ClipboardItem::new_string(
                path.to_string_lossy().into_owned(),
            )),
            None => cx.propagate(),
        }
    }

    /// Only a real move of the pointer picks a row, so a still pointer
    /// under the opening palette leaves the first row highlighted.
    fn pointer_moved(&mut self, at: Point<Pixels>) -> bool {
        self.pointer.replace(at).is_some_and(|last| last != at)
    }

    fn scope_label(&self, cx: &App) -> String {
        let name = self.scope().and_then(|p| {
            self.ws
                .upgrade()
                .and_then(|ws| ws.read(cx).state().project(p).map(|p| p.name.clone()))
        });
        name.unwrap_or_else(|| "All projects".into())
    }

    fn render_header(&self, cx: &App) -> impl IntoElement {
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.))
            .h(px(52.))
            .px(px(16.))
            .border_b_1()
            .border_color(color("border-100"))
            .child(
                Icon::new(IconName::Search)
                    .size(px(16.))
                    .text_color(color("text-secondary")),
            )
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&self.input)
                        .appearance(false)
                        .p_0()
                        .text_size(px(15.))
                        .aria_label("Search"),
                ),
            )
            .child({
                let scope = self.scope_label(cx);
                super::common::chip(scope.clone())
                    .id("palette-scope")
                    .aria_label(scope)
                    .test_support()
            })
    }

    fn render_row(&self, pos: usize, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        let selected = pos == self.selected;
        let lead = match &row.lead {
            Lead::Dot(token) => super::common::dot(token).into_any_element(),
            Lead::Hash => div()
                .font_family(mono_font())
                .text_color(color("text-secondary"))
                .child("#")
                .into_any_element(),
            Lead::Agent(name) => monogram(name, 16.).rounded(px(4.)).into_any_element(),
            Lead::File => icon(IconName::FileText),
            Lead::Plus => icon(IconName::Plus),
            Lead::Pulse => Icon::default()
                .data(PULSE_SVG)
                .size(px(13.))
                .text_color(color("text-secondary"))
                .into_any_element(),
            Lead::Stop => Icon::new(IconName::Square)
                .size(px(11.))
                .text_color(color("danger"))
                .into_any_element(),
            Lead::Folder => icon(IconName::Folder),
            Lead::None => div().into_any_element(),
        };
        let keys = row.keys.or(selected.then_some("↵"));
        let target = row.target.clone();
        div()
            .id(("palette-row", pos))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.))
            .h(px(ROW_H))
            .mx(px(6.))
            .px(px(10.))
            .rounded(px(6.))
            .whitespace_nowrap()
            .overflow_hidden()
            .cursor_pointer()
            .when(selected, |r| r.bg(color("surface-300")))
            // Clicking a row leaves the cursor in the field.
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .role(Role::ListBoxOption)
            .aria_label(row.label.clone())
            .aria_description(row.sub.clone())
            .aria_selected(selected)
            .child(
                div()
                    .flex()
                    .flex_none()
                    .w(px(16.))
                    .justify_center()
                    .child(lead),
            )
            .child(
                div()
                    .flex_none()
                    .max_w(px(360.))
                    .overflow_hidden()
                    .text_ellipsis()
                    .when(row.mono, |d| d.font_family(mono_font()).text_size(px(12.)))
                    .text_color(color(if row.danger { "danger" } else { "text-primary" }))
                    .child(highlighted(&row.label, row.highlight.clone())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(12.))
                    .when(row.mono || row.sub_mono, |d| d.font_family(mono_font()))
                    .text_color(color("text-tertiary"))
                    .child(row.sub.clone()),
            )
            .children(keys.map(key_chip))
            .on_mouse_move(cx.listener(move |this, e: &MouseMoveEvent, _, cx| {
                if this.pointer_moved(e.position) && this.selected != pos {
                    this.selected = pos;
                    cx.notify();
                }
            }))
            .on_click(
                cx.listener(move |this, _, window, cx| this.perform(target.clone(), window, cx)),
            )
            .test_support()
            .into_any_element()
    }
}

fn icon(name: IconName) -> AnyElement {
    Icon::new(name)
        .size(px(13.))
        .text_color(color("text-secondary"))
        .into_any_element()
}

/// The board's `kbd` chip.
fn key_chip(keys: &'static str) -> impl IntoElement {
    div()
        .flex_none()
        .ml_auto()
        .px(px(5.))
        .rounded(px(4.))
        .border_1()
        .border_color(color("border-200"))
        .font_family(mono_font())
        .text_size(px(11.))
        .text_color(color("text-tertiary"))
        .child(keys)
}

/// `label` with `range` on a blue tint.
fn highlighted(label: &str, range: Option<std::ops::Range<usize>>) -> StyledText {
    let style = HighlightStyle {
        color: Some(color("text-primary")),
        background_color: Some(color("accent-blue").opacity(MATCH_ALPHA)),
        ..Default::default()
    };
    StyledText::new(label.to_string()).with_highlights(range.map(|r| (r, style)))
}

/// "↑↓ move", "↵ open", "tab all projects", "esc close", and on a file
/// row "⌘C copy path".
fn footer(file: bool) -> impl IntoElement {
    let hint = |keys: &'static str, text: &'static str| {
        div()
            .id(SharedString::from(format!("palette-hint-{text}")))
            .flex()
            .gap(px(4.))
            .aria_label(format!("{keys} {text}"))
            .child(div().font_family(mono_font()).child(keys))
            .child(text)
            .test_support()
    };
    div()
        .id("palette-footer")
        .flex()
        .flex_none()
        .gap(px(14.))
        .px(px(16.))
        .py(px(8.))
        .mt(px(6.))
        .border_t_1()
        .border_color(color("border-100"))
        .text_size(px(11.))
        .text_color(color("text-tertiary"))
        .child(hint("↑↓", "move"))
        .child(hint("↵", "open"))
        .when(file, |f| f.child(hint("⌘C", "copy path")))
        .child(hint("tab", "all projects"))
        .child(div().ml_auto().child(hint("esc", "close")))
        .test_support()
}

impl Render for Palette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sections = self.sections(cx);
        let count: usize = sections.iter().map(|s| s.rows.len()).sum();
        self.selected = self.selected.min(count.saturating_sub(1));
        let mut list = div()
            .id("palette-list")
            .flex()
            .flex_col()
            .pb(px(2.))
            .max_h(px(LIST_MAX_H))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .role(Role::ListBox)
            .aria_label("Results");
        let mut pos = 0;
        let mut child = 0;
        let mut selected_child = None;
        let mut on_file = false;
        for section in &sections {
            list = list.child(
                div()
                    .id(SharedString::from(format!("palette-section-{pos}")))
                    .pt(px(10.))
                    .px(px(16.))
                    .pb(px(4.))
                    .text_size(px(11.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(color("text-tertiary"))
                    .aria_label(section.title.clone())
                    .child(section.title.to_uppercase())
                    .test_support(),
            );
            child += 1;
            for row in &section.rows {
                if pos == self.selected {
                    selected_child = Some(child);
                    on_file = matches!(row.target, Target::File(_));
                }
                list = list.child(self.render_row(pos, row, cx));
                pos += 1;
                child += 1;
            }
        }
        let searching = self.search.is_some() && !self.query.trim().is_empty();
        let note = if self.failed {
            Some(SEARCH_DOWN)
        } else if searching {
            Some("Searching…")
        } else if count == 0 {
            Some("No matches")
        } else {
            None
        };
        if let Some(note) = note {
            let padding = if count == 0 { 24. } else { 8. };
            list = list.child(
                div()
                    .id("palette-note")
                    .py(px(padding))
                    .px(px(16.))
                    .when(count == 0, |d| d.text_center())
                    .text_size(px(12.))
                    .text_color(color("text-tertiary"))
                    .aria_label(note)
                    .child(note)
                    .test_support(),
            );
        }
        if std::mem::take(&mut self.reveal)
            && let Some(child) = selected_child
        {
            // The first row keeps its section's title in view.
            let child = if self.selected == 0 { 0 } else { child };
            self.scroll.scroll_to_item(child);
        }
        div()
            .id("palette")
            .key_context(CONTEXT)
            .flex()
            .flex_col()
            .w_full()
            .text_size(px(13.))
            .text_color(color("text-primary"))
            .on_action(cx.listener(|this, _: &PaletteUp, _, cx| this.select_by(-1, cx)))
            .on_action(cx.listener(|this, _: &PaletteDown, _, cx| this.select_by(1, cx)))
            .on_action(
                cx.listener(|this, _: &PaletteOpen, window, cx| this.open_selected(window, cx)),
            )
            .on_action(cx.listener(|this, _: &PaletteScope, _, cx| this.toggle_scope(cx)))
            .on_action(cx.listener(|this, _: &PaletteCopyPath, _, cx| this.copy_path(cx)))
            .on_action(cx.listener(|this, _: &PaletteClose, window, cx| {
                window.close_dialog(cx);
                this.ws.update(cx, |ws, _| ws.palette = None).ok();
            }))
            .child(self.render_header(cx))
            .child(list)
            .child(footer(on_file))
            .test_support()
    }
}

impl Workspace {
    /// Opens the palette, or puts the cursor back in it if it is open.
    pub(super) fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(palette) = &self.palette {
            let input = palette.read(cx).input.clone();
            input.update(cx, |input, cx| input.focus(window, cx));
            return;
        }
        let home = self.palette_project();
        let ws = cx.entity().downgrade();
        let client = self.client.clone();
        let palette = cx.new(|cx| Palette::new(ws, client, home, window, cx));
        self.palette = Some(palette.clone());
        let first_build = std::cell::Cell::new(true);
        let closed = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, window, cx| {
            if first_build.replace(false) {
                let input = palette.read(cx).input.clone();
                input.update(cx, |input, cx| input.focus(window, cx));
            }
            let closed = closed.clone();
            dialog
                .entrance(DialogEntrance::None)
                .w(px(WIDTH))
                .margin_top(px(TOP))
                .p_0()
                .gap_0()
                .rounded(px(12.))
                .bg(color("surface-raised"))
                .border_color(color("border-200"))
                .shadow(vec![BoxShadow {
                    color: hsla(0., 0., 0., 0.8),
                    offset: point(px(0.), px(16.)),
                    blur_radius: px(48.),
                    spread_radius: px(0.),
                    inset: false,
                }])
                .overflow_hidden()
                .close_button(false)
                .on_close(move |_, _, cx| {
                    closed.update(cx, |ws, _| ws.palette = None).ok();
                })
                .child(palette.clone())
        });
        cx.notify();
    }

    /// The open palette, if any.
    #[cfg(test)]
    pub(super) fn palette(&self) -> Option<&Entity<Palette>> {
        self.palette.as_ref()
    }

    /// The project of what is open: the palette searches it first.
    fn palette_project(&self) -> Option<ProjectId> {
        let state = self.state();
        self.nav
            .channel
            .and_then(|c| state.channel(c))
            .map(|c| c.project)
            .or_else(|| self.nav.thread.and_then(|t| state.thread_project(t)))
    }

    /// Where "New thread in #{channel}" goes: the open channel, else the
    /// last one you went to, else the first one there is.
    fn palette_channel(&self) -> Option<crew_core::ids::ChannelId> {
        let state = self.state();
        let known = |c: &crew_core::ids::ChannelId| state.channel(*c).is_some();
        self.nav
            .channel
            .filter(known)
            .or(self.recents.channel.filter(known))
            .or_else(|| {
                state
                    .projects()
                    .into_iter()
                    .find_map(|p| state.channels(p.id).first().map(|c| c.id))
            })
    }

    /// Where a file the search found is on disk.
    fn file_path(&self, hit: &FileHit) -> Option<PathBuf> {
        self.file_base(hit).map(|base| base.join(&hit.path))
    }

    /// The worktree or project folder holding the file.
    fn file_base(&self, hit: &FileHit) -> Option<PathBuf> {
        let state = self.state();
        match hit.thread {
            Some(t) => state
                .thread(t)
                .and_then(|t| t.worktree.as_ref())
                .map(|wt| wt.path.clone()),
            None => state.project(hit.project).map(|p| p.root.clone()),
        }
    }

    /// Does what a palette row is for.
    fn palette_target(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        match target {
            Target::Thread(t) => self.open_place(Place::Thread(t), window, cx),
            Target::Channel(c) => self.open_place(Place::Channel(c), window, cx),
            Target::Agent(a) => self.open_place(Place::Agent(a), window, cx),
            Target::Project(p) => self.open_place(Place::Project(p), window, cx),
            Target::File(hit) => self.open_file_hit(hit, window, cx),
            Target::NewThread { channel, text } => {
                if self.nav.channel != Some(channel) || self.nav.thread.is_some() {
                    self.open_place(Place::Channel(channel), window, cx);
                } else {
                    self.leave_pages(cx);
                }
                self.new_thread(window, cx);
                if let Some(text) = text {
                    self.composer
                        .update(cx, |composer, cx| composer.set_text(&text, window, cx));
                }
            }
            Target::Running => self.show_running(window, cx),
            Target::StopAll => self.stop_all(window, cx),
            Target::AddProject => {
                self.open_settings(Page::Projects, window, cx);
                self.choose_folder(FolderFor::Settings, window, cx);
            }
            Target::ShowAll(_) => {}
        }
    }

    /// A changed worktree file opens in Review changes; any other in your
    /// editor, as Open in editor does.
    fn open_file_hit(&mut self, hit: FileHit, window: &mut Window, cx: &mut Context<Self>) {
        if let (true, Some(thread)) = (hit.changed, hit.thread)
            && self.state().thread(thread).is_some()
        {
            self.leave_pages(cx);
            self.open_review_at(thread, hit.path, window, cx);
            return;
        }
        let Some(base) = self.file_base(&hit) else {
            return;
        };
        let app = self.editor_app();
        let file = base.join(&hit.path);
        let args = crate::editor::open_args(app.as_deref(), &base, Some(&file));
        let opener = self.opener.clone();
        let opened = cx.background_executor().spawn(async move { opener(args) });
        self.spawn_request(window, cx, async move |_, _| {
            opened.await.map_err(|e| {
                eprintln!("crew: open failed: {e}");
                (crate::editor::open_failed(app.as_deref()), None)
            })
        });
    }
}
