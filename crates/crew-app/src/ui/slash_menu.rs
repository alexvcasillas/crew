//! The `/` menu of the composer: the skills Claude Code and Codex can run
//! in the project, matching what follows the `/`.

use std::ops::Range;

use crew_core::agent::Runtime;
use crew_proto::{SkillInfo, SkillSource};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::provider_icon;
use super::composer::{Composer, ComposerEvent};
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};

pub const SLASH_MENU_WIDTH: f32 = 460.;
pub const NO_SKILLS: &str = "No skills match.";
pub const NO_SKILLS_HELP: &str = "Crew lists skills from your CLIs' folders.";
/// How many skills the Recent section lists.
pub const RECENT_SHOWN: usize = 3;
/// Space between the menu and the composer.
const MENU_GAP: f32 = 6.;
const ROW_HEIGHT: f32 = 36.;
/// A section's caption, with its padding.
const CAPTION_HEIGHT: f32 = 28.;
/// Past this the list scrolls.
const LIST_MAX_HEIGHT: f32 = 420.;

/// A skill as the menu lists it: one row per name and source, with each
/// runtime that has it.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    pub source: SkillSource,
    /// The runtimes that have it, Claude Code first, each with its file.
    pub runtimes: Vec<(Runtime, String)>,
    /// `name` in lowercase, to filter without allocating.
    lower: String,
}

impl SkillEntry {
    /// What the Recent section remembers it by: `global:unslop`.
    pub fn key(&self) -> String {
        format!("{}:{}", self.source.key(), self.name)
    }

    /// The chip after the row: `project`, `global` or the plugin's name.
    pub fn source_tag(&self) -> String {
        source_tag(&self.source)
    }

    /// The skill's file for `runtime`, if it has one there.
    pub fn path(&self, runtime: &Runtime) -> Option<&str> {
        self.runtimes
            .iter()
            .find(|(r, _)| r == runtime)
            .map(|(_, p)| p.as_str())
    }

    pub fn has(&self, runtime: &Runtime) -> bool {
        self.path(runtime).is_some()
    }

    /// In both Claude Code and Codex.
    pub fn both(&self) -> bool {
        self.runtimes.len() > 1
    }
}

/// `project`, `global` or the plugin's name.
pub fn source_tag(source: &SkillSource) -> String {
    match source {
        SkillSource::Project => "project".into(),
        SkillSource::Global => "global".into(),
        SkillSource::Plugin(name) => name.clone(),
    }
}

/// `Claude` or `Codex`.
pub fn runtime_name(runtime: &Runtime) -> &'static str {
    match runtime {
        Runtime::ClaudeCode => "Claude",
        Runtime::Codex => "Codex",
    }
}

/// The menu's entries from crewd's list: hidden skills left out, and a
/// name both runtimes have from the same source as one entry.
pub fn entries(list: &[SkillInfo]) -> Vec<SkillEntry> {
    let mut out: Vec<SkillEntry> = Vec::new();
    for skill in list.iter().filter(|s| s.shown) {
        match out
            .iter_mut()
            .find(|e| e.name == skill.name && e.source == skill.source)
        {
            Some(entry) => {
                if !entry.has(&skill.runtime) {
                    entry
                        .runtimes
                        .push((skill.runtime.clone(), skill.path.clone()));
                    entry
                        .runtimes
                        .sort_by_key(|(r, _)| *r != Runtime::ClaudeCode);
                }
                if entry.description.is_empty() {
                    entry.description = skill.description.clone();
                }
            }
            None => out.push(SkillEntry {
                name: skill.name.clone(),
                description: skill.description.clone(),
                source: skill.source.clone(),
                runtimes: vec![(skill.runtime.clone(), skill.path.clone())],
                lower: skill.name.to_lowercase(),
            }),
        }
    }
    out
}

/// A section of the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Recent,
    Project,
    Global,
    Plugins,
}

