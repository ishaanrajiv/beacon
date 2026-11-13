//! Templates: text with `{name}` placeholders, `{{` and `}}` for literal braces. Beacon renders a
//! kind's title, body, link and recovery text from them (`spec/declarations.md`, "Templates").
//!
//! Values are plain text. Before they go in, [`clean`] strips control characters and folds runs of
//! whitespace (newlines included) into one space, so a value can never add a line or hide text.
//! Clients show titles and bodies as text, never as HTML or Markdown. In a link, values are
//! percent-encoded ([`percent_encode`]).

/// One piece of a template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Text(String),
    Placeholder(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    pub segments: Vec<Segment>,
}

impl Template {
    /// Reads a template. The error is a sentence about what is wrong.
    pub fn parse(text: &str) -> Result<Template, String> {
        let mut segments = Vec::new();
        let mut literal = String::new();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    literal.push('{');
                }
                '{' => {
                    let mut name = String::new();
                    let mut closed = false;
                    for c in chars.by_ref() {
                        if c == '}' {
                            closed = true;
                            break;
                        }
                        name.push(c);
                    }
                    if !closed {
                        return Err(format!("`{{{name}` has no closing `}}`"));
                    }
                    if name.is_empty() {
                        return Err("`{}` names nothing".to_string());
                    }
                    if !crate::names::is_ident(&name) {
                        return Err(format!("`{{{name}}}` is not a placeholder name"));
                    }
                    if !literal.is_empty() {
                        segments.push(Segment::Text(std::mem::take(&mut literal)));
                    }
                    segments.push(Segment::Placeholder(name));
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    literal.push('}');
                }
                '}' => return Err("a `}` closes nothing".to_string()),
                c => literal.push(c),
            }
        }
        if !literal.is_empty() {
            segments.push(Segment::Text(literal));
        }
        Ok(Template { segments })
    }

    /// The placeholder names, in order, repeats included.
    pub fn placeholders(&self) -> impl Iterator<Item = &str> {
        self.segments.iter().filter_map(|s| match s {
            Segment::Placeholder(name) => Some(name.as_str()),
            Segment::Text(_) => None,
        })
    }

    /// The fixed text, with placeholders left out.
    pub fn literal(&self) -> String {
        self.segments
            .iter()
            .filter_map(|s| match s {
                Segment::Text(text) => Some(text.as_str()),
                Segment::Placeholder(_) => None,
            })
            .collect()
    }

    /// The text with each placeholder replaced by `value(name)`.
    pub fn render(&self, mut value: impl FnMut(&str) -> String) -> String {
        let mut out = String::new();
        for segment in &self.segments {
            match segment {
                Segment::Text(text) => out.push_str(text),
                Segment::Placeholder(name) => out.push_str(&value(name)),
            }
        }
        out
    }
}

/// Strips control characters and folds every run of whitespace into one space, trimmed.
pub fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            space = true;
        } else if !c.is_control() && !is_invisible(c) {
            if space && !out.is_empty() {
                out.push(' ');
            }
            space = false;
            out.push(c);
        }
    }
    out
}

/// Characters that change how text is laid out without being seen: bidirectional overrides and
/// zero-width marks. A value could use them to make text read differently from what it is.
fn is_invisible(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
}

/// Cuts `text` to at most `max` characters, at a word boundary, ending in `…`. Text that already
/// fits is returned as it is. A first word longer than half the limit is cut mid-word.
pub fn truncate_words(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let room: String = text.chars().take(max - 1).collect();
    let next_is_space = text.chars().nth(max - 1).is_some_and(char::is_whitespace);
    let cut = if next_is_space {
        room.as_str()
    } else {
        match room.rfind(char::is_whitespace) {
            Some(at) if room[..at].chars().count() * 2 >= max - 1 => &room[..at],
            _ => room.as_str(),
        }
    };
    let cut = cut.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | ':' | '-' | '·'));
    format!("{cut}…")
}

/// Percent-encodes everything but RFC 3986's unreserved characters, for a value in a link.
pub fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_placeholders_and_escapes() {
        let t = Template::parse("PR #{number} is ready {{really}}").unwrap();
        assert_eq!(t.placeholders().collect::<Vec<_>>(), ["number"]);
        assert_eq!(t.literal(), "PR # is ready {really}");
        assert_eq!(t.render(|_| "196".to_string()), "PR #196 is ready {really}");
        assert_eq!(Template::parse("").unwrap().segments, []);
    }

    #[test]
    fn refuses_broken_templates() {
        assert!(Template::parse("{reason").unwrap_err().contains("no closing"));
        assert!(Template::parse("a {} b").unwrap_err().contains("names nothing"));
        assert!(
            Template::parse("{Reason}")
                .unwrap_err()
                .contains("not a placeholder name")
        );
        assert!(
            Template::parse("{last ok}")
                .unwrap_err()
                .contains("not a placeholder name")
        );
        assert!(Template::parse("done }").unwrap_err().contains("closes nothing"));
    }

    #[test]
    fn cleans_values() {
        assert_eq!(clean("  two\n\nlines\tand  spaces "), "two lines and spaces");
        assert_eq!(clean("bell\u{7}"), "bell");
        assert_eq!(clean("a\u{202E}b\u{200B}c"), "abc");
        assert_eq!(clean(""), "");
    }

    #[test]
    fn truncates_at_a_word_boundary() {
        assert_eq!(truncate_words("short", 10), "short");
        assert_eq!(truncate_words("The disk is full again", 12), "The disk is…");
        assert_eq!(truncate_words("The disk, is full", 10), "The disk…");
        assert_eq!(truncate_words("Supercalifragilistic", 8), "Superca…");
        assert_eq!(truncate_words("a verylongwordthatgoeson", 10), "a verylon…");
        assert_eq!(truncate_words("one two", 4), "one…");
        assert_eq!(truncate_words("ab", 1), "…");
        assert_eq!(truncate_words("ab", 0), "");
        assert_eq!(truncate_words("héllo wörld ünïcode", 13).chars().count(), 12);
        for max in 1..30 {
            assert!(
                truncate_words("The quick brown fox jumps over the lazy dog", max)
                    .chars()
                    .count()
                    <= max
            );
        }
    }

    #[test]
    fn encodes_link_values() {
        assert_eq!(percent_encode("a b/c?d=é"), "a%20b%2Fc%3Fd%3D%C3%A9");
        assert_eq!(percent_encode("thread-7_x.y~"), "thread-7_x.y~");
    }
}
