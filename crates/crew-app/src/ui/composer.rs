//! Where you write in a thread, or the first message of a new one. Typing
//! `@` opens the mention menu.

use crew_core::mention::parse_mentions;
use crew_core::slug::slugify;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::{Disableable as _, Icon, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::{monogram, provider_icon};
use super::mention_menu::{MentionAgent, matching, mention_at};
use super::workspace::thread_title;
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px};

actions!(
    composer,
    [
        /// Highlight the agent above in the mention menu.
        MentionUp,
        /// Highlight the agent below in the mention menu.
        MentionDown,
        /// Close the mention menu.
        MentionClose,
        /// Close the new thread composer.
        CancelNewThread,
        /// Start the thread written in the new thread composer.
        StartThread,
    ]
);

/// While the menu is open, these keys drive it instead of the text.
const MENU_CONTEXT: &str = "Mentioning";
const COMPOSER_CONTEXT: &str = "Composer";
/// The new thread composer, where esc cancels.
const NEW_THREAD_CONTEXT: &str = "NewThread";
/// A paperclip outline, drawn for Crew.
const PAPERCLIP: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M16 8v8.5a4 4 0 0 1-8 0V6.5a2.5 2.5 0 0 1 5 0V16a1 1 0 0 1-2 0V8"/></svg>"#;
/// Width of the @ and attach buttons, so they match.
const FOOTER_BUTTON: f32 = 28.;
const REPLY_PLACEHOLDER: &str = "Reply in thread. Type @ to bring in an agent.";
const FIRST_PLACEHOLDER: &str = "Describe the task. The first line becomes the title.";
const EMPTY_PLACEHOLDER: &str = "Start a thread. Type @ to bring in an agent.";

/// What the composer is for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ComposerMode {
    /// Replying in the open thread.
    #[default]
    Reply,
    /// The first message of a new thread, at the top of the channel.
    NewThread,
    /// The first thread of a channel that has none.
    Empty,
}

impl ComposerMode {
    fn placeholder(self) -> &'static str {
        match self {
            Self::Reply => REPLY_PLACEHOLDER,
            Self::NewThread => FIRST_PLACEHOLDER,
            Self::Empty => EMPTY_PLACEHOLDER,
        }
    }
}

/// The branch a new thread whose first message is `text` would get, by
/// the rules crewd names it with: `crew/fix-the-login`.
pub fn branch_preview(text: &str) -> String {
    format!("crew/{}", slugify(&thread_title(text), &|_| false))
}

/// The agents `text` mentions, in the order first mentioned.
pub fn mentioned<'a>(agents: &'a [MentionAgent], text: &str) -> Vec<&'a MentionAgent> {
    let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
    let (known, _) = parse_mentions(text, &names);
    known
        .iter()
        .filter_map(|name| agents.iter().find(|a| a.name == *name))
        .collect()
}

/// The model part of a runtime chip: `opus` from `claude · opus`.
pub fn model_short(chip: &str) -> &str {
    chip.split_once(" · ").map_or(chip, |(_, model)| model)
}

/// Bindings for the mention menu. They beat the text field's own.
pub fn bindings() -> Vec<KeyBinding> {
    let menu = Some("Mentioning > Input");
    vec![
        KeyBinding::new("up", MentionUp, menu),
        KeyBinding::new("down", MentionDown, menu),
        KeyBinding::new("escape", MentionClose, menu),
        KeyBinding::new("escape", CancelNewThread, Some("NewThread > Input")),
        // Bound after the app's ⌘↵, so it wins while you write a thread.
        KeyBinding::new("cmd-enter", StartThread, Some("NewThread > Input")),
    ]
}

/// What you asked the composer to send.
pub enum ComposerEvent {
    Send(String),
    /// Cancel or esc in the new thread composer.
    Cancel,
    /// The mention menu's "Add one in Settings." was clicked.
    OpenAgentSettings,
}

/// An open mention menu.
#[derive(Debug, Clone, PartialEq)]
struct Mention {
    /// Byte offset of the `@`.
    at: usize,
    query: String,
    selected: usize,
}

