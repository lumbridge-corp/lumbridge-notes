//! Pure rendering: draws `&App` into a ratatui frame. Layout and scroll
//! math already happened in [`App::prepare`]; nothing here mutates state.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span as TextSpan};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, Bar, Find, Level, Screen};
use crate::buffer::Pos;
use crate::editor::Editor;
use crate::layout::{display_grapheme, grapheme_width, graphemes, str_width, LineMode};
use crate::markdown::{Leaf, LineInfo, Span, SpanKind};
use crate::picker::Picker;
use crate::search::Search;
use crate::store;
use crate::text::{ellipsize, fit_hints, sanitize};
use crate::theme::Theme;

mod assist;
pub use assist::title_width as assist_title_width;
pub use assist::{
    menu_index, menu_rect, menu_rows, panel_button_hit, panel_max_scroll, toolbar, toolbar_hit,
    wrap, ButtonLook, ToolButton,
};

/// Widest the note text gets; wider panes centre it.
pub const MAX_TEXT_WIDTH: u16 = 100;
/// Smaller panes only show a "too small" note.
pub const MIN_WIDTH: u16 = 12;
pub const MIN_HEIGHT: u16 = 5;
/// Panes at least this wide show the picker preview.
const PREVIEW_MIN_WIDTH: u16 = 90;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorLayout {
    pub title: Rect,
    /// The rows between the title and the bar / status line.
    pub body: Rect,
    /// Where note text goes inside `body` (margins, max width).
    pub text: Rect,
    pub bar: Option<Rect>,
    pub status: Rect,
    /// The AI answer panel under the text, when open.
    pub panel: Option<Rect>,
}

pub fn editor_layout(area: Rect, bar: bool, panel: bool) -> EditorLayout {
    let row = |y: u16| Rect::new(area.x, y, area.width, 1);
    let title = row(area.y);
    let status = row(area.y + area.height.saturating_sub(1));
    let reserved = if bar { 3 } else { 2 };
    let full_height = area.height.saturating_sub(reserved);
    let body_y = area.y + 1.min(area.height);
    // The panel takes the lower part of the body, leaving the note at least
    // three rows.
    let panel_height = if panel && full_height >= 7 {
        (full_height * 9 / 20).max(6).min(full_height - 3)
    } else {
        0
    };
    let body_height = full_height - panel_height;
    let body = Rect::new(area.x, body_y, area.width, body_height);
    let panel = (panel_height > 0)
        .then(|| Rect::new(area.x, body_y + body_height, area.width, panel_height));
    let bar = bar.then(|| row(area.y + area.height.saturating_sub(2)));
    let margin = if body.width >= 30 { 1 } else { 0 };
    let inner = body.width.saturating_sub(2 * margin);
    let width = inner.min(MAX_TEXT_WIDTH);
    let x = body.x + margin + (inner - width) / 2;
    let text = Rect::new(x, body.y, width, body.height);
    EditorLayout {
        title,
        body,
        text,
        bar,
        status,
        panel,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListLayout {
    pub title: Rect,
    pub input: Rect,
    pub list: Rect,
    pub preview: Option<Rect>,
    pub status: Rect,
}

pub fn list_layout(area: Rect) -> ListLayout {
    let row = |y: u16| Rect::new(area.x, y, area.width, 1);
    let title = row(area.y);
    let input = row(area.y + 1.min(area.height));
    let status = row(area.y + area.height.saturating_sub(1));
    let list_y = area.y + 3.min(area.height);
    let list_height = area.height.saturating_sub(4);
    if area.width >= PREVIEW_MIN_WIDTH {
        let list_width = area.width * 9 / 20;
        let list = Rect::new(area.x, list_y, list_width, list_height);
        let preview = Rect::new(
            area.x + list_width + 2,
            list_y,
            area.width.saturating_sub(list_width + 3),
            list_height,
        );
        ListLayout {
            title,
            input,
            list,
            preview: Some(preview),
            status,
        }
    } else {
        ListLayout {
            title,
            input,
            list: Rect::new(area.x, list_y, area.width, list_height),
            preview: None,
            status,
        }
    }
}

/// Rows per entry in the picker and search lists: title and detail, or just
/// the title when the list is a single row tall.
pub fn entry_rows(list: Rect) -> u16 {
    if list.height >= 2 {
        2
    } else {
        1
    }
}

/// Editor key hints, most important first (whole hints are dropped from the
/// end when the status line is narrow).
const EDITOR_HINTS: [&str; 13] = [
    "^Q quit",
    "^S save",
    "^O notes",
    "^K ai",
    "^R read",
    "^D dictate",
    "^G search",
    "^F find",
    "^N new",
    "^P template",
    "^E source",
    "^Z undo",
    "^Y redo",
];

pub fn render(app: &App, frame: &mut Frame) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        let text = ellipsize("lumbridge-notes: pane too small", usize::from(area.width));
        if area.height > 0 {
            frame.buffer_mut().set_stringn(
                area.x,
                area.y,
                &text,
                usize::from(area.width),
                app.theme.dim(),
            );
        }
        return;
    }
    match &app.screen {
        Screen::Editor => render_editor(app, frame, area),
        Screen::Picker(picker) => render_picker(app, picker, frame, area),
        Screen::Search(search) => render_search(app, search, frame, area),
    }
}