impl Section {
    pub fn title(self) -> &'static str {
        match self {
            Section::Recent => "Recent",
            Section::Project => "This project",
            Section::Global => "Global",
            Section::Plugins => "Plugins",
        }
    }

    fn of(source: &SkillSource) -> Section {
        match source {
            SkillSource::Project => Section::Project,
            SkillSource::Global => Section::Global,
            SkillSource::Plugin(_) => Section::Plugins,
        }
    }
}

/// One row: the entry's index and the bytes of its name the query
/// matched.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuRow {
    pub entry: usize,
    pub matched: Option<Range<usize>>,
}

/// The menu for `query`: its sections in order, each with its rows. With
/// no query, up to [`RECENT_SHOWN`] skills of `recent` (newest first) come
/// first and are left out of their own section. With one, names holding
/// it are listed, those starting with it first in each section.
pub fn menu_rows(
    entries: &[SkillEntry],
    query: &str,
    recent: &[String],
) -> Vec<(Section, Vec<MenuRow>)> {
    let query = query.to_lowercase();
    let mut sections: Vec<(Section, Vec<MenuRow>)> = Vec::new();
    let mut push =
        |section: Section, row: MenuRow| match sections.iter_mut().find(|(s, _)| *s == section) {
            Some((_, rows)) => rows.push(row),
            None => sections.push((section, vec![row])),
        };
    if query.is_empty() {
        let recents: Vec<usize> = recent
            .iter()
            .filter_map(|key| entries.iter().position(|e| e.key() == *key))
            .take(RECENT_SHOWN)
            .collect();
        for &entry in &recents {
            push(
                Section::Recent,
                MenuRow {
                    entry,
                    matched: None,
                },
            );
        }
        for section in [Section::Project, Section::Global, Section::Plugins] {
            for (ix, e) in entries.iter().enumerate() {
                if Section::of(&e.source) == section && !recents.contains(&ix) {
                    push(
                        section,
                        MenuRow {
                            entry: ix,
                            matched: None,
                        },
                    );
                }
            }
        }
        return sections;
    }
    for section in [Section::Project, Section::Global, Section::Plugins] {
        let mut found: Vec<(bool, MenuRow)> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| Section::of(&e.source) == section)
            .filter_map(|(ix, e)| {
                let at = e.lower.find(&query)?;
                // Lowercasing may change byte lengths; then nothing is
                // marked.
                let matched = (e.lower.len() == e.name.len()).then(|| at..at + query.len());
                Some((at == 0, MenuRow { entry: ix, matched }))
            })
            .collect();
        found.sort_by_key(|(prefix, _)| !prefix);
        for (_, row) in found {
            push(section, row);
        }
    }
    sections
}

/// The rows of `sections` in order, as the selection walks them.
pub fn flat(sections: &[(Section, Vec<MenuRow>)]) -> Vec<&MenuRow> {
    sections.iter().flat_map(|(_, rows)| rows).collect()
}

/// Where the selection starts: on the first skill you used lately, else
/// the first row.
pub fn first_selected(entries: &[SkillEntry], rows: &[&MenuRow], recent: &[String]) -> usize {
    recent
        .iter()
        .find_map(|key| rows.iter().position(|r| entries[r.entry].key() == *key))
        .unwrap_or(0)
}

/// The `/` being typed at `cursor`: its byte offset and the name so far.
/// It counts at the start of the text or after a space.
pub fn slash_at(text: &str, cursor: usize) -> Option<(usize, &str)> {
    let before = text.get(..cursor)?;
    let at = before.rfind('/')?;
    let query = &before[at + 1..];
    let word = query
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'));
    let starts = before[..at]
        .chars()
        .next_back()
        .is_none_or(char::is_whitespace);
    (word && starts).then_some((at, query))
}

/// About how tall the menu is with these sections, or with nothing to
/// list.
pub fn slash_menu_height(sections: &[(Section, Vec<MenuRow>)]) -> f32 {
    // Padding and border, then the key hints.
    let frame = 14. + 25.;
    if sections.is_empty() {
        return frame + 60.;
    }
    let list: f32 = sections
        .iter()
        .map(|(_, rows)| CAPTION_HEIGHT + ROW_HEIGHT * rows.len() as f32)
        .sum();
    frame + list.min(LIST_MAX_HEIGHT)
}

