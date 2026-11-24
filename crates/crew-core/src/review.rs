//! Review findings in an agent's reply: lines of the form
//! `path:line: comment`, which Crew pins to the diff.

/// One finding: a comment about a line of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Relative to the worktree, without a leading `./`.
    pub path: String,
    pub line: u32,
    pub comment: String,
}

/// The findings in `text`, in order. A line counts when it reads
/// `path:line: comment`, `path:line - comment` or `path:line` followed by
/// a dash and the comment, optionally as a list item and with the path in
/// backticks. An absolute path under `root` is made relative to it.
pub fn parse_findings(text: &str, root: &str) -> Vec<Finding> {
    text.lines()
        .filter_map(|line| finding(line, root))
        .collect()
}

/// Dashes that may stand between the line number and the comment.
const DASHES: [char; 3] = ['-', '\u{2013}', '\u{2014}'];

fn finding(line: &str, root: &str) -> Option<Finding> {
    let line = unwrap_location(strip_list_marker(line.trim()));
    let (path, rest) = line.split_once(':')?;
    if path.is_empty() || path.contains(|c: char| c.is_whitespace() || c == '\\') {
        return None;
    }
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let number: u32 = rest[..digits].parse().ok().filter(|&n| n > 0)?;
    let after = &rest[digits..];
    let comment = if let Some(comment) = after.strip_prefix(':') {
        comment
            .starts_with(char::is_whitespace)
            .then_some(comment)?
    } else if after.starts_with(char::is_whitespace) {
        let comment = after.trim_start().strip_prefix(DASHES)?;
        comment
            .starts_with(char::is_whitespace)
            .then_some(comment)?
    } else {
        return None;
    };
    let comment = comment.trim();
    if comment.is_empty() {
        return None;
    }
    Some(Finding {
        path: relative(path, root),
        line: number,
        comment: comment.to_string(),
    })
}

/// `line` without a leading `-`, `*`, `+`, `1.` or `1)` list marker.
fn strip_list_marker(line: &str) -> &str {
    let rest = if let Some(rest) = line.strip_prefix(['-', '*', '+']) {
        rest
    } else {
        let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        match line[digits..].strip_prefix(['.', ')']) {
            Some(rest) if digits > 0 => rest,
            _ => return line,
        }
    };
    if rest.starts_with(char::is_whitespace) {
        rest.trim_start()
    } else {
        line
    }
}

/// `line` with backticks or bold markers around its start dropped, so
/// `` `a.rs:3`: x `` and `` `a.rs`:3: x `` both read `a.rs:3: x`.
fn unwrap_location(line: &str) -> String {
    let mut line = line.to_string();
    'unwrap: loop {
        for mark in ["**", "`"] {
            if let Some(rest) = line.strip_prefix(mark)
                && let Some(end) = rest.find(mark)
                && end > 0
            {
                line = format!("{}{}", &rest[..end], &rest[end + mark.len()..]);
                continue 'unwrap;
            }
        }
        return line;
    }
}