// ---------------------------------------------------------------------------
// Editor
// ---------------------------------------------------------------------------

fn render_editor(app: &App, frame: &mut Frame, area: Rect) {
    let theme = app.theme;
    let layout = editor_layout(area, app.bar_shown(), app.assist.panel.is_some());
    let Some(editor) = &app.editor else {
        draw_status(
            frame.buffer_mut(),
            layout.status,
            theme,
            app,
            &["^O notes", "^Q quit"],
            "",
        );
        return;
    };
    let buf = frame.buffer_mut();
    let title_end = assist::draw_toolbar(buf, app, area, theme);
    let title_area = Rect::new(
        layout.title.x,
        layout.title.y,
        title_end.saturating_sub(layout.title.x),
        layout.title.height,
    );
    draw_title(buf, title_area, theme, editor);
    // The paragraph being read aloud gets a bar in the left margin.
    let reading = app
        .assist
        .speaking
        .as_ref()
        .filter(|speaking| speaking.path == editor.path)
        .and_then(|speaking| speaking.current_lines());
    let find = match &app.bar {
        Bar::Find(find) => Some(find),
        _ => None,
    };
    let selection = editor.buffer.selection();
    let text_area = editor.view.area;
    for (y, pos) in editor.view.rows.iter().enumerate() {
        let screen_y = text_area.y + y as u16;
        if screen_y >= layout.body.y + layout.body.height {
            break;
        }
        draw_row(
            buf,
            theme,
            editor,
            RowTarget {
                line: pos.line,
                row: pos.row,
                y: screen_y,
                text: text_area,
                body: layout.body,
            },
            find,
            selection,
        );
        if reading
            .as_ref()
            .is_some_and(|lines| lines.contains(&pos.line))
            && text_area.x > layout.body.x
        {
            buf.set_stringn(text_area.x - 1, screen_y, "▎", 1, theme.reading());
        }
    }
    if let Some(panel) = layout.panel {
        assist::draw_panel(buf, app, panel, theme);
    }
    let hints: &[&str] = match &app.bar {
        Bar::Find(_) => &["Enter/↓ next", "↑ prev", "Esc close"],
        Bar::NewNote(_) | Bar::NewItem { .. } => &["Enter create", "Esc cancel"],
        Bar::Ask(_) => &["Enter ask", "Esc cancel"],
        Bar::None if app.assist.menu.is_some() => &["↑↓ choose", "Enter pick", "Esc close"],
        Bar::None if app.assist.panel.is_some() => &[
            "Enter use",
            "r replace",
            "i insert",
            "a append",
            "c copy",
            "g retry",
            "Esc close",
        ],
        Bar::None => &EDITOR_HINTS,
    };
    let right = status_right(editor);
    draw_status(buf, layout.status, theme, app, hints, &right);
    match (&app.bar, layout.bar) {
        (Bar::Find(find), Some(bar)) => {
            let count = match (find.current, find.matches.len()) {
                (_, 0) if find.input.text.is_empty() => String::new(),
                (_, 0) => "no matches".to_owned(),
                (Some(index), len) => format!(
                    "{}/{len}{}",
                    index + 1,
                    if len >= crate::editor::FIND_LIMIT {
                        "+"
                    } else {
                        ""
                    }
                ),
                (None, len) => format!("{len}"),
            };
            let x = draw_input(buf, bar, theme, " Find: ", &find.input.text, &count);
            frame.set_cursor_position((x(find.input.cursor_x()), bar.y));
        }
        (Bar::NewNote(input), Some(bar)) => {
            let x = draw_input(buf, bar, theme, " New note title: ", &input.text, "");
            frame.set_cursor_position((x(input.cursor_x()), bar.y));
        }
        (Bar::Ask(input), Some(bar)) => {
            let scope = if editor.buffer.selection().is_some() {
                "about the selection"
            } else {
                "about this note"
            };
            let x = draw_input(buf, bar, theme, " ✦ Ask AI: ", &input.text, scope);
            frame.set_cursor_position((x(input.cursor_x()), bar.y));
        }
        (Bar::NewItem { prompt, input }, Some(bar)) => {
            let label = if *prompt {
                " New prompt name: "
            } else {
                " New template name: "
            };
            let x = draw_input(buf, bar, theme, label, &input.text, "");
            frame.set_cursor_position((x(input.cursor_x()), bar.y));
        }
        (Bar::None, Some(bar)) if app.assist.live.is_some() => {
            assist::draw_live_bar(buf, app, bar, theme);
            if let (Some((cx, cy)), None) = (editor.view.cursor, app.assist.menu.as_ref()) {
                frame.set_cursor_position((text_area.x + cx, text_area.y + cy));
            }
        }
        _ => {
            let modal = app.assist.menu.is_some() || app.assist.panel.is_some();
            if let (Some((cx, cy)), false) = (editor.view.cursor, modal) {
                frame.set_cursor_position((text_area.x + cx, text_area.y + cy));
            }
        }
    }
    assist::draw_menu(frame.buffer_mut(), app, area, theme);
}

