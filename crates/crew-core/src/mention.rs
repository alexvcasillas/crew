//! Finds @mentions of agents in a message body.

/// Returns `(known, unknown)` mentions in order of first appearance.
///
/// Known names use the casing from `agent_names`. Unknown names keep the
/// casing as typed. Duplicates are dropped.
pub fn parse_mentions(body: &str, agent_names: &[&str]) -> (Vec<String>, Vec<String>) {
    let mut known: Vec<String> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    let mut prev: Option<char> = None;
    let mut chars = body.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let starts = c == '@' && prev.is_none_or(|p| p.is_whitespace() || p == '(');
        prev = Some(c);
        if !starts {
            continue;
        }
        let start = i + 1;
        let mut end = start;
        while let Some(&(j, n)) = chars.peek() {
            if n.is_alphanumeric() || n == '-' || n == '_' {
                end = j + n.len_utf8();
                prev = Some(n);
                chars.next();
            } else {
                break;
            }
        }
        let word = &body[start..end];
        if word.is_empty() {
            continue;
        }
        match agent_names.iter().find(|n| n.eq_ignore_ascii_case(word)) {
            Some(name) => {
                if !known.iter().any(|k| k == name) {
                    known.push((*name).to_string());
                }
            }
            None => {
                if !unknown.iter().any(|u| u.eq_ignore_ascii_case(word)) {
                    unknown.push(word.to_string());
                }
            }
        }
    }
    (known, unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMES: &[&str] = &["Forge", "Triage"];

    #[test]
    fn mentions_in_order_and_deduped() {
        let (known, unknown) = parse_mentions("@forge then @Triage and @forge", NAMES);
        assert_eq!(known, vec!["Forge", "Triage"]);
        assert!(unknown.is_empty());
    }

    #[test]
    fn email_is_not_a_mention() {
        let (known, unknown) = parse_mentions("mail foo@bar.com or a@Forge", NAMES);
        assert!(known.is_empty());
        assert!(unknown.is_empty());
    }

    #[test]
    fn unknown_mention_reported() {
        let (known, unknown) = parse_mentions("hi @Nobody and @Nobody", NAMES);
        assert!(known.is_empty());
        assert_eq!(unknown, vec!["Nobody"]);
    }

    #[test]
    fn trailing_punctuation_is_not_part_of_name() {
        let (known, _) = parse_mentions("(@forge), @triage? @Forge! @triage: @forge.", NAMES);
        assert_eq!(known, vec!["Forge", "Triage"]);
    }

    #[test]
    fn mention_at_start_and_name_chars() {
        let (known, unknown) = parse_mentions("@forge-2 @my_bot", NAMES);
        assert!(known.is_empty());
        assert_eq!(unknown, vec!["forge-2", "my_bot"]);
    }

    #[test]
    fn bare_at_is_ignored() {
        let (known, unknown) = parse_mentions("@ hello @", NAMES);
        assert!(known.is_empty() && unknown.is_empty());
    }
}
