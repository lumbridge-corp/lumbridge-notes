//! Drawing (and the shared geometry for clicking) the toolbar, the dropdown
//! menus and the AI answer panel. Geometry functions are pure so the app's
//! mouse handling hits exactly what was drawn.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::{ellipsize, sanitize, str_width};
use crate::app::{App, Button, MenuKind, Panel, PanelButton};
use crate::layout::{
    display_grapheme, grapheme_width, graphemes, layout_line, LineLayout, LineMode,
};
use crate::markdown::{Document, Leaf};
use crate::theme::Theme;

/// Room always kept for the note title left of the buttons.
const TITLE_MIN: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonLook {
    Normal,
    /// Its job is running.
    Active,
    /// The service behind it is not set up.
    Off,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolButton {
    pub rect: Rect,
    pub button: Button,
    pub label: String,
    pub look: ButtonLook,
}

const BUTTONS: [Button; 8] = [
    Button::Name,
    Button::Template,
    Button::Ai,
    Button::Read,
    Button::Dictate,
    Button::Save,
    Button::Notes,
    Button::Quit,
];

/// Labels from widest (tier 0) to icons only (tier 4).
fn label(button: Button, tier: usize) -> &'static str {
    let labels: [&str; 5] = match button {
        Button::Name => ["✎ Name", "✎ Name", "✎ Name", "✎", "✎"],
        Button::Template => ["▤ Template ▾", "▤ Tmpl", "▤ Tmpl", "▤", "▤"],
        Button::Ai => ["✦ AI ▾", "✦ AI", "✦ AI", "✦ AI", "✦"],
        Button::Read => ["▶ Read ▾", "▶ Read", "▶ Read", "▶", "▶"],
        Button::Dictate => ["● Dictate", "● Rec", "● Rec", "●", "●"],
        Button::Save => ["Save", "Save", "⤓", "⤓", "⤓"],
        Button::Notes => ["Notes", "Notes", "☰", "☰", "☰"],
        Button::Quit => ["Quit", "Quit", "✕", "✕", "✕"],
    };
    labels[tier.min(4)]
}

/// Label tiers to try, each with full title room or just enough for the
/// unsaved dot: labels beat the title, which the heading below repeats.
const ATTEMPTS: [(usize, bool); 7] = [
    (0, true),
    (1, true),
    (2, true),
    (2, false),
    (3, true),
    (3, false),
    (4, false),
];

fn elapsed(since: Instant, now: Option<Instant>) -> Duration {
    now.map_or(Duration::ZERO, |now| now.saturating_duration_since(since))
}

fn spinner(since: Instant, now: Option<Instant>) -> &'static str {
    const FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];
    FRAMES[(elapsed(since, now).as_millis() / 150) as usize % FRAMES.len()]
}