pub struct Composer {
    input: Entity<TextareaState>,
    /// Agents you can mention.
    pub(super) agents: Vec<MentionAgent>,
    /// The project's name, for the menu's caption.
    pub(super) project: String,
    /// Shown in the footer, such as who is working.
    hint: Option<String>,
    mode: ComposerMode,
    /// The project's main branch, which a new worktree starts from.
    base: String,
    mention: Option<Mention>,
    /// The `@` whose menu you closed: it stays closed until you type
    /// another.
    dismissed: Option<usize>,
    busy: bool,
    /// Where the composer's top was last drawn, in window px, so the
    /// mention menu knows if it fits above.
    pub(super) top: std::rc::Rc<std::cell::Cell<Option<f32>>>,
    _input_events: Subscription,
}

impl EventEmitter<ComposerEvent> for Composer {}

impl Composer {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 8)
                .submit_on_enter(true)
                .placeholder(REPLY_PLACEHOLDER)
        });
        let events = cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
            InputEvent::Change => this.text_changed(cx),
            InputEvent::PressEnter {
                shift: false,
                secondary: false,
            } => this.enter(window, cx),
            _ => {}
        });
        Self {
            input,
            agents: Vec::new(),
            project: String::new(),
            hint: None,
            mode: ComposerMode::Reply,
            base: String::new(),
            mention: None,
            dismissed: None,
            busy: false,
            top: Default::default(),
            _input_events: events,
        }
    }

    pub fn text(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    /// Waiting for a new thread: Send and ↵ do nothing meanwhile.
    pub fn busy(&self) -> bool {
        self.busy
    }

    pub fn set_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        if self.busy != busy {
            self.busy = busy;
            cx.notify();
        }
    }

    pub fn menu_open(&self) -> bool {
        self.mention.is_some()
    }

    /// Who you can mention, in which project, and the footer hint.
    pub fn set_context(
        &mut self,
        agents: Vec<MentionAgent>,
        project: String,
        hint: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.agents != agents || self.project != project || self.hint != hint {
            self.agents = agents;
            self.project = project;
            self.hint = hint;
            cx.notify();
        }
    }

    pub fn mode(&self) -> ComposerMode {
        self.mode
    }

    /// Switches between replying and writing a new thread's first message.
    pub fn set_mode(&mut self, mode: ComposerMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode != mode {
            self.mode = mode;
            self.input.update(cx, |input, cx| {
                input.set_placeholder(mode.placeholder(), window, cx)
            });
            cx.notify();
        }
    }

    /// The branch a new worktree starts from.
    pub fn set_base(&mut self, base: String, cx: &mut Context<Self>) {
        if self.base != base {
            self.base = base;
            cx.notify();
        }
    }

    /// Adds `@name ` at the end of the text and puts the cursor after it.
    pub fn add_mention(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text(cx);
        let spaced = text.chars().next_back().is_none_or(char::is_whitespace);
        let text = format!("{text}{}@{name} ", if spaced { "" } else { " " });
        self.set_text(&text, window, cx);
        self.focus(window, cx);
    }

    /// Whether the text field has focus.
    pub fn focused(&self, window: &Window, cx: &App) -> bool {
        self.input.read(cx).focus_handle(cx).is_focused(window)
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    /// Replaces the text, cursor at its end, as when you come back to a
    /// draft.
    pub fn set_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            input.set_value(text.to_string(), window, cx);
            input.set_selected_range(text.len()..text.len(), cx);
        });
        self.mention = None;
        self.dismissed = None;
        cx.notify();
    }

    fn text_changed(&mut self, cx: &mut Context<Self>) {
        let input = self.input.read(cx);
        let text = input.value();
        let found = mention_at(&text, input.cursor()).map(|(at, q)| (at, q.to_string()));
        if self.dismissed.is_some() && found.as_ref().map(|f| f.0) != self.dismissed {
            self.dismissed = None;
        }
        let mention = match found {
            Some((at, _)) if self.dismissed == Some(at) => None,
            Some((at, query)) => {
                let selected = match &self.mention {
                    Some(m) if m.at == at && m.query == query => m.selected,
                    _ => 0,
                };
                Some(Mention {
                    at,
                    query,
                    selected,
                })
            }
            None => None,
        };
        if mention != self.mention {
            self.mention = mention;
            cx.notify();
        }
    }

    fn enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = self.mention.as_ref().and_then(|m| {
            matching(&self.agents, &m.query)
                .get(m.selected)
                .map(|a| a.name.clone())
        });
        match picked {
            Some(name) => self.insert_mention(&name, window, cx),
            None if self.mention.is_some() => self.close_menu(cx),
            None => self.send(window, cx),
        }
    }

    /// Sends what you wrote and empties the field.
    pub fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text(cx);
        let text = text.trim();
        if text.is_empty() || self.busy {
            return;
        }
        cx.emit(ComposerEvent::Send(text.to_string()));
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.mention = None;
        self.dismissed = None;
        cx.notify();
    }

    /// Replaces the `@` being typed with `@name `.
    pub(super) fn insert_mention(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mention) = self.mention.take() else {
            return;
        };
        self.input.update(cx, |input, cx| {
            let end = input.cursor();
            input.set_selected_range(mention.at..end, cx);
            input.replace(format!("@{name} "), window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.mode == ComposerMode::NewThread {
            cx.emit(ComposerEvent::Cancel);
        } else {
            cx.propagate();
        }
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        if let Some(mention) = self.mention.take() {
            self.dismissed = Some(mention.at);
            cx.notify();
        }
    }

    fn move_selection(&mut self, by: isize, cx: &mut Context<Self>) {
        let Some(mention) = &mut self.mention else {
            return;
        };
        let n = matching(&self.agents, &mention.query).len();
        if n == 0 {
            return;
        }
        mention.selected = (mention.selected as isize + by).rem_euclid(n as isize) as usize;
        cx.notify();
    }

    /// Types `@` where the cursor is, opening the menu.
    fn start_mention(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            let text = input.value();
            let cursor = input.cursor();
            let spaced = text[..cursor]
                .chars()
                .next_back()
                .is_none_or(char::is_whitespace);
            input.replace(if spaced { "@" } else { " @" }, window, cx);
            input.focus(window, cx);
        });
        self.text_changed(cx);
    }
}

