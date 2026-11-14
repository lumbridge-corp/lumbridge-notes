//! Small text helpers shared by find, search, the picker and the UI.

use std::ops::Range;

use crate::layout::{display_grapheme, grapheme_width, graphemes};

/// Whether matching `needle` should ignore case (smart case: only when the
/// needle has no uppercase letters).
pub fn smart_case(needle: &str) -> bool {
    !needle.chars().any(char::is_uppercase)
}

/// Byte ranges of the non-overlapping matches of `needle` in `hay`. With
/// `ignore_case`, ASCII letters match either case; other characters must
/// match exactly (so byte offsets stay those of `hay`).
pub fn find_all(hay: &str, needle: &str, ignore_case: bool, limit: usize) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    if needle.is_empty() || needle.len() > hay.len() || limit == 0 {
        return out;
    }
    if !ignore_case {
        for (start, matched) in hay.match_indices(needle) {
            out.push(start..start + matched.len());
            if out.len() >= limit {
                break;
            }
        }
        return out;
    }
    let hay_bytes = hay.as_bytes();
    let needle_bytes = needle.as_bytes();
    let first = needle_bytes[0].to_ascii_lowercase();
    let mut at = 0;
    while at + needle_bytes.len() <= hay_bytes.len() {
        if hay_bytes[at].to_ascii_lowercase() == first
            && hay.is_char_boundary(at)
            && hay_bytes[at..at + needle_bytes.len()].eq_ignore_ascii_case(needle_bytes)
            && hay.is_char_boundary(at + needle_bytes.len())
        {
            out.push(at..at + needle_bytes.len());
            if out.len() >= limit {
                break;
            }
            at += needle_bytes.len();
        } else {
            at += 1;
        }
    }
    out
}

/// The first match of `needle` in `hay` (see [`find_all`]).
pub fn find_first(hay: &str, needle: &str, ignore_case: bool) -> Option<Range<usize>> {
    find_all(hay, needle, ignore_case, 1).into_iter().next()
}

/// Text made safe to draw: tabs become spaces, control characters U+FFFD.
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (_, grapheme) in graphemes(text) {
        out.push_str(&display_grapheme(grapheme));
    }
    out
}

/// The longest prefix of `text` that fits in `width` cells, and its width.
pub fn fit(text: &str, width: usize) -> (&str, usize) {
    let mut used = 0;
    for (offset, grapheme) in graphemes(text) {
        let cells = grapheme_width(grapheme);
        if used + cells > width {
            return (&text[..offset], used);
        }
        used += cells;
    }
    (text, used)
}

/// `text` cut to `width` cells with a trailing `…` when it does not fit.
pub fn ellipsize(text: &str, width: usize) -> String {
    let (head, used) = fit(text, width);
    if head.len() == text.len() {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let (head, _) = if used >= width {
        fit(text, width - 1)
    } else {
        (head, used)
    };
    format!("{head}…")
}

/// Key hints joined by two spaces, keeping only as many whole hints (most
/// important first) as fit in `width` cells; never a hint cut mid-word
/// unless even the first is too wide.
pub fn fit_hints(hints: &[&str], width: usize) -> String {
    let mut out = String::new();
    for hint in hints {
        let sep = if out.is_empty() { "" } else { "  " };
        if crate::layout::str_width(&out) + sep.len() + crate::layout::str_width(hint) > width {
            break;
        }
        out.push_str(sep);
        out.push_str(hint);
    }
    if out.is_empty() {
        if let Some(first) = hints.first() {
            return ellipsize(first, width);
        }
    }
    out
}

/// Words in a line, for the status line.
pub fn word_count(line: &str) -> u32 {
    line.split_whitespace().count() as u32
}

/// Standard base64 (for OSC 52).
pub fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// The OSC 52 sequence that puts `text` on the host terminal's clipboard.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_with_smart_case() {
        assert!(smart_case("abc"));
        assert!(!smart_case("aBc"));
        assert_eq!(
            find_all("Foo foo FOO", "foo", true, 10),
            vec![0..3, 4..7, 8..11]
        );
        assert_eq!(find_all("Foo foo FOO", "foo", false, 10), vec![4..7]);
        assert_eq!(find_all("aaaa", "aa", true, 10), vec![0..2, 2..4]);
        assert_eq!(find_all("éa ÉA", "a", true, 10), vec![2..3, 6..7]);
        assert_eq!(find_all("abc", "abc", true, 0), Vec::<Range<usize>>::new());
        assert_eq!(find_first("xyz", "Y", false), None);
    }

    #[test]
    fn fits_and_ellipsizes_by_width() {
        assert_eq!(fit("日本語", 5), ("日本", 4));
        assert_eq!(ellipsize("hello world", 8), "hello w…");
        assert_eq!(ellipsize("short", 8), "short");
        assert_eq!(ellipsize("日本語です", 5), "日本…");
        assert_eq!(sanitize("a\tb\u{1}"), "a    b\u{fffd}");
    }

    #[test]
    fn base64_and_osc52() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
        assert_eq!(word_count("  two  words "), 2);
    }
}