fn draw_title(buf: &mut Buffer, area: Rect, theme: Theme, editor: &Editor) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let width = usize::from(area.width);
    let dirty = editor.is_dirty();
    let marker = if dirty { " ●" } else { "" };
    let title = sanitize(&editor.title());
    let room = width.saturating_sub(1 + str_width(marker));
    let title = ellipsize(&title, room);
    let mut x = area.x + 1;
    let (end, _) = buf.set_stringn(x, area.y, &title, room, theme.title());
    x = end;
    if dirty {
        let (end, _) = buf.set_stringn(x, area.y, marker, 2, theme.unsaved());
        x = end;
    }
    let used = usize::from(x - area.x);
    if used + 3 < width {
        let path = ellipsize(&sanitize(&editor.display_path), width - used - 3);
        buf.set_stringn(x + 2, area.y, &path, width - used - 2, theme.dim());
    }
}

fn status_right(editor: &Editor) -> String {
    let cursor = editor.buffer.cursor();
    let state = if editor.conflict {
        "conflict"
    } else if editor.read_only.is_some() {
        "read-only"
    } else if editor.deleted {
        "deleted"
    } else if editor.is_dirty() {
        "unsaved"
    } else if editor.stamp.is_none() {
        "new"
    } else {
        "saved"
    };
    let mode = if editor.source_mode { "source · " } else { "" };
    let words = editor.word_total;
    let word_label = if words == 1 { "word" } else { "words" };
    format!(
        "{mode}{}:{} · {words} {word_label} · {state} ",
        cursor.line + 1,
        editor.buffer.grapheme_col(cursor) + 1
    )
}

fn draw_status(buf: &mut Buffer, area: Rect, theme: Theme, app: &App, hints: &[&str], right: &str) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let width = usize::from(area.width);
    let right_width = str_width(right).min(width);
    let right_style = match app.editor.as_ref() {
        Some(editor) if editor.conflict || editor.deleted => theme.warn(),
        Some(editor) if editor.is_dirty() => theme.unsaved(),
        _ => theme.dim(),
    };
    let right_x = area.x + (width - right_width) as u16;
    buf.set_stringn(right_x, area.y, right, right_width, right_style);
    let room = width.saturating_sub(right_width + 2);
    let (text, style) = match &app.message {
        Some(message) => (
            ellipsize(&sanitize(&message.text), room),
            match message.level {
                Level::Info => theme.ok(),
                Level::Warn => theme.warn(),
                Level::Error => theme.error(),
            },
        ),
        None => (fit_hints(hints, room), theme.dim()),
    };
    buf.set_stringn(area.x + 1, area.y, &text, room, style);
}

/// Draws `label text … right` on a bar row; returns a function mapping the
/// input's cursor column to a screen column.
fn draw_input(
    buf: &mut Buffer,
    area: Rect,
    theme: Theme,
    label: &str,
    text: &str,
    right: &str,
) -> impl Fn(usize) -> u16 {
    let width = usize::from(area.width);
    buf.set_stringn(area.x, area.y, label, width, theme.accent());
    let label_width = str_width(label).min(width);
    let right_width = str_width(right);
    if right_width + label_width + 2 < width {
        buf.set_stringn(
            area.x + (width - right_width - 1) as u16,
            area.y,
            right,
            right_width,
            theme.dim(),
        );
    }
    let room = width.saturating_sub(label_width + right_width + 2);
    // Keep the end of a long query visible.
    let shown = sanitize(text);
    let total = str_width(&shown);
    let skip = total.saturating_sub(room);
    let mut x = 0;
    let mut start = 0;
    for (offset, grapheme) in graphemes(&shown) {
        if x >= skip {
            start = offset;
            break;
        }
        x += grapheme_width(grapheme);
        start = offset + grapheme.len();
    }
    let visible = shown.get(start..).unwrap_or("");
    buf.set_stringn(
        area.x + label_width as u16,
        area.y,
        visible,
        room,
        Style::default(),
    );
    let base = area.x + label_width as u16;
    let max = area.x + area.width.saturating_sub(1);
    move |cursor_x: usize| (base + cursor_x.saturating_sub(x) as u16).min(max)
}

struct RowTarget {
    line: usize,
    row: usize,
    y: u16,
    text: Rect,
    body: Rect,
}

