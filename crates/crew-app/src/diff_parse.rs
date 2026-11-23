//! Reads a unified diff, as `git diff` prints it, into files, hunks and
//! numbered lines.

/// What happened to a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileKind {
    Added,
    Deleted,
    Modified,
    /// Moved from the path it names.
    Renamed(String),
}

impl FileKind {
    /// The status letter: `A`, `D`, `M` or `R`.
    pub fn letter(&self) -> char {
        match self {
            FileKind::Added => 'A',
            FileKind::Deleted => 'D',
            FileKind::Modified => 'M',
            FileKind::Renamed(_) => 'R',
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
    /// `\ No newline at end of file`, about the line before.
    NoNewline,
}

/// One line of a hunk with its numbers in the old and new file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    /// Without its leading sign.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The `@@ -1,3 +1,4 @@ fn main` line.
    pub header: String,
    pub lines: Vec<Line>,
}

/// One file of a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// The new path, or the old one for a deleted file.
    pub path: String,
    pub kind: FileKind,
    pub hunks: Vec<Hunk>,
    /// Shown in place of hunks, such as for a binary file.
    pub note: Option<String>,
    pub added: u32,
    pub removed: u32,
}

impl FileDiff {
    /// Lines in all hunks, headers not counted.
    pub fn line_count(&self) -> usize {
        self.hunks.iter().map(|h| h.lines.len()).sum()
    }
}

/// The files of `patch`, in order. Text outside any file is skipped.
pub fn parse(patch: &str) -> Vec<FileDiff> {
    let mut files = Vec::new();
    let mut lines = patch.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix("diff --git ") else {
            continue;
        };
        let (mut old_path, mut new_path) = git_paths(rest);
        let mut kind = FileKind::Modified;
        let mut rename_from = None;
        let mut binary = false;
        // Extended headers, up to the first hunk or the next file.
        while let Some(&next) = lines.peek() {
            if next.starts_with("@@") || next.starts_with("diff --git ") {
                break;
            }
            lines.next();
            if next.starts_with("new file mode") {
                kind = FileKind::Added;
            } else if next.starts_with("deleted file mode") {
                kind = FileKind::Deleted;
            } else if let Some(from) = next.strip_prefix("rename from ") {
                rename_from = Some(unquote(from));
            } else if let Some(to) = next.strip_prefix("rename to ") {
                new_path = Some(unquote(to));
            } else if next.starts_with("Binary files ") || next == "GIT binary patch" {
                binary = true;
            } else if let Some(path) = next.strip_prefix("--- ") {
                if let Some(p) = side_path(path, "a/") {
                    old_path = Some(p);
                }
            } else if let Some(path) = next.strip_prefix("+++ ")
                && let Some(p) = side_path(path, "b/")
            {
                new_path = Some(p);
            }
        }
        if let Some(from) = rename_from {
            kind = FileKind::Renamed(from);
        }
        let path = match kind {
            FileKind::Deleted => old_path.or(new_path),
            _ => new_path.or(old_path),
        }
        .unwrap_or_default();

        let mut hunks = Vec::new();
        while let Some(&next) = lines.peek() {
            let Some((old_start, old_len, new_start, new_len)) = hunk_range(next) else {
                break;
            };
            lines.next();
            let mut hunk = Hunk {
                header: next.to_string(),
                lines: Vec::new(),
            };
            let (mut old, mut new) = (old_start, new_start);
            let (mut old_left, mut new_left) = (old_len, new_len);
            while old_left > 0 || new_left > 0 || lines.peek().is_some_and(|l| l.starts_with('\\'))
            {
                let Some(body) = lines.next() else { break };
                let (kind, text) = match body.chars().next() {
                    Some('+') => (LineKind::Added, &body[1..]),
                    Some('-') => (LineKind::Removed, &body[1..]),
                    Some('\\') => (LineKind::NoNewline, body.trim_start_matches("\\ ")),
                    Some(' ') => (LineKind::Context, &body[1..]),
                    // An empty context line whose space was trimmed.
                    _ => (LineKind::Context, body),
                };
                let (o, n) = match kind {
                    LineKind::Context => {
                        old_left = old_left.saturating_sub(1);
                        new_left = new_left.saturating_sub(1);
                        old += 1;
                        new += 1;
                        (Some(old - 1), Some(new - 1))
                    }
                    LineKind::Removed => {
                        old_left = old_left.saturating_sub(1);
                        old += 1;
                        (Some(old - 1), None)
                    }
                    LineKind::Added => {
                        new_left = new_left.saturating_sub(1);
                        new += 1;
                        (None, Some(new - 1))
                    }
                    LineKind::NoNewline => (None, None),
                };
                hunk.lines.push(Line {
                    kind,
                    old: o,
                    new: n,
                    text: text.to_string(),
                });
            }
            hunks.push(hunk);
        }

        let count = |k: LineKind| {
            let n = hunks
                .iter()
                .flat_map(|h| &h.lines)
                .filter(|l| l.kind == k)
                .count();
            u32::try_from(n).unwrap_or(u32::MAX)
        };
        let (added, removed) = (count(LineKind::Added), count(LineKind::Removed));
        let note = if binary {
            Some("Binary file not shown.".to_string())
        } else if hunks.is_empty() {
            Some(match &kind {
                FileKind::Renamed(from) => format!("Renamed from {from}."),
                FileKind::Added => "Empty file.".to_string(),
                FileKind::Deleted => "Empty file removed.".to_string(),
                FileKind::Modified => "Mode changed.".to_string(),
            })
        } else {
            None
        };
        files.push(FileDiff {
            path,
            kind,
            hunks,
            note,
            added,
            removed,
        });
    }
    files
}