/// A button's label and look in the current state.
fn button_face(app: &App, button: Button, tier: usize) -> (String, ButtonLook) {
    let assist = &app.assist;
    let config = assist.config();
    let short = tier >= 3;
    match button {
        Button::Name if assist.naming.is_some() => {
            let spin = assist.naming.as_ref().map_or("…", |_| {
                spinner(app.now.unwrap_or_else(Instant::now), app.now)
            });
            let text = if short {
                spin.to_owned()
            } else {
                format!("{spin} Naming")
            };
            (text, ButtonLook::Active)
        }
        Button::Read if assist.speaking.is_some() => {
            let text = if short { "■" } else { "■ Stop" };
            (text.to_owned(), ButtonLook::Active)
        }
        Button::Dictate if assist.recording.is_some() => {
            let secs = assist
                .recording
                .as_ref()
                .map_or(0, |recording| elapsed(recording.started, app.now).as_secs());
            (
                format!("■ {}:{:02}", secs / 60, secs % 60),
                ButtonLook::Active,
            )
        }
        Button::Dictate if assist.live.is_some() => {
            let live = assist.live.as_ref();
            let text = match live.and_then(|live| live.stopping) {
                Some(since) => format!("{} Ending", spinner(since, app.now)),
                None => {
                    let secs = live.map_or(0, |live| elapsed(live.started, app.now).as_secs());
                    format!("■ {}:{:02}", secs / 60, secs % 60)
                }
            };
            (text, ButtonLook::Active)
        }
        Button::Dictate if assist.polishing.is_some() => {
            let spin = assist
                .polishing
                .map_or("…", |(_, since)| spinner(since, app.now));
            let text = if short {
                spin.to_owned()
            } else {
                format!("{spin} Commas")
            };
            (text, ButtonLook::Active)
        }
        Button::Dictate if assist.transcribing.is_some() => {
            let spin = assist
                .transcribing
                .map_or("…", |(_, since)| spinner(since, app.now));
            let text = if short {
                spin.to_owned()
            } else {
                format!("{spin} Typing")
            };
            (text, ButtonLook::Active)
        }
        Button::Name if !config.ai_ready() => (label(button, tier).to_owned(), ButtonLook::Off),
        Button::Read if !config.speech_ready() => (label(button, tier).to_owned(), ButtonLook::Off),
        Button::Dictate if !config.dictation_ready() => {
            (label(button, tier).to_owned(), ButtonLook::Off)
        }
        _ => (label(button, tier).to_owned(), ButtonLook::Normal),
    }
}

/// The toolbar buttons on `row`, right-aligned, in the widest labels that
/// leave the title some room. Empty without an open note.
pub fn toolbar(app: &App, row: Rect, title_width: usize) -> Vec<ToolButton> {
    if app.editor.is_none() || row.height == 0 {
        return Vec::new();
    }
    let width = usize::from(row.width);
    for (attempt, (tier, full_title)) in ATTEMPTS.into_iter().enumerate() {
        let title_room = if full_title {
            title_width.min(TITLE_MIN) + 3
        } else {
            3
        };
        let faces: Vec<(Button, String, ButtonLook)> = BUTTONS
            .iter()
            .map(|&button| {
                let (text, look) = button_face(app, button, tier);
                (button, text, look)
            })
            .collect();
        // Each button is padded by a space either side; one space between
        // buttons and two before the file buttons.
        let total: usize = faces
            .iter()
            .map(|(_, text, _)| str_width(text) + 2)
            .sum::<usize>()
            + faces.len()
            + 1;
        if total + title_room > width && attempt + 1 < ATTEMPTS.len() {
            continue;
        }
        let mut buttons: Vec<ToolButton> = Vec::new();
        let mut x = row.x + row.width;
        let floor = row.x + title_room as u16;
        for (button, text, look) in faces.into_iter().rev() {
            let w = (str_width(&text) + 2) as u16;
            let gap = match (buttons.is_empty(), button) {
                (true, _) => 0,
                // A wider gap between the assistants and the file buttons.
                (false, Button::Dictate) => 2,
                (false, _) => 1,
            };
            if x < floor + w + gap {
                break;
            }
            x -= w + gap;
            buttons.push(ToolButton {
                rect: Rect::new(x, row.y, w, 1),
                button,
                label: text,
                look,
            });
        }
        buttons.reverse();
        return buttons;
    }
    Vec::new()
}

/// The note title's width as drawn on the toolbar row.
pub fn title_width(app: &App) -> usize {
    app.editor.as_ref().map_or(0, |editor| {
        str_width(&sanitize(&editor.title())) + if editor.is_dirty() { 2 } else { 0 }
    })
}

fn toolbar_row(area: Rect) -> Rect {
    Rect::new(area.x, area.y, area.width, 1.min(area.height))
}

/// The toolbar button under a click.
pub fn toolbar_hit(app: &App, area: Rect, column: u16, row: u16) -> Option<Button> {
    if !matches!(app.screen, crate::app::Screen::Editor) {
        return None;
    }
    toolbar(app, toolbar_row(area), title_width(app))
        .into_iter()
        .find(|tool| tool.rect.contains((column, row).into()))
        .map(|tool| tool.button)
}

