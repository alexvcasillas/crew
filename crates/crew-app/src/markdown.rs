//! Reads the Markdown agents write into a small model of blocks and styled
//! text that the views draw. Anything outside that model reads as plain
//! text.

use std::ops::Range;

/// A parsed message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Doc {
    pub blocks: Vec<Block>,
}

/// One block of a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Paragraph(Text),
    Heading(Text),
    /// `start` is the first number of a numbered list, `None` for bullets.
    List {
        start: Option<u64>,
        items: Vec<Vec<Block>>,
    },
    /// A fenced or indented code block, without its last newline.
    Code(String),
}

/// Text and how each stretch of it looks. The spans cover all of `text`
/// in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Text {
    pub text: String,
    pub spans: Vec<Span>,
}

/// A stretch of text in one style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub range: Range<usize>,
    pub style: Style,
}

/// How a stretch of text looks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub mention: bool,
    /// Where a link goes.
    pub link: Option<String>,
}

impl Text {
    /// Adds `s` in `style`, joining the last span when it looks the same.
    fn push(&mut self, s: &str, style: Style) {
        if s.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text.push_str(s);
        let end = self.text.len();
        match self.spans.last_mut() {
            Some(last) if last.style == style && last.range.end == start => last.range.end = end,
            _ => self.spans.push(Span {
                range: start..end,
                style,
            }),
        }
    }
}

/// What kind of block the text being read belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Open {
    Paragraph,
    Heading,
}

/// A list or list item still being read.
enum Frame {
    List {
        start: Option<u64>,
        items: Vec<Vec<Block>>,
    },
    Item(Vec<Block>),
}

/// The state of a parse: open containers, open text and inline styles.
#[derive(Default)]
struct Reader {
    blocks: Vec<Block>,
    frames: Vec<Frame>,
    text: Option<(Open, Text)>,
    code: Option<String>,
    bold: usize,
    italic: usize,
    links: Vec<String>,
}

impl Reader {
    fn style(&self) -> Style {
        Style {
            bold: self.bold > 0,
            italic: self.italic > 0,
            link: self.links.last().cloned(),
            ..Style::default()
        }
    }

    fn add_text(&mut self, s: &str, style: Style) {
        if let Some(code) = &mut self.code {
            code.push_str(s);
            return;
        }
        // A tight list item's text comes without a paragraph around it.
        let (_, text) = self
            .text
            .get_or_insert_with(|| (Open::Paragraph, Text::default()));
        text.push(s, style);
    }

    fn open(&mut self, kind: Open) {
        self.close();
        self.text = Some((kind, Text::default()));
    }

    /// Ends the open text, if any, as a block.
    fn close(&mut self) {
        if let Some((kind, text)) = self.text.take()
            && !text.text.is_empty()
        {
            self.push(match kind {
                Open::Paragraph => Block::Paragraph(text),
                Open::Heading => Block::Heading(text),
            });
        }
    }

    fn push(&mut self, block: Block) {
        match self.frames.last_mut() {
            Some(Frame::Item(blocks)) => blocks.push(block),
            _ => self.blocks.push(block),
        }
    }

    fn start(&mut self, tag: pulldown_cmark::Tag<'_>) {
        use pulldown_cmark::Tag;
        match tag {
            Tag::Paragraph => self.open(Open::Paragraph),
            Tag::Heading { .. } => self.open(Open::Heading),
            Tag::CodeBlock(_) => {
                self.close();
                self.code = Some(String::new());
            }
            Tag::List(start) => {
                self.close();
                self.frames.push(Frame::List {
                    start,
                    items: Vec::new(),
                });
            }
            Tag::Item => {
                self.close();
                self.frames.push(Frame::Item(Vec::new()));
            }
            Tag::Strong => self.bold += 1,
            Tag::Emphasis => self.italic += 1,
            Tag::Link { dest_url, .. } => self.links.push(dest_url.into_string()),
            Tag::BlockQuote(_) | Tag::HtmlBlock => self.close(),
            _ => {}
        }
    }

    fn end(&mut self, tag: pulldown_cmark::TagEnd) {
        use pulldown_cmark::TagEnd;
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::BlockQuote(_) => self.close(),
            TagEnd::HtmlBlock => {
                if let Some((_, text)) = &mut self.text {
                    trim_end(text);
                }
                self.close();
            }
            TagEnd::CodeBlock => {
                let mut code = self.code.take().unwrap_or_default();
                if code.ends_with('\n') {
                    code.pop();
                }
                self.push(Block::Code(code));
            }
            TagEnd::Item => {
                self.close();
                if let Some(Frame::Item(blocks)) = self.frames.pop()
                    && let Some(Frame::List { items, .. }) = self.frames.last_mut()
                {
                    items.push(blocks);
                }
            }
            TagEnd::List(_) => {
                self.close();
                if let Some(Frame::List { start, items }) = self.frames.pop() {
                    self.push(Block::List { start, items });
                }
            }
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Link => {
                self.links.pop();
            }
            _ => {}
        }
    }
}