/// Whether the menu opens below a composer whose top is at `top`.
pub fn opens_below(top: f32, height: f32) -> bool {
    top - f32::from(ui_px(MENU_GAP)) - height
        < f32::from(scale::chrome(super::title_bar::TITLE_BAR_HEIGHT))
}

impl Composer {
    /// The open `/` menu, above the composer, or below it when there isn't
    /// room above.
    pub(super) fn render_slash_menu(
        &self,
        query: &str,
        selected: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let sections = menu_rows(&self.skills, query, &self.recent_skills);
        let height = f32::from(scale::row(slash_menu_height(&sections)));
        let below = self.top.get().is_some_and(|top| opens_below(top, height));
        let empty_mode = self.mode() == super::composer::ComposerMode::Empty;
        let label = if sections.is_empty() {
            "No skills match".to_string()
        } else if query.is_empty() {
            "Skills".to_string()
        } else {
            format!("Skills matching {query}")
        };
        let menu = div()
            .id("slash-menu")
            .absolute()
            .map(|menu| match (below, empty_mode) {
                // In an empty channel it drops over the composer, under the
                // line you type on.
                (true, true) => menu.top(scale::row(54.)).left(ui_px(12.)),
                (true, false) => menu.top_full().mt(ui_px(MENU_GAP)).left_0(),
                (false, _) => menu.bottom_full().mb(ui_px(MENU_GAP)).left_0(),
            })
            .w(ui_px(SLASH_MENU_WIDTH))
            .flex()
            .flex_col()
            .p(ui_px(6.))
            .bg(color("surface-raised"))
            .border_1()
            .border_color(color("border-200"))
            .rounded(ui_px(8.))
            .shadow_lg()
            .role(Role::ListBox)
            .aria_label(label);
        if sections.is_empty() {
            return menu
                .child(render_no_match(cx))
                .child(footer(
                    &[("esc", "close")],
                    Some("Keep typing to send it as text"),
                ))
                .test_support();
        }
        let mut items: Vec<AnyElement> = Vec::new();
        let mut ix = 0usize;
        for (section, rows) in &sections {
            items.push(
                div()
                    .id(SharedString::from(format!(
                        "slash-section-{}",
                        section.title()
                    )))
                    .px(ui_px(10.))
                    .pt(scale::row(8.))
                    .pb(scale::row(4.))
                    .text_size(scale::text(11.))
                    .font_weight(weight(FontWeight::MEDIUM))
                    .text_color(color("text-tertiary"))
                    .aria_label(section.title())
                    .child(section.title().to_uppercase())
                    .test_support()
                    .into_any_element(),
            );
            for row in rows {
                let entry = row.entry;
                items.push(
                    render_option(ix, &self.skills[entry], row, ix == selected)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.pick_skill(entry, window, cx);
                        }))
                        .test_support()
                        .into_any_element(),
                );
                ix += 1;
            }
        }
        menu.child(
            div()
                .id("slash-list")
                .flex()
                .flex_col()
                .max_h(scale::row(LIST_MAX_HEIGHT))
                .overflow_y_scroll()
                .track_scroll(&self.slash_scroll)
                .children(items),
        )
        .child(footer(
            &[("↑↓", "choose"), ("↵", "use"), ("esc", "close")],
            None,
        ))
        .test_support()
    }
}