fn draw_row(
    buf: &mut Buffer,
    theme: Theme,
    editor: &Editor,
    target: RowTarget,
    find: Option<&Find>,
    selection: Option<(Pos, Pos)>,
) {
    let RowTarget {
        line,
        row,
        y,
        text: area,
        body,
    } = target;
    let source = editor.buffer.line(line);
    let Some(info) = editor.doc.line(line) else {
        return;
    };
    let mode = editor.view.modes.of(line);
    let Some(layout) = editor.cache.get(line, mode) else {
        return;
    };
    let Some(row_ref) = layout.rows.get(row) else {
        return;
    };
    let code = matches!(info.leaf, Leaf::Code { .. } | Leaf::CodeFence { .. });
    if code {
        buf.set_style(Rect::new(body.x, y, body.width, 1), theme.code_block());
    }
    let right = area.x + area.width;
    let rendered = mode == LineMode::Rendered;
    // A rule (or setext underline) fills the row after its quote bars and
    // list indentation, which are drawn as usual below. Without colour, a
    // hidden code fence is drawn as a dashed line so the block stays visible.
    let fill = match info.leaf {
        Leaf::Rule | Leaf::SetextUnderline { .. } if rendered => Some(("─", theme.rule())),
        Leaf::CodeFence { .. } if rendered && !theme.color => Some(("┄", theme.dim())),
        _ => None,
    };
    if let Some((glyph, style)) = fill {
        let start = area.x + row_ref.end_x() as u16;
        if start < right {
            let width = usize::from(right - start);
            buf.set_stringn(start, y, glyph.repeat(width), width, style);
        }
    }
    let (current_line, current_range) = match find.and_then(|find| {
        find.current
            .and_then(|index| find.matches.get(index))
            .map(|(line, range)| (*line, range.clone()))
    }) {
        Some((line, range)) => (Some(line), range),
        None => (None, 0..0),
    };
    let matches = find.map_or(&[][..], |find| find.matches_on(line));
    let selected = |byte: usize| -> bool {
        selection.is_some_and(|(start, end)| {
            let pos = Pos::new(line, byte);
            pos >= start && pos < end
        })
    };
    for piece in &row_ref.pieces {
        let Some(span) = layout.span(piece) else {
            continue;
        };
        let style = span_style(theme, info, span, mode);
        let text = layout.piece_text(source, piece);
        let verbatim = span.is_verbatim();
        let base = span.source.start + piece.text.start;
        let mut x = area.x + piece.x as u16;
        for (offset, grapheme) in graphemes(text) {
            let width = grapheme_width(grapheme);
            if usize::from(x) + width > usize::from(right) {
                break;
            }
            let byte = if verbatim {
                Some(base + offset)
            } else if !span.source.is_empty() {
                Some(span.source.start)
            } else {
                None
            };
            let mut cell_style = style;
            if let Some(byte) = byte {
                if matches.iter().any(|(_, range)| range.contains(&byte)) {
                    let current = current_line == Some(line) && current_range.contains(&byte);
                    cell_style = cell_style.patch(if current {
                        theme.find_current()
                    } else {
                        theme.find_match()
                    });
                }
                if selected(byte) {
                    cell_style = cell_style.patch(theme.selection());
                }
            }
            buf.set_stringn(x, y, display_grapheme(grapheme), width, cell_style);
            x += width as u16;
        }
    }
    // A selection that runs past the end of the line shows one cell for the
    // line break.
    if row + 1 == layout.rows.len() {
        if let Some((start, end)) = selection {
            let eol = Pos::new(line, source.len());
            let end_x = area.x + row_ref.end_x() as u16;
            if eol >= start && eol < end && end_x < right {
                buf.set_stringn(end_x, y, " ", 1, theme.selection());
            }
        }
    }
    if mode == LineMode::Rendered && row == 0 {
        if let Leaf::CodeFence {
            lang: Some(lang),
            opening: true,
        } = &info.leaf
        {
            let label = sanitize(lang);
            let width = str_width(&label);
            if width + 2 < usize::from(area.width) {
                let x = right - width as u16 - 1;
                buf.set_stringn(
                    x,
                    y,
                    &label,
                    width,
                    theme.code_block().patch(theme.code_label()),
                );
            }
        }
    }
}