/// Drops trailing newlines from the end of `text`.
fn trim_end(text: &mut Text) {
    let len = text.text.trim_end_matches('\n').len();
    text.text.truncate(len);
    text.spans.retain_mut(|s| {
        s.range.end = s.range.end.min(len);
        s.range.start < s.range.end
    });
}

/// Reads `source` as Markdown.
pub fn parse(source: &str) -> Doc {
    use pulldown_cmark::{Event, Parser};
    let mut reader = Reader::default();
    for event in Parser::new(source) {
        match event {
            Event::Start(tag) => reader.start(tag),
            Event::End(tag) => reader.end(tag),
            Event::Text(s) | Event::Html(s) | Event::InlineHtml(s) => {
                let style = reader.style();
                reader.add_text(&s, style);
            }
            Event::Code(s) => {
                let style = Style {
                    code: true,
                    ..reader.style()
                };
                reader.add_text(&s, style);
            }
            Event::SoftBreak | Event::HardBreak => {
                let style = reader.style();
                reader.add_text("\n", style);
            }
            Event::Rule => {
                reader.open(Open::Paragraph);
                reader.add_text("---", Style::default());
                reader.close();
            }
            _ => {}
        }
    }
    reader.close();
    Doc {
        blocks: reader.blocks,
    }
}

/// The spans of `text` with known `@Agent` mentions cut out and marked,
/// outside code and links.
pub fn with_mentions(text: &Text, names: &[String]) -> Vec<Span> {
    let mentions = mention_spans(&text.text, names);
    let mut spans = Vec::new();
    for span in &text.spans {
        if span.style.code || span.style.link.is_some() {
            spans.push(span.clone());
            continue;
        }
        let mut at = span.range.start;
        for m in mentions
            .iter()
            .filter(|m| span.range.start <= m.start && m.end <= span.range.end)
        {
            if at < m.start {
                spans.push(Span {
                    range: at..m.start,
                    style: span.style.clone(),
                });
            }
            spans.push(Span {
                range: m.clone(),
                style: Style {
                    mention: true,
                    ..span.style.clone()
                },
            });
            at = m.end;
        }
        if at < span.range.end {
            spans.push(Span {
                range: at..span.range.end,
                style: span.style.clone(),
            });
        }
    }
    spans
}

/// Byte ranges of `@Name` for known agent names.
pub fn mention_spans(body: &str, names: &[String]) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    for (at, _) in body.match_indices('@') {
        let starts = body[..at]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || c == '(');
        let word_len = body[at + 1..]
            .find(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(body.len() - at - 1);
        let word = &body[at + 1..at + 1 + word_len];
        if starts && names.iter().any(|n| n.eq_ignore_ascii_case(word)) {
            spans.push(at..at + 1 + word_len);
        }
    }
    spans
}

/// Whether a link may be opened: only `http` and `https` ones.
pub fn opens(url: &str) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

#[cfg(test)]
mod tests {
    use super::{Block, Doc, Span, Style, Text, mention_spans, opens, parse, with_mentions};

    /// Each stretch of `text` with a short tag for its style.
    fn styled(text: &Text) -> Vec<(&str, &'static str)> {
        text.spans
            .iter()
            .map(|s| (&text.text[s.range.clone()], tag(&s.style)))
            .collect()
    }

