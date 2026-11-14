//! The text being edited: lines, cursor, selection, undo/redo.
//!
//! Positions are `(line, col)` where `col` is a **byte offset** into the line
//! that always sits on a grapheme-cluster boundary. Byte offsets make the
//! mapping to Markdown source ranges direct and keep cursor motion O(1) on
//! very long lines; [`Buffer::grapheme_col`] gives the user-facing column.
//!
//! Lines are stored without line endings. The file's line ending (LF or CRLF,
//! from its first line break) and whether it ended with a newline are kept and
//! restored by [`Buffer::to_text`]. A file that mixes LF and CRLF keeps each
//! line's own ending; new line breaks get the first line break's ending.

use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};

/// Undo groups kept; older ones are dropped.
const UNDO_LIMIT: usize = 5000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct Pos {
    pub line: usize,
    /// Byte offset into the line, on a grapheme boundary.
    pub col: usize,
}

impl Pos {
    pub const fn new(line: usize, col: usize) -> Pos {
        Pos { line, col }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineEnding {
    #[default]
    Lf,
    CrLf,
}

impl LineEnding {
    pub fn as_str(self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::CrLf => "\r\n",
        }
    }
}

/// Lines that changed since the last [`Buffer::take_damage`]: lines
/// `start..old_end` of the old text became lines `start..new_end` of the new.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Damage {
    pub start: usize,
    pub old_end: usize,
    pub new_end: usize,
}

impl Damage {
    /// Everything changed.
    pub fn full(old_len: usize, new_len: usize) -> Damage {
        Damage {
            start: 0,
            old_end: old_len,
            new_end: new_len,
        }
    }

    /// Combines `self` (text0 -> text1) with `next` (text1 -> text2) into one
    /// damage from text0 to text2.
    pub fn merge(self, next: Damage) -> Damage {
        let start = self.start.min(next.start);
        // The union's end in text1 coordinates.
        let end1 = self.new_end.max(next.old_end);
        Damage {
            start,
            old_end: self.old_end + (end1 - self.new_end),
            new_end: next.new_end + (end1 - next.old_end),
        }
    }

    /// Line count difference (new - old).
    pub fn delta(&self) -> isize {
        self.new_end as isize - self.old_end as isize
    }
}

/// How an edit groups with the previous one for undo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    /// Typed text: consecutive typing merges (split at word starts).
    Typing,
    /// Backspace runs merge.
    DeleteBack,
    /// Delete-key runs merge.
    DeleteForward,
    /// Never merges.
    Other,
}

#[derive(Debug, Clone)]
struct Edit {
    start: Pos,
    removed: String,
    inserted: String,
}

type CursorState = (Pos, Option<Pos>);

#[derive(Debug, Clone)]
struct Group {
    edits: Vec<Edit>,
    before: CursorState,
    after: CursorState,
    kind: EditKind,
    id: u64,
}

#[derive(Debug, Clone)]
pub struct Buffer {
    lines: Vec<String>,
    /// The ending of new line breaks (the file's first one).
    ending: LineEnding,
    trailing_newline: bool,
    /// End the text with a line break when it does not already (a note this
    /// editor creates, so it ends like the new-note template). Never set for
    /// a file read from disk, whose last line is kept as it was.
    final_newline: bool,
    /// Per line, whether its line break is CRLF; only kept for a file that
    /// mixes endings (`None` means every break is `ending`).
    crlf: Option<Vec<bool>>,
    cursor: Pos,
    anchor: Option<Pos>,
    undo: Vec<Group>,
    redo: Vec<Group>,
    /// Set by cursor motion so the next edit starts a new undo group.
    sealed: bool,
    compound: Option<Group>,
    compound_depth: usize,
    revision: u64,
    /// Identity of the current text for dirty tracking (survives undo/redo).
    state: u64,
    next_id: u64,
    saved_state: u64,
    damage: Option<Damage>,
}

impl Default for Buffer {
    fn default() -> Self {
        Buffer::from_text("")
    }
}

impl Buffer {
    pub fn from_text(text: &str) -> Buffer {
        let split = split_text(text);
        Buffer {
            lines: split.lines,
            ending: split.ending,
            trailing_newline: split.trailing_newline,
            final_newline: false,
            crlf: split.crlf,
            cursor: Pos::default(),
            anchor: None,
            undo: Vec::new(),
            redo: Vec::new(),
            sealed: true,
            compound: None,
            compound_depth: 0,
            revision: 0,
            state: 0,
            next_id: 1,
            saved_state: 0,
            damage: None,
        }
    }

    /// Replaces the whole text (e.g. a reload from disk): history is cleared,
    /// the buffer is clean, and the cursor is kept where possible.
    pub fn reload(&mut self, text: &str) {
        let old_len = self.lines.len();
        let split = split_text(text);
        self.lines = split.lines;
        self.ending = split.ending;
        self.trailing_newline = split.trailing_newline;
        self.final_newline = false;
        self.crlf = split.crlf;
        self.undo.clear();
        self.redo.clear();
        self.compound = None;
        self.compound_depth = 0;
        self.sealed = true;
        self.cursor = self.clamp(self.cursor);
        self.anchor = self.anchor.map(|anchor| self.clamp(anchor));
        self.revision += 1;
        self.state = self.next_id;
        self.next_id += 1;
        self.saved_state = self.state;
        self.add_damage(Damage::full(old_len, self.lines.len()));
    }