/// The style of one span on a line.
pub fn span_style(theme: Theme, info: &LineInfo, span: &Span, mode: LineMode) -> Style {
    let rendered = mode == LineMode::Rendered;
    let code_leaf = matches!(info.leaf, Leaf::Code { .. } | Leaf::CodeFence { .. });
    let backdrop = if code_leaf {
        theme.code_block()
    } else {
        Style::default()
    };
    let mut style = match info.leaf {
        Leaf::Heading { level } => {
            let style = theme.heading(level);
            if rendered && level <= 2 {
                style.add_modifier(Modifier::UNDERLINED)
            } else {
                style
            }
        }
        Leaf::SetextUnderline { level } => theme.heading(level),
        Leaf::Code { .. } | Leaf::CodeFence { .. } => theme.code_block(),
        Leaf::TableRow { header: true } => Style::default().add_modifier(Modifier::BOLD),
        Leaf::FrontMatter { .. } => theme.front_matter(),
        Leaf::Html => theme.html(),
        Leaf::Other => theme.dim(),
        _ => Style::default(),
    };
    let heading = matches!(info.leaf, Leaf::Heading { .. });
    if info.quote_depth > 0 && !heading && !code_leaf {
        style = style.patch(theme.quote_text());
    }
    let mut style = match span.kind {
        SpanKind::Text => style,
        SpanKind::Code => {
            if code_leaf {
                style
            } else {
                style.patch(theme.inline_code())
            }
        }
        SpanKind::Marker => style.patch(theme.marker()),
        SpanKind::QuoteBar => backdrop.patch(theme.quote_bar()),
        SpanKind::Bullet => backdrop.patch(theme.bullet()),
        SpanKind::Number => backdrop.patch(theme.number()),
        SpanKind::Checkbox { checked } => backdrop.patch(theme.checkbox(checked)),
        SpanKind::Indent | SpanKind::TablePad => backdrop,
        SpanKind::TableSep => backdrop.patch(theme.table_border()),
        SpanKind::Html => style.patch(theme.html()),
        SpanKind::ImageMarker => backdrop.patch(theme.image()),
        SpanKind::FrontMatter => theme.front_matter(),
    };
    if matches!(span.kind, SpanKind::Text | SpanKind::Code) {
        let inline = span.inline;
        if inline.strong {
            style = style.add_modifier(Modifier::BOLD);
        }
        if inline.emph {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if inline.strike {
            style = style.patch(theme.dim()).add_modifier(Modifier::CROSSED_OUT);
        }
        if inline.link {
            style = style.patch(theme.link());
        }
        if inline.image {
            style = style.patch(theme.image());
        }
        // Every line of a checked task, not only the one with the box.
        let checked = rendered && info.task == Some(true);
        if checked {
            style = style.patch(theme.dim()).add_modifier(Modifier::CROSSED_OUT);
        }
    }
    style
}

// ---------------------------------------------------------------------------
// Picker and search
// ---------------------------------------------------------------------------

fn draw_list_header(
    buf: &mut Buffer,
    layout: &ListLayout,
    theme: Theme,
    title: &str,
    detail: &str,
) {
    let width = usize::from(layout.title.width);
    let (end, _) = buf.set_stringn(
        layout.title.x + 1,
        layout.title.y,
        title,
        width,
        theme.title(),
    );
    let used = usize::from(end - layout.title.x);
    if used + 3 < width {
        let detail = ellipsize(&sanitize(detail), width - used - 3);
        buf.set_stringn(
            end + 2,
            layout.title.y,
            &detail,
            width - used - 2,
            theme.dim(),
        );
    }
}

fn render_picker(app: &App, picker: &Picker, frame: &mut Frame, area: Rect) {
    let theme = app.theme;
    let layout = list_layout(area);
    let buf = frame.buffer_mut();
    let count = picker.notes.len();
    let detail = format!(
        "{} · {count} note{}",
        picker.root_display,
        if count == 1 { "" } else { "s" }
    );
    draw_list_header(buf, &layout, theme, "Notes", &detail);
    let x = draw_input(buf, layout.input, theme, " › ", &picker.input.text, "");
    let cursor = (x(picker.input.cursor_x()), layout.input.y);
    let list = layout.list;
    if picker.filtered.is_empty() {
        let text = if picker.notes.is_empty() {
            "No notes yet. Type a title and press Enter.".to_owned()
        } else {
            "No match. Enter creates a note with this title.".to_owned()
        };
        buf.set_stringn(
            list.x + 2,
            list.y,
            &text,
            usize::from(list.width.saturating_sub(2)),
            theme.dim(),
        );
    }
    let per = entry_rows(list);
    let rows = usize::from(list.height / per);
    for (slot, &index) in picker
        .filtered
        .iter()
        .enumerate()
        .skip(picker.scroll)
        .take(rows)
    {
        let Some(note) = picker.notes.get(index) else {
            continue;
        };
        let y = list.y + ((slot - picker.scroll) * usize::from(per)) as u16;
        let selected = slot == picker.selected;
        if selected {
            buf.set_style(Rect::new(list.x, y, list.width, per), theme.picked());
            buf.set_stringn(list.x, y, "▌", 1, theme.accent());
        }
        let width = usize::from(list.width.saturating_sub(3));
        let title = ellipsize(&sanitize(&note.title), width);
        buf.set_stringn(
            list.x + 2,
            y,
            &title,
            width,
            Style::default().add_modifier(Modifier::BOLD),
        );
        let mut detail = store::local_date(note.modified);
        detail.push_str(" · ");
        detail.push_str(&note.relative);
        for tag in &note.tags {
            detail.push_str(" #");
            detail.push_str(tag);
        }
        if per > 1 {
            let detail = ellipsize(&sanitize(&detail), width);
            buf.set_stringn(list.x + 2, y + 1, &detail, width, theme.dim());
        }
    }
    if let (Some(preview), Some(note)) = (layout.preview, picker.selected_note()) {
        draw_separator(buf, theme, preview);
        let mut lines = vec![Line::from(TextSpan::styled(
            sanitize(&note.title),
            theme.heading(1),
        ))];
        if !note.tags.is_empty() {
            let tags: Vec<String> = note.tags.iter().map(|tag| format!("#{tag}")).collect();
            lines.push(Line::from(TextSpan::styled(
                sanitize(&tags.join(" ")),
                theme.dim(),
            )));
        }
        lines.push(Line::default());
        lines.extend(preview_lines(
            note.body(),
            theme,
            usize::from(preview.height),
        ));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), preview);
    }
    let buf = frame.buffer_mut();
    // With no note open, Esc leaves the app.
    let esc = if app.editor.is_some() {
        "Esc back"
    } else {
        "Esc quit"
    };
    draw_status(
        buf,
        layout.status,
        theme,
        app,
        &[esc, "Enter open", "↑↓ select", "^N new", "^G search"],
        "",
    );
    frame.set_cursor_position(cursor);
}

fn draw_separator(buf: &mut Buffer, theme: Theme, preview: Rect) {
    if preview.x < 2 {
        return;
    }
    for y in preview.y..preview.y + preview.height {
        buf.set_stringn(preview.x - 2, y, "│", 1, theme.border());
    }
}

/// The first lines of a note body, lightly styled, for the preview pane.
fn preview_lines(body: &str, theme: Theme, max: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut in_code = false;
    let body = body.trim_start_matches(['\n', '\r']);
    for line in body.lines().take(max.max(1) * 2) {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code = !in_code;
            continue;
        }
        let clean = sanitize(line);
        let style = if in_code {
            theme.code_block()
        } else if let Some(level) = heading_level(trimmed) {
            let text = trimmed[level..].trim().to_owned();
            out.push(Line::from(TextSpan::styled(
                sanitize(&text),
                theme.heading(level as u8),
            )));
            continue;
        } else if trimmed.starts_with('>') {
            theme.quote_text()
        } else {
            Style::default()
        };
        out.push(Line::from(TextSpan::styled(clean, style)));
    }
    out
}