impl Render for Composer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let menu = self
            .mention
            .as_ref()
            .map(|m| self.render_mention_menu(&m.query, m.selected, cx));
        let context = match (&menu, self.mode) {
            (Some(_), _) => MENU_CONTEXT,
            (None, ComposerMode::NewThread) => NEW_THREAD_CONTEXT,
            (None, _) => COMPOSER_CONTEXT,
        };
        let new_thread = self.mode == ComposerMode::NewThread;
        let footer = match self.mode {
            ComposerMode::Reply => self.render_reply_footer(cx).into_any_element(),
            ComposerMode::NewThread => self.render_new_thread_footer(cx).into_any_element(),
            ComposerMode::Empty => self.render_empty_footer(cx).into_any_element(),
        };
        div()
            .id("composer")
            .relative()
            .flex()
            .flex_col()
            .border_1()
            .border_color(color(if new_thread {
                "accent-blue"
            } else {
                "border-200"
            }))
            .rounded(ui_px(8.))
            .bg(color("bg-100"))
            .key_context(context)
            .on_action(cx.listener(|this, _: &MentionUp, _, cx| this.move_selection(-1, cx)))
            .on_action(cx.listener(|this, _: &MentionDown, _, cx| this.move_selection(1, cx)))
            .on_action(cx.listener(|this, _: &MentionClose, _, cx| this.close_menu(cx)))
            .on_action(cx.listener(|this, _: &CancelNewThread, _, cx| this.cancel(cx)))
            .on_action(cx.listener(|this, _: &StartThread, window, cx| this.send(window, cx)))
            .child({
                let top = self.top.clone();
                canvas(
                    move |bounds, _, _| top.set(Some(bounds.origin.y.as_f32())),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full()
            })
            .when(new_thread, |c| c.child(self.render_with(cx)))
            .child(
                div()
                    .id("composer-input")
                    .map(|area| match self.mode {
                        ComposerMode::Reply => area.px(ui_px(4.)).pt(scale::row(4.)),
                        ComposerMode::NewThread => area.px(ui_px(8.)).pt(scale::row(6.)),
                        ComposerMode::Empty => area
                            .px(ui_px(12.))
                            .pt(scale::row(10.))
                            .text_size(scale::text(15.)),
                    })
                    .child(Textarea::new(&self.input).appearance(false))
                    .test_support(),
            )
            .child(footer)
            .children(menu.map(|menu| deferred(menu).with_priority(1)))
            .test_support()
    }
}