    /// The text with the original line endings and trailing newline.
    pub fn to_text(&self) -> String {
        let mut text = self.joined();
        if self.final_newline && !text.is_empty() && !text.ends_with('\n') {
            text.push_str(self.ending.as_str());
        }
        text
    }

    /// Makes [`Buffer::to_text`] end with a line break (a new note).
    pub fn set_final_newline(&mut self) {
        self.final_newline = true;
    }

    /// Turns the trailing line break into an empty last line the cursor can
    /// go to, without changing the text: `# T\n\n` becomes the lines
    /// `# T`, ``, `` rather than `# T`, `` plus a flag. With
    /// [`Buffer::set_final_newline`], text typed there still ends with a
    /// line break.
    pub fn open_last_line(&mut self) {
        if !self.trailing_newline {
            return;
        }
        let cr = self.ending == LineEnding::CrLf;
        if let Some(crlf) = &mut self.crlf {
            crlf.push(cr);
        }
        self.lines.push(String::new());
        self.trailing_newline = false;
        let len = self.lines.len();
        self.add_damage(Damage::full(len - 1, len));
        self.revision += 1;
    }

    fn joined(&self) -> String {
        let Some(crlf) = &self.crlf else {
            let ending = self.ending.as_str();
            let mut text = self.lines.join(ending);
            if self.trailing_newline {
                text.push_str(ending);
            }
            return text;
        };
        let total: usize = self.lines.iter().map(|line| line.len() + 2).sum();
        let mut text = String::with_capacity(total);
        let last = self.lines.len().saturating_sub(1);
        for (index, line) in self.lines.iter().enumerate() {
            text.push_str(line);
            if index < last || self.trailing_newline {
                let cr = crlf.get(index).copied().unwrap_or(false);
                text.push_str(if cr { "\r\n" } else { "\n" });
            }
        }
        text
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn line(&self, index: usize) -> &str {
        self.lines.get(index).map(String::as_str).unwrap_or("")
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn ending(&self) -> LineEnding {
        self.ending
    }

    pub fn trailing_newline(&self) -> bool {
        self.trailing_newline
    }

    /// Bumped on every change to the text.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn is_dirty(&self) -> bool {
        self.state != self.saved_state
    }

    /// The current text is what is on disk now. The next edit starts a new
    /// undo group: merging it into the saved one would keep the saved
    /// state's identity and leave the changed text looking clean.
    pub fn mark_saved(&mut self) {
        self.saved_state = self.state;
        self.sealed = true;
    }

    /// Lines changed since the last call (for incremental re-parsing).
    pub fn take_damage(&mut self) -> Option<Damage> {
        self.damage.take()
    }

    fn add_damage(&mut self, damage: Damage) {
        self.damage = Some(match self.damage {
            Some(previous) => previous.merge(damage),
            None => damage,
        });
    }

    // -- cursor & selection -------------------------------------------------

    pub fn cursor(&self) -> Pos {
        self.cursor
    }

    pub fn anchor(&self) -> Option<Pos> {
        self.anchor
    }

    /// The selection as an ordered `(start, end)`, if it is not empty.
    pub fn selection(&self) -> Option<(Pos, Pos)> {
        let anchor = self.anchor?;
        if anchor == self.cursor {
            return None;
        }
        Some((anchor.min(self.cursor), anchor.max(self.cursor)))
    }

    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection()?;
        Some(self.text_range(start, end))
    }

    /// Moves the cursor to `pos` (clamped). With `extend`, the selection
    /// grows from the current anchor (or cursor); otherwise it is cleared.
    pub fn set_cursor(&mut self, pos: Pos, extend: bool) {
        let pos = self.clamp(pos);
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
        self.cursor = pos;
        self.sealed = true;
    }

    /// Sets the selection explicitly (anchor to cursor).
    pub fn set_selection(&mut self, anchor: Pos, cursor: Pos) {
        self.anchor = Some(self.clamp(anchor));
        self.cursor = self.clamp(cursor);
        self.sealed = true;
    }

    pub fn clear_selection(&mut self) {
        self.anchor = None;
    }

    pub fn select_all(&mut self) {
        let end = self.end_pos();
        self.set_selection(Pos::default(), end);
    }

    pub fn end_pos(&self) -> Pos {
        let line = self.lines.len().saturating_sub(1);
        Pos::new(line, self.line(line).len())
    }

    /// Clamps to an existing line and a grapheme boundary at or before `col`.
    pub fn clamp(&self, pos: Pos) -> Pos {
        let line = pos.line.min(self.lines.len().saturating_sub(1));
        let text = self.line(line);
        let mut col = pos.col.min(text.len());
        while !text.is_char_boundary(col) {
            col -= 1;
        }
        if !is_grapheme_boundary(text, col) {
            col = prev_grapheme(text, col);
        }
        Pos::new(line, col)
    }

    /// The user-facing column: graphemes before the cursor.
    pub fn grapheme_col(&self, pos: Pos) -> usize {
        let text = self.line(pos.line);
        let head = text.get(..pos.col.min(text.len())).unwrap_or("");
        if head.is_ascii() {
            head.len()
        } else {
            head.graphemes(true).count()
        }
    }

