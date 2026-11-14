//! Markdown-aware edits on a [`Buffer`]: Enter continues lists and quotes,
//! Ctrl-T toggles task checkboxes, Tab / Shift-Tab indent list items.
//!
//! These read the line's prefix textually ([`LinePrefix`]) so they work the
//! same in raw and rendered views. Each operation is one undo step.

use std::ops::Range;

use crate::buffer::{Buffer, Pos};
use crate::layout::str_width;

/// Spaces inserted by Tab outside list items (to the next multiple).
pub const TAB_STOP: usize = 4;
/// How far back to look for a parent or sibling list item.
const LOOKBACK: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerKind {
    Bullet(char),
    Ordered { number: u64, delimiter: char },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListMarker {
    pub kind: MarkerKind,
    /// The marker itself (`-`, `12.`), without the spaces after it.
    pub range: Range<usize>,
    /// `Some((checked, range of "[ ]"))` for a task item.
    pub task: Option<(bool, Range<usize>)>,
}

/// The syntax at the start of a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinePrefix {
    /// End of the leading `>` markers (and the space after each); 0 if none.
    pub quote_end: usize,
    /// Whitespace bytes after `quote_end`, before the marker or content.
    pub indent: usize,
    pub marker: Option<ListMarker>,
    /// Where the line's text starts.
    pub content_start: usize,
}

impl LinePrefix {
    /// Width of the marker plus the spaces after it (the nesting unit).
    fn marker_width(&self) -> usize {
        match &self.marker {
            Some(marker) => {
                let after_marker = match &marker.task {
                    Some((_, task)) => task.start,
                    None => self.content_start,
                };
                after_marker
                    .saturating_sub(marker.range.start)
                    .max(marker.range.len() + 1)
            }
            None => TAB_STOP,
        }
    }
}

pub fn parse_line_prefix(line: &str) -> LinePrefix {
    let bytes = line.as_bytes();
    let mut quote_end = 0;
    loop {
        let spaces = bytes[quote_end..]
            .iter()
            .take_while(|&&b| b == b' ')
            .count();
        if spaces <= 3 && bytes.get(quote_end + spaces) == Some(&b'>') {
            quote_end += spaces + 1;
            if bytes.get(quote_end) == Some(&b' ') {
                quote_end += 1;
            }
        } else {
            break;
        }
    }
    let indent = bytes[quote_end..]
        .iter()
        .take_while(|&&b| b == b' ' || b == b'\t')
        .count();
    let start = quote_end + indent;
    let rest = &line[start..];
    let none = LinePrefix {
        quote_end,
        indent,
        marker: None,
        content_start: start,
    };
    if is_thematic_break(rest) {
        return none;
    }
    let followed_by_space =
        |at: usize| matches!(rest.as_bytes().get(at), None | Some(b' ') | Some(b'\t'));
    let (kind, len) = match rest.as_bytes().first() {
        Some(&b) if matches!(b, b'-' | b'*' | b'+') && followed_by_space(1) => {
            (MarkerKind::Bullet(char::from(b)), 1)
        }
        Some(b) if b.is_ascii_digit() => {
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            let delimiter = rest.as_bytes().get(digits).copied();
            match delimiter {
                Some(d @ (b'.' | b')')) if digits <= 9 && followed_by_space(digits + 1) => {
                    let Ok(number) = rest[..digits].parse() else {
                        return none;
                    };
                    (
                        MarkerKind::Ordered {
                            number,
                            delimiter: char::from(d),
                        },
                        digits + 1,
                    )
                }
                _ => return none,
            }
        }
        _ => return none,
    };
    let marker_end = start + len;
    let spaces = line[marker_end..]
        .bytes()
        .take_while(|&b| b == b' ' || b == b'\t')
        .count();
    let mut content_start = marker_end + spaces;
    let mut task = None;
    if spaces > 0 {
        let after = &line[content_start..];
        let checked = match after.get(..3) {
            Some("[ ]") => Some(false),
            Some("[x]") | Some("[X]") => Some(true),
            _ => None,
        };
        if let Some(checked) = checked {
            if matches!(after.as_bytes().get(3), None | Some(b' ') | Some(b'\t')) {
                task = Some((checked, content_start..content_start + 3));
                content_start += 3;
                if after.as_bytes().get(3).is_some() {
                    content_start += 1;
                }
            }
        }
    }
    LinePrefix {
        quote_end,
        indent,
        marker: Some(ListMarker {
            kind,
            range: start..marker_end,
            task,
        }),
        content_start,
    }
}