pub fn draw_toolbar(buf: &mut Buffer, app: &App, area: Rect, theme: Theme) -> u16 {
    let row = toolbar_row(area);
    let buttons = toolbar(app, row, title_width(app));
    for tool in &buttons {
        let hovered = app
            .hover
            .is_some_and(|(x, y)| tool.rect.contains((x, y).into()));
        let style = match tool.look {
            ButtonLook::Normal if hovered => theme.button_hover(),
            ButtonLook::Normal => theme.button(),
            ButtonLook::Active => theme.button_active(),
            ButtonLook::Off => theme.button_off(),
        };
        let text = format!(" {} ", tool.label);
        buf.set_stringn(
            tool.rect.x,
            tool.rect.y,
            &text,
            usize::from(tool.rect.width),
            style,
        );
    }
    // Where the title must stop.
    buttons
        .first()
        .map_or(row.x + row.width, |tool| tool.rect.x.saturating_sub(1))
}

/// The bar under the note during live dictation: a red dot and the words
/// being spoken (the end of them, when long).
pub fn draw_live_bar(buf: &mut Buffer, app: &App, area: Rect, theme: Theme) {
    let width = usize::from(area.width);
    buf.set_stringn(area.x, area.y, " ● ", width, theme.error());
    let room = width.saturating_sub(4);
    let partial = sanitize(&app.assist.live_partial);
    let (text, style) = if partial.is_empty() {
        let waiting = app
            .assist
            .live
            .as_ref()
            .is_some_and(|live| live.stopping.is_some());
        let text = if waiting {
            "finishing…"
        } else {
            "listening…"
        };
        (text.to_owned(), theme.dim())
    } else {
        // Keep the newest words in view.
        let total = str_width(&partial);
        let mut skip = total.saturating_sub(room.saturating_sub(1));
        let mut start = 0;
        for (offset, grapheme) in graphemes(&partial) {
            if skip == 0 {
                start = offset;
                break;
            }
            skip = skip.saturating_sub(grapheme_width(grapheme));
            start = offset + grapheme.len();
        }
        let shown = partial.get(start..).unwrap_or("");
        let prefix = if start > 0 { "…" } else { "" };
        (format!("{prefix}{shown}"), theme.front_matter())
    };
    buf.set_stringn(area.x + 3, area.y, &text, room, style);
}

// ---------------------------------------------------------------------------
// Menus
// ---------------------------------------------------------------------------

fn menu_button(kind: MenuKind) -> Button {
    match kind {
        MenuKind::Template => Button::Template,
        MenuKind::Ai => Button::Ai,
        MenuKind::Read => Button::Read,
    }
}

/// Where the open menu is drawn (border included).
pub fn menu_rect(app: &App, area: Rect) -> Option<Rect> {
    let menu = app.assist.menu.as_ref()?;
    if area.height < 4 || area.width < 12 {
        return None;
    }
    let anchor = toolbar(app, toolbar_row(area), title_width(app))
        .into_iter()
        .find(|tool| tool.button == menu_button(menu.kind))
        .map_or(area.x, |tool| tool.rect.x);
    let content = menu
        .items
        .iter()
        .map(|item| {
            let detail = str_width(&item.detail);
            str_width(&item.label) + if detail > 0 { detail + 3 } else { 0 }
        })
        .max()
        .unwrap_or(10);
    let width = (content + 4).clamp(24, usize::from(area.width).min(72)) as u16;
    let max_rows = area.height.saturating_sub(3);
    let height = (menu.items.len() as u16).min(max_rows) + 2;
    let x = anchor.min(area.x + area.width - width);
    Some(Rect::new(x, area.y + 1, width, height))
}

/// Visible menu rows.
pub fn menu_rows(app: &App, area: Rect) -> usize {
    menu_rect(app, area).map_or(1, |rect| usize::from(rect.height.saturating_sub(2)))
}