    pub fn move_left(&mut self, extend: bool) {
        if !extend {
            if let Some((start, _)) = self.selection() {
                return self.set_cursor(start, false);
            }
        }
        let Pos { line, col } = self.cursor;
        let pos = if col > 0 {
            Pos::new(line, prev_grapheme(self.line(line), col))
        } else if line > 0 {
            Pos::new(line - 1, self.line(line - 1).len())
        } else {
            self.cursor
        };
        self.set_cursor(pos, extend);
    }

    pub fn move_right(&mut self, extend: bool) {
        if !extend {
            if let Some((_, end)) = self.selection() {
                return self.set_cursor(end, false);
            }
        }
        let Pos { line, col } = self.cursor;
        let text = self.line(line);
        let pos = if col < text.len() {
            Pos::new(line, next_grapheme(text, col))
        } else if line + 1 < self.lines.len() {
            Pos::new(line + 1, 0)
        } else {
            self.cursor
        };
        self.set_cursor(pos, extend);
    }

    pub fn move_word_left(&mut self, extend: bool) {
        let pos = self.word_left_of(self.cursor);
        self.set_cursor(pos, extend);
    }

    pub fn move_word_right(&mut self, extend: bool) {
        let pos = self.word_right_of(self.cursor);
        self.set_cursor(pos, extend);
    }

    /// Smart home: first non-blank column, or column 0 when already there.
    pub fn move_home(&mut self, extend: bool) {
        let text = self.line(self.cursor.line);
        let first = text.len() - text.trim_start().len();
        let col = if self.cursor.col == first { 0 } else { first };
        self.set_cursor(Pos::new(self.cursor.line, col), extend);
    }

    pub fn move_end(&mut self, extend: bool) {
        let line = self.cursor.line;
        self.set_cursor(Pos::new(line, self.line(line).len()), extend);
    }

    pub fn move_doc_start(&mut self, extend: bool) {
        self.set_cursor(Pos::default(), extend);
    }

    pub fn move_doc_end(&mut self, extend: bool) {
        self.set_cursor(self.end_pos(), extend);
    }

    pub fn word_left_of(&self, pos: Pos) -> Pos {
        if pos.col == 0 {
            if pos.line == 0 {
                return pos;
            }
            return Pos::new(pos.line - 1, self.line(pos.line - 1).len());
        }
        let text = self.line(pos.line);
        let head = &text[..pos.col.min(text.len())];
        let mut col = head.len();
        let mut class = None;
        for (start, grapheme) in head.grapheme_indices(true).rev() {
            let this = char_class(grapheme);
            match class {
                None if this == CharClass::Space => {}
                None => class = Some(this),
                Some(current) if current != this => break,
                Some(_) => {}
            }
            col = start;
        }
        Pos::new(pos.line, col)
    }

    pub fn word_right_of(&self, pos: Pos) -> Pos {
        let text = self.line(pos.line);
        if pos.col >= text.len() {
            if pos.line + 1 < self.lines.len() {
                return Pos::new(pos.line + 1, 0);
            }
            return pos;
        }
        let mut col = pos.col;
        let mut class = None;
        for (start, grapheme) in text[pos.col..].grapheme_indices(true) {
            let this = char_class(grapheme);
            match class {
                None if this == CharClass::Space => {}
                None => class = Some(this),
                Some(current) if current != this => break,
                Some(_) => {}
            }
            col = pos.col + start + grapheme.len();
        }
        Pos::new(pos.line, col)
    }

    // -- reading --------------------------------------------------------------

    /// The text between two positions, joined with `\n`.
    pub fn text_range(&self, start: Pos, end: Pos) -> String {
        let (start, end) = (self.clamp(start.min(end)), self.clamp(start.max(end)));
        if start.line == end.line {
            return self.line(start.line)[start.col..end.col].to_owned();
        }
        let mut out = String::new();
        out.push_str(&self.line(start.line)[start.col..]);
        for line in start.line + 1..end.line {
            out.push('\n');
            out.push_str(self.line(line));
        }
        out.push('\n');
        out.push_str(&self.line(end.line)[..end.col]);
        out
    }

    // -- editing ----------------------------------------------------------------

    /// Starts a compound edit: everything until the matching
    /// [`Buffer::end_compound`] undoes as one step. Nests.
    pub fn begin_compound(&mut self) {
        if self.compound_depth == 0 {
            self.compound = Some(Group {
                edits: Vec::new(),
                before: (self.cursor, self.anchor),
                after: (self.cursor, self.anchor),
                kind: EditKind::Other,
                id: 0,
            });
        }
        self.compound_depth += 1;
    }

    pub fn end_compound(&mut self) {
        if self.compound_depth == 0 {
            return;
        }
        self.compound_depth -= 1;
        if self.compound_depth > 0 {
            return;
        }
        if let Some(mut group) = self.compound.take() {
            if group.edits.is_empty() {
                return;
            }
            group.after = (self.cursor, self.anchor);
            group.id = self.next_id;
            self.next_id += 1;
            self.state = group.id;
            self.push_undo(group);
            self.sealed = true;
        }
    }

    /// Replaces `start..end` with `text` (newlines normalised to `\n`) as an
    /// undoable edit and returns the end of the inserted text. The cursor is
    /// not moved; callers place it.
    pub fn replace(&mut self, start: Pos, end: Pos, text: &str) -> Pos {
        self.edit(start, end, text, EditKind::Other)
    }