impl Composer {
    /// The @ button.
    fn render_at(&self, cx: &mut Context<Self>) -> impl IntoElement {
        Button::new("composer-at")
            .ghost()
            .small()
            .w(ui_px(FOOTER_BUTTON))
            .label("@")
            .tooltip("Mention an agent")
            .on_click(cx.listener(|this, _, window, cx| {
                this.start_mention(window, cx);
            }))
    }

    fn render_reply_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap(ui_px(8.))
            .px(ui_px(8.))
            .pb(scale::row(8.))
            .child(self.render_at(cx))
            .child(
                // Same size and colour as @. Not disabled, which would
                // fade it; it only says what is coming.
                Button::new("composer-attach")
                    .ghost()
                    .small()
                    .w(ui_px(FOOTER_BUTTON))
                    .icon(Icon::default().data(PAPERCLIP).size(ui_px(14.)))
                    .tooltip("Attachments are coming soon."),
            )
            .child(
                div()
                    .id("composer-hint")
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(scale::text(12.))
                    .text_color(color("text-tertiary"))
                    .when_some(self.hint.clone(), |hint, text| {
                        hint.aria_label(text.clone()).child(text)
                    })
                    .test_support(),
            )
            .child(
                Button::new("composer-send")
                    .primary()
                    .small()
                    .label("Send ↵")
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
            )
    }

    /// "New thread · With" and the agents the text mentions.
    fn render_with(&self, cx: &App) -> impl IntoElement {
        let text = self.text(cx);
        let agents = mentioned(&self.agents, &text);
        let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
        let label = format!("With {}", names.join(", ")).trim_end().to_string();
        let quiet = |text: &'static str| {
            div()
                .text_size(scale::text(12.))
                .text_color(color("text-secondary"))
                .child(text)
        };
        div()
            .id("new-thread-with")
            .flex()
            .flex_wrap()
            .items_center()
            .gap(ui_px(8.))
            .px(ui_px(12.))
            .pt(scale::row(10.))
            .min_h(scale::row(36.))
            .aria_label(label)
            .child(quiet("New thread"))
            .child(div().text_color(color("border-200")).child("·"))
            .child(quiet("With"))
            .children(agents.into_iter().map(|agent| {
                div()
                    .id(SharedString::from(format!(
                        "new-thread-agent-{}",
                        agent.name
                    )))
                    .flex()
                    .items_center()
                    .gap(ui_px(6.))
                    .h(scale::row(26.))
                    .px(ui_px(6.))
                    .border_1()
                    .border_color(color("border-200"))
                    .rounded(ui_px(6.))
                    .bg(color("surface-100"))
                    .text_size(scale::text(12.))
                    .aria_label(agent.name.clone())
                    .child(monogram(&agent.name, 18.).rounded(ui_px(4.)))
                    .child(agent.name.clone())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(ui_px(4.))
                            .font_family(code_family())
                            .text_size(scale::mono(11.))
                            .text_color(color("text-secondary"))
                            .child(provider_icon(&agent.runtime, 12.))
                            .child(model_short(&agent.chip).to_string()),
                    )
                    .test_support()
            }))
            .test_support()
    }

    /// The branch line, Cancel and Start thread.
    fn render_new_thread_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let branch = branch_preview(&self.text(cx));
        let line = format!(
            "Starts in a new worktree on branch {branch} off {}",
            self.base
        );
        let mono = |text: String| {
            div()
                .font_family(code_family())
                .text_size(scale::mono(12.))
                .text_color(color("text-secondary"))
                .child(text)
        };
        div()
            .flex()
            .items_center()
            .gap(ui_px(4.))
            .px(ui_px(8.))
            .pt(scale::row(6.))
            .pb(scale::row(8.))
            .border_t_1()
            .border_color(color("border-100"))
            .child(self.render_at(cx))
            .child(
                div()
                    .id("new-thread-branch")
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .gap(ui_px(4.))
                    .ml(ui_px(6.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(scale::text(12.))
                    .text_color(color("text-tertiary"))
                    .aria_label(line)
                    .child("Starts in a new worktree on branch")
                    .child(mono(branch))
                    .child("off")
                    .child(mono(self.base.clone()))
                    .test_support(),
            )
            .child(
                Button::new("new-thread-cancel")
                    .ghost()
                    .small()
                    .label("Cancel")
                    .child(keys("esc"))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(ComposerEvent::Cancel))),
            )
            .child(
                Button::new("new-thread-start")
                    .primary()
                    .small()
                    .label("Start thread")
                    .child(keys("↵"))
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
            )
    }

    /// Where the first thread starts, and Start thread.
    fn render_empty_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let line = format!("Starts in a new worktree off {}", self.base);
        div()
            .flex()
            .items_center()
            .gap(ui_px(4.))
            .pl(ui_px(10.))
            .pr(ui_px(8.))
            .pt(scale::row(6.))
            .pb(scale::row(8.))
            .child(self.render_at(cx))
            .child(
                div()
                    .id("empty-composer-base")
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .gap(ui_px(4.))
                    .ml(ui_px(4.))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_size(scale::text(12.))
                    .text_color(color("text-tertiary"))
                    .aria_label(line)
                    .child("Starts in a new worktree off")
                    .child(
                        div()
                            .font_family(code_family())
                            .text_size(scale::mono(12.))
                            .child(self.base.clone()),
                    )
                    .test_support(),
            )
            .child(
                Button::new("empty-start")
                    .primary()
                    .small()
                    .label("Start thread")
                    .child(keys("↵"))
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
            )
    }
}

