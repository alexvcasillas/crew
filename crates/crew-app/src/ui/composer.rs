//! Where you write in a thread, or the first message of a new one. Typing
//! `@` opens the mention menu, `/` the skills menu.

use crew_core::agent::Runtime;
use crew_core::ids::ProjectId;
use crew_core::mention::parse_mentions;
use crew_core::slug::slugify;
use crew_proto::{ModelInfo, SkillInfo, SkillRef};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InlineToken, InputEvent, Textarea, TextareaState};
use gpui_kit::component::{Disableable as _, Icon, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::common::{monogram, provider_icon};
use super::mention_menu::{MentionAgent, matching, mention_at};
use super::runs_with::{
    RunsMenu, RunsWith, Target, model_name, resolve, skill_chip, wrong_runtime_note,
};
use super::slash_menu::{SkillEntry, entries, first_selected, flat, menu_rows, slash_at};
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
const REPLY_PLACEHOLDER: &str = "Reply in thread";
const FIRST_PLACEHOLDER: &str = "Describe the task. The first line becomes the title.";
const EMPTY_PLACEHOLDER: &str = "Start a thread. Type @ to bring in an agent, / for a skill.";

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

/// The byte ranges of the chips in the text.
fn token_ranges(
    tokens: &[gpui_kit::component::input::InlineTokenSpan],
) -> Vec<std::ops::Range<usize>> {
    tokens.iter().map(|t| t.range()).collect()
}

/// `text` without the chips at `ranges`, and the space after each.
pub fn body_of(text: &str, ranges: &[std::ops::Range<usize>]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for range in ranges {
        out.push_str(&text[at..range.start]);
        at = range.end;
        if text[at..].starts_with(' ') {
            at += 1;
        }
    }
    out.push_str(&text[at..]);
    out
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

/// A message to send.
#[derive(Debug, Clone, PartialEq)]
pub struct Outgoing {
    pub text: String,
    /// The skill it runs, and with which agent.
    pub skill: Option<SkillRef>,
    /// The model the skill runs on for this message alone.
    pub model_override: Option<String>,
    /// The skill's key, for the menu's Recent section.
    pub skill_key: Option<String>,
    /// The skill and choices as picked, to put back if the send fails.
    pub picked: Option<Box<Picked>>,
}

/// A skill picked in the composer, with who and what runs it.
#[derive(Debug, Clone, PartialEq)]
pub struct Picked {
    pub skill: SkillEntry,
    pub runtime_choice: Option<Runtime>,
    pub picked_agent: Option<String>,
    pub model_choice: Option<String>,
}

/// What you asked the composer to send.
pub enum ComposerEvent {
    Send(Outgoing),
    /// The `/` menu opened: the project's skills are wanted.
    SkillsWanted,
    /// The `/` menu's "Manage skills" was clicked.
    ManageSkills,
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

/// An open `/` menu.
#[derive(Debug, Clone, PartialEq)]
struct Slash {
    /// Byte offset of the `/`.
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
    /// An open `/` menu.
    slash: Option<Slash>,
    /// The `/` whose menu you closed.
    slash_dismissed: Option<usize>,
    pub(super) slash_scroll: ScrollHandle,
    /// The project's skills, as the menu lists them.
    pub(super) skills: Vec<SkillEntry>,
    /// The project and version of the list `skills` came from. `None`
    /// until crewd answered.
    skills_from: Option<(ProjectId, u64)>,
    /// Keys of the skills you used lately, newest first.
    pub(super) recent_skills: Vec<String>,
    /// The skill this message runs.
    skill: Option<SkillEntry>,
    /// The runtime picked with the switch, for a skill both have.
    runtime_choice: Option<Runtime>,
    /// The agent picked to run the skill, by name.
    picked_agent: Option<String>,
    /// The model picked for this message.
    pub(super) model_choice: Option<String>,
    pub(super) runs_menu: Option<RunsMenu>,
    /// Each runtime's models.
    pub(super) models: Vec<(Runtime, Vec<ModelInfo>)>,
    /// The agent a reply goes to.
    reply_to: Option<String>,
    /// The skill whose chip was just deleted, for undo to bring back.
    dropped: Option<Picked>,
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
            slash: None,
            slash_dismissed: None,
            slash_scroll: ScrollHandle::new(),
            skills: Vec::new(),
            skills_from: None,
            recent_skills: Vec::new(),
            skill: None,
            runtime_choice: None,
            picked_agent: None,
            model_choice: None,
            runs_menu: None,
            models: Vec::new(),
            reply_to: None,
            dropped: None,
            top: Default::default(),
            _input_events: events,
        }
    }

    /// What you wrote, without the skill's chip.
    pub fn text(&self, cx: &App) -> String {
        let input = self.input.read(cx);
        body_of(&input.value(), &token_ranges(input.tokens()))
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
        self.mention.is_some() || self.slash_menu_shown()
    }

    /// Whether the `/` menu is drawn: open, with the skills known.
    pub fn slash_menu_shown(&self) -> bool {
        self.slash.is_some() && self.skills_from.is_some()
    }

    /// The skill this message runs, by name.
    pub fn skill(&self) -> Option<&str> {
        self.skill.as_ref().map(|s| s.name.as_str())
    }

    /// The skill picked, with who and what runs it, to keep with a draft.
    pub fn picked(&self) -> Option<Picked> {
        self.skill.as_ref().map(|skill| Picked {
            skill: skill.clone(),
            runtime_choice: self.runtime_choice.clone(),
            picked_agent: self.picked_agent.clone(),
            model_choice: self.model_choice.clone(),
        })
    }

    /// The project and version of the skills list the menu has.
    pub fn skills_from(&self) -> Option<(ProjectId, u64)> {
        self.skills_from
    }

    /// The project's skills, version `rev`, from crewd's list; `None`
    /// while unknown.
    pub fn set_skills(
        &mut self,
        from: Option<(ProjectId, u64)>,
        list: &[SkillInfo],
        cx: &mut Context<Self>,
    ) {
        if self.skills_from == from {
            return;
        }
        self.skills_from = from;
        self.skills = entries(list);
        if let Some(slash) = &mut self.slash {
            let n = flat(&menu_rows(&self.skills, &slash.query, &self.recent_skills)).len();
            if slash.selected >= n {
                slash.selected = 0;
            }
        }
        cx.notify();
    }

    /// The skills you used lately, newest first, and each runtime's
    /// models.
    pub fn set_skill_context(
        &mut self,
        recent: Vec<String>,
        models: Vec<(Runtime, Vec<ModelInfo>)>,
        reply_to: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.recent_skills != recent || self.models != models || self.reply_to != reply_to {
            self.recent_skills = recent;
            self.models = models;
            self.reply_to = reply_to;
            cx.notify();
        }
    }

    /// Who runs the skill picked, if one is.
    fn runs_with(&self, cx: &App) -> Option<(&SkillEntry, RunsWith<'_>)> {
        let skill = self.skill.as_ref()?;
        let text = self.text(cx);
        let runs = resolve(
            skill,
            &self.agents,
            &text,
            self.runtime_choice.as_ref(),
            self.picked_agent.as_deref(),
            self.reply_to.as_deref(),
        );
        Some((skill, runs))
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
        // Typed at the end, so the skill's chip stays.
        self.input.update(cx, |input, cx| {
            let text = input.value();
            let spaced = text.chars().next_back().is_none_or(char::is_whitespace);
            let end = text.len();
            input.set_selected_range(end..end, cx);
            input.replace(
                format!("{}@{name} ", if spaced { "" } else { " " }),
                window,
                cx,
            );
        });
        self.mention = None;
        self.dismissed = None;
        self.slash = None;
        cx.notify();
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
    /// draft. A skill picked is dropped.
    pub fn set_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            input.set_value(text.to_string(), window, cx);
            input.set_selected_range(text.len()..text.len(), cx);
        });
        self.mention = None;
        self.dismissed = None;
        self.slash = None;
        self.slash_dismissed = None;
        self.clear_skill();
        cx.notify();
    }

    fn clear_skill(&mut self) {
        self.skill = None;
        self.runtime_choice = None;
        self.picked_agent = None;
        self.model_choice = None;
        self.runs_menu = None;
    }

    fn text_changed(&mut self, cx: &mut Context<Self>) {
        let input = self.input.read(cx);
        // Deleting the chip drops the skill; undo bringing the chip back
        // brings the skill back, with who and what was picked for it.
        let chip = input.tokens().first().map(|t| t.token().id().to_string());
        match (&self.skill, chip) {
            (Some(_), None) => {
                self.dropped = self.picked();
                self.clear_skill();
                cx.notify();
            }
            (None, Some(key)) => {
                let picked = match self.dropped.take() {
                    Some(p) if p.skill.key() == key => Some(p),
                    _ => self
                        .skills
                        .iter()
                        .find(|e| e.key() == key)
                        .map(|skill| Picked {
                            skill: skill.clone(),
                            runtime_choice: None,
                            picked_agent: None,
                            model_choice: None,
                        }),
                };
                if let Some(p) = picked {
                    self.skill = Some(p.skill);
                    self.runtime_choice = p.runtime_choice;
                    self.picked_agent = p.picked_agent;
                    self.model_choice = p.model_choice;
                    self.slash = None;
                    cx.notify();
                }
            }
            _ => {}
        }
        let input = self.input.read(cx);
        let text = input.value();
        let found = mention_at(&text, input.cursor()).map(|(at, q)| (at, q.to_string()));
        if self.dismissed.is_some() && found.as_ref().map(|f| f.0) != self.dismissed {
            self.dismissed = None;
        }
        let slash_found = (self.skill.is_none() && found.is_none())
            .then(|| slash_at(&text, input.cursor()).map(|(at, q)| (at, q.to_string())))
            .flatten();
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
        if self.slash_dismissed.is_some()
            && slash_found.as_ref().map(|f| f.0) != self.slash_dismissed
        {
            self.slash_dismissed = None;
        }
        let slash = match slash_found {
            Some((at, _)) if self.slash_dismissed == Some(at) => None,
            Some((at, query)) => {
                let selected = match &self.slash {
                    Some(s) if s.at == at && s.query == query => s.selected,
                    _ => {
                        let sections = menu_rows(&self.skills, &query, &self.recent_skills);
                        first_selected(&self.skills, &flat(&sections), &self.recent_skills)
                    }
                };
                Some(Slash {
                    at,
                    query,
                    selected,
                })
            }
            None => None,
        };
        if slash != self.slash {
            let opened = self.slash.is_none() && slash.is_some();
            self.slash = slash;
            if opened {
                cx.emit(ComposerEvent::SkillsWanted);
            }
            self.scroll_to_selected();
            cx.notify();
        }
    }

    fn enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(slash) = &self.slash {
            let picked = self.skills_from.and_then(|_| {
                let sections = menu_rows(&self.skills, &slash.query, &self.recent_skills);
                flat(&sections).get(slash.selected).map(|r| r.entry)
            });
            match picked {
                Some(entry) => self.pick_skill(entry, window, cx),
                None => self.close_menu(cx),
            }
            return;
        }
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

    /// Sends what you wrote and empties the field. With a skill, only
    /// once an agent on its runtime can run it.
    pub fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text(cx).trim().to_string();
        if self.busy {
            return;
        }
        let outgoing = match self.runs_with(cx) {
            None if text.is_empty() => return,
            None => Outgoing {
                text,
                skill: None,
                model_override: None,
                skill_key: None,
                picked: None,
            },
            Some((skill, runs)) => {
                let Some(agent) = runs.agent() else {
                    return;
                };
                let models = self.models_of_runtime(&runs.runtime);
                let default = model_name(&runs.runtime, &agent.model, &models);
                let model_override = self
                    .model_choice
                    .clone()
                    .filter(|m| model_name(&runs.runtime, m, &models) != default);
                Outgoing {
                    text,
                    skill: Some(SkillRef {
                        runtime: runs.runtime.clone(),
                        name: skill.name.clone(),
                        path: skill.path(&runs.runtime).unwrap_or_default().to_string(),
                        agent: Some(agent.id),
                    }),
                    model_override,
                    skill_key: Some(skill.key()),
                    picked: self.picked().map(Box::new),
                }
            }
        };
        cx.emit(ComposerEvent::Send(outgoing));
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.mention = None;
        self.dismissed = None;
        self.slash = None;
        self.slash_dismissed = None;
        self.clear_skill();
        cx.notify();
    }

    fn models_of_runtime(&self, runtime: &Runtime) -> Vec<ModelInfo> {
        self.models
            .iter()
            .find(|(r, _)| r == runtime)
            .map(|(_, m)| m.clone())
            .unwrap_or_default()
    }

    /// Takes the skill at `entry` for this message, dropping the `/` being
    /// typed.
    pub(super) fn pick_skill(&mut self, entry: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(slash) = self.slash.take() else {
            return;
        };
        let Some(skill) = self.skills.get(entry).cloned() else {
            return;
        };
        let end = self.input.read(cx).cursor();
        self.clear_skill();
        if self.insert_chip(&skill, slash.at..end, window, cx) {
            self.skill = Some(skill);
        }
        cx.notify();
    }

    /// Puts `skill`'s chip in place of `range`, then a space, the cursor
    /// after it. Whether it went in.
    fn insert_chip(
        &mut self,
        skill: &SkillEntry,
        range: std::ops::Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.input.update(cx, |input, cx| {
            let chip = format!("/{}", skill.name);
            let token = InlineToken::new(skill.key(), chip.clone());
            let start = range.start;
            if input
                .replace_range_with_token(range, token, window, cx)
                .is_err()
            {
                return false;
            }
            let after = start + chip.len();
            input.set_selected_range(after..after, cx);
            if !input.value()[after..].starts_with(' ') {
                input.replace(" ", window, cx);
            } else {
                input.set_selected_range(after + 1..after + 1, cx);
            }
            input.focus(window, cx);
            true
        })
    }

    /// Puts back a skill picked for a message that failed to send, its
    /// chip at the start of the text.
    pub fn restore_pick(&mut self, picked: Picked, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_skill();
        if !self.insert_chip(&picked.skill, 0..0, window, cx) {
            return;
        }
        self.skill = Some(picked.skill);
        self.runtime_choice = picked.runtime_choice;
        self.picked_agent = picked.picked_agent;
        self.model_choice = picked.model_choice;
        cx.notify();
    }

    pub(super) fn choose_runtime(&mut self, runtime: Runtime, cx: &mut Context<Self>) {
        if self.runtime_choice.as_ref() != Some(&runtime) {
            self.runtime_choice = Some(runtime);
            self.picked_agent = None;
            self.model_choice = None;
            self.runs_menu = None;
            cx.notify();
        }
    }

    pub(super) fn pick_agent(&mut self, name: String, cx: &mut Context<Self>) {
        self.picked_agent = Some(name);
        self.runs_menu = None;
        cx.notify();
    }

    pub(super) fn pick_model(&mut self, model: String, cx: &mut Context<Self>) {
        self.model_choice = Some(model);
        self.runs_menu = None;
        cx.notify();
    }

    pub(super) fn toggle_runs_menu(&mut self, menu: RunsMenu, cx: &mut Context<Self>) {
        self.runs_menu = if self.runs_menu == Some(menu) {
            None
        } else {
            Some(menu)
        };
        cx.notify();
    }

    pub(super) fn close_runs_menu(&mut self, cx: &mut Context<Self>) {
        if self.runs_menu.take().is_some() {
            cx.notify();
        }
    }

    /// Keeps the highlighted skill in view.
    fn scroll_to_selected(&self) {
        let Some(slash) = &self.slash else {
            return;
        };
        let sections = menu_rows(&self.skills, &slash.query, &self.recent_skills);
        let mut item = 0;
        let mut row = 0;
        for (_, rows) in &sections {
            item += 1;
            if slash.selected < row + rows.len() {
                item += slash.selected - row;
                self.slash_scroll.scroll_to_item(item);
                return;
            }
            row += rows.len();
            item += rows.len();
        }
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
        if let Some(slash) = self.slash.take() {
            self.slash_dismissed = Some(slash.at);
            cx.notify();
        }
    }

    fn move_selection(&mut self, by: isize, cx: &mut Context<Self>) {
        if let Some(slash) = &mut self.slash {
            let n = flat(&menu_rows(&self.skills, &slash.query, &self.recent_skills)).len();
            if n > 0 {
                slash.selected = (slash.selected as isize + by).rem_euclid(n as isize) as usize;
                self.scroll_to_selected();
                cx.notify();
            }
            return;
        }
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

    /// Types `/` where the cursor is, opening the skills menu. With a
    /// skill already picked, it only puts the cursor in the text.
    fn start_slash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.skill.is_some() {
            self.focus(window, cx);
            return;
        }
        self.input.update(cx, |input, cx| {
            let text = input.value();
            let cursor = input.cursor();
            let spaced = text[..cursor]
                .chars()
                .next_back()
                .is_none_or(char::is_whitespace);
            input.replace(if spaced { "/" } else { " /" }, window, cx);
            input.focus(window, cx);
        });
        self.text_changed(cx);
    }
}

