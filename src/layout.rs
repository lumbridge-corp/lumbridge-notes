//! Soft wrapping and the mapping between screen cells and source positions.
//!
//! A source line is laid out either **raw** (its whole source, tinted) or
//! **rendered** (the live-preview spans from [`crate::markdown`]). The result,
//! a [`LineLayout`], is a list of screen rows made of [`Piece`]s: slices of
//! one span's display text at a screen column. Because every span knows its
//! source byte range, a screen cell maps back to a source column
//! ([`LineLayout::hit`]) and a source column to a screen cell
//! ([`LineLayout::locate`]) in both modes.
//!
//! Widths are Unicode-aware (CJK and emoji take two cells); a tab is
//! [`TAB_WIDTH`] cells and other control characters one cell, drawn via
//! [`display_grapheme`]. Rendering must use [`graphemes`] and
//! [`grapheme_width`] so it agrees with the layout cell for cell.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Range;

use unicode_segmentation::{GraphemeIndices, UnicodeSegmentation};
use unicode_width::UnicodeWidthStr;

use crate::buffer::Damage;
use crate::markdown::{raw_spans, LineInfo, Span, SpanKind};

pub const TAB_WIDTH: usize = 4;
/// Cached line layouts before the cache starts over.
const CACHE_LIMIT: usize = 8192;

/// Screen cells taken by one grapheme cluster.
pub fn grapheme_width(grapheme: &str) -> usize {
    match grapheme.as_bytes() {
        [b'\t'] => TAB_WIDTH,
        [byte] if byte.is_ascii_control() => 1,
        [byte] if byte.is_ascii() => 1,
        _ => {
            if grapheme.chars().next().is_some_and(char::is_control) {
                1
            } else {
                UnicodeWidthStr::width(grapheme)
            }
        }
    }
}

/// What to draw for a grapheme: tabs become spaces and control characters a
/// replacement glyph, so nothing reaches the terminal that could move its cursor.
pub fn display_grapheme(grapheme: &str) -> Cow<'_, str> {
    if grapheme == "\t" {
        Cow::Borrowed("    ")
    } else if grapheme.chars().next().is_some_and(char::is_control) {
        Cow::Borrowed("\u{fffd}")
    } else {
        Cow::Borrowed(grapheme)
    }
}

/// Screen width of a string.
pub fn str_width(text: &str) -> usize {
    graphemes(text)
        .map(|(_, grapheme)| grapheme_width(grapheme))
        .sum()
}

/// Grapheme clusters with their byte offsets; one byte at a time for ASCII.
pub fn graphemes(text: &str) -> Graphemes<'_> {
    if text.is_ascii() {
        Graphemes::Ascii { text, pos: 0 }
    } else {
        Graphemes::Unicode(text.grapheme_indices(true))
    }
}