/// `path` without `./`, and relative to `root` when it is under it. On
/// macOS `/tmp` is `/private/tmp`, so `root` matches with or without
/// `/private`.
fn relative(path: &str, root: &str) -> String {
    let mut path = path;
    while let Some(rest) = path.strip_prefix("./") {
        path = rest;
    }
    let root = root.trim_end_matches('/');
    if !path.starts_with('/') || !root.starts_with('/') {
        return path.to_string();
    }
    let bare = root.strip_prefix("/private").filter(|r| r.starts_with('/'));
    let private = format!("/private{root}");
    let roots = [Some(root), bare, Some(private.as_str())];
    roots
        .into_iter()
        .flatten()
        .find_map(|root| path.strip_prefix(root)?.strip_prefix('/'))
        .unwrap_or(path)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{Finding, parse_findings};

    const ROOT: &str = "/work/demo";

    fn one(path: &str, line: u32, comment: &str) -> Vec<Finding> {
        vec![Finding {
            path: path.into(),
            line,
            comment: comment.into(),
        }]
    }

    #[test]
    fn colon_dash_and_em_dash_forms_are_read() {
        for text in [
            "src/a.rs:12: Check the length.",
            "src/a.rs:12 - Check the length.",
            "src/a.rs:12 \u{2014} Check the length.",
            "src/a.rs:12 \u{2013} Check the length.",
            "  src/a.rs:12:   Check the length.  ",
        ] {
            assert_eq!(
                parse_findings(text, ROOT),
                one("src/a.rs", 12, "Check the length."),
                "{text}"
            );
        }
    }

    #[test]
    fn a_line_with_no_separator_is_not_a_finding() {
        for text in [
            "src/a.rs:12 Check the length.",
            "src/a.rs:12",
            "src/a.rs:12:",
            "src/a.rs:12 - ",
            "src/a.rs: no line",
            "src/a.rs:0: zero is not a line",
            "src/a.rs:x1: not a number",
            "Looks good to me.",
            "",
        ] {
            assert!(parse_findings(text, ROOT).is_empty(), "{text}");
        }
    }

    #[test]
    fn list_markers_and_backticks_are_dropped() {
        for text in [
            "- src/a.rs:3: Rename it.",
            "* src/a.rs:3: Rename it.",
            "+ src/a.rs:3: Rename it.",
            "1. src/a.rs:3: Rename it.",
            "12) src/a.rs:3: Rename it.",
            "`src/a.rs:3`: Rename it.",
            "`src/a.rs`:3: Rename it.",
            "- `src/a.rs:3` - Rename it.",
            "**src/a.rs:3**: Rename it.",
        ] {
            assert_eq!(
                parse_findings(text, ROOT),
                one("src/a.rs", 3, "Rename it."),
                "{text}"
            );
        }
    }

    #[test]
    fn paths_are_made_relative_to_the_root() {
        for text in [
            "./src/a.rs:7: Trim it.",
            "/work/demo/src/a.rs:7: Trim it.",
            "`/work/demo/src/a.rs:7` \u{2014} Trim it.",
        ] {
            assert_eq!(
                parse_findings(text, ROOT),
                one("src/a.rs", 7, "Trim it."),
                "{text}"
            );
        }
        // A root given with a trailing slash works the same.
        assert_eq!(
            parse_findings("/work/demo/src/a.rs:7: Trim it.", "/work/demo/"),
            one("src/a.rs", 7, "Trim it.")
        );
        // On macOS /tmp and /var are /private/tmp and /private/var, and a
        // tool may report either.
        assert_eq!(
            parse_findings("/private/tmp/w/src/a.rs:7: Trim it.", "/tmp/w"),
            one("src/a.rs", 7, "Trim it.")
        );
        assert_eq!(
            parse_findings("/tmp/w/src/a.rs:7: Trim it.", "/private/tmp/w"),
            one("src/a.rs", 7, "Trim it.")
        );
        // Outside the root it stays as written.
        assert_eq!(
            parse_findings("/work/demo-2/a.rs:7: Trim it.", ROOT),
            one("/work/demo-2/a.rs", 7, "Trim it.")
        );
    }

    #[test]
    fn urls_and_windows_paths_are_ignored() {
        for text in [
            "http://x:80 - y",
            "see https://example.com:8080/path: down",
            "C:\\src\\a.rs:3: Rename it.",
            "C:/src/a.rs:3: Rename it.",
            "src\\a.rs:3: Rename it.",
            "Step 1: do this",
            "Note 2: and that",
        ] {
            assert!(parse_findings(text, ROOT).is_empty(), "{text}");
        }
    }

    #[test]
    fn findings_among_prose_are_all_found_in_order() {
        let text = "I read the change.\n\n\
                    - src/a.rs:3: Rename it.\n\
                    - README.md:1 - Say what it does.\n\
                    \n\
                    Otherwise fine.\n\
                    Makefile:9: The `test` target is gone.";
        let got: Vec<(String, u32, String)> = parse_findings(text, ROOT)
            .into_iter()
            .map(|f| (f.path, f.line, f.comment))
            .collect();
        assert_eq!(
            got,
            [
                ("src/a.rs".into(), 3, "Rename it.".into()),
                ("README.md".into(), 1, "Say what it does.".into()),
                ("Makefile".into(), 9, "The `test` target is gone.".into()),
            ]
        );
    }
}