/// Where the quote markers of `line` end without its innermost level
/// (`"> > "` gives 2, `"> "` gives 0).
fn outer_quote_end(line: &str) -> usize {
    let bytes = line.as_bytes();
    let mut ends = vec![0];
    let mut at = 0;
    loop {
        let spaces = bytes[at..].iter().take_while(|&&b| b == b' ').count();
        if spaces <= 3 && bytes.get(at + spaces) == Some(&b'>') {
            at += spaces + 1;
            if bytes.get(at) == Some(&b' ') {
                at += 1;
            }
            ends.push(at);
        } else {
            break;
        }
    }
    ends.len()
        .checked_sub(2)
        .and_then(|index| ends.get(index))
        .copied()
        .unwrap_or(0)
}

fn is_thematic_break(text: &str) -> bool {
    let compact: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    compact.len() >= 3
        && matches!(compact[0], b'-' | b'*' | b'_')
        && compact.iter().all(|&b| b == compact[0])
}

/// Enter. In a fenced code block (`in_code`) it only keeps indentation.
pub fn newline(buffer: &mut Buffer, in_code: bool) {
    buffer.begin_compound();
    buffer.delete_selection();
    let pos = buffer.cursor();
    let line = buffer.line(pos.line).to_owned();
    let leading: String = line
        .chars()
        .take_while(|ch| *ch == ' ' || *ch == '\t')
        .collect();
    if in_code {
        let keep = if pos.col >= leading.len() {
            leading.as_str()
        } else {
            ""
        };
        buffer.insert_text(&format!("\n{keep}"));
        buffer.end_compound();
        return;
    }
    let prefix = parse_line_prefix(&line);
    let has_syntax = prefix.marker.is_some() || prefix.quote_end > 0;
    // A bare `-`, `*`, `+` or `1.` with nothing after it is text being typed
    // (a setext underline, say), not a list item the user started.
    let bare_marker = prefix
        .marker
        .as_ref()
        .is_some_and(|marker| marker.task.is_none() && prefix.content_start == marker.range.end);
    if bare_marker {
        buffer.insert_text("\n");
        buffer.end_compound();
        return;
    }
    if !has_syntax || pos.col < prefix.content_start {
        let keep = if !has_syntax && pos.col >= leading.len() {
            leading.as_str()
        } else {
            ""
        };
        buffer.insert_text(&format!("\n{keep}"));
        buffer.end_compound();
        return;
    }
    let empty = line[prefix.content_start..].trim().is_empty();
    if empty {
        if prefix.marker.is_some() && prefix.indent > 0 {
            // An empty nested item moves out a level.
            outdent_lines(buffer, pos.line, pos.line);
            let end = buffer.line(pos.line).len();
            buffer.set_cursor(Pos::new(pos.line, end), false);
        } else {
            // An empty item ends the list (inside its quote, if any); an
            // empty quote line steps out one quote level.
            let keep = if prefix.marker.is_some() {
                prefix.quote_end
            } else {
                outer_quote_end(&line)
            };
            let keep_text = line[..keep].trim_end().to_owned();
            let keep_text = if keep_text.is_empty() {
                keep_text
            } else {
                format!("{keep_text} ")
            };
            buffer.replace(
                Pos::new(pos.line, 0),
                Pos::new(pos.line, line.len()),
                &keep_text,
            );
            buffer.set_cursor(Pos::new(pos.line, keep_text.len()), false);
        }
        buffer.end_compound();
        return;
    }
    let mut continuation = line[..prefix.quote_end + prefix.indent].to_owned();
    if let Some(marker) = &prefix.marker {
        match marker.kind {
            MarkerKind::Bullet(ch) => {
                continuation.push(ch);
                continuation.push(' ');
            }
            MarkerKind::Ordered { number, delimiter } => {
                continuation.push_str(&format!("{}{delimiter} ", number.saturating_add(1)));
            }
        }
        if marker.task.is_some() {
            continuation.push_str("[ ] ");
        }
    }
    buffer.insert_text(&format!("\n{continuation}"));
    buffer.end_compound();
}