fn heading_level(line: &str) -> Option<usize> {
    let level = line.bytes().take_while(|&byte| byte == b'#').count();
    ((1..=6).contains(&level) && line[level..].starts_with([' ', '\t'])).then_some(level)
}

fn render_search(app: &App, search: &Search, frame: &mut Frame, area: Rect) {
    let theme = app.theme;
    let layout = list_layout(area);
    let buf = frame.buffer_mut();
    let detail = format!(
        "{} · {} note{}",
        search.root_display,
        search.notes.len(),
        if search.notes.len() == 1 { "" } else { "s" }
    );
    draw_list_header(buf, &layout, theme, "Search", &detail);
    let count = if search.input.text.trim().is_empty() {
        String::new()
    } else if search.truncated {
        format!("{}+ results", search.hits.len())
    } else {
        format!(
            "{} result{}{}",
            search.hits.len(),
            if search.hits.len() == 1 { "" } else { "s" },
            // Some huge notes were only searched in part.
            if search.partial { " (partial)" } else { "" }
        )
    };
    let x = draw_input(buf, layout.input, theme, " / ", &search.input.text, &count);
    let cursor = (x(search.input.cursor_x()), layout.input.y);
    let list = layout.list;
    if search.hits.is_empty() && !search.input.text.trim().is_empty() {
        buf.set_stringn(
            list.x + 2,
            list.y,
            "No matches.",
            usize::from(list.width),
            theme.dim(),
        );
    }
    let per = entry_rows(list);
    let rows = usize::from(list.height / per);
    for (slot, hit) in search
        .hits
        .iter()
        .enumerate()
        .skip(search.scroll)
        .take(rows)
    {
        let Some(note) = search.notes.get(hit.note) else {
            continue;
        };
        let y = list.y + ((slot - search.scroll) * usize::from(per)) as u16;
        if slot == search.selected {
            buf.set_style(Rect::new(list.x, y, list.width, per), theme.picked());
            buf.set_stringn(list.x, y, "▌", 1, theme.accent());
        }
        let width = usize::from(list.width.saturating_sub(3));
        let head = format!("{}:{}", sanitize(&note.title), hit.line + 1);
        buf.set_stringn(
            list.x + 2,
            y,
            ellipsize(&head, width),
            width,
            Style::default().add_modifier(Modifier::BOLD),
        );
        if per < 2 {
            continue;
        }
        // Snippet with the match highlighted.
        let before = hit.snippet.get(..hit.matched.start).unwrap_or("");
        let matched = hit.snippet.get(hit.matched.clone()).unwrap_or("");
        let after = hit.snippet.get(hit.matched.end..).unwrap_or("");
        let mut x = list.x + 2;
        let limit = list.x + 2 + width as u16;
        for (text, style) in [
            (before, theme.dim()),
            (matched, theme.hit()),
            (after, theme.dim()),
        ] {
            if x >= limit {
                break;
            }
            let (end, _) = buf.set_stringn(x, y + 1, text, usize::from(limit - x), style);
            x = end;
        }
    }
    if let (Some(preview), Some(hit)) = (layout.preview, search.selected_hit()) {
        if let Some(note) = search.notes.get(hit.note) {
            draw_separator(buf, theme, preview);
            let start = hit.line.saturating_sub(3);
            let lines: Vec<Line> = note
                .text
                .lines()
                .enumerate()
                .skip(start)
                .take(usize::from(preview.height))
                .map(|(index, line)| {
                    let style = if index == hit.line {
                        theme.hit()
                    } else {
                        Style::default()
                    };
                    Line::from(TextSpan::styled(sanitize(line), style))
                })
                .collect();
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), preview);
        }
    }
    let buf = frame.buffer_mut();
    draw_status(
        buf,
        layout.status,
        theme,
        app,
        &["Esc back", "Enter open", "↑↓ select", "^O notes"],
        "",
    );
    frame.set_cursor_position(cursor);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Pos;
    use crate::store::NoteEntry;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use ratatui::Terminal;
    use std::path::{Path, PathBuf};
    use std::time::UNIX_EPOCH;

    fn app_with(text: &str, cursor: Pos, color: bool) -> App {
        let mut editor = Editor::from_text(Path::new("/tmp/notes/test.md"), text, None, None);
        editor.buffer.set_cursor(cursor, false);
        App::with_editor(PathBuf::from("/tmp/notes"), Theme::new(color), editor)
    }

    fn draw(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        app.prepare(Rect::new(0, 0, width, height));
        terminal.draw(|frame| render(app, frame)).unwrap();
        terminal
    }

    fn rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn draws_live_preview_with_the_cursor_line_raw() {
        let text = "+++\ntitle = \"Plan\"\ntags = [\"a\", \"b\"]\n+++\n# Heading\n\n- [ ] task\n- [x] done\n**bold** and `code`\n\n---\n> quote";
        let mut app = app_with(text, Pos::new(8, 0), true);
        let mut terminal = draw(&mut app, 60, 16);
        let rows = rows(&terminal);
        // At 60 columns the buttons keep short labels and the title (which
        // the heading below repeats) shrinks.
        assert!(rows[0].starts_with(" Pl"), "{rows:?}");
        assert!(
            rows[0].contains("▶ Read") && rows[0].ends_with("✕"),
            "{rows:?}"
        );
        assert_eq!(rows[1].trim(), "Plan · a, b");
        assert_eq!(rows[2].trim(), "Heading");
        assert_eq!(rows[4].trim(), "☐ task");
        assert_eq!(rows[5].trim(), "☑ done");
        // The cursor line is raw.
        assert_eq!(rows[6].trim(), "**bold** and `code`");
        assert_eq!(rows[8].trim(), "─".repeat(58));
        assert_eq!(rows[9].trim(), "▎ quote");
        assert!(rows[15].contains("9:1"));
        assert!(rows[15].contains("words"));
        // Styles: H1 bold + underlined, checked task struck through.
        let buffer = terminal.backend().buffer();
        let heading = &buffer[(1, 2)];
        assert!(heading
            .modifier
            .contains(Modifier::BOLD | Modifier::UNDERLINED));
        let done = &buffer[(5, 5)];
        assert!(done.modifier.contains(Modifier::CROSSED_OUT));
        assert_eq!(terminal.get_cursor_position().ok(), Some((1, 6).into()));
    }

    #[test]
    fn code_blocks_tables_and_wrapping() {
        let text = "x\n\n```rust\nfn main() {}\n```\n\n| a | b |\n|---|---|\n| long cell | y |\n\nalpha beta gamma delta epsilon zeta eta theta";
        let mut app = app_with(text, Pos::new(0, 0), true);
        let terminal = draw(&mut app, 34, 16);
        let rows = rows(&terminal);
        // Fence lines are hidden but keep their row; the label is on the right.
        assert!(rows[3].ends_with("rust"), "{rows:?}");
        assert!(!rows[3].contains("```"));
        assert_eq!(rows[4].trim(), "fn main() {}");
        assert_eq!(rows[7].trim(), "│ a         │ b │");
        assert_eq!(rows[8].trim(), "├───────────┼───┤");
        assert_eq!(rows[11].trim(), "alpha beta gamma delta epsilon");
        assert_eq!(rows[12].trim(), "zeta eta theta");
        let buffer = terminal.backend().buffer();
        assert_ne!(buffer[(5, 4)].bg, Color::Reset, "code rows are tinted");
    }

    #[test]
    fn no_color_uses_only_modifiers() {
        let mut app = app_with("# Title\n\n*em* [link](u)", Pos::new(1, 0), false);
        let terminal = draw(&mut app, 40, 6);
        let buffer = terminal.backend().buffer();
        for cell in buffer.content() {
            assert!(
                !matches!(cell.fg, Color::Rgb(..)) && !matches!(cell.bg, Color::Rgb(..)),
                "colour used with NO_COLOR: {cell:?}"
            );
        }
        let rows = rows(&terminal);
        assert_eq!(rows[3].trim(), "em link");
        assert!(buffer[(5, 3)].modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn find_bar_and_selection_are_drawn() {
        let mut app = app_with("one two one", Pos::new(0, 0), true);
        app.handle_event(
            crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('f'),
                crossterm::event::KeyModifiers::CONTROL,
            )),
            std::time::Instant::now(),
        );
        app.handle_paste("one", std::time::Instant::now());
        let terminal = draw(&mut app, 40, 6);
        let rows = rows(&terminal);
        assert!(rows[4].starts_with(" Find: one"), "{rows:?}");
        assert!(rows[4].ends_with("1/2"));
        let buffer = terminal.backend().buffer();
        assert_ne!(
            buffer[(1, 1)].bg,
            buffer[(5, 1)].bg,
            "current match highlighted"
        );
    }

    fn entry(title: &str, text: &str) -> NoteEntry {
        NoteEntry {
            path: PathBuf::from(format!("/n/{title}.md")),
            relative: format!("{title}.md"),
            title: title.to_owned(),
            tags: vec!["t".to_owned()],
            modified: UNIX_EPOCH,
            text: text.to_owned(),
            truncated: false,
            body_start: 0,
        }
    }

    #[test]
    fn picker_lists_notes_with_a_preview() {
        let mut app = App::new(PathBuf::from("/n"), Theme::new(true));
        app.screen = Screen::Picker(Picker::with_notes(
            PathBuf::from("/n"),
            vec![
                entry("Alpha", "\n# Alpha\nbody text"),
                entry("Beta", "beta"),
            ],
        ));
        let terminal = draw(&mut app, 100, 12);
        let rows = rows(&terminal);
        assert!(rows[0].starts_with(" Notes"));
        assert!(rows[0].contains("2 notes"));
        assert!(rows[3].contains("▌ Alpha"), "{rows:?}");
        // Modified dates are local (UNIX_EPOCH is 1969-12-31 west of UTC).
        let date = store::local_date(UNIX_EPOCH);
        assert!(
            rows[4].contains(&format!("{date} · Alpha.md #t")),
            "{rows:?}"
        );
        assert!(rows[5].contains("Beta"));
        assert!(rows[3].contains("│ Alpha"));
        assert!(rows[6].contains("│ Alpha"), "preview heading: {rows:?}");
        assert!(rows[7].contains("body text"));
    }

    #[test]
    fn search_lists_hits_with_snippets() {
        let mut app = App::new(PathBuf::from("/n"), Theme::new(true));
        let mut search = Search::with_notes(
            PathBuf::from("/n"),
            vec![entry("Alpha", "first\nthe kernel pin\n")],
        );
        search.input = crate::input::LineInput::new("kernel");
        search.run();
        app.screen = Screen::Search(search);
        let terminal = draw(&mut app, 60, 10);
        let rows = rows(&terminal);
        assert!(rows[1].contains("/ kernel"));
        assert!(rows[1].ends_with("1 result"));
        assert!(rows[3].contains("Alpha:2"));
        assert!(rows[4].contains("the kernel pin"));
    }

    #[test]
    fn rules_in_quotes_and_lists_keep_their_prefix() {
        let mut app = app_with(
            "x\n\n> quoted\n>\n> ---\n\n* ---\n\n> a\n> ===",
            Pos::new(0, 0),
            true,
        );
        let terminal = draw(&mut app, 30, 14);
        let rows = rows(&terminal);
        assert_eq!(rows[5].trim(), format!("▎ {}", "─".repeat(26)), "{rows:?}");
        assert_eq!(rows[7].trim(), format!("• {}", "─".repeat(26)), "{rows:?}");
        assert!(rows[10].trim().starts_with("▎ ─"), "{rows:?}");
    }

    #[test]
    fn every_line_of_a_checked_task_is_struck() {
        let mut app = app_with("x\n\n- [x] alpha\n  bravo", Pos::new(0, 0), true);
        let terminal = draw(&mut app, 30, 8);
        let rows = rows(&terminal);
        assert_eq!(rows[4].trim(), "bravo", "{rows:?}");
        let buffer = terminal.backend().buffer();
        let x = rows[4].find('b').unwrap() as u16;
        assert!(buffer[(x, 4)].modifier.contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn the_quit_hint_shows_at_80_columns() {
        let mut app = app_with("text", Pos::new(0, 0), true);
        let terminal = draw(&mut app, 80, 6);
        let status = rows(&terminal)[5].clone();
        assert!(status.contains("^Q quit"), "{status}");
        assert!(!status.contains('…'), "whole hints only: {status}");
        let terminal = draw(&mut app, 40, 6);
        assert!(rows(&terminal)[5].contains("^Q quit"));
        // The start screen's picker: Esc quits there.
        let mut picker = App::new(PathBuf::from("/n"), Theme::new(true));
        picker.screen = Screen::Picker(Picker::with_notes(PathBuf::from("/n"), Vec::new()));
        let terminal = draw(&mut picker, 60, 6);
        assert!(rows(&terminal)[5].contains("Esc quit"));
    }

    #[test]
    fn five_row_lists_still_show_entries() {
        let mut app = App::new(PathBuf::from("/n"), Theme::new(true));
        app.screen = Screen::Picker(Picker::with_notes(
            PathBuf::from("/n"),
            vec![entry("Alpha", "a"), entry("Beta", "b")],
        ));
        let terminal = draw(&mut app, 20, 5);
        let shown = rows(&terminal);
        assert!(shown[3].contains("Alpha"), "{shown:?}");
        let mut search =
            Search::with_notes(PathBuf::from("/n"), vec![entry("Alpha", "the kernel")]);
        search.input = crate::input::LineInput::new("kernel");
        search.run();
        let mut app = App::new(PathBuf::from("/n"), Theme::new(true));
        app.screen = Screen::Search(search);
        let terminal = draw(&mut app, 30, 5);
        assert!(rows(&terminal)[3].contains("Alpha:1"));
    }

    #[test]
    fn without_colour_code_stays_visible() {
        let mut app = app_with(
            "x\n\nSome `code` here\n\n```rust\nlet x = 1;\n```",
            Pos::new(0, 0),
            false,
        );
        let terminal = draw(&mut app, 40, 10);
        let rows = rows(&terminal);
        let buffer = terminal.backend().buffer();
        let x = rows[3].find("code").unwrap() as u16;
        assert!(
            buffer[(x, 3)].modifier.contains(Modifier::REVERSED),
            "{rows:?}"
        );
        assert!(
            rows[5].contains('┄') && rows[5].trim_end_matches('┄').ends_with("rust"),
            "{rows:?}"
        );
        assert!(buffer[(2, 6)].modifier.contains(Modifier::DIM));
    }

    #[test]
    fn small_panes_never_panic() {
        let text = "+++\ntitle = \"T\"\n+++\n# Heading 日本語\n\n- [ ] task 🎉\n| a | b |\n|---|---|\n| 1 | 2 |\n```rust\ncode\n```";
        for width in 1..34u16 {
            for height in 1..9u16 {
                let mut app = app_with(text, Pos::new(4, 0), true);
                draw(&mut app, width, height);
                app.handle_paste("x", std::time::Instant::now());
                draw(&mut app, width, height);
                let mut picker = App::new(PathBuf::from("/n"), Theme::new(true));
                picker.screen = Screen::Picker(Picker::with_notes(
                    PathBuf::from("/n"),
                    vec![entry("Alpha 日本語", "# Alpha\nbody")],
                ));
                draw(&mut picker, width, height);
                let mut empty = App::new(PathBuf::from("/n"), Theme::new(false));
                empty.screen = Screen::Picker(Picker::with_notes(PathBuf::from("/n"), Vec::new()));
                draw(&mut empty, width, height);
            }
        }
    }
}