    fn tag(style: &Style) -> &'static str {
        match style {
            Style { mention: true, .. } => "mention",
            Style { link: Some(_), .. } => "link",
            Style { code: true, .. } => "code",
            Style {
                bold: true,
                italic: true,
                ..
            } => "bold italic",
            Style { bold: true, .. } => "bold",
            Style { italic: true, .. } => "italic",
            _ => "plain",
        }
    }

    fn paragraph(doc: &Doc, ix: usize) -> &Text {
        match &doc.blocks[ix] {
            Block::Paragraph(text) => text,
            other => panic!("a paragraph, got {other:?}"),
        }
    }

    #[test]
    fn plain_text_is_one_plain_paragraph() {
        let doc = parse("just words");
        assert_eq!(doc.blocks.len(), 1);
        assert_eq!(styled(paragraph(&doc, 0)), [("just words", "plain")]);
        assert_eq!(parse(""), Doc::default());
    }

    #[test]
    fn inline_styles_are_read() {
        let doc = parse("**The change:** made *it* `fast` and ***both***");
        assert_eq!(
            styled(paragraph(&doc, 0)),
            [
                ("The change:", "bold"),
                (" made ", "plain"),
                ("it", "italic"),
                (" ", "plain"),
                ("fast", "code"),
                (" and ", "plain"),
                ("both", "bold italic"),
            ]
        );
    }

    #[test]
    fn blank_lines_split_paragraphs_and_single_newlines_stay() {
        let doc = parse("one\ntwo\n\nthree");
        assert_eq!(doc.blocks.len(), 2);
        assert_eq!(paragraph(&doc, 0).text, "one\ntwo");
        assert_eq!(paragraph(&doc, 1).text, "three");
    }

    #[test]
    fn headings_are_read_at_any_level() {
        let doc = parse("# Big\n\n### **Small**");
        let Block::Heading(big) = &doc.blocks[0] else {
            panic!("a heading")
        };
        assert_eq!(styled(big), [("Big", "plain")]);
        let Block::Heading(small) = &doc.blocks[1] else {
            panic!("a heading")
        };
        assert_eq!(styled(small), [("Small", "bold")]);
    }

    #[test]
    fn lists_hold_items_and_one_nested_level() {
        let doc = parse("- one **bold**\n- two\n  1. inner\n  2. more\n\n3. three\n4. four");
        let Block::List { start, items } = &doc.blocks[0] else {
            panic!("a list")
        };
        assert_eq!(*start, None);
        assert_eq!(items.len(), 2);
        let Block::Paragraph(first) = &items[0][0] else {
            panic!("item text")
        };
        assert_eq!(styled(first), [("one ", "plain"), ("bold", "bold")]);
        let Block::List {
            start: Some(1),
            items: inner,
        } = &items[1][1]
        else {
            panic!("a nested numbered list, got {:?}", items[1])
        };
        assert_eq!(inner.len(), 2);
        assert_eq!(items[1][0], Block::Paragraph(plain("two")));
        let Block::List {
            start: Some(3),
            items: numbered,
        } = &doc.blocks[1]
        else {
            panic!("a numbered list from 3, got {:?}", doc.blocks[1])
        };
        assert_eq!(numbered[1], vec![Block::Paragraph(plain("four"))]);
    }

    #[test]
    fn fenced_code_keeps_its_text() {
        let doc = parse("Run:\n\n```sh\ncargo test\n  --all\n```\nafter");
        assert_eq!(doc.blocks[1], Block::Code("cargo test\n  --all".into()));
        assert_eq!(paragraph(&doc, 2).text, "after");
    }

    #[test]
    fn links_keep_where_they_go() {
        let doc = parse("see [the docs](https://example.com/a) now");
        let text = paragraph(&doc, 0);
        assert_eq!(
            styled(text),
            [("see ", "plain"), ("the docs", "link"), (" now", "plain")]
        );
        assert_eq!(
            text.spans[1].style.link.as_deref(),
            Some("https://example.com/a")
        );
        assert!(opens("https://example.com"));
        assert!(opens("HTTP://example.com"));
        assert!(!opens("file:///etc/passwd"));
        assert!(!opens("javascript:alert(1)"));
    }

    #[test]
    fn other_markdown_reads_as_plain_text() {
        let doc = parse("> quoted\n\n---\n\n<b>html</b>\n\n![alt](x.png)");
        let texts: Vec<String> = doc
            .blocks
            .iter()
            .map(|b| match b {
                Block::Paragraph(t) => t.text.clone(),
                other => panic!("plain text, got {other:?}"),
            })
            .collect();
        assert_eq!(texts, ["quoted", "---", "<b>html</b>", "alt"]);
    }

    #[test]
    fn mentions_are_marked_outside_code_and_links() {
        let names = ["Forge".to_string()];
        let doc = parse("ask **@Forge** or @forge, not `@Forge` or @nobody");
        let text = paragraph(&doc, 0);
        let spans = with_mentions(text, &names);
        let tagged: Vec<(&str, &str)> = spans
            .iter()
            .map(|s| (&text.text[s.range.clone()], tag(&s.style)))
            .collect();
        assert_eq!(
            tagged,
            [
                ("ask ", "plain"),
                ("@Forge", "mention"),
                (" or ", "plain"),
                ("@forge", "mention"),
                (", not ", "plain"),
                ("@Forge", "code"),
                (" or @nobody", "plain"),
            ]
        );
        assert!(spans[1].style.bold, "a mention keeps its emphasis");
    }

    #[test]
    fn known_mentions_are_found() {
        let names = ["Forge".to_string()];
        let body = "@forge read a.txt, not @nobody or me@forge";
        assert_eq!(mention_spans(body, &names), vec![0..6_usize]);
    }

    fn plain(text: &str) -> Text {
        Text {
            text: text.into(),
            spans: vec![Span {
                range: 0..text.len(),
                style: Style::default(),
            }],
        }
    }
}
