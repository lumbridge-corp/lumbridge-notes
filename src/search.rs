//! Full-text search across the notebook (Ctrl-G).

use std::ops::Range;
use std::path::PathBuf;

use crate::input::LineInput;
use crate::layout::str_width;
use crate::store::{self, NoteEntry};
use crate::text;

/// Results kept in total and per note.
pub const MAX_RESULTS: usize = 1000;
const MAX_PER_NOTE: usize = 50;
/// Characters of context kept before a match in a snippet.
const CONTEXT_BEFORE: usize = 24;
/// Bytes of big notes read in full for search (the listing keeps only the
/// first `store::MAX_LISTED_BYTES` of each note).
const FULL_TEXT_BUDGET: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// Index into [`Search::notes`].
    pub note: usize,
    /// Line in the note file (0-based, front matter included).
    pub line: usize,
    /// The matching line, sanitised and trimmed around the match.
    pub snippet: String,
    /// The match inside `snippet`.
    pub matched: Range<usize>,
}

#[derive(Debug)]
pub struct Search {
    pub root: PathBuf,
    /// `root` for display (computed once, not per frame).
    pub root_display: String,
    pub input: LineInput,
    pub notes: Vec<NoteEntry>,
    pub hits: Vec<Hit>,
    pub selected: usize,
    pub scroll: usize,
    /// More matches exist than were kept.
    pub truncated: bool,
    /// Some big notes are searched only in part (over the read budget).
    pub partial: bool,
}

impl Search {
    pub fn open(root: PathBuf) -> Search {
        let mut notes = store::list_notes(&root);
        let partial = read_full_texts(&mut notes, FULL_TEXT_BUDGET);
        let mut search = Search::with_notes(root, notes);
        search.partial = partial;
        search
    }

    pub fn with_notes(root: PathBuf, notes: Vec<NoteEntry>) -> Search {
        Search {
            root_display: crate::editor::display_path(&root),
            root,
            input: LineInput::default(),
            notes,
            hits: Vec::new(),
            selected: 0,
            scroll: 0,
            truncated: false,
            partial: false,
        }
    }

    pub fn selected_hit(&self) -> Option<&Hit> {
        self.hits.get(self.selected)
    }

    /// Re-runs the search after the query changed.
    pub fn run(&mut self) {
        self.hits.clear();
        self.selected = 0;
        self.scroll = 0;
        self.truncated = false;
        let query = self.input.text.trim();
        if query.is_empty() {
            return;
        }
        let ignore_case = text::smart_case(query);
        'notes: for (note_index, note) in self.notes.iter().enumerate() {
            let mut in_note = 0;
            for (line_index, line) in note.text.lines().enumerate() {
                let Some(range) = text::find_first(line, query, ignore_case) else {
                    continue;
                };
                if self.hits.len() >= MAX_RESULTS {
                    self.truncated = true;
                    break 'notes;
                }
                let (snippet, matched) = snippet(line, range);
                self.hits.push(Hit {
                    note: note_index,
                    line: line_index,
                    snippet,
                    matched,
                });
                in_note += 1;
                if in_note >= MAX_PER_NOTE {
                    break;
                }
            }
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.hits.is_empty() {
            self.selected = 0;
            return;
        }
        let last = self.hits.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
    }

    pub fn prepare(&mut self, height: usize) {
        self.scroll =
            crate::picker::keep_visible(self.selected, self.scroll, height, self.hits.len());
    }
}

/// Replaces the capped text of big notes with their whole text, within
/// `budget` bytes in all, so search covers every line. Returns whether some
/// note could not be read in full.
fn read_full_texts(notes: &mut [NoteEntry], budget: usize) -> bool {
    let mut left = budget;
    let mut partial = false;
    for note in notes.iter_mut().filter(|note| note.truncated) {
        let size = std::fs::metadata(&note.path).map_or(usize::MAX, |meta| {
            usize::try_from(meta.len()).unwrap_or(usize::MAX)
        });
        if size > left {
            partial = true;
            continue;
        }
        match std::fs::read(&note.path) {
            Ok(bytes) => {
                left = left.saturating_sub(bytes.len());
                // Keep the body offset: the front matter is in the part
                // already read.
                note.text = String::from_utf8_lossy(&bytes).into_owned();
                note.truncated = false;
            }
            Err(_) => partial = true,
        }
    }
    partial
}