impl Render for Composer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let menu = match (&self.mention, &self.slash) {
            (Some(m), _) => Some(
                self.render_mention_menu(&m.query, m.selected, cx)
                    .into_any_element(),
            ),
            (None, Some(s)) if self.skills_from.is_some() => Some(
                self.render_slash_menu(&s.query, s.selected, cx)
                    .into_any_element(),
            ),
            _ => None,
        };
        let context = match (&menu, self.mode) {
            (Some(_), _) => MENU_CONTEXT,
            (None, ComposerMode::NewThread) => NEW_THREAD_CONTEXT,
            (None, _) => COMPOSER_CONTEXT,
        };
        let new_thread = self.mode == ComposerMode::NewThread;
        // A skill adds its chip, the note when the mention is on the
        // other runtime, and the "Runs with" row: in place of a reply's
        // footer, above a new thread's.
        let mut chip_runtime = None;
        let mut note = None;
        let mut runs_row = None;
        if let Some((skill, runs)) = self.runs_with(cx) {
            chip_runtime = Some(runs.runtime.clone());
            if let Target::WrongRuntime { mentioned, .. } = runs.target {
                note = Some(self.render_runtime_note(wrong_runtime_note(
                    mentioned,
                    &skill.name,
                    &runs.runtime,
                )));
            }
            let send = self.mode == ComposerMode::Reply;
            let skill = skill.clone();
            let runs = runs.clone();
            runs_row = Some(
                self.render_runs_with(&skill, &runs, send, cx)
                    .into_any_element(),
            );
        }
        let footer = match self.mode {
            ComposerMode::Reply if runs_row.is_some() => None,
            ComposerMode::Reply => Some(self.render_reply_footer(cx).into_any_element()),
            ComposerMode::NewThread => Some(self.render_new_thread_footer(cx).into_any_element()),
            ComposerMode::Empty => Some(self.render_empty_footer(cx).into_any_element()),
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
                    .child({
                        // The skill sits inline, as a chip, before the text.
                        let runtime = chip_runtime.unwrap_or(Runtime::ClaudeCode);
                        Textarea::new(&self.input)
                            .appearance(false)
                            .token(move |token, _, _| {
                                skill_chip(
                                    "composer-skill",
                                    &runtime,
                                    token.token().text().trim_start_matches('/'),
                                )
                                .h(token.line_height())
                                .test_support()
                            })
                    })
                    .test_support(),
            )
            .children(note)
            .children(runs_row)
            .children(footer)
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

    /// The @ and / buttons.
    fn render_at_slash(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.slash_menu_shown();
        div().flex().flex_none().child(self.render_at(cx)).child(
            Button::new("composer-slash")
                .ghost()
                .small()
                .w(ui_px(FOOTER_BUTTON))
                .label("/")
                .when(open, |b| {
                    b.bg(color("surface-300")).text_color(color("text-primary"))
                })
                .tooltip("Insert a skill")
                .on_click(cx.listener(|this, _, window, cx| {
                    this.start_slash(window, cx);
                })),
        )
    }

    fn render_reply_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap(ui_px(8.))
            .px(ui_px(8.))
            .pb(scale::row(8.))
            .child(self.render_at_slash(cx))
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
            .child(self.render_at_slash(cx))
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
            .child(self.render_at_slash(cx))
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
    use super::{body_of, branch_preview, mentioned, model_short};
    use crate::model::ThreadStatus;
    use crate::ui::mention_menu::MentionAgent;
    use crew_core::agent::Runtime;

    fn agent(name: &str) -> MentionAgent {
        MentionAgent {
            id: crew_core::ids::AgentId::new(),
            name: name.into(),
            role: String::new(),
            chip: "claude · opus".into(),
            model: "opus".into(),
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

    #[test]
    fn the_body_leaves_the_skill_chip_out() {
        assert_eq!(
            body_of("/unslop the README", std::slice::from_ref(&(0..7))),
            "the README"
        );
        assert_eq!(body_of("/unslop ", std::slice::from_ref(&(0..7))), "");
        assert_eq!(
            body_of("fix /lint now", std::slice::from_ref(&(4..9))),
            "fix now"
        );
        assert_eq!(body_of("plain", &[]), "plain");
    }
}
