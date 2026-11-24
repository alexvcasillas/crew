//! Draws a parsed Markdown message: paragraphs, headings, lists, code
//! blocks, tables and styled text with `@Agent` mentions and links.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crew_core::ids::{MessageId, RunId};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::markdown::{Align, Block, Doc, Text, opens, parse, with_code_gaps, with_mentions};
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_family, ui_px};

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
        .gap(ui_px(6.))
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
            .text_size(scale::text(14.))
            .child(rich_text(text, names, &key, FontWeight::SEMIBOLD))
            .into_any_element(),
        Block::Code(code) => div()
            .flex()
            .flex_col()
            .px(ui_px(10.))
            .py(scale::row(8.))
            .bg(color("surface-100"))
            .border_1()
            .border_color(color("border-100"))
            .rounded(ui_px(6.))
            .overflow_hidden()
            .font_family(code_family())
            .text_size(scale::mono(12.))
            .line_height(scale::mono_line(18.))
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
            .gap(ui_px(4.))
            .children(items.iter().enumerate().map(|(ix, item)| {
                let marker = match start {
                    Some(n) => format!("{}.", n + ix as u64),
                    None => "•".to_string(),
                };
                *next += 1;
                div()
                    .id(SharedString::from(format!("{id}-{next}")))
                    .flex()
                    .gap(ui_px(6.))
                    .role(Role::ListItem)
                    .aria_label(plain_text(item))
                    .child(
                        div()
                            .flex_none()
                            .min_w(ui_px(14.))
                            .text_color(color("text-secondary"))
                            .child(marker),
                    )
                    .child(blocks(item, names, id, next).flex_1())
                    .test_support()
            }))
            .into_any_element(),
        Block::Table { aligns, head, rows } => table(aligns, head, rows, names, &key),
    }
}

