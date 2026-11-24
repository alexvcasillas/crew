//! Draws a parsed Markdown message: paragraphs, headings, lists, code
//! blocks and styled text with `@Agent` mentions and links.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crew_core::ids::{MessageId, RunId};
use gpui_kit::*;

use crate::fonts::{mono_font, ui_font};
use crate::markdown::{Block, Doc, Text, opens, parse, with_mentions};
use crate::theme::color;

/// What a cached parse belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DocKey {
    Message(MessageId),
    /// A run's event, by its number in the run.
    Event(RunId, u64),
    /// An item of the open terminal session, by its seq.
    Terminal(u64),
}

/// Parsed messages, kept since messages never change.
#[derive(Default)]
pub struct DocCache {
    docs: RefCell<HashMap<DocKey, Rc<Doc>>>,
}

impl DocCache {
    /// The parse of `source`, read once per key.
    pub fn get(&self, key: DocKey, source: &str) -> Rc<Doc> {
        self.docs
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| Rc::new(parse(source)))
            .clone()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.docs.borrow().len()
    }
}

/// Draws `doc`. `id` names its elements apart from other docs'.
pub fn render_doc(doc: &Doc, names: &[String], id: &str) -> Div {
    let mut next = 0;
    blocks(&doc.blocks, names, id, &mut next)
}

fn blocks(blocks: &[Block], names: &[String], id: &str, next: &mut usize) -> Div {
    div()
        .flex()
        .flex_col()
        .min_w_0()
        .gap(px(6.))
        .children(blocks.iter().map(|b| block(b, names, id, next)))
}

fn block(block: &Block, names: &[String], id: &str, next: &mut usize) -> AnyElement {
    *next += 1;
    let key = format!("{id}-{next}");
    match block {
        Block::Paragraph(text) => {
            rich_text(text, names, &key, FontWeight::NORMAL).into_any_element()
        }
        Block::Heading(text) => div()
            .text_size(px(14.))
            .child(rich_text(text, names, &key, FontWeight::SEMIBOLD))
            .into_any_element(),
        Block::Code(code) => div()
            .flex()
            .flex_col()
            .px(px(10.))
            .py(px(8.))
            .bg(color("surface-100"))
            .border_1()
            .border_color(color("border-100"))
            .rounded(px(6.))
            .overflow_hidden()
            .font_family(mono_font())
            .text_size(px(12.))
            .line_height(px(18.))
            .text_color(color("text-primary"))
            .children(code.split('\n').map(|line| {
                div()
                    .whitespace_nowrap()
                    .child(if line.is_empty() { " " } else { line }.to_string())
            }))
            .into_any_element(),
        Block::List { start, items } => div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .children(items.iter().enumerate().map(|(ix, item)| {
                let marker = match start {
                    Some(n) => format!("{}.", n + ix as u64),
                    None => "•".to_string(),
                };
                *next += 1;
                div()
                    .id(SharedString::from(format!("{id}-{next}")))
                    .flex()
                    .gap(px(6.))
                    .role(Role::ListItem)
                    .aria_label(plain_text(item))
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(14.))
                            .text_color(color("text-secondary"))
                            .child(marker),
                    )
                    .child(blocks(item, names, id, next).flex_1())
                    .test_support()
            }))
            .into_any_element(),
    }
}

/// Text in its styles: bold, italic, `code` in mono on a background,
/// mentions and links in blue. Clicking a web link opens it.
fn rich_text(text: &Text, names: &[String], id: &str, weight: FontWeight) -> Div {
    let mut links: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    let runs: Vec<TextRun> = with_mentions(text, names)
        .into_iter()
        .map(|span| {
            let style = span.style;
            let mut run_font = font(if style.code { mono_font() } else { ui_font() });
            run_font.weight = if style.bold {
                FontWeight::BOLD
            } else if style.mention && weight.0 < FontWeight::MEDIUM.0 {
                FontWeight::MEDIUM
            } else {
                weight
            };
            if style.italic {
                run_font.style = FontStyle::Italic;
            }
            let token = if style.mention || style.link.is_some() {
                "accent-blue"
            } else {
                "text-primary"
            };
            if let Some(url) = style.link {
                links.push((span.range.clone(), url));
            }
            TextRun {
                len: span.range.len(),
                font: run_font,
                color: color(token),
                background_color: style.code.then(|| color("surface-300")),
                underline: None,
                strikethrough: None,
            }
        })
        .collect();
    #[cfg(test)]
    let markers = bold_markers(&text.text, &runs, id);
    let styled = StyledText::new(text.text.clone()).with_runs(runs);
    let element = if links.is_empty() {
        styled.into_any_element()
    } else {
        let (ranges, urls): (Vec<_>, Vec<_>) = links.into_iter().unzip();
        InteractiveText::new(SharedString::from(id.to_string()), styled)
            .on_click(ranges, move |ix, _, cx| {
                if let Some(url) = urls.get(ix).filter(|u| opens(u)) {
                    cx.open_url(url);
                }
            })
            .into_any_element()
    };
    let out = div().min_w_0().child(element);
    #[cfg(test)]
    let out = out.children(markers);
    out
}

/// Empty elements labelled `bold <text>` for each bold run, so window
/// tests can see what was drawn bold.
#[cfg(test)]
fn bold_markers(text: &str, runs: &[TextRun], id: &str) -> Vec<AnyElement> {
    let mut at = 0;
    let mut markers = Vec::new();
    for run in runs {
        if run.font.weight == FontWeight::BOLD {
            markers.push(
                div()
                    .id(SharedString::from(format!("{id}-bold-{at}")))
                    .aria_label(format!("bold {}", &text[at..at + run.len]))
                    .test_support()
                    .into_any_element(),
            );
        }
        at += run.len;
    }
    markers
}

/// The words of `blocks`, for screen readers.
fn plain_text(blocks: &[Block]) -> String {
    let mut words: Vec<String> = Vec::new();
    for block in blocks {
        match block {
            Block::Paragraph(t) | Block::Heading(t) => words.push(t.text.clone()),
            Block::Code(code) => words.push(code.clone()),
            Block::List { items, .. } => words.extend(items.iter().map(|i| plain_text(i))),
        }
    }
    words.join(" ")
}
