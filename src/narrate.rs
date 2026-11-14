//! Turning a note into text to read aloud: the rendered words without
//! Markdown syntax, front matter or code, cut into chunks a speech server
//! takes comfortably, each remembering the source lines it came from so the
//! editor can mark what is being read.

use std::ops::Range;

use crate::markdown::{Document, Leaf, SpanKind};

/// A chunk aims for this many characters (a few sentences).
const TARGET: usize = 360;
/// A paragraph longer than this is split at sentence ends.
const MAX: usize = 600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub text: String,
    /// Source lines the chunk covers.
    pub lines: Range<usize>,
}

/// One block's spoken text (a heading, a list item, a paragraph...).
struct Part {
    text: String,
    lines: Range<usize>,
}

/// Chunks for `lines[range]`.
pub fn chunks<S: AsRef<str>>(doc: &Document, lines: &[S], range: Range<usize>) -> Vec<Chunk> {
    let parts = parts(doc, lines, range);
    pack(parts)
}

fn spoken_line(doc: &Document, line: &str, index: usize) -> Option<(String, bool)> {
    let info = doc.line(index)?;
    if info.hidden {
        return None;
    }
    let breaks = match &info.leaf {
        Leaf::Blank
        | Leaf::FrontMatter { .. }
        | Leaf::Rule
        | Leaf::SetextUnderline { .. }
        | Leaf::TableDelimiter
        | Leaf::Html
        | Leaf::CodeFence { .. }
        | Leaf::Code { .. } => return None,
        Leaf::Heading { .. } => true,
        _ => info.item.is_some(),
    };
    let mut text = String::new();
    let table = matches!(info.leaf, Leaf::TableRow { .. });
    for span in &info.spans {
        match span.kind {
            SpanKind::Text | SpanKind::Code | SpanKind::Number => text.push_str(span.text(line)),
            SpanKind::TableSep if table && !text.trim().is_empty() => text.push_str(", "),
            _ => {}
        }
    }
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = text.trim_end_matches([',', ' ']).to_owned();
    if text.is_empty() {
        return None;
    }
    // Headings, items and table rows stand alone; a pause after each.
    let alone = breaks || table;
    Some((text, alone))
}

fn parts<S: AsRef<str>>(doc: &Document, lines: &[S], range: Range<usize>) -> Vec<Part> {
    let mut parts: Vec<Part> = Vec::new();
    let mut open = false;
    let mut in_code = false;
    for index in range {
        let Some(line) = lines.get(index) else { break };
        let line = line.as_ref();
        let code = doc
            .line(index)
            .is_some_and(|info| matches!(info.leaf, Leaf::Code { .. } | Leaf::CodeFence { .. }));
        if code {
            if !in_code {
                parts.push(Part {
                    text: "Code block.".to_owned(),
                    lines: index..index + 1,
                });
            } else if let Some(last) = parts.last_mut() {
                last.lines.end = index + 1;
            }
            in_code = true;
            open = false;
            continue;
        }
        in_code = false;
        match spoken_line(doc, line, index) {
            None => open = false,
            Some((text, alone)) => {
                if open && !alone {
                    if let Some(last) = parts.last_mut() {
                        last.text.push(' ');
                        last.text.push_str(&text);
                        last.lines.end = index + 1;
                        continue;
                    }
                }
                parts.push(Part {
                    text,
                    lines: index..index + 1,
                });
                // A list item's wrapped continuation lines join it.
                open = true;
            }
        }
    }
    parts
}

fn ends_sentence(text: &str) -> bool {
    text.trim_end()
        .ends_with(['.', '!', '?', ':', ';', '…', '。', '！', '？'])
}

fn pack(parts: Vec<Part>) -> Vec<Chunk> {
    let mut chunks: Vec<Chunk> = Vec::new();
    for part in parts {
        let mut text = part.text;
        if !ends_sentence(&text) {
            text.push('.');
        }
        for piece in split_long(&text) {
            match chunks.last_mut() {
                Some(last) if last.text.len() + piece.len() < TARGET => {
                    last.text.push(' ');
                    last.text.push_str(&piece);
                    last.lines.end = last.lines.end.max(part.lines.end);
                }
                _ => chunks.push(Chunk {
                    text: piece,
                    lines: part.lines.clone(),
                }),
            }
        }
    }
    chunks
}

/// Splits text longer than [`MAX`] at sentence ends (or spaces, failing that).
fn split_long(text: &str) -> Vec<String> {
    if text.len() <= MAX {
        return vec![text.to_owned()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    for word in text.split_inclusive(' ') {
        current.push_str(word);
        let full = current.len() >= MAX;
        let sentence = current.len() >= TARGET && ends_sentence(word);
        if full || sentence {
            out.push(current.trim().to_owned());
            current.clear();
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_owned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str) -> Vec<Chunk> {
        let lines: Vec<&str> = text.split('\n').collect();
        let doc = Document::parse(&lines);
        chunks(&doc, &lines, 0..lines.len())
    }

    #[test]
    fn reads_rendered_words_without_syntax_or_front_matter() {
        let got = read(
            "+++\ntitle = \"T\"\n+++\n\n# Plan\n\nSome **bold** and `code` and [a link](http://x).\nStill the same paragraph\n\n- [ ] buy milk\n- done thing\n\n```rust\nfn x() {}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n",
        );
        let all: String = got
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(all.contains("Plan."), "{all}");
        assert!(
            all.contains("Some bold and code and a link. Still the same paragraph."),
            "{all}"
        );
        assert!(all.contains("buy milk."), "{all}");
        assert!(all.contains("Code block."), "{all}");
        assert!(all.contains("1, 2."), "{all}");
        assert!(!all.contains("title"), "{all}");
        assert!(!all.contains("**") && !all.contains("fn x"), "{all}");
        assert!(got[0].lines.start >= 4);
    }

    #[test]
    fn long_text_is_split_and_lines_are_tracked() {
        let sentence = "This is a sentence that goes on for a while. ";
        let text = format!("# H\n\n{}\n\nTail.", sentence.repeat(40));
        let got = read(&text);
        assert!(got.len() > 3);
        assert!(got.iter().all(|chunk| chunk.text.len() <= MAX + 60));
        assert_eq!(got.last().unwrap().lines, 4..5);
        assert!(got.iter().any(|chunk| chunk.lines == (2..3)));
        assert!(read("").is_empty());
    }
}
