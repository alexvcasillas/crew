//! Short, filesystem and branch safe names.

/// Longest slug, suffix included.
pub const MAX_LEN: usize = 48;

/// Turns `title` into a slug of `[a-z0-9-]`, at most 48 chars. Runs of
/// other characters become one `-`. If `taken` says the slug is in use,
/// `-2`, `-3`, ... is appended. An empty result becomes `untitled`.
pub fn slugify(title: &str, taken: &dyn Fn(&str) -> bool) -> String {
    let mut base = String::new();
    for c in title.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            base.push(c);
        } else if !base.is_empty() && !base.ends_with('-') {
            base.push('-');
        }
    }
    if base.trim_end_matches('-').is_empty() {
        base = "untitled".into();
    }
    let first = cut(&base, MAX_LEN);
    if !taken(&first) {
        return first;
    }
    (2..)
        .map(|n| {
            let suffix = format!("-{n}");
            format!("{}{suffix}", cut(&base, MAX_LEN - suffix.len()))
        })
        .find(|s| !taken(s))
        .expect("some suffix is free")
}

/// First `max` bytes of an ASCII slug, without a trailing `-`.
fn cut(s: &str, max: usize) -> String {
    s[..s.len().min(max)].trim_end_matches('-').to_string()
}

/// Said when a channel name has anything but `[a-z0-9-]`.
pub const BAD_CHANNEL_NAME: &str = "Use lowercase letters, numbers and dashes.";

/// A channel name as typed, without spaces around it or a leading `#`.
/// It must be lowercase letters, numbers and dashes.
pub fn channel_name(text: &str) -> Result<String, &'static str> {
    let name = text.trim();
    let name = name.strip_prefix('#').unwrap_or(name);
    let fine = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if fine {
        Ok(name.to_string())
    } else {
        Err(BAD_CHANNEL_NAME)
    }
}