/// A shortcut after a button's label.
fn keys(text: &'static str) -> impl IntoElement {
    div()
        .font_family(code_family())
        .text_size(scale::mono(11.))
        .opacity(0.6)
        .child(text)
}

#[cfg(test)]
mod tests {
    use super::{branch_preview, mentioned, model_short};
    use crate::model::ThreadStatus;
    use crate::ui::mention_menu::MentionAgent;
    use crew_core::agent::Runtime;

    fn agent(name: &str) -> MentionAgent {
        MentionAgent {
            name: name.into(),
            role: String::new(),
            chip: "claude · opus".into(),
            runtime: Runtime::ClaudeCode,
            status: ThreadStatus::Idle,
        }
    }

    #[test]
    fn the_branch_preview_follows_the_first_line() {
        assert_eq!(
            branch_preview("@Forge add a --retry-jitter flag\nmore"),
            "crew/forge-add-a-retry-jitter-flag"
        );
        assert_eq!(branch_preview("\n  Fix the LOGIN!  "), "crew/fix-the-login");
        assert_eq!(branch_preview(""), "crew/untitled");
    }

    #[test]
    fn the_with_row_names_known_mentions_once() {
        let agents = [agent("Forge"), agent("Sentinel")];
        let names = |text: &str| -> Vec<String> {
            mentioned(&agents, text)
                .iter()
                .map(|a| a.name.clone())
                .collect()
        };
        assert_eq!(
            names("@sentinel look, then @Forge, @Forge, @Nobody"),
            ["Sentinel", "Forge"]
        );
        assert!(names("no one").is_empty());
        assert_eq!(model_short("claude · opus"), "opus");
        assert_eq!(model_short("opus"), "opus");
    }
}