/// Ctrl-T: flips a task's checkbox, turns a list item into a task, or turns
/// a plain line into a task item.
pub fn toggle_task(buffer: &mut Buffer, line_index: usize) {
    if line_index >= buffer.line_count() {
        return;
    }
    let line = buffer.line(line_index).to_owned();
    let prefix = parse_line_prefix(&line);
    let (at, removed, inserted): (usize, usize, String) = match &prefix.marker {
        Some(ListMarker {
            task: Some((checked, range)),
            ..
        }) => {
            let flipped = if *checked { "[ ]" } else { "[x]" };
            (range.start, range.len(), flipped.to_owned())
        }
        Some(marker) => {
            if prefix.content_start > marker.range.end {
                (prefix.content_start, 0, "[ ] ".to_owned())
            } else {
                (marker.range.end, 0, " [ ] ".to_owned())
            }
        }
        None => (prefix.quote_end + prefix.indent, 0, "- [ ] ".to_owned()),
    };
    apply_line_edit(buffer, line_index, at, removed, &inserted);
}

/// Flips exactly the checkbox at `range` (its `[ ]` / `[x]` source) on
/// `line`, as found by the Markdown parse; unlike [`toggle_task`] it never
/// re-reads the line, so a nested `- - [ ] x` toggles the box that was
/// clicked.
pub fn toggle_checkbox(buffer: &mut Buffer, line_index: usize, range: Range<usize>) {
    if line_index >= buffer.line_count() {
        return;
    }
    let flipped = match buffer.line(line_index).get(range.clone()) {
        Some("[ ]") => "[x]",
        Some("[x]") | Some("[X]") => "[ ]",
        _ => return,
    };
    apply_line_edit(buffer, line_index, range.start, range.len(), flipped);
}

/// Replaces `removed` bytes at `at` on one line and keeps the cursor and
/// selection on the same text.
fn apply_line_edit(buffer: &mut Buffer, line: usize, at: usize, removed: usize, inserted: &str) {
    let cursor = buffer.cursor();
    let anchor = buffer.anchor();
    buffer.begin_compound();
    buffer.replace(Pos::new(line, at), Pos::new(line, at + removed), inserted);
    let shift = |pos: Pos| shift_pos(pos, line, at, removed, inserted.len(), true);
    restore(buffer, shift(cursor), anchor.map(shift));
    buffer.end_compound();
}

/// Where `pos` ends up after `removed` bytes at `at` became `inserted`
/// bytes. A position exactly at a pure insertion point moves only when
/// `push` is set.
fn shift_pos(pos: Pos, line: usize, at: usize, removed: usize, inserted: usize, push: bool) -> Pos {
    if pos.line != line || pos.col < at || (pos.col == at && removed == 0 && !push) {
        return pos;
    }
    if pos.col < at + removed {
        return Pos::new(line, at + inserted.min(pos.col - at));
    }
    Pos::new(line, pos.col - removed + inserted)
}

fn restore(buffer: &mut Buffer, cursor: Pos, anchor: Option<Pos>) {
    match anchor {
        Some(anchor) => buffer.set_selection(anchor, cursor),
        None => buffer.set_cursor(cursor, false),
    }
}

/// The lines a Tab / Shift-Tab acts on, and whether it acts on lines at all.
fn target_lines(buffer: &Buffer) -> (usize, usize, bool) {
    match buffer.selection() {
        Some((start, end)) if start.line != end.line => {
            // A selection ending at column 0 does not include that line.
            let last = if end.col == 0 { end.line - 1 } else { end.line };
            (start.line, last.max(start.line), true)
        }
        _ => {
            let line = buffer.cursor().line;
            let is_item = parse_line_prefix(buffer.line(line)).marker.is_some();
            (line, line, is_item)
        }
    }
}