pub enum Graphemes<'a> {
    Ascii { text: &'a str, pos: usize },
    Unicode(GraphemeIndices<'a>),
}

impl<'a> Iterator for Graphemes<'a> {
    type Item = (usize, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Graphemes::Ascii { text, pos } => {
                if *pos >= text.len() {
                    return None;
                }
                let start = *pos;
                // "\r\n" is one cluster even in ASCII.
                let len = if text.as_bytes()[start] == b'\r'
                    && text.as_bytes().get(start + 1) == Some(&b'\n')
                {
                    2
                } else {
                    1
                };
                *pos += len;
                Some((start, &text[start..start + len]))
            }
            Graphemes::Unicode(inner) => inner.next(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineMode {
    Raw,
    Rendered,
}

/// A slice of one span's display text placed on a screen row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    /// Index into [`LineLayout::spans`].
    pub span: usize,
    /// Byte range within the span's display text.
    pub text: Range<usize>,
    /// Screen column (relative to the text area).
    pub x: usize,
    pub width: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Row {
    pub pieces: Vec<Piece>,
}

impl Row {
    /// The column after the last piece.
    pub fn end_x(&self) -> usize {
        self.pieces.last().map_or(0, |piece| piece.x + piece.width)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineLayout {
    pub mode: LineMode,
    /// The spans the pieces point into (raw or rendered, plus the
    /// continuation prefix spans at the end).
    pub spans: Vec<Span>,
    pub rows: Vec<Row>,
    pub width: usize,
    line_len: usize,
}

struct Atom {
    span: usize,
    start: usize,
    end: usize,
    width: usize,
}

struct Wrapper {
    width: usize,
    rows: Vec<Row>,
    row: Row,
    x: usize,
    /// Column where the row's own content starts (after the wrap prefix).
    row_start: usize,
    /// Continuation prefix pieces: (span, text length, width).
    cont: Vec<(usize, usize, usize)>,
    /// Where content starts on a continuation row (the prefix's width).
    fresh_start: usize,
}

impl Wrapper {
    fn append(&mut self, atom: &Atom) {
        if let Some(last) = self.row.pieces.last_mut() {
            if last.span == atom.span
                && last.text.end == atom.start
                && last.x + last.width == self.x
            {
                last.text.end = atom.end;
                last.width += atom.width;
                self.x += atom.width;
                return;
            }
        }
        self.row.pieces.push(Piece {
            span: atom.span,
            text: atom.start..atom.end,
            x: self.x,
            width: atom.width,
        });
        self.x += atom.width;
    }

    fn new_row(&mut self) {
        let row = std::mem::take(&mut self.row);
        self.rows.push(row);
        self.x = 0;
        for &(span, len, width) in &self.cont {
            self.row.pieces.push(Piece {
                span,
                text: 0..len,
                x: self.x,
                width,
            });
            self.x += width;
        }
        self.row_start = self.x;
    }

    fn has_content(&self) -> bool {
        self.x > self.row_start
    }

    /// Places one grapheme that must not be split from nothing else.
    fn place(&mut self, atom: &Atom) {
        if self.x + atom.width > self.width && self.has_content() {
            self.new_row();
        }
        self.append(atom);
    }

    fn place_word(&mut self, word: &mut Vec<Atom>, word_width: &mut usize) {
        if word.is_empty() {
            return;
        }
        // Move the word down only if it fits on a fresh row; otherwise it
        // would be broken there anyway, so break it here and never leave a
        // bullet or quote bar alone on its row.
        let room = self.width.saturating_sub(self.fresh_start);
        if self.x + *word_width > self.width && self.has_content() && *word_width <= room {
            self.new_row();
        }
        for atom in word.iter() {
            self.place(atom);
        }
        word.clear();
        *word_width = 0;
    }
}

/// Lays out one source line at `width` columns.
pub fn layout_line(line: &str, info: &LineInfo, mode: LineMode, width: usize) -> LineLayout {
    let width = width.max(1);
    if mode == LineMode::Rendered && info.hidden {
        return LineLayout {
            mode,
            spans: Vec::new(),
            rows: Vec::new(),
            width,
            line_len: line.len(),
        };
    }
    let mut spans = match mode {
        LineMode::Raw => raw_spans(line, info),
        LineMode::Rendered => info.spans.clone(),
    };
    let content_spans = spans.len();
    let mut cont = Vec::new();
    let mut fresh_start = 0;
    if mode == LineMode::Rendered && !info.cont.is_empty() {
        let total: usize = info
            .cont
            .iter()
            .map(|span| str_width(span.text(line)))
            .sum();
        if total * 2 <= width {
            fresh_start = total;
            for span in &info.cont {
                let text = span.text(line);
                cont.push((spans.len(), text.len(), str_width(text)));
                spans.push(span.clone());
            }
        }
    }
    let mut wrapper = Wrapper {
        width,
        rows: Vec::new(),
        row: Row::default(),
        x: 0,
        row_start: 0,
        cont,
        fresh_start,
    };
    let mut word: Vec<Atom> = Vec::new();
    let mut word_width = 0;
    for (index, span) in spans.iter().enumerate().take(content_spans) {
        let text = span.text(line);
        for (offset, grapheme) in graphemes(text) {
            let atom = Atom {
                span: index,
                start: offset,
                end: offset + grapheme.len(),
                width: grapheme_width(grapheme),
            };
            if grapheme.chars().all(char::is_whitespace) {
                wrapper.place_word(&mut word, &mut word_width);
                wrapper.place(&atom);
                continue;
            }
            word_width += atom.width;
            word.push(atom);
            if word_width > width.saturating_sub(wrapper.fresh_start).max(1) {
                // Longer than a row: break it anywhere, from where we are.
                for atom in word.drain(..) {
                    wrapper.place(&atom);
                }
                word_width = 0;
            }
        }
    }
    wrapper.place_word(&mut word, &mut word_width);
    let Wrapper { mut rows, row, .. } = wrapper;
    rows.push(row);
    if mode == LineMode::Raw && rows.last().is_some_and(|row| row.end_x() >= width) {
        // Room for the cursor after the last character.
        rows.push(Row::default());
    }
    LineLayout {
        mode,
        spans,
        rows,
        width,
        line_len: line.len(),
    }
}

impl LineLayout {
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// The display text of a piece.
    pub fn piece_text<'a>(&'a self, line: &'a str, piece: &Piece) -> &'a str {
        self.spans
            .get(piece.span)
            .map(|span| span.text(line))
            .and_then(|text| text.get(piece.text.clone()))
            .unwrap_or("")
    }

    pub fn span(&self, piece: &Piece) -> Option<&Span> {
        self.spans.get(piece.span)
    }

    fn is_content(&self, piece: &Piece) -> bool {
        self.spans.get(piece.span).is_some_and(|span| {
            !span.kind.is_decoration() && (!span.source.is_empty() || span.display.is_none())
        })
    }

    /// The source range a piece stands for.
    pub fn piece_source(&self, piece: &Piece) -> Range<usize> {
        let Some(span) = self.spans.get(piece.span) else {
            return 0..0;
        };
        if span.is_verbatim() {
            span.source.start + piece.text.start..span.source.start + piece.text.end
        } else {
            span.source.clone()
        }
    }

    /// The piece under screen column `x` of `row`.
    pub fn piece_at(&self, row: usize, x: usize) -> Option<&Piece> {
        self.rows
            .get(row)?
            .pieces
            .iter()
            .find(|piece| x >= piece.x && x < piece.x + piece.width)
    }

    /// The checkbox source range under a click, if any.
    pub fn checkbox_at(&self, row: usize, x: usize) -> Option<Range<usize>> {
        let piece = self.piece_at(row, x)?;
        let span = self.spans.get(piece.span)?;
        matches!(span.kind, SpanKind::Checkbox { .. }).then(|| span.source.clone())
    }

    /// The source byte offset for a click at `x` on `row` (a grapheme
    /// boundary for verbatim text; the start of a substituted span).
    pub fn hit(&self, line: &str, row: usize, x: usize) -> usize {
        let Some(row_ref) = self.rows.get(row) else {
            return self.line_len;
        };
        let content: Vec<&Piece> = row_ref
            .pieces
            .iter()
            .filter(|piece| self.is_content(piece))
            .collect();
        for piece in &content {
            if x < piece.x {
                // On a decoration before this piece.
                return self.piece_source(piece).start;
            }
            if x < piece.x + piece.width {
                return self.hit_in_piece(line, piece, x);
            }
        }
        match content.last() {
            Some(piece) if row + 1 < self.rows.len() => {
                // Past the end of a wrapped row: its last grapheme, so the
                // position stays on this row (its end byte starts the next).
                self.last_on_row(line, piece)
            }
            Some(piece) => self.piece_source(piece).end,
            None => {
                if self.mode == LineMode::Raw || row + 1 == self.rows.len() {
                    // Past the end of an empty or decoration-only row.
                    row_ref
                        .pieces
                        .iter()
                        .filter_map(|piece| self.spans.get(piece.span))
                        .map(|span| span.source.end)
                        .max()
                        .unwrap_or(if self.mode == LineMode::Raw {
                            self.line_len
                        } else {
                            0
                        })
                } else {
                    0
                }
            }
        }
    }

    /// Like [`LineLayout::hit`], but for the end of a selection dragged
    /// rightwards: the boundary at `x` reached from the left. Right after
    /// rendered `link text` that is the end of `text`, not the start of
    /// what follows the hidden `](url)`.
    pub fn hit_end(&self, line: &str, row: usize, x: usize) -> usize {
        let after = self.hit(line, row, x);
        let Some(before_x) = x.checked_sub(1) else {
            return after;
        };
        let Some(piece) = self
            .rows
            .get(row)
            .and_then(|row_ref| {
                row_ref
                    .pieces
                    .iter()
                    .find(|piece| before_x >= piece.x && before_x < piece.x + piece.width)
            })
            .filter(|piece| self.is_content(piece))
        else {
            return after;
        };
        let verbatim = self
            .spans
            .get(piece.span)
            .is_some_and(|span| span.is_verbatim());
        if !verbatim {
            return after;
        }
        let source = self.piece_source(piece);
        let start = self.hit_in_piece(line, piece, before_x);
        let end = crate::buffer::next_grapheme(line, start).min(source.end);
        if end < after {
            end
        } else {
            after
        }
    }

    /// The source byte of the last grapheme of `piece` (a substituted span
    /// maps as a whole, to its start).
    fn last_on_row(&self, line: &str, piece: &Piece) -> usize {
        let source = self.piece_source(piece);
        let verbatim = self
            .spans
            .get(piece.span)
            .is_some_and(|span| span.is_verbatim());
        if !verbatim {
            return source.start;
        }
        crate::buffer::prev_grapheme(line, source.end).max(source.start)
    }

    fn hit_in_piece(&self, line: &str, piece: &Piece, x: usize) -> usize {
        let Some(span) = self.spans.get(piece.span) else {
            return 0;
        };
        if !span.is_verbatim() {
            return span.source.start;
        }
        let text = span.text(line);
        let slice = text.get(piece.text.clone()).unwrap_or("");
        let mut gx = piece.x;
        for (offset, grapheme) in graphemes(slice) {
            let width = grapheme_width(grapheme);
            if x < gx + width {
                return span.source.start + piece.text.start + offset;
            }
            gx += width;
        }
        span.source.start + piece.text.end
    }

    /// The screen `(row, x)` of source byte `byte` (in a concealed gap, the
    /// next visible piece; past the end, after the last piece).
    pub fn locate(&self, line: &str, byte: usize) -> (usize, usize) {
        for (row_index, row) in self.rows.iter().enumerate() {
            for piece in row.pieces.iter().filter(|piece| self.is_content(piece)) {
                let source = self.piece_source(piece);
                if byte < source.start {
                    return (row_index, piece.x);
                }
                if byte < source.end {
                    let Some(span) = self.spans.get(piece.span) else {
                        return (row_index, piece.x);
                    };
                    if !span.is_verbatim() {
                        return (row_index, piece.x);
                    }
                    let text = span.text(line);
                    let from = span.source.start + piece.text.start;
                    let head = text
                        .get(piece.text.start..piece.text.start + (byte - from))
                        .unwrap_or("");
                    return (row_index, piece.x + str_width(head));
                }
            }
        }
        match self.rows.last() {
            Some(row) => (self.rows.len() - 1, row.end_x()),
            None => (0, 0),
        }
    }

    /// The source range shown on `row` (for selection and search highlights).
    pub fn row_source(&self, row: usize) -> Range<usize> {
        let Some(row) = self.rows.get(row) else {
            return self.line_len..self.line_len;
        };
        let mut start = usize::MAX;
        let mut end = 0;
        for piece in row.pieces.iter().filter(|piece| self.is_content(piece)) {
            let source = self.piece_source(piece);
            start = start.min(source.start);
            end = end.max(source.end);
        }
        if start == usize::MAX {
            0..0
        } else {
            start..end
        }
    }
}

// ---------------------------------------------------------------------------
// Rows across lines: scrolling and visual motion
// ---------------------------------------------------------------------------

/// One screen row of the document: row `row` of source line `line`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct ScreenPos {
    pub line: usize,
    pub row: usize,
}

impl ScreenPos {
    pub const fn new(line: usize, row: usize) -> ScreenPos {
        ScreenPos { line, row }
    }
}

/// Moves `pos` to a row that exists (hidden lines have none).
pub fn normalize(
    pos: ScreenPos,
    line_count: usize,
    mut rows_of: impl FnMut(usize) -> usize,
) -> ScreenPos {
    if line_count == 0 {
        return ScreenPos::default();
    }
    let line = pos.line.min(line_count - 1);
    let rows = rows_of(line);
    if rows > 0 {
        return ScreenPos::new(line, pos.row.min(rows - 1));
    }
    for next in line + 1..line_count {
        if rows_of(next) > 0 {
            return ScreenPos::new(next, 0);
        }
    }
    for prev in (0..line).rev() {
        let rows = rows_of(prev);
        if rows > 0 {
            return ScreenPos::new(prev, rows - 1);
        }
    }
    ScreenPos::new(line, 0)
}

/// Moves `delta` screen rows (negative = up), stopping at the ends.
pub fn step_rows(
    pos: ScreenPos,
    delta: isize,
    line_count: usize,
    mut rows_of: impl FnMut(usize) -> usize,
) -> ScreenPos {
    let mut current = normalize(pos, line_count, &mut rows_of);
    let mut remaining = delta.unsigned_abs();
    while remaining > 0 {
        let next = if delta > 0 {
            if current.row + 1 < rows_of(current.line) {
                Some(ScreenPos::new(current.line, current.row + 1))
            } else {
                (current.line + 1..line_count)
                    .find(|&line| rows_of(line) > 0)
                    .map(|line| ScreenPos::new(line, 0))
            }
        } else if current.row > 0 {
            Some(ScreenPos::new(current.line, current.row - 1))
        } else {
            (0..current.line).rev().find_map(|line| {
                let rows = rows_of(line);
                (rows > 0).then(|| ScreenPos::new(line, rows - 1))
            })
        };
        match next {
            Some(next) => current = next,
            None => break,
        }
        remaining -= 1;
    }
    current
}

/// The top row that keeps `target` visible in a `height`-row view, moving
/// `top` as little as possible.
pub fn scroll_to_show(
    top: ScreenPos,
    target: ScreenPos,
    height: usize,
    line_count: usize,
    mut rows_of: impl FnMut(usize) -> usize,
) -> ScreenPos {
    let height = height.max(1);
    let top = normalize(top, line_count, &mut rows_of);
    let target = normalize(target, line_count, &mut rows_of);
    if target <= top {
        return target;
    }
    let mut current = top;
    for _ in 1..height {
        let next = step_rows(current, 1, line_count, &mut rows_of);
        if next == current {
            break;
        }
        current = next;
        if current == target {
            return top;
        }
    }
    step_rows(target, -(height as isize - 1), line_count, &mut rows_of)
}

/// The document rows on screen, top to bottom (fewer than `height` at the end).
pub fn visible_rows(
    top: ScreenPos,
    height: usize,
    line_count: usize,
    mut rows_of: impl FnMut(usize) -> usize,
) -> Vec<ScreenPos> {
    let mut out = Vec::with_capacity(height);
    if line_count == 0 || height == 0 {
        return out;
    }
    let mut current = normalize(top, line_count, &mut rows_of);
    if rows_of(current.line) == 0 {
        return out;
    }
    out.push(current);
    while out.len() < height {
        let next = step_rows(current, 1, line_count, &mut rows_of);
        if next == current {
            break;
        }
        out.push(next);
        current = next;
    }
    out
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

/// Line layouts keyed by `(line, mode)` for one width. Invalidate with
/// [`LayoutCache::apply_damage`] using the damage returned by
/// [`crate::markdown::Document::update`].
#[derive(Debug, Default)]
pub struct LayoutCache {
    width: usize,
    map: HashMap<(usize, LineMode), LineLayout>,
}

impl LayoutCache {
    pub fn new() -> LayoutCache {
        LayoutCache::default()
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// Drops everything when the width changes.
    pub fn set_width(&mut self, width: usize) {
        if width != self.width {
            self.width = width;
            self.map.clear();
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    /// Forgets lines that changed and renumbers the ones after them.
    pub fn apply_damage(&mut self, damage: Damage) {
        if damage.start == 0 && damage.old_end == 0 && damage.new_end == 0 {
            return;
        }
        let delta = damage.delta();
        let old = std::mem::take(&mut self.map);
        for ((line, mode), layout) in old {
            if line < damage.start {
                self.map.insert((line, mode), layout);
            } else if line >= damage.old_end {
                let moved = (line as isize + delta) as usize;
                self.map.insert((moved, mode), layout);
            }
        }
    }

    pub fn get(&self, line: usize, mode: LineMode) -> Option<&LineLayout> {
        self.map.get(&(line, mode))
    }

    /// Lays the line out if it is not cached.
    pub fn ensure(
        &mut self,
        line: usize,
        mode: LineMode,
        text: &str,
        info: &LineInfo,
    ) -> &LineLayout {
        if self.map.len() >= CACHE_LIMIT && !self.map.contains_key(&(line, mode)) {
            self.map.clear();
        }
        let width = self.width;
        self.map
            .entry((line, mode))
            .or_insert_with(|| layout_line(text, info, mode, width))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::Document;

    fn doc(text: &str) -> (Vec<String>, Document) {
        let lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
        let document = Document::parse(&lines);
        (lines, document)
    }

    fn row_texts(layout: &LineLayout, line: &str) -> Vec<String> {
        layout
            .rows
            .iter()
            .map(|row| {
                row.pieces
                    .iter()
                    .map(|piece| layout.piece_text(line, piece))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn widths_handle_cjk_emoji_tabs_and_controls() {
        assert_eq!(str_width("abc"), 3);
        assert_eq!(str_width("中文"), 4);
        assert_eq!(str_width("😀"), 2);
        assert_eq!(str_width("e\u{301}"), 1);
        assert_eq!(str_width("a\tb"), 2 + TAB_WIDTH);
        assert_eq!(str_width("\u{1b}"), 1);
        assert_eq!(display_grapheme("\u{1b}"), "\u{fffd}");
        assert_eq!(display_grapheme("\t"), "    ");
        let pieces: Vec<(usize, &str)> = graphemes("a\r\nb").collect();
        assert_eq!(pieces, vec![(0, "a"), (1, "\r\n"), (3, "b")]);
    }

    #[test]
    fn raw_wraps_at_word_boundaries() {
        let (lines, document) = doc("hello brave new world");
        let layout = layout_line(&lines[0], &document.lines()[0], LineMode::Raw, 11);
        assert_eq!(
            row_texts(&layout, &lines[0]),
            vec!["hello brave", " new world"]
        );
        // Every source byte is on screen exactly once.
        let joined: String = row_texts(&layout, &lines[0]).concat();
        assert_eq!(joined, lines[0]);
    }

    #[test]
    fn long_words_break_anywhere_and_cjk_never_splits() {
        let (lines, document) = doc("abcdefghij");
        let layout = layout_line(&lines[0], &document.lines()[0], LineMode::Raw, 4);
        assert_eq!(row_texts(&layout, &lines[0]), vec!["abcd", "efgh", "ij"]);
        let (lines, document) = doc("中文字符");
        let layout = layout_line(&lines[0], &document.lines()[0], LineMode::Raw, 5);
        assert_eq!(row_texts(&layout, &lines[0]), vec!["中文", "字符"]);
        assert!(layout.rows.iter().all(|row| row.end_x() <= 5));
        // A wide grapheme in a 1-column view still gets placed.
        let layout = layout_line(&lines[0], &document.lines()[0], LineMode::Raw, 1);
        assert_eq!(layout.rows.len(), 5);
    }

    #[test]
    fn raw_adds_a_row_for_the_cursor_after_a_full_last_row() {
        let (lines, document) = doc("abcd");
        let layout = layout_line(&lines[0], &document.lines()[0], LineMode::Raw, 4);
        assert_eq!(layout.rows.len(), 2);
        assert_eq!(layout.locate(&lines[0], 4), (1, 0));
        let rendered = layout_line(&lines[0], &document.lines()[0], LineMode::Rendered, 4);
        assert_eq!(rendered.rows.len(), 1);
    }

    #[test]
    fn rendered_list_wraps_with_hanging_indent() {
        let (lines, document) = doc("- one two three four");
        let layout = layout_line(&lines[0], &document.lines()[0], LineMode::Rendered, 12);
        let rows = row_texts(&layout, &lines[0]);
        assert_eq!(rows, vec!["• one two ", "  three four"]);
        assert_eq!(layout.rows[1].pieces[1].x, 2);
    }

    #[test]
    fn a_word_too_wide_for_a_fresh_row_breaks_beside_its_bullet() {
        for (text, expected) in [
            ("- abcdefghijkl", vec!["• abcdefghijk", "  l"]),
            ("> abcdefghijkl", vec!["▎ abcdefghijk", "▎ l"]),
        ] {
            let (lines, document) = doc(text);
            let layout = layout_line(&lines[0], &document.lines()[0], LineMode::Rendered, 13);
            assert_eq!(row_texts(&layout, &lines[0]), expected, "{text:?}");
        }
        // A word that fits on a fresh row still moves down whole.
        let (lines, document) = doc("- ab abcdefghij");
        let layout = layout_line(&lines[0], &document.lines()[0], LineMode::Rendered, 13);
        assert_eq!(row_texts(&layout, &lines[0]), vec!["• ab ", "  abcdefghij"]);
    }

    #[test]
    fn hitting_past_a_wrapped_row_stays_on_that_row() {
        let line = "aaaa bbbbbbbbb";
        let (lines, document) = doc(line);
        let layout = layout_line(&lines[0], &document.lines()[0], LineMode::Raw, 10);
        assert_eq!(row_texts(&layout, line), vec!["aaaa ", "bbbbbbbbb"]);
        // Up from the end (row 1, x 9) lands on row 0, not the start of row 1.
        let byte = layout.hit(line, 0, 9);
        assert_eq!(layout.locate(line, byte).0, 0, "byte {byte}");
        assert_eq!(byte, 4);
        // Rendered list rows behave the same.
        let line = "- one two three four";
        let (lines, document) = doc(line);
        let layout = layout_line(&lines[0], &document.lines()[0], LineMode::Rendered, 12);
        let byte = layout.hit(line, 0, 11);
        assert_eq!(layout.locate(line, byte).0, 0, "byte {byte}");
    }

    #[test]
    fn hit_and_locate_round_trip_in_raw_mode() {
        let line = "ab 中文 😀 tab\there";
        let (lines, document) = doc(line);
        let layout = layout_line(&lines[0], &document.lines()[0], LineMode::Raw, 7);
        for (offset, _) in graphemes(line) {
            let (row, x) = layout.locate(line, offset);
            assert_eq!(
                layout.hit(line, row, x),
                offset,
                "offset {offset} at {row},{x}"
            );
        }
        let (row, x) = layout.locate(line, line.len());
        assert_eq!(layout.hit(line, row, x), line.len());
        // Clicking the second cell of a wide char lands before it.
        let wide = line.find('中').unwrap();
        let (row, x) = layout.locate(line, wide);
        assert_eq!(layout.hit(line, row, x + 1), wide);
        // Past the end of the last row.
        let last = layout.rows.len() - 1;
        assert_eq!(layout.hit(line, last, 99), line.len());
    }

    #[test]
    fn rendered_hits_map_to_source_columns() {
        let line = "## Hello **bold** [link](http://x) end";
        let (lines, document) = doc(line);
        let layout = layout_line(&lines[0], &document.lines()[0], LineMode::Rendered, 80);
        assert_eq!(row_texts(&layout, line), vec!["Hello bold link end"]);
        // Screen x 0 is 'H' at source 3; x 6 is 'b' at 11; x 11 is 'l' at 19.
        assert_eq!(layout.hit(line, 0, 0), 3);
        assert_eq!(layout.hit(line, 0, 6), 11);
        assert_eq!(layout.hit(line, 0, 7), 12);
        assert_eq!(layout.hit(line, 0, 11), 19);
        assert_eq!(layout.hit(line, 0, 100), line.len());
        // Concealed bytes locate to the next visible piece.
        assert_eq!(layout.locate(line, 0), (0, 0));
        assert_eq!(layout.locate(line, 9), (0, 6));
        assert_eq!(layout.locate(line, 12), (0, 7));
        assert_eq!(layout.row_source(0), 3..line.len());
    }

    #[test]
    fn rendered_bullets_and_checkboxes_hit() {
        let line = "- [ ] task";
        let (lines, document) = doc(line);
        let layout = layout_line(&lines[0], &document.lines()[0], LineMode::Rendered, 40);
        // The box takes the bullet's place.
        assert_eq!(row_texts(&layout, line), vec!["☐ task"]);
        assert_eq!(layout.checkbox_at(0, 0), Some(2..5));
        assert_eq!(layout.checkbox_at(0, 1), Some(2..5));
        assert_eq!(layout.checkbox_at(0, 2), None);
        assert_eq!(layout.hit(line, 0, 2), 6);
        let item = "- item";
        let (items, item_doc) = doc(item);
        let plain = layout_line(&items[0], &item_doc.lines()[0], LineMode::Rendered, 40);
        assert_eq!(row_texts(&plain, item), vec!["• item"]);
        assert_eq!(plain.hit(item, 0, 0), 0, "bullet maps to the marker");
        assert_eq!(plain.hit(item, 0, 3), 3);
        let raw = layout_line(line, &document.lines()[0], LineMode::Raw, 40);
        assert_eq!(raw.checkbox_at(0, 3), Some(2..5));
    }

    #[test]
    fn hidden_lines_take_no_rows_and_motion_skips_them() {
        let (lines, document) = doc("+++\ntitle = \"x\"\n+++\nbody\nmore");
        let rows: Vec<usize> = (0..lines.len())
            .map(|index| {
                layout_line(
                    &lines[index],
                    &document.lines()[index],
                    LineMode::Rendered,
                    20,
                )
                .rows
                .len()
            })
            .collect();
        assert_eq!(rows, vec![1, 0, 0, 1, 1]);
        let rows_of = |line: usize| rows[line];
        assert_eq!(
            step_rows(ScreenPos::new(0, 0), 1, 5, rows_of),
            ScreenPos::new(3, 0)
        );
        assert_eq!(
            step_rows(ScreenPos::new(3, 0), -1, 5, rows_of),
            ScreenPos::new(0, 0)
        );
        assert_eq!(
            normalize(ScreenPos::new(1, 0), 5, rows_of),
            ScreenPos::new(3, 0)
        );
        assert_eq!(
            visible_rows(ScreenPos::new(0, 0), 10, 5, rows_of),
            vec![
                ScreenPos::new(0, 0),
                ScreenPos::new(3, 0),
                ScreenPos::new(4, 0)
            ]
        );
    }

    #[test]
    fn scrolling_keeps_the_target_visible() {
        // Line i has (i % 3) + 1 rows.
        let rows_of = |line: usize| line % 3 + 1;
        let top = ScreenPos::new(0, 0);
        assert_eq!(
            scroll_to_show(top, ScreenPos::new(1, 1), 5, 100, rows_of),
            top
        );
        let moved = scroll_to_show(top, ScreenPos::new(4, 0), 3, 100, rows_of);
        assert_eq!(
            visible_rows(moved, 3, 100, rows_of).last(),
            Some(&ScreenPos::new(4, 0))
        );
        assert_eq!(
            scroll_to_show(ScreenPos::new(10, 0), ScreenPos::new(2, 2), 3, 100, rows_of),
            ScreenPos::new(2, 2)
        );
        assert_eq!(
            step_rows(ScreenPos::new(99, 0), 5, 100, rows_of),
            ScreenPos::new(99, 0)
        );
        assert_eq!(
            step_rows(ScreenPos::new(0, 0), -5, 100, rows_of),
            ScreenPos::new(0, 0)
        );
        assert_eq!(
            step_rows(ScreenPos::new(0, 0), 3, 100, rows_of),
            ScreenPos::new(2, 0)
        );
    }

    #[test]
    fn cache_renumbers_after_damage() {
        let (lines, document) = doc("a\nb\nc\nd");
        let mut cache = LayoutCache::new();
        cache.set_width(10);
        for (index, (line, info)) in lines.iter().zip(document.lines()).enumerate() {
            cache.ensure(index, LineMode::Raw, line, info);
        }
        cache.apply_damage(Damage {
            start: 1,
            old_end: 2,
            new_end: 4,
        });
        assert!(cache.get(0, LineMode::Raw).is_some());
        assert!(cache.get(1, LineMode::Raw).is_none());
        assert!(cache.get(4, LineMode::Raw).is_some(), "c moved from 2 to 4");
        assert!(cache.get(5, LineMode::Raw).is_some());
        cache.set_width(11);
        assert!(cache.get(0, LineMode::Raw).is_none());
    }

    #[test]
    fn five_megabyte_line_lays_out_quickly() {
        let line = "word ".repeat(1_000_000);
        let (lines, document) = doc(&line);
        let start = std::time::Instant::now();
        let layout = layout_line(&lines[0], &document.lines()[0], LineMode::Raw, 80);
        assert!(start.elapsed().as_secs() < 10);
        assert!(layout.rows.len() > 60_000);
        let (row, x) = layout.locate(&lines[0], 2_500_000);
        assert_eq!(layout.hit(&lines[0], row, x), 2_500_000);
    }
}