fn render_option(ix: usize, skill: &SkillEntry, row: &MenuRow, selected: bool) -> Stateful<Div> {
    let name = format!("/{}", skill.name);
    let mut text = StyledText::new(name.clone());
    if let Some(range) = &row.matched {
        text = text.with_highlights(vec![(
            range.start + 1..range.end + 1,
            HighlightStyle {
                background_color: Some(color("accent-blue").opacity(0.2)),
                ..Default::default()
            },
        )]);
    }
    let (marks, word): (Vec<Runtime>, &str) = if skill.both() {
        (vec![Runtime::ClaudeCode, Runtime::Codex], "Both")
    } else {
        let runtime = skill.runtimes[0].0.clone();
        let word = runtime_name(&runtime);
        (vec![runtime], word)
    };
    div()
        .id(("slash-option", ix))
        .flex()
        .flex_none()
        .items_center()
        .gap(ui_px(10.))
        .h(scale::row(ROW_HEIGHT))
        .px(ui_px(10.))
        .rounded(ui_px(6.))
        .whitespace_nowrap()
        .text_size(scale::text(12.))
        .cursor_pointer()
        .when(selected, |row| row.bg(color("surface-300")))
        .hover(|row| row.bg(color("surface-200")))
        .role(Role::ListBoxOption)
        .aria_label(name)
        .aria_selected(selected)
        .child(
            div()
                .flex_none()
                .font_family(code_family())
                .text_size(scale::mono(12.))
                .text_color(color("text-primary"))
                .child(text),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(color("text-tertiary"))
                .child(skill.description.clone()),
        )
        .child(
            div()
                .id(("slash-runtime", ix))
                .flex()
                .flex_none()
                .items_center()
                .gap(ui_px(4.))
                .text_size(scale::text(11.))
                .text_color(color("text-secondary"))
                .aria_label(word)
                .children(marks.iter().map(|r| provider_icon(r, 12.)))
                .child(word)
                .test_support(),
        )
        .child(
            super::common::chip(skill.source_tag())
                .id(("slash-source", ix))
                .aria_label(skill.source_tag())
                .test_support(),
        )
}

/// "No skills match." and the link to manage them.
fn render_no_match(cx: &mut Context<Composer>) -> impl IntoElement {
    div()
        .id("slash-empty")
        .px(ui_px(10.))
        .pt(scale::row(14.))
        .pb(scale::row(10.))
        .aria_label(NO_SKILLS)
        .child(
            div()
                .font_weight(weight(FontWeight::MEDIUM))
                .child(NO_SKILLS),
        )
        .child(
            div()
                .flex()
                .gap(ui_px(4.))
                .mt(scale::row(2.))
                .text_size(scale::text(12.))
                .line_height(scale::line(18.))
                .text_color(color("text-tertiary"))
                .child(NO_SKILLS_HELP)
                .child(
                    div()
                        .id("slash-manage")
                        .text_color(color("accent-blue"))
                        .cursor_pointer()
                        .role(Role::Link)
                        .child("Manage skills")
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(ComposerEvent::ManageSkills)))
                        .test_support(),
                ),
        )
        .test_support()
}

/// The key hints under the list, and a note after them.
fn footer(keys: &[(&'static str, &'static str)], note: Option<&'static str>) -> impl IntoElement {
    div()
        .flex()
        .gap(ui_px(12.))
        .mt(scale::row(4.))
        .px(ui_px(10.))
        .pt(scale::row(6.))
        .pb(scale::row(2.))
        .border_t_1()
        .border_color(color("border-100"))
        .text_size(scale::text(11.))
        .text_color(color("text-tertiary"))
        .children(keys.iter().map(|(keys, what)| {
            div()
                .flex()
                .gap(ui_px(4.))
                .child(div().font_family(code_family()).child(*keys))
                .child(*what)
        }))
        .children(note.map(|note| {
            div()
                .id("slash-note")
                .aria_label(note)
                .child(note)
                .test_support()
        }))
}

#[cfg(test)]
mod tests {
    use super::{Section, entries, first_selected, flat, menu_rows, slash_at};
    use crew_core::agent::Runtime;
    use crew_proto::{SkillInfo, SkillSource};

    fn skill(runtime: Runtime, name: &str, source: SkillSource) -> SkillInfo {
        SkillInfo {
            runtime,
            name: name.into(),
            description: format!("{name} things"),
            source,
            path: format!("/work/demo/{name}/SKILL.md"),
            shown: true,
        }
    }