/// Tab: indents list items (or every selected line); elsewhere inserts
/// spaces to the next tab stop.
pub fn indent(buffer: &mut Buffer) {
    let (first, last, lines) = target_lines(buffer);
    if !lines {
        // The screen column (CJK and emoji take two cells, a tab four).
        let cursor = buffer.cursor();
        let line = buffer.line(cursor.line);
        let col = str_width(line.get(..cursor.col).unwrap_or(line));
        let spaces = TAB_STOP - col % TAB_STOP;
        buffer.insert_text(&" ".repeat(spaces));
        return;
    }
    let cursor = buffer.cursor();
    let anchor = buffer.anchor();
    let (mut cursor, mut anchor) = (cursor, anchor);
    buffer.begin_compound();
    for line in first..=last {
        let text = buffer.line(line).to_owned();
        if text.trim().is_empty() && first != last {
            continue;
        }
        let prefix = parse_line_prefix(&text);
        let unit = indent_unit(buffer, line, &prefix);
        let at = prefix.quote_end;
        buffer.replace(Pos::new(line, at), Pos::new(line, at), &" ".repeat(unit));
        cursor = shift_pos(cursor, line, at, 0, unit, false);
        anchor = anchor.map(|anchor| shift_pos(anchor, line, at, 0, unit, false));
    }
    restore(buffer, cursor, anchor);
    buffer.end_compound();
}

/// Shift-Tab: outdents the current or selected lines.
pub fn outdent(buffer: &mut Buffer) {
    let (first, last, _) = target_lines(buffer);
    outdent_lines(buffer, first, last);
}

fn outdent_lines(buffer: &mut Buffer, first: usize, last: usize) {
    let (mut cursor, mut anchor) = (buffer.cursor(), buffer.anchor());
    buffer.begin_compound();
    for line in first..=last {
        let text = buffer.line(line).to_owned();
        let prefix = parse_line_prefix(&text);
        if prefix.indent == 0 {
            continue;
        }
        let at = prefix.quote_end;
        let remove = if text[at..].starts_with('\t') {
            1
        } else {
            outdent_amount(buffer, line, &prefix).min(prefix.indent)
        };
        buffer.replace(Pos::new(line, at), Pos::new(line, at + remove), "");
        cursor = shift_pos(cursor, line, at, remove, 0, false);
        anchor = anchor.map(|anchor| shift_pos(anchor, line, at, remove, 0, false));
    }
    restore(buffer, cursor, anchor);
    buffer.end_compound();
}

/// Spaces that nest `line` under the item above it: that sibling's marker
/// width (so `1.` children get 3 spaces), else the line's own.
fn indent_unit(buffer: &Buffer, line: usize, prefix: &LinePrefix) -> usize {
    if prefix.marker.is_none() {
        return TAB_STOP;
    }
    for above in (line.saturating_sub(LOOKBACK)..line).rev() {
        let other = parse_line_prefix(buffer.line(above));
        if other.marker.is_some() && other.indent == prefix.indent {
            return other.marker_width();
        }
        if other.marker.is_some() && other.indent < prefix.indent {
            break;
        }
    }
    prefix.marker_width()
}