/// The menu entry on screen row `row`.
pub fn menu_index(app: &App, area: Rect, row: u16) -> Option<usize> {
    let menu = app.assist.menu.as_ref()?;
    let rect = menu_rect(app, area)?;
    if row <= rect.y || row >= rect.y + rect.height - 1 {
        return None;
    }
    let index = menu.scroll + usize::from(row - rect.y - 1);
    (index < menu.items.len()).then_some(index)
}

pub fn draw_menu(buf: &mut Buffer, app: &App, area: Rect, theme: Theme) {
    let (Some(menu), Some(rect)) = (app.assist.menu.as_ref(), menu_rect(app, area)) else {
        return;
    };
    let popup = theme.popup();
    for y in rect.y..rect.y + rect.height {
        buf.set_stringn(
            rect.x,
            y,
            " ".repeat(usize::from(rect.width)),
            usize::from(rect.width),
            popup,
        );
    }
    draw_frame(buf, rect, theme);
    let inner = usize::from(rect.width.saturating_sub(4));
    let rows = usize::from(rect.height.saturating_sub(2));
    for (offset, item) in menu.items.iter().enumerate().skip(menu.scroll).take(rows) {
        let y = rect.y + 1 + (offset - menu.scroll) as u16;
        if !item.selectable() {
            let text = ellipsize(&format!("{}  {}", item.label, item.detail), inner);
            buf.set_stringn(
                rect.x + 2,
                y,
                &text,
                inner,
                popup.patch(theme.dim()).add_modifier(Modifier::BOLD),
            );
            continue;
        }
        let picked = offset == menu.selected;
        let style = if picked {
            theme.button_default()
        } else {
            popup
        };
        buf.set_stringn(rect.x + 1, y, " ".repeat(inner + 2), inner + 2, style);
        let label = ellipsize(&sanitize(&item.label), inner);
        let label_width = str_width(&label);
        buf.set_stringn(rect.x + 2, y, &label, inner, style);
        let room = inner.saturating_sub(label_width + 3);
        if room > 3 && !item.detail.is_empty() {
            let detail = ellipsize(&sanitize(&item.detail), room);
            let x = rect.x + 2 + (inner - str_width(&detail)) as u16;
            let detail_style = if picked {
                style
            } else {
                popup.patch(theme.dim())
            };
            buf.set_stringn(x, y, &detail, room, detail_style);
        }
    }
    if menu.items.len() > rows {
        let more = format!(" {}/{} ", menu.selected + 1, menu.items.len());
        let x = rect.x + rect.width.saturating_sub(str_width(&more) as u16 + 1);
        buf.set_stringn(
            x,
            rect.y + rect.height - 1,
            &more,
            str_width(&more),
            popup.patch(theme.dim()),
        );
    }
}

fn draw_frame(buf: &mut Buffer, rect: Rect, theme: Theme) {
    let style = theme.popup().patch(theme.border());
    let w = usize::from(rect.width);
    if w < 2 || rect.height < 2 {
        return;
    }
    let bottom = rect.y + rect.height - 1;
    buf.set_stringn(rect.x, rect.y, format!("╭{}╮", "─".repeat(w - 2)), w, style);
    buf.set_stringn(rect.x, bottom, format!("╰{}╯", "─".repeat(w - 2)), w, style);
    for y in rect.y + 1..bottom {
        buf.set_stringn(rect.x, y, "│", 1, style);
        buf.set_stringn(rect.x + rect.width - 1, y, "│", 1, style);
    }
}

// ---------------------------------------------------------------------------
// The AI panel
// ---------------------------------------------------------------------------

/// Word-wraps `text` to `width` display columns.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(4);
    let mut out = Vec::new();
    for line in text.split('\n') {
        let line = sanitize(line);
        if line.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut current_width = 0;
        for word in line.split_inclusive(' ') {
            let word_width = str_width(word);
            if current_width + word_width > width && !current.is_empty() {
                out.push(current.trim_end().to_owned());
                current.clear();
                current_width = 0;
            }
            if word_width > width {
                // A word longer than the row is broken by grapheme.
                for (_, grapheme) in graphemes(word) {
                    let w = grapheme_width(grapheme);
                    if current_width + w > width {
                        out.push(std::mem::take(&mut current));
                        current_width = 0;
                    }
                    current.push_str(grapheme);
                    current_width += w;
                }
            } else {
                current.push_str(word);
                current_width += word_width;
            }
        }
        out.push(current.trim_end().to_owned());
    }
    out
}

