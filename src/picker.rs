//! The note picker: fuzzy filter over the notebook, newest first.

use std::path::PathBuf;

use crate::input::LineInput;
use crate::store::{self, NoteEntry};

#[derive(Debug)]
pub struct Picker {
    pub root: PathBuf,
    /// `root` for display (computed once, not per frame).
    pub root_display: String,
    pub input: LineInput,
    pub notes: Vec<NoteEntry>,
    /// Indices into `notes` that match the query, best first.
    pub filtered: Vec<usize>,
    pub selected: usize,
    /// First list row shown (set by [`Picker::prepare`]).
    pub scroll: usize,
}

impl Picker {
    /// Lists the notes in `root` (a missing folder is an empty notebook).
    pub fn open(root: PathBuf) -> Picker {
        let notes = store::list_notes(&root);
        Picker::with_notes(root, notes)
    }

    pub fn with_notes(root: PathBuf, notes: Vec<NoteEntry>) -> Picker {
        let mut picker = Picker {
            root_display: crate::editor::display_path(&root),
            root,
            input: LineInput::default(),
            notes,
            filtered: Vec::new(),
            selected: 0,
            scroll: 0,
        };
        picker.refilter();
        picker
    }

    pub fn query(&self) -> &str {
        &self.input.text
    }

    pub fn selected_note(&self) -> Option<&NoteEntry> {
        self.filtered
            .get(self.selected)
            .and_then(|&index| self.notes.get(index))
    }

    /// Re-runs the filter after the query changed.
    pub fn refilter(&mut self) {
        let terms: Vec<String> = self
            .input
            .text
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let mut scored: Vec<(i64, usize)> = self
            .notes
            .iter()
            .enumerate()
            .filter_map(|(index, note)| note_score(note, &terms).map(|score| (score, index)))
            .collect();
        // Stable: equal scores keep newest first.
        scored.sort_by_key(|&(score, _)| std::cmp::Reverse(score));
        self.filtered = scored.into_iter().map(|(_, index)| index).collect();
        self.selected = 0;
        self.scroll = 0;
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        let last = self.filtered.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
    }

    /// Keeps the selection inside a list of `height` rows.
    pub fn prepare(&mut self, height: usize) {
        self.scroll = keep_visible(self.selected, self.scroll, height, self.filtered.len());
    }
}

/// The first row to show so `selected` stays inside `height` rows.
pub fn keep_visible(selected: usize, scroll: usize, height: usize, len: usize) -> usize {
    let height = height.max(1);
    let mut scroll = scroll.min(len.saturating_sub(1));
    if selected < scroll {
        scroll = selected;
    } else if selected >= scroll + height {
        scroll = selected + 1 - height;
    }
    scroll
}

/// Every term must match the title, a tag or the path; `None` if not.
fn note_score(note: &NoteEntry, terms: &[String]) -> Option<i64> {
    let mut total = 0;
    for term in terms {
        let title = fuzzy_score(term, &note.title).map(|score| score * 3);
        let tags = note
            .tags
            .iter()
            .filter_map(|tag| fuzzy_score(term, tag).map(|score| score * 2))
            .max();
        let path = fuzzy_score(term, &note.relative);
        total += [title, tags, path].into_iter().flatten().max()?;
    }
    Some(total)
}

/// A subsequence match of `needle` (lowercase) in `hay`, scored: contiguous
/// runs, word starts and a prefix score higher. `None` when it does not match.
pub fn fuzzy_score(needle: &str, hay: &str) -> Option<i64> {
    if needle.is_empty() {
        return Some(0);
    }
    let hay_lower: Vec<char> = hay.chars().flat_map(char::to_lowercase).collect();
    let needle: Vec<char> = needle.chars().collect();
    let mut score = 0i64;
    let mut hay_index = 0;
    let mut previous: Option<usize> = None;
    for &ch in &needle {
        let found = (hay_index..hay_lower.len()).find(|&index| hay_lower[index] == ch)?;
        score += 1;
        if previous.is_some_and(|prev| prev + 1 == found) {
            score += 5;
        }
        let word_start = found == 0
            || hay_lower
                .get(found - 1)
                .is_some_and(|prev| !prev.is_alphanumeric());
        if word_start {
            score += 3;
        }
        if found == 0 {
            score += 2;
        }
        previous = Some(found);
        hay_index = found + 1;
    }
    let hay_string: String = hay_lower.iter().collect();
    let needle_string: String = needle.iter().collect();
    if hay_string.contains(&needle_string) {
        score += 10;
    }
    // Shorter haystacks rank a little higher.
    score -= (hay_lower.len() / 16) as i64;
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn note(title: &str, tags: &[&str], relative: &str, age: u64) -> NoteEntry {
        NoteEntry {
            path: PathBuf::from(relative),
            relative: relative.to_owned(),
            title: title.to_owned(),
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
            modified: UNIX_EPOCH + Duration::from_secs(1_000_000 - age),
            text: String::new(),
            truncated: false,
            body_start: 0,
        }
    }

    #[test]
    fn fuzzy_scores_prefer_contiguous_and_word_starts() {
        assert!(fuzzy_score("dsk", "desk").is_some());
        assert!(fuzzy_score("xyz", "desk").is_none());
        let exact = fuzzy_score("desk", "desk down").unwrap_or(0);
        let scattered = fuzzy_score("desk", "the eldest kid").unwrap_or(0);
        assert!(exact > scattered);
        assert_eq!(fuzzy_score("", "anything"), Some(0));
    }

    #[test]
    fn filters_by_title_tags_and_path_newest_first() {
        let notes = vec![
            note("Desk down", &["fleet"], "desk-down.md", 1),
            note("Groceries", &["home"], "groceries.md", 2),
            note("Fleet plan", &[], "work/fleet-plan.md", 3),
        ];
        let mut picker = Picker::with_notes(PathBuf::from("/n"), notes);
        assert_eq!(picker.filtered, vec![0, 1, 2]);
        picker.input = LineInput::new("fleet");
        picker.refilter();
        let titles: Vec<&str> = picker
            .filtered
            .iter()
            .map(|&index| picker.notes[index].title.as_str())
            .collect();
        assert_eq!(titles, vec!["Fleet plan", "Desk down"]);
        picker.input = LineInput::new("home groc");
        picker.refilter();
        assert_eq!(picker.filtered, vec![1]);
        picker.input = LineInput::new("nothing-matches");
        picker.refilter();
        assert!(picker.selected_note().is_none());
    }

    #[test]
    fn keeps_selection_visible() {
        assert_eq!(keep_visible(0, 0, 5, 10), 0);
        assert_eq!(keep_visible(7, 0, 5, 10), 3);
        assert_eq!(keep_visible(2, 3, 5, 10), 2);
    }
}