    fn names(
        entries: &[super::SkillEntry],
        q: &str,
        recent: &[String],
    ) -> Vec<(Section, Vec<String>)> {
        menu_rows(entries, q, recent)
            .into_iter()
            .map(|(s, rows)| {
                (
                    s,
                    rows.iter().map(|r| entries[r.entry].name.clone()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn a_slash_counts_at_the_start_or_after_a_space() {
        assert_eq!(slash_at("/", 1), Some((0, "")));
        assert_eq!(slash_at("/un", 3), Some((0, "un")));
        assert_eq!(slash_at("fix it /docs:wr", 15), Some((7, "docs:wr")));
        assert_eq!(slash_at("src/lib", 7), None, "inside a word");
        assert_eq!(slash_at("/un then", 8), None, "past a space");
        assert_eq!(slash_at("a\n/x", 4), Some((2, "x")));
    }

    #[test]
    fn hidden_skills_are_left_out_and_both_runtimes_share_a_row() {
        let mut hidden = skill(Runtime::ClaudeCode, "secret", SkillSource::Global);
        hidden.shown = false;
        let list = [
            skill(Runtime::Codex, "unslop", SkillSource::Global),
            skill(Runtime::ClaudeCode, "unslop", SkillSource::Global),
            skill(Runtime::ClaudeCode, "review", SkillSource::Project),
            skill(Runtime::Codex, "review", SkillSource::Global),
            hidden,
        ];
        let e = entries(&list);
        assert_eq!(e.len(), 3);
        assert!(e[0].both());
        assert_eq!(e[0].runtimes[0].0, Runtime::ClaudeCode, "Claude first");
        assert!(!e[1].both(), "another source is another row");
        assert_eq!(e[1].source_tag(), "project");
    }

    #[test]
    fn sections_follow_sources_with_recent_first() {
        let list = [
            skill(Runtime::ClaudeCode, "tdd", SkillSource::Global),
            skill(Runtime::ClaudeCode, "unslop", SkillSource::Global),
            skill(Runtime::ClaudeCode, "bench", SkillSource::Project),
            skill(Runtime::ClaudeCode, "unused-deps", SkillSource::Project),
            skill(
                Runtime::ClaudeCode,
                "browser:run-flow",
                SkillSource::Plugin("browser".into()),
            ),
        ];
        let e = entries(&list);
        let recent = vec!["global:unslop".to_string(), "global:gone".to_string()];
        assert_eq!(
            names(&e, "", &recent),
            [
                (Section::Recent, vec!["unslop".to_string()]),
                (
                    Section::Project,
                    vec!["bench".to_string(), "unused-deps".to_string()]
                ),
                (Section::Global, vec!["tdd".to_string()]),
                (Section::Plugins, vec!["browser:run-flow".to_string()]),
            ]
        );
        assert_eq!(
            names(&e, "UN", &recent),
            [
                (Section::Project, vec!["unused-deps".to_string()]),
                (Section::Global, vec!["unslop".to_string()]),
                (Section::Plugins, vec!["browser:run-flow".to_string()]),
            ]
        );
        let sections = menu_rows(&e, "un", &recent);
        let rows = flat(&sections);
        assert_eq!(rows[2].matched, Some(9..11), "browser:r|un|-flow");
        assert_eq!(first_selected(&e, &rows, &recent), 1, "the recent one");
        assert!(menu_rows(&e, "zip", &recent).is_empty());
    }

    #[test]
    fn three_hundred_skills_filter_at_once() {
        let list: Vec<SkillInfo> = (0..300)
            .map(|i| {
                skill(
                    Runtime::ClaudeCode,
                    &format!("plug{}:skill-{i}", i % 7),
                    SkillSource::Plugin(format!("plug{}", i % 7)),
                )
            })
            .collect();
        let e = entries(&list);
        let started = std::time::Instant::now();
        for q in ["s", "sk", "ski", "skill-1", "skill-12", "skill-123"] {
            let _ = menu_rows(&e, q, &[]);
        }
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
        assert_eq!(flat(&menu_rows(&e, "skill-12", &[])).len(), 11);
    }
}