/// `-1,3 +1,4` from `@@ -1,3 +1,4 @@ …`: starts and lengths. A missing
/// length is 1.
fn hunk_range(line: &str) -> Option<(u32, u32, u32, u32)> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, _) = rest.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |r: &str| -> Option<(u32, u32)> {
        match r.split_once(',') {
            Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
            None => Some((r.parse().ok()?, 1)),
        }
    };
    let (os, ol) = range(old)?;
    let (ns, nl) = range(new)?;
    Some((os, ol, ns, nl))
}

/// The two paths of `a/x b/x`. When both sides read alike the split is
/// exact, even with spaces in the path.
fn git_paths(rest: &str) -> (Option<String>, Option<String>) {
    let strip = |p: String, prefix: &str| p.strip_prefix(prefix).map(String::from);
    // A quoted old side: `"a/x y" b/z` or `"a/x y" "b/z"`.
    if rest.starts_with('"') {
        let Some(end) = quoted_end(rest) else {
            return (None, None);
        };
        let old = unquote(&rest[..end]);
        let new = unquote(rest[end..].trim_start());
        return (strip(old, "a/"), strip(new, "b/"));
    }
    // A quoted new side only: `a/x "b/y z"`.
    if rest.ends_with('"')
        && let Some(at) = rest.rfind(" \"b/")
    {
        let old = rest[..at].to_string();
        let new = unquote(&rest[at + 1..]);
        return (strip(old, "a/"), strip(new, "b/"));
    }
    let Some(body) = rest.strip_prefix("a/") else {
        return (None, None);
    };
    // `x b/x`: same length on both sides.
    if body.len() >= 3 && (body.len() - 3).is_multiple_of(2) {
        let half = (body.len() - 3) / 2;
        if body.is_char_boundary(half)
            && body[half..].starts_with(" b/")
            && body[..half] == body[half + 3..]
        {
            let p = body[..half].to_string();
            return (Some(p.clone()), Some(p));
        }
    }
    match body.split_once(" b/") {
        Some((old, new)) => (Some(old.to_string()), Some(new.to_string())),
        None => (None, None),
    }
}

/// The byte just past the closing quote of the quoted string `s` starts
/// with.
fn quoted_end(s: &str) -> Option<usize> {
    let mut escaped = false;
    for (i, b) in s.bytes().enumerate().skip(1) {
        match b {
            _ if escaped => escaped = false,
            b'\\' => escaped = true,
            b'"' => return Some(i + 1),
            _ => {}
        }
    }
    None
}

/// The path on a `---`/`+++` line, without `prefix`. `None` for
/// `/dev/null`.
fn side_path(path: &str, prefix: &str) -> Option<String> {
    let path = path.trim_end_matches('\t');
    let path = unquote(path);
    if path == "/dev/null" {
        return None;
    }
    Some(path.strip_prefix(prefix).unwrap_or(&path).to_string())
}

