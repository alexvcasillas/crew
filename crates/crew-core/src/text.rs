//! Making text from agents and CLIs safe to show on one line.

/// Characters that can hide or reorder what you read: control
/// characters, bidi marks, line and paragraph separators, zero-width
/// characters and the byte order mark.
pub fn is_hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061c}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{2069}'
                | '\u{feff}'
        )
}

/// `text` without its [hidden](is_hidden) characters.
pub fn strip_hidden(text: &str) -> String {
    text.chars().filter(|c| !is_hidden(*c)).collect()
}

/// One line safe to draw: tabs and carriage returns become spaces and
/// other [hidden](is_hidden) characters go.
pub fn clean_line(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            '\t' | '\r' => Some(' '),
            c if is_hidden(c) => None,
            c => Some(c),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_characters_go() {
        let text = "a\u{0}\u{1b}b\u{200b}\u{200d}c\u{2028}d\u{2029}e\u{2060}\u{2065}f\u{feff}g\u{202e}h\u{2066}i\u{061c}j\nk";
        assert_eq!(strip_hidden(text), "abcdefghijk");
        assert_eq!(clean_line("a\tb\rc\u{200c}d"), "a b cd");
        assert_eq!(strip_hidden("é ü 日本"), "é ü 日本");
    }
}