    fn edit(&mut self, start: Pos, end: Pos, text: &str, kind: EditKind) -> Pos {
        let (start, end) = (self.clamp(start.min(end)), self.clamp(start.max(end)));
        let text = normalize_newlines(text);
        let removed = self.text_range(start, end);
        if removed.is_empty() && text.is_empty() {
            return start;
        }
        let before = (self.cursor, self.anchor);
        let new_end = self.raw_replace(start, end, &text);
        let edit = Edit {
            start,
            removed,
            inserted: text.into_owned(),
        };
        self.record(edit, kind, before);
        new_end
    }

    /// Inserts typed `text` at the cursor, replacing the selection. Typing
    /// runs undo together.
    pub fn insert_text(&mut self, text: &str) {
        let typing = !text.contains(['\n', '\r']) && self.selection().is_none();
        let kind = if typing {
            EditKind::Typing
        } else {
            EditKind::Other
        };
        self.insert_as(text, kind);
    }

    /// Inserts pasted `text`: always its own undo step, and the next
    /// keystroke starts a new one.
    pub fn paste_text(&mut self, text: &str) {
        self.insert_as(text, EditKind::Other);
        self.sealed = true;
    }

    fn insert_as(&mut self, text: &str, kind: EditKind) {
        let (start, end) = self.selection().unwrap_or((self.cursor, self.cursor));
        let end = self.edit(start, end, text, kind);
        self.anchor = None;
        self.cursor = end;
        self.update_after();
    }