/// Git's C-style quoting: `"a\tb"` reads as `a<tab>b`. Unquoted text is
/// kept as it is.
fn unquote(s: &str) -> String {
    let Some(inner) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return s.to_string();
    };
    let mut bytes = Vec::new();
    let mut chars = inner.bytes().peekable();
    while let Some(b) = chars.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        match chars.next() {
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(b'"') => bytes.push(b'"'),
            Some(b'\\') => bytes.push(b'\\'),
            Some(d @ b'0'..=b'7') => {
                let mut n = u32::from(d - b'0');
                for _ in 0..2 {
                    match chars.peek() {
                        Some(&o @ b'0'..=b'7') => {
                            n = n * 8 + u32::from(o - b'0');
                            chars.next();
                        }
                        _ => break,
                    }
                }
                bytes.push(u8::try_from(n).unwrap_or(b'?'));
            }
            Some(other) => bytes.push(other),
            None => {}
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{FileKind, LineKind, parse};

    const MODIFIED: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,4 +1,4 @@ fn main
 one
-two
+deux
 three

@@ -10 +10,2 @@
 ten
+eleven
";

    #[test]
    fn hunks_number_their_lines() {
        let files = parse(MODIFIED);
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.path, "src/lib.rs");
        assert_eq!(f.kind, FileKind::Modified);
        assert_eq!((f.added, f.removed), (2, 1));
        assert_eq!(f.note, None);
        assert_eq!(f.hunks.len(), 2);
        assert_eq!(f.hunks[0].header, "@@ -1,4 +1,4 @@ fn main");
        let rows: Vec<_> = f.hunks[0]
            .lines
            .iter()
            .map(|l| (l.kind, l.old, l.new, l.text.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                (LineKind::Context, Some(1), Some(1), "one"),
                (LineKind::Removed, Some(2), None, "two"),
                (LineKind::Added, None, Some(2), "deux"),
                (LineKind::Context, Some(3), Some(3), "three"),
                // A blank context line whose leading space was trimmed.
                (LineKind::Context, Some(4), Some(4), ""),
            ]
        );
        let second: Vec<_> = f.hunks[1].lines.iter().map(|l| (l.old, l.new)).collect();
        assert_eq!(second, [(Some(10), Some(10)), (None, Some(11))]);
        assert_eq!(f.line_count(), 7);
    }

    #[test]
    fn added_deleted_and_no_newline() {
        let patch = "\
diff --git a/new.txt b/new.txt
new file mode 100644
index 0000000..5626abf
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+one
+two
\\ No newline at end of file
diff --git a/old.txt b/old.txt
deleted file mode 100644
index 5626abf..0000000
--- a/old.txt
+++ /dev/null
@@ -1 +0,0 @@
-gone
";
        let files = parse(patch);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "new.txt");
        assert_eq!(files[0].kind, FileKind::Added);
        assert_eq!((files[0].added, files[0].removed), (2, 0));
        let last = files[0].hunks[0].lines.last().unwrap();
        assert_eq!(last.kind, LineKind::NoNewline);
        assert_eq!(last.text, "No newline at end of file");
        assert_eq!(files[1].path, "old.txt");
        assert_eq!(files[1].kind, FileKind::Deleted);
        assert_eq!(files[1].hunks[0].lines[0].old, Some(1));
    }

    #[test]
    fn binary_files_and_renames_read_as_one_line() {
        let patch = "\
diff --git a/logo.png b/logo.png
new file mode 100644
index 0000000..aaaaaaa
Binary files /dev/null and b/logo.png differ
diff --git a/a.txt b/b.txt
similarity index 100%
rename from a.txt
rename to b.txt
diff --git a/c.txt b/d.txt
similarity index 80%
rename from c.txt
rename to d.txt
index 1111111..2222222 100644
--- a/c.txt
+++ b/d.txt
@@ -1 +1 @@
-x
+y
";
        let files = parse(patch);
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].path, "logo.png");
        assert_eq!(files[0].kind.letter(), 'A');
        assert_eq!(files[0].note.as_deref(), Some("Binary file not shown."));
        assert!(files[0].hunks.is_empty());
        assert_eq!(files[1].path, "b.txt");
        assert_eq!(files[1].kind, FileKind::Renamed("a.txt".into()));
        assert_eq!(files[1].note.as_deref(), Some("Renamed from a.txt."));
        assert_eq!(files[2].path, "d.txt");
        assert_eq!(files[2].kind.letter(), 'R');
        assert_eq!(files[2].note, None);
        assert_eq!((files[2].added, files[2].removed), (1, 1));
    }

    #[test]
    fn hunk_lengths_decide_where_a_hunk_ends() {
        // Content that looks like headers stays content.
        let patch = "\
diff --git a/my notes.md b/my notes.md
index 1111111..2222222 100644
--- a/my notes.md
+++ b/my notes.md
@@ -1,2 +1,3 @@
 keep
--- a/fake
+diff --git a/x b/x
+@@ -1 +1 @@
";
        let files = parse(patch);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "my notes.md");
        let texts: Vec<&str> = files[0].hunks[0]
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(
            texts,
            ["keep", "-- a/fake", "diff --git a/x b/x", "@@ -1 +1 @@"]
        );
        assert_eq!((files[0].added, files[0].removed), (2, 1));
    }

    #[test]
    fn quoted_paths_are_unquoted() {
        let patch = "\
diff --git \"a/caf\\303\\251.txt\" \"b/caf\\303\\251.txt\"
new file mode 100644
--- /dev/null
+++ \"b/caf\\303\\251.txt\"
@@ -0,0 +1 @@
+hi
";
        let files = parse(patch);
        assert_eq!(files[0].path, "café.txt");
        // Only one side quoted, as in a binary rename with no `---` lines.
        assert_eq!(
            super::git_paths("\"a/tab\\there\" b/plain"),
            (Some("tab\there".into()), Some("plain".into()))
        );
        assert_eq!(
            super::git_paths("a/plain \"b/new\\tname\""),
            (Some("plain".into()), Some("new\tname".into()))
        );
        assert_eq!(parse("").len(), 0);
        assert_eq!(parse("not a diff\n").len(), 0);
    }
}