/// A table in a bordered box as wide as the space it has. Columns share
/// that width by how long their longest cell is, and cells wrap.
fn table(
    aligns: &[Align],
    head: &[Text],
    rows: &[Vec<Text>],
    names: &[String],
    id: &str,
) -> AnyElement {
    let weights = column_weights(head, rows);
    let mins = column_min_widths(head, rows);
    let row = |cells: &[Text], ix: usize, header: bool| {
        let row_id = format!("{id}-row-{ix}");
        div()
            .id(SharedString::from(row_id.clone()))
            .flex()
            .w_full()
            .role(Role::Row)
            .aria_label(
                cells
                    .iter()
                    .map(|c| c.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
            )
            .when(header, |d| d.bg(color("surface-100")))
            .when(ix > 0, |d| d.border_t_1().border_color(color("border-100")))
            .children(cells.iter().enumerate().map(|(col, cell)| {
                let cell_id = format!("{row_id}-{col}");
                let (weight, token, size, line) = if header {
                    (FontWeight::SEMIBOLD, "text-secondary", 12., 18.)
                } else {
                    (FontWeight::NORMAL, "text-primary", 13., 20.)
                };
                let align = aligns.get(col).copied().unwrap_or_default();
                div()
                    .flex_basis(relative(0.))
                    .flex_grow(weights.get(col).copied().unwrap_or(6.))
                    .min_w(scale::text(mins.get(col).copied().unwrap_or(0.)))
                    .px(ui_px(10.))
                    .py(scale::row(6.))
                    .text_size(scale::text(size))
                    .line_height(scale::line(line))
                    .when(align == Align::Center, |d| d.text_center())
                    .when(align == Align::Right, |d| d.text_right())
                    .child(rich_text_in(cell, names, &cell_id, weight, token))
            }))
            .test_support()
    };
    div()
        .id(SharedString::from(format!("{id}-table")))
        .flex()
        .flex_col()
        .w_full()
        .max_w_full()
        .border_1()
        .border_color(color("border-100"))
        .rounded(ui_px(8.))
        .overflow_hidden()
        .role(Role::Table)
        .child(row(head, 0, true))
        .children(
            rows.iter()
                .enumerate()
                .map(|(ix, cells)| row(cells, ix + 1, false)),
        )
        .test_support()
        .into_any_element()
}

/// Each column's share of a table's width: its longest cell in
/// characters, kept between 6 and 40.
fn column_weights(head: &[Text], rows: &[Vec<Text>]) -> Vec<f32> {
    (0..head.len())
        .map(|col| {
            std::iter::once(head)
                .chain(rows.iter().map(Vec::as_slice))
                .filter_map(|cells| cells.get(col))
                .map(|c| c.text.chars().count())
                .max()
                .unwrap_or(0)
                .clamp(6, 40) as f32
        })
        .collect()
}

#[cfg(test)]
mod table_width_tests {
    use super::{Text, column_min_widths};

    #[test]
    fn a_column_is_never_narrower_than_its_longest_word() {
        let t = |s: &str| Text {
            text: s.into(),
            spans: Vec::new(),
        };
        let head = vec![t("Author"), t("Status")];
        let rows = vec![vec![
            t("release bot"),
            t("a-very-long-hyphenated-status-word"),
        ]];
        let mins = column_min_widths(&head, &rows);
        assert_eq!(mins[0], 7.0 * 7.5 + 20.);
        assert_eq!(mins[1], 14.0 * 7.5 + 20.);
    }
}

/// The narrowest each column may get: room for its longest word, so a
/// narrow table wraps between words instead of inside them. Very long
/// words (paths, URLs) are capped and may still break.
fn column_min_widths(head: &[Text], rows: &[Vec<Text>]) -> Vec<f32> {
    /// Rough width of one character at 13 px, plus the cell's padding.
    const CHAR: f32 = 7.5;
    const PADDING: f32 = 20.;
    const LONGEST_WORD: usize = 14;
    (0..head.len())
        .map(|col| {
            let word = std::iter::once(head)
                .chain(rows.iter().map(Vec::as_slice))
                .filter_map(|cells| cells.get(col))
                .flat_map(|c| c.text.split_whitespace().map(|w| w.chars().count()))
                .max()
                .unwrap_or(0)
                .min(LONGEST_WORD);
            word as f32 * CHAR + PADDING
        })
        .collect()
}

/// Text in its styles: bold, italic, `code` in mono on a background,
/// mentions and links in blue. Clicking a web link opens it.
fn rich_text(text: &Text, names: &[String], id: &str, weight: FontWeight) -> Div {
    rich_text_in(text, names, id, weight, "text-primary")
}

/// [`rich_text`] with plain text in the color `token`.
fn rich_text_in(text: &Text, names: &[String], id: &str, weight: FontWeight, token: &str) -> Div {
    let plain = token;
    let mut links: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    let runs: Vec<TextRun> = with_code_gaps(&text.text, with_mentions(text, names))
        .into_iter()
        .map(|(span, gap)| {
            let style = span.style;
            let mono = style.code || gap;
            let mut run_font = font(if mono { code_family() } else { ui_family() });
            run_font.weight = scale::weight(if style.bold {
                FontWeight::BOLD
            } else if style.mention && weight.0 < FontWeight::MEDIUM.0 {
                FontWeight::MEDIUM
            } else {
                weight
            });
            if style.italic {
                run_font.style = FontStyle::Italic;
            }
            let token = if style.mention || style.link.is_some() {
                "accent-blue"
            } else {
                plain
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
    #[cfg(test)]
    drawn::record(&text.text, &runs);
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
        // Bold as drawn with the settings in use, Bolder text or not.
        if run.font.weight == scale::weight(FontWeight::BOLD) {
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
            Block::Table { head, rows, .. } => words.extend(
                std::iter::once(head)
                    .chain(rows)
                    .flatten()
                    .map(|c| c.text.clone()),
            ),
        }
    }
    words.join(" ")
}

/// How each drawn text was split into runs, so window tests can check
/// what was drawn in which font.
#[cfg(test)]
pub mod drawn {
    use std::cell::RefCell;

    use gpui_kit::TextRun;

    use crate::theme::scale::code_family;

    /// A stretch of drawn text, whether in the code font, and whether on
    /// the code background.
    pub type Part = (String, bool, bool);

    thread_local! {
        static DRAWN: RefCell<Vec<(String, Vec<Part>)>> = const { RefCell::new(Vec::new()) };
    }

    pub(super) fn record(text: &str, runs: &[TextRun]) {
        let mut at = 0;
        let parts = runs
            .iter()
            .map(|r| {
                let part = text[at..at + r.len].to_string();
                at += r.len;
                (
                    part,
                    r.font.family == code_family(),
                    r.background_color.is_some(),
                )
            })
            .collect();
        DRAWN.with(|d| {
            let mut d = d.borrow_mut();
            d.retain(|(t, _)| t != text);
            d.push((text.to_string(), parts));
        });
    }

    /// The runs `text` was last drawn in, if it was.
    pub fn parts(text: &str) -> Option<Vec<Part>> {
        DRAWN.with(|d| {
            d.borrow()
                .iter()
                .find(|(t, _)| t == text)
                .map(|(_, p)| p.clone())
        })
    }
}