/// Spaces to remove so `line` lines up with its parent item.
fn outdent_amount(buffer: &Buffer, line: usize, prefix: &LinePrefix) -> usize {
    if prefix.marker.is_none() {
        return TAB_STOP;
    }
    for above in (line.saturating_sub(LOOKBACK)..line).rev() {
        let other = parse_line_prefix(buffer.line(above));
        if other.marker.is_some() && other.indent < prefix.indent {
            return prefix.indent - other.indent;
        }
    }
    prefix.indent
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_end(text: &str, line: usize) -> Buffer {
        let mut buffer = Buffer::from_text(text);
        let end = buffer.line(line).len();
        buffer.set_cursor(Pos::new(line, end), false);
        buffer
    }

    #[test]
    fn parses_prefixes() {
        let prefix = parse_line_prefix("> > - [x] done");
        assert_eq!(prefix.quote_end, 4);
        assert_eq!(prefix.indent, 0);
        let marker = prefix.marker.unwrap();
        assert_eq!(marker.kind, MarkerKind::Bullet('-'));
        assert_eq!(marker.task, Some((true, 6..9)));
        assert_eq!(prefix.content_start, 10);

        let prefix = parse_line_prefix("   12) item");
        assert_eq!(prefix.indent, 3);
        assert_eq!(
            prefix.marker.unwrap().kind,
            MarkerKind::Ordered {
                number: 12,
                delimiter: ')'
            }
        );
        assert_eq!(prefix.content_start, 7);

        assert!(parse_line_prefix("---").marker.is_none());
        assert!(parse_line_prefix("* * *").marker.is_none());
        assert!(parse_line_prefix("-not a list").marker.is_none());
        assert!(parse_line_prefix("1.5 is a number").marker.is_none());
        assert!(parse_line_prefix("-").marker.is_some());
        assert_eq!(parse_line_prefix("- [ ]").content_start, 5);
        assert_eq!(parse_line_prefix("- [x]tight").marker.unwrap().task, None);
    }

    #[test]
    fn enter_continues_bullets_numbers_tasks_and_quotes() {
        let mut buffer = at_end("- one", 0);
        newline(&mut buffer, false);
        assert_eq!(buffer.to_text(), "- one\n- ");
        assert_eq!(buffer.cursor(), Pos::new(1, 2));

        let mut buffer = at_end("  9. nine", 0);
        newline(&mut buffer, false);
        assert_eq!(buffer.to_text(), "  9. nine\n  10. ");

        let mut buffer = at_end("* [x] done", 0);
        newline(&mut buffer, false);
        assert_eq!(buffer.to_text(), "* [x] done\n* [ ] ");

        let mut buffer = at_end("> quoted", 0);
        newline(&mut buffer, false);
        assert_eq!(buffer.to_text(), "> quoted\n> ");

        let mut buffer = at_end("> - in quote", 0);
        newline(&mut buffer, false);
        assert_eq!(buffer.to_text(), "> - in quote\n> - ");
    }

    #[test]
    fn enter_splits_text_after_the_cursor() {
        let mut buffer = Buffer::from_text("- hello world");
        buffer.set_cursor(Pos::new(0, 8), false);
        newline(&mut buffer, false);
        assert_eq!(buffer.to_text(), "- hello \n- world");
        assert_eq!(buffer.cursor(), Pos::new(1, 2));
        // At column 0 it just opens a line above.
        let mut buffer = Buffer::from_text("- item");
        newline(&mut buffer, false);
        assert_eq!(buffer.to_text(), "\n- item");
    }

    #[test]
    fn enter_on_empty_item_ends_or_outdents_the_list() {
        let mut buffer = at_end("- one\n- ", 1);
        newline(&mut buffer, false);
        assert_eq!(buffer.to_text(), "- one\n");
        assert_eq!(buffer.cursor(), Pos::new(1, 0));

        let mut buffer = at_end("- one\n  - ", 1);
        newline(&mut buffer, false);
        assert_eq!(buffer.to_text(), "- one\n- ");

        let mut buffer = at_end("> - x\n> - [ ] ", 1);
        newline(&mut buffer, false);
        assert_eq!(buffer.to_text(), "> - x\n> ");

        let mut buffer = at_end("> a\n> ", 1);
        newline(&mut buffer, false);
        assert_eq!(buffer.to_text(), "> a\n");
        // One undo step restores it.
        buffer.undo();
        assert_eq!(buffer.to_text(), "> a\n> ");
    }

    #[test]
    fn enter_after_a_bare_marker_keeps_it_and_opens_a_line() {
        for (text, line) in [
            ("Heading\n-", 1),
            ("-", 0),
            ("1.", 0),
            ("+", 0),
            ("Total\n*", 1),
        ] {
            let mut buffer = at_end(text, line);
            newline(&mut buffer, false);
            assert_eq!(buffer.to_text(), format!("{text}\n"), "{text:?}");
            assert_eq!(buffer.cursor(), Pos::new(line + 1, 0));
        }
    }

    #[test]
    fn enter_on_an_empty_nested_quote_steps_out_one_level() {
        let mut buffer = at_end("> > a\n> > ", 1);
        newline(&mut buffer, false);
        assert_eq!(buffer.to_text(), "> > a\n> ");
        assert_eq!(buffer.cursor(), Pos::new(1, 2));
        newline(&mut buffer, false);
        assert_eq!(buffer.to_text(), "> > a\n");
        assert_eq!(outer_quote_end("> > > x"), 4);
        assert_eq!(outer_quote_end("text"), 0);
    }

    #[test]
    fn toggle_checkbox_flips_only_the_clicked_box() {
        let mut buffer = Buffer::from_text("- - [ ] x\n1. - [x] y\nplain");
        toggle_checkbox(&mut buffer, 0, 4..7);
        toggle_checkbox(&mut buffer, 1, 5..8);
        // Not a checkbox there: nothing happens.
        toggle_checkbox(&mut buffer, 2, 0..3);
        assert_eq!(buffer.to_text(), "- - [x] x\n1. - [ ] y\nplain");
    }

    #[test]
    fn tab_stops_count_screen_columns() {
        let mut buffer = Buffer::from_text("日本x");
        buffer.move_end(false);
        indent(&mut buffer);
        // 5 cells wide: three spaces reach the stop at cell 8.
        assert_eq!(buffer.to_text(), "日本x   ");
    }

    #[test]
    fn enter_keeps_indentation_in_prose_and_code() {
        let mut buffer = at_end("    indented", 0);
        newline(&mut buffer, false);
        assert_eq!(buffer.to_text(), "    indented\n    ");
        let mut buffer = at_end("  - code-looking", 0);
        newline(&mut buffer, true);
        assert_eq!(buffer.to_text(), "  - code-looking\n  ");
        let mut buffer = Buffer::from_text("selected");
        buffer.select_all();
        newline(&mut buffer, false);
        assert_eq!(buffer.to_text(), "\n");
    }

    #[test]
    fn toggles_tasks() {
        let mut buffer = Buffer::from_text("- [ ] a\n- b\nplain\n> quoted\n\n-");
        toggle_task(&mut buffer, 0);
        toggle_task(&mut buffer, 1);
        toggle_task(&mut buffer, 2);
        toggle_task(&mut buffer, 3);
        toggle_task(&mut buffer, 4);
        toggle_task(&mut buffer, 5);
        assert_eq!(
            buffer.to_text(),
            "- [x] a\n- [ ] b\n- [ ] plain\n> - [ ] quoted\n- [ ] \n- [ ] "
        );
        toggle_task(&mut buffer, 0);
        assert!(buffer.line(0).starts_with("- [ ] a"));
        toggle_task(&mut buffer, 99);
    }

    #[test]
    fn toggle_task_keeps_the_cursor_on_its_text() {
        let mut buffer = Buffer::from_text("- item");
        buffer.set_cursor(Pos::new(0, 4), false);
        toggle_task(&mut buffer, 0);
        assert_eq!(buffer.to_text(), "- [ ] item");
        assert_eq!(buffer.cursor(), Pos::new(0, 8));
        buffer.undo();
        assert_eq!(buffer.to_text(), "- item");
        assert_eq!(buffer.cursor(), Pos::new(0, 4));
    }

    #[test]
    fn tab_indents_items_under_their_sibling() {
        let mut buffer = at_end("- a\n- b", 1);
        indent(&mut buffer);
        assert_eq!(buffer.to_text(), "- a\n  - b");
        assert_eq!(buffer.cursor(), Pos::new(1, 5));
        outdent(&mut buffer);
        assert_eq!(buffer.to_text(), "- a\n- b");

        let mut buffer = at_end("1. a\n2. b", 1);
        indent(&mut buffer);
        assert_eq!(buffer.to_text(), "1. a\n   2. b");
        outdent(&mut buffer);
        assert_eq!(buffer.to_text(), "1. a\n2. b");

        let mut buffer = at_end("> - a\n> - b", 1);
        indent(&mut buffer);
        assert_eq!(buffer.to_text(), "> - a\n>   - b");
    }

    #[test]
    fn tab_elsewhere_inserts_spaces_and_shift_tab_removes_them() {
        let mut buffer = Buffer::from_text("ab");
        buffer.move_end(false);
        indent(&mut buffer);
        assert_eq!(buffer.to_text(), "ab  ");
        let mut buffer = Buffer::from_text("      text");
        outdent(&mut buffer);
        assert_eq!(buffer.to_text(), "  text");
        let mut buffer = Buffer::from_text("\ttext");
        outdent(&mut buffer);
        assert_eq!(buffer.to_text(), "text");
    }

    #[test]
    fn tab_on_a_selection_indents_every_line_as_one_step() {
        let mut buffer = Buffer::from_text("- a\n- b\n\n- c\nnext");
        buffer.set_selection(Pos::new(1, 0), Pos::new(4, 0));
        indent(&mut buffer);
        assert_eq!(buffer.to_text(), "- a\n  - b\n\n  - c\nnext");
        assert_eq!(buffer.selection(), Some((Pos::new(1, 0), Pos::new(4, 0))));
        buffer.undo();
        assert_eq!(buffer.to_text(), "- a\n- b\n\n- c\nnext");
    }
}