    /// Deletes the selection, if any. Returns whether something was deleted.
    pub fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection() else {
            return false;
        };
        self.edit(start, end, "", EditKind::Other);
        self.anchor = None;
        self.cursor = start;
        self.update_after();
        true
    }

    pub fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        let Pos { line, col } = self.cursor;
        let start = if col > 0 {
            Pos::new(line, prev_grapheme(self.line(line), col))
        } else if line > 0 {
            Pos::new(line - 1, self.line(line - 1).len())
        } else {
            return;
        };
        self.edit(start, self.cursor, "", EditKind::DeleteBack);
        self.cursor = start;
        self.update_after();
    }

    pub fn delete_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        let Pos { line, col } = self.cursor;
        let text = self.line(line);
        let end = if col < text.len() {
            Pos::new(line, next_grapheme(text, col))
        } else if line + 1 < self.lines.len() {
            Pos::new(line + 1, 0)
        } else {
            return;
        };
        self.edit(self.cursor, end, "", EditKind::DeleteForward);
        self.update_after();
    }

    pub fn delete_word_back(&mut self) {
        if self.delete_selection() {
            return;
        }
        let start = self.word_left_of(self.cursor);
        if start == self.cursor {
            return;
        }
        self.edit(start, self.cursor, "", EditKind::Other);
        self.cursor = start;
        self.update_after();
    }

    pub fn delete_word_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        let end = self.word_right_of(self.cursor);
        if end == self.cursor {
            return;
        }
        self.edit(self.cursor, end, "", EditKind::Other);
        self.update_after();
    }

    /// Records the cursor after an edit into the open undo group.
    fn update_after(&mut self) {
        let after = (self.cursor, self.anchor);
        if let Some(group) = self.compound.as_mut() {
            group.after = after;
        } else if let Some(group) = self.undo.last_mut() {
            if group.id == self.state {
                group.after = after;
            }
        }
    }

    fn record(&mut self, edit: Edit, kind: EditKind, before: CursorState) {
        self.redo.clear();
        if let Some(group) = self.compound.as_mut() {
            group.edits.push(edit);
            return;
        }
        let mergeable = !self.sealed
            && self.undo.last().is_some_and(|group| {
                group.id == self.state
                    && group.kind == kind
                    && group
                        .edits
                        .last()
                        .is_some_and(|last| continues(last, &edit, kind))
            });
        self.sealed = false;
        if mergeable {
            if let Some(group) = self.undo.last_mut() {
                group.edits.push(edit);
                return;
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        self.state = id;
        self.push_undo(Group {
            edits: vec![edit],
            before,
            after: before,
            kind,
            id,
        });
    }

    fn push_undo(&mut self, group: Group) {
        self.undo.push(group);
        if self.undo.len() > UNDO_LIMIT {
            let excess = self.undo.len() - UNDO_LIMIT;
            self.undo.drain(..excess);
        }
    }

    pub fn undo(&mut self) -> bool {
        if self.compound_depth > 0 {
            return false;
        }
        let Some(group) = self.undo.pop() else {
            return false;
        };
        for edit in group.edits.iter().rev() {
            let end = end_of_insert(edit.start, &edit.inserted);
            self.raw_replace(edit.start, end, &edit.removed);
        }
        self.cursor = self.clamp(group.before.0);
        self.anchor = group.before.1.map(|anchor| self.clamp(anchor));
        self.state = self.undo.last().map_or(0, |group| group.id);
        self.redo.push(group);
        self.sealed = true;
        true
    }

    pub fn redo(&mut self) -> bool {
        if self.compound_depth > 0 {
            return false;
        }
        let Some(group) = self.redo.pop() else {
            return false;
        };
        for edit in &group.edits {
            let end = end_of_insert(edit.start, &edit.removed);
            self.raw_replace(edit.start, end, &edit.inserted);
        }
        self.cursor = self.clamp(group.after.0);
        self.anchor = group.after.1.map(|anchor| self.clamp(anchor));
        self.state = group.id;
        self.undo.push(group);
        self.sealed = true;
        true
    }

    /// The primitive edit: no undo, records damage and bumps the revision.
    /// `start..end` must be clamped and ordered; `text` uses `\n` only.
    fn raw_replace(&mut self, start: Pos, end: Pos, text: &str) -> Pos {
        let suffix = self.lines[end.line][end.col..].to_owned();
        let prefix_len = start.col;
        let mut pieces = text.split('\n');
        let first = pieces.next().unwrap_or("");
        let mut new_lines: Vec<String> = Vec::new();
        let mut head = String::with_capacity(prefix_len + first.len());
        head.push_str(&self.lines[start.line][..prefix_len]);
        head.push_str(first);
        new_lines.push(head);
        for piece in pieces {
            new_lines.push(piece.to_owned());
        }
        let last_index = new_lines.len() - 1;
        let end_pos = Pos::new(start.line + last_index, new_lines[last_index].len());
        new_lines[last_index].push_str(&suffix);
        let inserted = new_lines.len();
        self.lines.splice(start.line..=end.line, new_lines);
        let default_crlf = self.ending == LineEnding::CrLf;
        if let Some(crlf) = self.crlf.as_mut() {
            // New breaks get the default ending; the last line keeps the
            // ending of the old `end` line.
            let last = crlf.get(end.line).copied().unwrap_or(default_crlf);
            let mut flags = vec![default_crlf; inserted - 1];
            flags.push(last);
            let to = (end.line + 1).min(crlf.len());
            crlf.splice(start.line.min(to)..to, flags);
        }
        self.revision += 1;
        self.add_damage(Damage {
            start: start.line,
            old_end: end.line + 1,
            new_end: start.line + inserted,
        });
        end_pos
    }
}

/// Whether `edit` continues the run ending in `last` for undo grouping.
fn continues(last: &Edit, edit: &Edit, kind: EditKind) -> bool {
    match kind {
        EditKind::Typing => {
            if !edit.removed.is_empty() || !last.removed.is_empty() {
                return false;
            }
            if end_of_insert(last.start, &last.inserted) != edit.start {
                return false;
            }
            // A new word after whitespace starts a new undo step.
            let last_space = last.inserted.ends_with(char::is_whitespace);
            let new_word = edit.inserted.starts_with(|ch: char| !ch.is_whitespace());
            !(last_space && new_word)
        }
        EditKind::DeleteBack => {
            edit.inserted.is_empty() && end_of_insert(edit.start, &edit.removed) == last.start
        }
        EditKind::DeleteForward => edit.inserted.is_empty() && edit.start == last.start,
        EditKind::Other => false,
    }
}

fn end_of_insert(start: Pos, text: &str) -> Pos {
    match text.rfind('\n') {
        None => Pos::new(start.line, start.col + text.len()),
        Some(index) => Pos::new(
            start.line + text.matches('\n').count(),
            text.len() - index - 1,
        ),
    }
}

struct Split {
    lines: Vec<String>,
    ending: LineEnding,
    trailing_newline: bool,
    crlf: Option<Vec<bool>>,
}

/// Splits a file into lines. Every line break's `\r` is removed, whatever
/// the file's main ending, so a mixed file never leaves a stray `\r` in a
/// line; each break's own ending is remembered when they differ.
fn split_text(text: &str) -> Split {
    let mut lines = Vec::new();
    let mut flags = Vec::new();
    let mut first = None;
    let mut mixed = false;
    for piece in text.split_inclusive('\n') {
        let Some(line) = piece.strip_suffix('\n') else {
            // The last line, without a line break.
            lines.push(piece.to_owned());
            flags.push(first.unwrap_or(false));
            continue;
        };
        let (line, cr) = match line.strip_suffix('\r') {
            Some(line) => (line, true),
            None => (line, false),
        };
        let main = *first.get_or_insert(cr);
        mixed |= cr != main;
        lines.push(line.to_owned());
        flags.push(cr);
    }
    // "" is one empty line; "a\n" is ["a"] plus the trailing newline.
    if lines.is_empty() {
        lines.push(String::new());
        flags.push(false);
    }
    let trailing_newline = text.ends_with('\n');
    let ending = if first == Some(true) {
        LineEnding::CrLf
    } else {
        LineEnding::Lf
    };
    Split {
        lines,
        ending,
        trailing_newline,
        crlf: mixed.then_some(flags),
    }
}

/// `\r\n` and lone `\r` become `\n`.
pub fn normalize_newlines(text: &str) -> std::borrow::Cow<'_, str> {
    if text.contains('\r') {
        std::borrow::Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Space,
    Word,
    Punct,
}

fn char_class(grapheme: &str) -> CharClass {
    match grapheme.chars().next() {
        Some(ch) if ch.is_whitespace() => CharClass::Space,
        Some(ch) if ch.is_alphanumeric() || ch == '_' => CharClass::Word,
        _ => CharClass::Punct,
    }
}

fn is_ascii_byte(text: &str, index: usize) -> bool {
    text.as_bytes()
        .get(index)
        .is_some_and(|byte| byte.is_ascii())
}

/// The grapheme boundary after `at` (or `text.len()`).
pub fn next_grapheme(text: &str, at: usize) -> usize {
    if at >= text.len() {
        return text.len();
    }
    let bytes = text.as_bytes();
    if bytes[at].is_ascii()
        && bytes[at] != b'\r'
        && (at + 1 == text.len() || is_ascii_byte(text, at + 1))
    {
        return at + 1;
    }
    let mut cursor = GraphemeCursor::new(at, text.len(), true);
    match cursor.next_boundary(text, 0) {
        Ok(Some(next)) => next,
        _ => {
            let mut next = at + 1;
            while next < text.len() && !text.is_char_boundary(next) {
                next += 1;
            }
            next
        }
    }
}

/// The grapheme boundary before `at` (or 0).
pub fn prev_grapheme(text: &str, at: usize) -> usize {
    let at = at.min(text.len());
    if at == 0 {
        return 0;
    }
    let bytes = text.as_bytes();
    if bytes[at - 1].is_ascii()
        && bytes[at - 1] != b'\n'
        && (at == text.len() || is_ascii_byte(text, at))
        && (at == 1 || is_ascii_byte(text, at - 2))
        && !(bytes[at - 1] == b'\r' && bytes.get(at) == Some(&b'\n'))
    {
        return at - 1;
    }
    let mut cursor = GraphemeCursor::new(at, text.len(), true);
    match cursor.prev_boundary(text, 0) {
        Ok(Some(prev)) => prev,
        _ => {
            let mut prev = at - 1;
            while prev > 0 && !text.is_char_boundary(prev) {
                prev -= 1;
            }
            prev
        }
    }
}

pub fn is_grapheme_boundary(text: &str, at: usize) -> bool {
    if at == 0 || at >= text.len() {
        return at <= text.len();
    }
    if !text.is_char_boundary(at) {
        return false;
    }
    if is_ascii_byte(text, at) && is_ascii_byte(text, at - 1) {
        return !(text.as_bytes()[at - 1] == b'\r' && text.as_bytes()[at] == b'\n');
    }
    let mut cursor = GraphemeCursor::new(at, text.len(), true);
    cursor.is_boundary(text, 0).unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(text: &str) -> Buffer {
        Buffer::from_text(text)
    }

    #[test]
    fn preserves_line_endings_and_trailing_newline() {
        for text in [
            "",
            "a",
            "a\n",
            "a\nb",
            "a\nb\n",
            "a\r\nb\r\n",
            "a\r\nb",
            "\n",
            "\r\n",
            "\n\n",
            "x\r\n\r\n",
        ] {
            let buffer = buf(text);
            assert_eq!(buffer.to_text(), text, "{text:?}");
        }
        let buffer = buf("a\r\nb\r\n");
        assert_eq!(buffer.lines(), ["a", "b"]);
        assert_eq!(buffer.ending(), LineEnding::CrLf);
        assert!(buffer.trailing_newline());
        let buffer = buf("a\nb");
        assert_eq!(buffer.ending(), LineEnding::Lf);
        assert!(!buffer.trailing_newline());
        // A stray CR inside an LF file is kept verbatim.
        assert_eq!(buf("a\rb\n").to_text(), "a\rb\n");
    }

    #[test]
    fn crlf_edits_write_crlf() {
        let mut buffer = buf("one\r\ntwo\r\n");
        buffer.set_cursor(Pos::new(0, 3), false);
        buffer.insert_text("\r\nmid");
        assert_eq!(buffer.lines(), ["one", "mid", "two"]);
        assert_eq!(buffer.to_text(), "one\r\nmid\r\ntwo\r\n");
    }

    #[test]
    fn inserts_and_deletes_across_lines() {
        let mut buffer = buf("hello world");
        buffer.set_cursor(Pos::new(0, 5), false);
        buffer.insert_text("\nnew\n");
        assert_eq!(buffer.lines(), ["hello", "new", " world"]);
        assert_eq!(buffer.cursor(), Pos::new(2, 0));
        buffer.backspace();
        assert_eq!(buffer.lines(), ["hello", "new world"]);
        assert_eq!(buffer.cursor(), Pos::new(1, 3));
        buffer.set_cursor(Pos::new(0, 5), false);
        buffer.delete_forward();
        assert_eq!(buffer.lines(), ["hellonew world"]);
        buffer.set_cursor(Pos::new(0, 0), false);
        buffer.backspace();
        assert_eq!(buffer.lines(), ["hellonew world"]);
        buffer.move_doc_end(false);
        buffer.delete_forward();
        assert_eq!(buffer.lines(), ["hellonew world"]);
    }

    #[test]
    fn grapheme_aware_motion_and_deletion() {
        // e + combining acute, family emoji (ZWJ), CJK.
        let text = "ae\u{301}👨‍👩‍👧中b";
        let mut buffer = buf(text);
        let mut cols = vec![0];
        for _ in 0..6 {
            buffer.move_right(false);
            cols.push(buffer.cursor().col);
        }
        let e_end = 1 + "e\u{301}".len();
        let family_end = e_end + "👨‍👩‍👧".len();
        assert_eq!(
            cols,
            vec![
                0,
                1,
                e_end,
                family_end,
                family_end + 3,
                text.len(),
                text.len()
            ]
        );
        assert_eq!(buffer.grapheme_col(buffer.cursor()), 5);
        buffer.set_cursor(Pos::new(0, family_end), false);
        buffer.backspace();
        assert_eq!(buffer.line(0), "ae\u{301}中b");
        buffer.move_left(false);
        assert_eq!(buffer.cursor().col, 1);
        // Clamping into the middle of a cluster snaps back.
        assert_eq!(buffer.clamp(Pos::new(0, 2)), Pos::new(0, 1));
        assert_eq!(
            buffer.clamp(Pos::new(9, 99)),
            Pos::new(0, buffer.line(0).len())
        );
    }

    #[test]
    fn word_motion() {
        let mut buffer = buf("foo.bar  baz\nnext");
        buffer.move_word_right(false);
        assert_eq!(buffer.cursor().col, 3);
        buffer.move_word_right(false);
        assert_eq!(buffer.cursor().col, 4);
        buffer.move_word_right(false);
        assert_eq!(buffer.cursor().col, 7);
        buffer.move_word_right(false);
        assert_eq!(buffer.cursor().col, 12);
        buffer.move_word_right(false);
        assert_eq!(buffer.cursor(), Pos::new(1, 0));
        buffer.move_word_left(false);
        assert_eq!(buffer.cursor(), Pos::new(0, 12));
        buffer.move_word_left(false);
        assert_eq!(buffer.cursor().col, 9);
        buffer.move_word_left(false);
        assert_eq!(buffer.cursor().col, 4);
        buffer.delete_word_back();
        assert_eq!(buffer.line(0), "foobar  baz");
        buffer.set_cursor(Pos::new(0, 11), false);
        buffer.delete_word_back();
        assert_eq!(buffer.line(0), "foobar  ");
        buffer.set_cursor(Pos::new(0, 0), false);
        buffer.delete_word_forward();
        assert_eq!(buffer.line(0), "  ");
    }

    #[test]
    fn home_end_vertical_and_selection() {
        let mut buffer = buf("  indented\nab\nlonger line");
        buffer.move_end(false);
        buffer.move_home(false);
        assert_eq!(buffer.cursor().col, 2);
        buffer.move_home(false);
        assert_eq!(buffer.cursor().col, 0);
        buffer.set_cursor(Pos::new(1, 2), false);
        buffer.set_cursor(Pos::new(2, 8), true);
        assert_eq!(buffer.selected_text().as_deref(), Some("\nlonger l"));
        buffer.move_end(false);
        assert_eq!(buffer.cursor(), Pos::new(2, 11));
        assert!(buffer.selection().is_none());
        buffer.select_all();
        assert_eq!(
            buffer.selected_text().unwrap(),
            "  indented\nab\nlonger line"
        );
        buffer.move_left(false);
        assert_eq!(buffer.cursor(), Pos::new(0, 0), "left collapses to start");
        buffer.move_doc_end(true);
        buffer.insert_text("x");
        assert_eq!(buffer.to_text(), "x");
    }

    #[test]
    fn undo_groups_typing_runs_by_word() {
        let mut buffer = buf("");
        for ch in "hello world".chars() {
            buffer.insert_text(&ch.to_string());
        }
        assert!(buffer.is_dirty());
        assert!(buffer.undo());
        assert_eq!(buffer.to_text(), "hello ");
        assert!(buffer.undo());
        assert_eq!(buffer.to_text(), "");
        assert!(!buffer.is_dirty(), "back at the saved state");
        assert!(!buffer.undo());
        assert!(buffer.redo());
        assert!(buffer.redo());
        assert_eq!(buffer.to_text(), "hello world");
        assert_eq!(buffer.cursor(), Pos::new(0, 11));
        assert!(!buffer.redo());
    }

    #[test]
    fn cursor_motion_breaks_undo_groups_and_edits_clear_redo() {
        let mut buffer = buf("");
        buffer.insert_text("a");
        buffer.insert_text("b");
        buffer.move_left(false);
        buffer.move_right(false);
        buffer.insert_text("c");
        buffer.undo();
        assert_eq!(buffer.to_text(), "ab");
        buffer.insert_text("d");
        assert!(!buffer.redo(), "an edit clears redo");
        buffer.undo();
        buffer.undo();
        assert_eq!(buffer.to_text(), "");
    }

    #[test]
    fn backspace_and_delete_runs_group() {
        let mut buffer = buf("abcdef");
        buffer.move_doc_end(false);
        buffer.backspace();
        buffer.backspace();
        buffer.backspace();
        assert_eq!(buffer.to_text(), "abc");
        buffer.undo();
        assert_eq!(buffer.to_text(), "abcdef");
        assert_eq!(buffer.cursor(), Pos::new(0, 6));
        buffer.set_cursor(Pos::new(0, 0), false);
        buffer.delete_forward();
        buffer.delete_forward();
        assert_eq!(buffer.to_text(), "cdef");
        buffer.undo();
        assert_eq!(buffer.to_text(), "abcdef");
    }

    #[test]
    fn selection_replace_and_compound_undo() {
        let mut buffer = buf("one two three");
        buffer.set_selection(Pos::new(0, 4), Pos::new(0, 7));
        buffer.insert_text("2");
        assert_eq!(buffer.to_text(), "one 2 three");
        buffer.undo();
        assert_eq!(buffer.to_text(), "one two three");
        assert_eq!(buffer.selection(), Some((Pos::new(0, 4), Pos::new(0, 7))));

        buffer.begin_compound();
        buffer.replace(Pos::new(0, 0), Pos::new(0, 3), "1");
        buffer.replace(Pos::new(0, 6), Pos::new(0, 11), "3");
        buffer.set_cursor(Pos::new(0, 1), false);
        buffer.end_compound();
        assert_eq!(buffer.to_text(), "1 two 3");
        buffer.undo();
        assert_eq!(buffer.to_text(), "one two three");
        buffer.redo();
        assert_eq!(buffer.to_text(), "1 two 3");
        assert_eq!(buffer.cursor(), Pos::new(0, 1));
    }

    #[test]
    fn dirty_tracking_and_saved_state() {
        let mut buffer = buf("x");
        assert!(!buffer.is_dirty());
        buffer.insert_text("a");
        buffer.mark_saved();
        assert!(!buffer.is_dirty());
        buffer.undo();
        assert!(buffer.is_dirty());
        buffer.redo();
        assert!(!buffer.is_dirty());
        buffer.reload("fresh\n");
        assert!(!buffer.is_dirty());
        assert!(!buffer.undo(), "a reload clears history");
        assert_eq!(buffer.to_text(), "fresh\n");
    }

    #[test]
    fn damage_tracks_changed_lines() {
        let mut buffer = buf("a\nb\nc\nd");
        assert_eq!(buffer.take_damage(), None);
        buffer.set_cursor(Pos::new(1, 1), false);
        buffer.insert_text("\nx\ny");
        assert_eq!(
            buffer.take_damage(),
            Some(Damage {
                start: 1,
                old_end: 2,
                new_end: 4
            })
        );
        // Two edits merge into one damage.
        buffer.set_cursor(Pos::new(0, 0), false);
        buffer.insert_text("z");
        buffer.set_selection(Pos::new(4, 0), Pos::new(5, 0));
        buffer.delete_selection();
        assert_eq!(
            buffer.take_damage(),
            Some(Damage {
                start: 0,
                old_end: 6,
                new_end: 5
            })
        );
    }

    #[test]
    fn damage_merge_is_composition() {
        // text0 = 10 lines; d1 replaces line 2 by 3 lines; d2 deletes lines 7..9.
        let d1 = Damage {
            start: 2,
            old_end: 3,
            new_end: 5,
        };
        let d2 = Damage {
            start: 7,
            old_end: 9,
            new_end: 7,
        };
        let merged = d1.merge(d2);
        assert_eq!(
            merged,
            Damage {
                start: 2,
                old_end: 7,
                new_end: 7
            }
        );
        assert_eq!(merged.delta(), d1.delta() + d2.delta());
    }

    #[test]
    fn normalises_pasted_newlines() {
        let mut buffer = buf("");
        buffer.insert_text("a\r\nb\rc");
        assert_eq!(buffer.lines(), ["a", "b", "c"]);
        assert_eq!(normalize_newlines("plain"), "plain");
    }

    #[test]
    fn mixed_line_endings_round_trip_and_never_leave_a_cr_in_a_line() {
        // CRLF first, LF later: untouched lines keep their own ending.
        let mut buffer = buf("a\r\nb\nc\n");
        assert_eq!(buffer.lines(), ["a", "b", "c"]);
        assert_eq!(buffer.to_text(), "a\r\nb\nc\n");
        buffer.set_cursor(Pos::new(2, 1), false);
        buffer.insert_text("!");
        assert_eq!(buffer.to_text(), "a\r\nb\nc!\n");
        // LF first, CRLF later: no stray CR stays inside a line.
        let mut buffer = buf("+++\ntitle = 'x'\r\n+++\r\nbody");
        assert_eq!(buffer.lines(), ["+++", "title = 'x'", "+++", "body"]);
        assert_eq!(buffer.to_text(), "+++\ntitle = 'x'\r\n+++\r\nbody");
        // A new line break gets the first break's ending (LF here); the
        // split line keeps its own (CRLF) at the end.
        buffer.set_cursor(Pos::new(1, 4), false);
        buffer.insert_text("\n");
        assert_eq!(buffer.to_text(), "+++\ntitl\ne = 'x'\r\n+++\r\nbody");
        buffer.undo();
        assert_eq!(buffer.to_text(), "+++\ntitle = 'x'\r\n+++\r\nbody");
        buffer.reload("x\r\ny\n");
        assert_eq!(buffer.to_text(), "x\r\ny\n");
    }

    #[test]
    fn typing_on_after_a_save_is_unsaved() {
        let mut buffer = buf("");
        buffer.insert_text("a");
        buffer.mark_saved();
        // Same word, no cursor motion: would merge into the saved group.
        buffer.insert_text("b");
        assert!(buffer.is_dirty());
        buffer.undo();
        assert_eq!(buffer.to_text(), "a");
        assert!(!buffer.is_dirty());
    }

    #[test]
    fn a_paste_is_its_own_undo_step() {
        let mut buffer = buf("");
        for ch in "abc".chars() {
            buffer.insert_text(&ch.to_string());
        }
        buffer.paste_text("PASTED-TEXT");
        buffer.insert_text("d");
        assert_eq!(buffer.to_text(), "abcPASTED-TEXTd");
        buffer.undo();
        assert_eq!(buffer.to_text(), "abcPASTED-TEXT");
        buffer.undo();
        assert_eq!(buffer.to_text(), "abc");
        buffer.undo();
        assert_eq!(buffer.to_text(), "");
    }

    #[test]
    fn long_line_motion_is_cheap() {
        let line = "x".repeat(5_000_000);
        let mut buffer = buf(&line);
        buffer.set_cursor(Pos::new(0, 2_500_000), false);
        let start = std::time::Instant::now();
        for _ in 0..10_000 {
            buffer.move_right(false);
            buffer.move_left(false);
        }
        assert!(start.elapsed().as_secs() < 5);
        assert_eq!(buffer.cursor().col, 2_500_000);
    }
}