/// Rows for the answer text inside a panel drawn in `rect`.
fn text_rows(rect: Rect) -> usize {
    usize::from(rect.height.saturating_sub(2))
}

fn text_width(rect: Rect) -> usize {
    usize::from(rect.width.saturating_sub(4))
}

/// The answer laid out with the editor's own Markdown renderer.
pub struct Answer {
    lines: Vec<String>,
    doc: Document,
    layouts: Vec<Option<LineLayout>>,
}

impl Answer {
    pub fn new(panel: &Panel, width: usize) -> Answer {
        let mut text = crate::services::unfence(&panel.text).trim_end().to_owned();
        if let Some(err) = &panel.error {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&format!("⚠ {err}"));
        }
        let lines: Vec<String> = if text.is_empty() {
            Vec::new()
        } else {
            text.split('\n')
                .map(|line| line.trim_end_matches('\r').to_owned())
                .collect()
        };
        let doc = Document::parse(&lines);
        let layouts = lines
            .iter()
            .enumerate()
            .map(|(index, line)| {
                doc.line(index)
                    .map(|info| layout_line(line, info, LineMode::Rendered, width))
            })
            .collect();
        Answer {
            lines,
            doc,
            layouts,
        }
    }

    /// Screen rows, as (line, row within the line).
    fn rows(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.layouts.iter().enumerate().flat_map(|(line, layout)| {
            let count = layout.as_ref().map_or(0, |layout| layout.rows.len());
            (0..count).map(move |row| (line, row))
        })
    }

    pub fn row_count(&self) -> usize {
        self.rows().count()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

/// Largest useful scroll offset.
pub fn panel_max_scroll(panel: &Panel, rect: Rect) -> usize {
    Answer::new(panel, text_width(rect))
        .row_count()
        .saturating_sub(text_rows(rect))
}

/// Draws the answer's rows `scroll..` into `area`.
fn draw_answer(buf: &mut Buffer, answer: &Answer, area: Rect, scroll: usize, theme: Theme) {
    let popup = theme.popup();
    let right = area.x + area.width;
    for (index, (line, row)) in answer
        .rows()
        .skip(scroll)
        .take(usize::from(area.height))
        .enumerate()
    {
        let y = area.y + index as u16;
        let (Some(Some(layout)), Some(info), Some(source)) = (
            answer.layouts.get(line),
            answer.doc.line(line),
            answer.lines.get(line),
        ) else {
            continue;
        };
        let Some(row_ref) = layout.rows.get(row) else {
            continue;
        };
        let error = source.starts_with('⚠');
        if matches!(info.leaf, Leaf::Rule | Leaf::SetextUnderline { .. }) {
            let start = area.x + row_ref.end_x() as u16;
            if start < right {
                let width = usize::from(right - start);
                buf.set_stringn(
                    start,
                    y,
                    "─".repeat(width),
                    width,
                    popup.patch(theme.rule()),
                );
            }
        }
        for piece in &row_ref.pieces {
            let Some(span) = layout.span(piece) else {
                continue;
            };
            let mut style = popup.patch(super::span_style(theme, info, span, LineMode::Rendered));
            if error {
                style = style.patch(theme.error());
            }
            let mut x = area.x + piece.x as u16;
            for (_, grapheme) in graphemes(layout.piece_text(source, piece)) {
                let width = grapheme_width(grapheme);
                if usize::from(x) + width > usize::from(right) {
                    break;
                }
                buf.set_stringn(x, y, display_grapheme(grapheme), width, style);
                x += width as u16;
            }
        }
    }
}

/// The panel buttons and where they sit on its last row.
pub fn panel_buttons(rect: Rect) -> Vec<(Rect, PanelButton)> {
    let y = rect.y + rect.height.saturating_sub(1);
    let mut x = rect.x + 2;
    let right = rect.x + rect.width.saturating_sub(1);
    let mut out = Vec::new();
    for button in PanelButton::ALL {
        let w = (str_width(button.label()) + 4) as u16;
        if x + w > right {
            break;
        }
        out.push((Rect::new(x, y, w, 1), button));
        x += w + 1;
    }
    out
}

pub fn panel_button_hit(app: &App, rect: Rect, column: u16, row: u16) -> Option<PanelButton> {
    app.assist.panel.as_ref()?;
    panel_buttons(rect)
        .into_iter()
        .find(|(hit, _)| hit.contains((column, row).into()))
        .map(|(_, button)| button)
}

pub fn draw_panel(buf: &mut Buffer, app: &App, rect: Rect, theme: Theme) {
    let Some(panel) = app.assist.panel.as_ref() else {
        return;
    };
    let popup = theme.popup();
    for y in rect.y..rect.y + rect.height {
        buf.set_stringn(
            rect.x,
            y,
            " ".repeat(usize::from(rect.width)),
            usize::from(rect.width),
            popup,
        );
    }
    let w = usize::from(rect.width);
    buf.set_stringn(
        rect.x,
        rect.y,
        "─".repeat(w),
        w,
        popup.patch(theme.border()),
    );
    let status = if panel.streaming() {
        format!("{} writing", spinner(panel.started, app.now))
    } else if panel.error.is_some() {
        "failed".to_owned()
    } else {
        "done".to_owned()
    };
    let scope = if panel.part { "selection" } else { "note" };
    let header = format!(" ✦ {} · {scope} · {status} ", sanitize(&panel.title));
    let header = ellipsize(&header, w.saturating_sub(2));
    buf.set_stringn(
        rect.x + 1,
        rect.y,
        &header,
        w,
        popup.patch(theme.accent()).add_modifier(Modifier::BOLD),
    );
    let width = text_width(rect);
    let answer = Answer::new(panel, width);
    let rows = text_rows(rect);
    let text_area = Rect::new(rect.x + 2, rect.y + 1, width as u16, rows as u16);
    draw_answer(buf, &answer, text_area, panel.scroll, theme);
    if answer.is_empty() && panel.streaming() {
        buf.set_stringn(
            rect.x + 2,
            rect.y + 1,
            "thinking…",
            width,
            popup.patch(theme.dim()),
        );
    }
    let default = panel.default_button();
    for (hit, button) in panel_buttons(rect) {
        let hovered = app.hover.is_some_and(|(x, y)| hit.contains((x, y).into()));
        let style = if button == default && !panel.streaming() {
            theme.button_default()
        } else if hovered {
            theme.button_hover()
        } else {
            theme.button()
        };
        let text = format!(" {} ", button.label());
        buf.set_stringn(hit.x, hit.y, &text, usize::from(hit.width), style);
        // Underline the key that presses it.
        if let Some(offset) = button
            .label()
            .to_lowercase()
            .find(button.key())
            .filter(|_| button != PanelButton::Retry && button != PanelButton::Close)
        {
            let x = hit.x + 1 + offset as u16;
            if let Some(cell) = buf.cell_mut((x, hit.y)) {
                cell.set_style(Style::default().add_modifier(Modifier::UNDERLINED));
            }
        }
    }
    if answer.row_count() > rows {
        let more = format!(
            " {}/{} ",
            (panel.scroll + rows).min(answer.row_count()),
            answer.row_count()
        );
        let x = rect.x + rect.width.saturating_sub(str_width(&more) as u16 + 1);
        buf.set_stringn(x, rect.y, &more, str_width(&more), popup.patch(theme.dim()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_breaks_on_words_and_long_words() {
        assert_eq!(wrap("one two three", 8), ["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("a\n\nb", 10), ["a", "", "b"]);
    }
}
