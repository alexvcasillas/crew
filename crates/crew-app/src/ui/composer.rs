//! Where you write in a thread, or the first message of a new one. Typing
//! `@` opens the mention menu.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::{Disableable as _, Icon, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::mention_menu::{MentionAgent, matching, mention_at};
use crate::theme::color;

actions!(
    composer,
    [
        /// Highlight the agent above in the mention menu.
        MentionUp,
        /// Highlight the agent below in the mention menu.
        MentionDown,
        /// Close the mention menu.
        MentionClose,
    ]
);

/// While the menu is open, these keys drive it instead of the text.
const MENU_CONTEXT: &str = "Mentioning";
const COMPOSER_CONTEXT: &str = "Composer";
/// A paperclip outline, drawn for Crew.
const PAPERCLIP: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M16 8v8.5a4 4 0 0 1-8 0V6.5a2.5 2.5 0 0 1 5 0V16a1 1 0 0 1-2 0V8"/></svg>"#;
const REPLY_PLACEHOLDER: &str = "Reply in thread. Type @ to bring in an agent.";
const FIRST_PLACEHOLDER: &str = "Describe the task. The first line becomes the title.";

/// Bindings for the mention menu. They beat the text field's own.
pub fn bindings() -> Vec<KeyBinding> {
    let menu = Some("Mentioning > Input");
    vec![
        KeyBinding::new("up", MentionUp, menu),
        KeyBinding::new("down", MentionDown, menu),
        KeyBinding::new("escape", MentionClose, menu),
    ]
}

/// What you asked the composer to send.
pub enum ComposerEvent {
    Send(String),
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
    /// Writing the first message of a new thread.
    first_message: bool,
    mention: Option<Mention>,
    /// The `@` whose menu you closed: it stays closed until you type
    /// another.
    dismissed: Option<usize>,
    busy: bool,
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
            InputEvent::PressEnter { shift: false, .. } => this.enter(window, cx),
            _ => {}
        });
        Self {
            input,
            agents: Vec::new(),
            project: String::new(),
            hint: None,
            first_message: false,
            mention: None,
            dismissed: None,
            busy: false,
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

    /// Switches between replying and writing a new thread's first message.
    pub fn set_first_message(&mut self, first: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.first_message != first {
            self.first_message = first;
            let placeholder = if first {
                FIRST_PLACEHOLDER
            } else {
                REPLY_PLACEHOLDER
            };
            self.input.update(cx, |input, cx| {
                input.set_placeholder(placeholder, window, cx)
            });
            cx.notify();
        }
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
        let context = if menu.is_some() {
            MENU_CONTEXT
        } else {
            COMPOSER_CONTEXT
        };
        div()
            .id("composer")
            .relative()
            .flex()
            .flex_col()
            .border_1()
            .border_color(color("border-200"))
            .rounded(px(8.))
            .bg(color("bg-100"))
            .key_context(context)
            .on_action(cx.listener(|this, _: &MentionUp, _, cx| this.move_selection(-1, cx)))
            .on_action(cx.listener(|this, _: &MentionDown, _, cx| this.move_selection(1, cx)))
            .on_action(cx.listener(|this, _: &MentionClose, _, cx| this.close_menu(cx)))
            .child(
                div()
                    .id("composer-input")
                    .px(px(4.))
                    .pt(px(4.))
                    .child(Textarea::new(&self.input).appearance(false))
                    .test_support(),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(8.))
                    .pb(px(8.))
                    .child(
                        Button::new("composer-at")
                            .ghost()
                            .small()
                            .label("@")
                            .tooltip("Mention an agent")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.start_mention(window, cx);
                            })),
                    )
                    .child(
                        Button::new("composer-attach")
                            .ghost()
                            .small()
                            .icon(Icon::default().data(PAPERCLIP))
                            .disabled(true)
                            .tooltip("Attachments are coming soon."),
                    )
                    .child(
                        div()
                            .id("composer-hint")
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
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
                    ),
            )
            .children(menu.map(|menu| deferred(menu).with_priority(1)))
            .test_support()
    }
}
