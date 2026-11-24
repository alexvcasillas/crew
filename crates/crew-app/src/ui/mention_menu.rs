//! The `@` menu above the composer: agents matching what follows the `@`.

use crew_core::agent::Runtime;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::{monogram, runtime_badge_for, status_word};
use super::composer::{Composer, ComposerEvent};
use crate::model::ThreadStatus;
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};

pub const MENU_WIDTH: f32 = 420.;
pub const NO_AGENTS: &str = "No agents yet. Add one in Settings.";
/// Space between the menu and the composer.
const MENU_GAP: f32 = 6.;
/// One agent's row.
const ROW_HEIGHT: f32 = 44.;

/// About how tall the menu is with `rows` agents listed, or with none
/// to list when `any_agents` is false.
pub fn menu_height(rows: usize, any_agents: bool) -> f32 {
    // Padding and border, then the caption and key hints.
    let frame = 14.;
    if !any_agents {
        return frame + 40.;
    }
    // "No agent matches" takes a row's place.
    frame + 24. + 29. + ROW_HEIGHT * rows.max(1) as f32
}

/// Whether the menu opens below a composer whose top is at `top` (window
/// px): when above it would run under the title bar. `height` is in
/// window px too.
pub fn opens_below(top: f32, height: f32) -> bool {
    top - f32::from(ui_px(MENU_GAP)) - height < f32::from(ui_px(super::title_bar::TITLE_BAR_HEIGHT))
}

/// An agent as the menu shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct MentionAgent {
    pub name: String,
    pub role: String,
    /// `claude · haiku`.
    pub chip: String,
    /// The runtime behind `chip`, for its provider mark.
    pub runtime: Runtime,
    pub status: ThreadStatus,
}

/// The `@` being typed at `cursor`: its byte offset and the name so far.
/// `None` if the cursor isn't in a mention.
pub fn mention_at(text: &str, cursor: usize) -> Option<(usize, &str)> {
    let before = text.get(..cursor)?;
    let at = before.rfind('@')?;
    let query = &before[at + 1..];
    let word = query
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_');
    let starts = before[..at]
        .chars()
        .next_back()
        .is_none_or(|c| c.is_whitespace() || c == '(');
    (word && starts).then_some((at, query))
}

/// Agents whose name holds `query`, ignoring case. Names that start with
/// it come first.
pub fn matching<'a>(agents: &'a [MentionAgent], query: &str) -> Vec<&'a MentionAgent> {
    let query = query.to_lowercase();
    let mut found: Vec<&MentionAgent> = agents
        .iter()
        .filter(|a| a.name.to_lowercase().contains(&query))
        .collect();
    found.sort_by_key(|a| !a.name.to_lowercase().starts_with(&query));
    found
}

impl Composer {
    /// The open menu, drawn above the composer, or below it when there
    /// isn't room above.
    pub(super) fn render_mention_menu(
        &self,
        query: &str,
        selected: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let agents = matching(&self.agents, query);
        let caption = format!("Agents in {}", self.project).to_uppercase();
        let height = f32::from(scale::row(menu_height(
            agents.len(),
            !self.agents.is_empty(),
        )));
        let below = self.top.get().is_some_and(|top| opens_below(top, height));
        let mut menu = div()
            .id("mention-menu")
            .absolute()
            .map(|menu| {
                if below {
                    menu.top_full().mt(ui_px(MENU_GAP))
                } else {
                    menu.bottom_full().mb(ui_px(MENU_GAP))
                }
            })
            .left_0()
            .w(ui_px(MENU_WIDTH))
            .flex()
            .flex_col()
            .py(scale::row(6.))
            .bg(color("surface-raised"))
            .border_1()
            .border_color(color("border-200"))
            .rounded(ui_px(8.))
            .shadow_lg();
        if self.agents.is_empty() {
            return menu.child(render_no_agents(cx)).test_support();
        }
        menu = menu.child(
            div()
                .px(ui_px(12.))
                .py(scale::row(4.))
                .text_size(scale::text(11.))
                .font_weight(weight(FontWeight::MEDIUM))
                .text_color(color("text-tertiary"))
                .child(caption),
        );
        if agents.is_empty() {
            menu = menu.child(
                div()
                    .px(ui_px(12.))
                    .py(scale::row(8.))
                    .text_color(color("text-tertiary"))
                    .child(format!("No agent matches “{query}”.")),
            );
        }
        let rows: Vec<AnyElement> = agents
            .iter()
            .enumerate()
            .map(|(ix, agent)| {
                let name = agent.name.clone();
                render_option(ix, agent, ix == selected)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.insert_mention(&name, window, cx);
                    }))
                    .test_support()
                    .into_any_element()
            })
            .collect();
        menu.children(rows)
            .child(
                div()
                    .flex()
                    .gap(ui_px(12.))
                    .mt(ui_px(4.))
                    .px(ui_px(12.))
                    .pt(scale::row(6.))
                    .border_t_1()
                    .border_color(color("border-100"))
                    .text_size(scale::text(11.))
                    .text_color(color("text-tertiary"))
                    .child(hint("↑↓", "choose"))
                    .child(hint("↵", "mention"))
                    .child(hint("esc", "close")),
            )
            .test_support()
    }
}