/// The line trimmed so the match is near the start, and the match's range
/// in the result. Control characters are replaced so it is safe to draw.
fn snippet(line: &str, matched: Range<usize>) -> (String, Range<usize>) {
    let mut start = line
        .get(..matched.start)
        .map_or(0, |head| head.len() - head.trim_start().len());
    let before = line.get(start..matched.start).unwrap_or("");
    if before.chars().count() > CONTEXT_BEFORE {
        let skip = before.chars().count() - CONTEXT_BEFORE;
        start += before
            .char_indices()
            .nth(skip)
            .map_or(0, |(offset, _)| offset);
        while !line.is_char_boundary(start) {
            start += 1;
        }
    }
    let ellipsis = if start > 0 && line[..start].trim().is_empty() {
        ""
    } else if start > 0 {
        "…"
    } else {
        ""
    };
    let head = text::sanitize(line.get(start..matched.start).unwrap_or(""));
    let hit = text::sanitize(line.get(matched.clone()).unwrap_or(""));
    let tail = text::sanitize(line.get(matched.end..).unwrap_or("").trim_end());
    let mut out = String::with_capacity(line.len().min(512));
    out.push_str(ellipsis);
    out.push_str(&head);
    let hit_start = out.len();
    out.push_str(&hit);
    let hit_end = out.len();
    out.push_str(&tail);
    // Very long lines: keep a few hundred cells.
    if str_width(&out) > 400 {
        out = text::ellipsize(&out, 400);
    }
    let hit_end = hit_end.min(out.len());
    (out, hit_start.min(hit_end)..hit_end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn note(title: &str, text: &str) -> NoteEntry {
        NoteEntry {
            path: PathBuf::from(format!("{title}.md")),
            relative: format!("{title}.md"),
            title: title.to_owned(),
            tags: Vec::new(),
            modified: UNIX_EPOCH,
            text: text.to_owned(),
            truncated: false,
            body_start: 0,
        }
    }

    #[test]
    fn finds_lines_across_notes() {
        let notes = vec![
            note("a", "+++\ntitle = \"a\"\n+++\nThe Kernel panics\r\nok\n"),
            note("b", "nothing\nkernel pin is 6.8\n"),
        ];
        let mut search = Search::with_notes(PathBuf::from("/n"), notes);
        search.input = LineInput::new("kernel");
        search.run();
        assert_eq!(search.hits.len(), 2);
        assert_eq!((search.hits[0].note, search.hits[0].line), (0, 3));
        assert_eq!(search.hits[0].snippet, "The Kernel panics");
        assert_eq!(
            &search.hits[0].snippet[search.hits[0].matched.clone()],
            "Kernel"
        );
        assert_eq!((search.hits[1].note, search.hits[1].line), (1, 1));
        // Uppercase in the query makes it case-sensitive.
        search.input = LineInput::new("Kernel");
        search.run();
        assert_eq!(search.hits.len(), 1);
        search.input = LineInput::new("  ");
        search.run();
        assert!(search.hits.is_empty());
    }

    #[test]
    fn big_notes_are_searched_past_the_listing_cap() {
        let dir = crate::test_support::TempDir::new("search-big");
        let mut text = "filler line\n".repeat(store::MAX_LISTED_BYTES / 12 + 100);
        text.push_str("NEEDLE_XYZ at the end\n");
        assert!(text.len() > store::MAX_LISTED_BYTES);
        std::fs::write(dir.path().join("big.md"), &text).unwrap();
        let mut search = Search::open(dir.path().to_path_buf());
        assert!(!search.partial);
        search.input = LineInput::new("NEEDLE_XYZ");
        search.run();
        assert_eq!(search.hits.len(), 1);
        // Over the budget, it says so instead of silently missing it.
        let mut notes = store::list_notes(dir.path());
        assert!(notes[0].truncated);
        assert!(read_full_texts(&mut notes, 1000));
    }

    #[test]
    fn snippets_trim_long_context() {
        let line = format!("{}needle and more", "x".repeat(100));
        let (text, matched) = snippet(&line, 100..106);
        assert!(text.starts_with('…'));
        assert_eq!(&text[matched], "needle");
        let (text, matched) = snippet("\tindented\u{1}needle", 10..16);
        assert_eq!(text, "indented\u{fffd}needle");
        assert_eq!(&text[matched], "needle");
    }
}