fn render_option(ix: usize, agent: &MentionAgent, selected: bool) -> Stateful<Div> {
    let (word, token) = status_word(agent.status);
    div()
        .id(("mention-option", ix))
        .flex()
        .items_center()
        .gap(ui_px(10.))
        .h(scale::row(44.))
        .mx(ui_px(6.))
        .px(ui_px(6.))
        .rounded(ui_px(6.))
        .cursor_pointer()
        .when(selected, |row| row.bg(color("surface-300")))
        .hover(|row| row.bg(color("surface-200")))
        .aria_label(agent.name.clone())
        .aria_selected(selected)
        .child(monogram(&agent.name, 28.))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .font_weight(weight(FontWeight::MEDIUM))
                        .child(agent.name.clone()),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(scale::text(12.))
                        .text_color(color("text-secondary"))
                        .child(agent.role.clone()),
                ),
        )
        .child(runtime_badge_for(&agent.runtime, agent.chip.clone()))
        .child(
            div()
                .id(("mention-status", ix))
                .w(ui_px(64.))
                .flex_none()
                .text_size(scale::text(11.))
                .text_color(color(token))
                .aria_label(word)
                .child(word)
                .test_support(),
        )
}

/// "No agents yet. Add one in Settings." The link opens Settings › Agents.
fn render_no_agents(cx: &mut Context<Composer>) -> impl IntoElement {
    div()
        .id("mention-empty")
        .flex()
        .gap(ui_px(4.))
        .px(ui_px(12.))
        .py(scale::row(10.))
        .text_color(color("text-secondary"))
        .aria_label(NO_AGENTS)
        .child("No agents yet.")
        .child(
            div()
                .id("mention-settings")
                .text_color(color("accent-blue"))
                .cursor_pointer()
                .role(Role::Link)
                .child("Add one in Settings.")
                .on_click(cx.listener(|_, _, _, cx| cx.emit(ComposerEvent::OpenAgentSettings)))
                .test_support(),
        )
        .test_support()
}

fn hint(keys: &'static str, what: &'static str) -> impl IntoElement {
    div()
        .flex()
        .gap(ui_px(4.))
        .child(
            div()
                .font_family(code_family())
                .text_color(color("text-secondary"))
                .child(keys),
        )
        .child(what)
}

#[cfg(test)]
mod tests {
    use super::{MentionAgent, matching, mention_at, menu_height, opens_below};
    use crate::model::ThreadStatus;
    use crew_core::agent::Runtime;

    fn agent(name: &str) -> MentionAgent {
        MentionAgent {
            name: name.into(),
            role: String::new(),
            chip: "claude · haiku".into(),
            runtime: Runtime::ClaudeCode,
            status: ThreadStatus::Idle,
        }
    }

    #[test]
    fn a_mention_is_an_at_sign_then_a_name_up_to_the_cursor() {
        assert_eq!(mention_at("@", 1), Some((0, "")));
        assert_eq!(mention_at("hi @Fo", 6), Some((3, "Fo")));
        assert_eq!(mention_at("hi @Fo", 5), Some((3, "F")));
        assert_eq!(mention_at("(@x", 3), Some((1, "x")));
        assert_eq!(mention_at("hi @Forge then", 14), None, "past a space");
        assert_eq!(mention_at("mail a@b", 8), None, "inside a word");
        assert_eq!(mention_at("no at", 5), None);
        assert_eq!(mention_at("é @", 4), Some((3, "")));
        assert_eq!(mention_at("é@", 3), None);
    }

    #[test]
    fn the_menu_opens_below_without_room_above() {
        let tall = menu_height(4, true);
        assert!(tall > 4. * 44.);
        assert!(opens_below(120., tall), "near the top of the window");
        assert!(!opens_below(800., tall), "a reply composer at the bottom");
        assert!(menu_height(0, false) < menu_height(0, true));
    }

    #[test]
    fn matching_puts_prefixes_first() {
        let agents = [agent("Prism"), agent("Scout"), agent("Sentinel")];
        let names = |q: &str| -> Vec<String> {
            matching(&agents, q)
                .iter()
                .map(|a| a.name.clone())
                .collect()
        };
        assert_eq!(names(""), ["Prism", "Scout", "Sentinel"]);
        assert_eq!(names("s"), ["Scout", "Sentinel", "Prism"]);
        assert_eq!(names("SEN"), ["Sentinel"]);
        assert!(names("zzz").is_empty());
    }
}
