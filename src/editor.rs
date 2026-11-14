//! One open note: its text, parse, layout cache, file state and viewport.
//!
//! Everything that changes state lives here or in [`crate::app`];
//! [`Editor::prepare`] does the layout and scroll math for a frame so that
//! [`crate::ui`] can draw from `&Editor` without mutating anything.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui::layout::Rect;

use crate::buffer::{Buffer, Pos};
use crate::edit;
use crate::layout::{
    normalize, scroll_to_show, step_rows, visible_rows, LayoutCache, LineLayout, LineMode,
    ScreenPos,
};
use crate::markdown::{Document, Leaf, Span, SpanKind};
use crate::store::{self, DiskState, SaveError, SavePolicy, Stamp};
use crate::text;

/// Quiet time after the last change before an autosave.
pub const AUTOSAVE_DELAY: Duration = Duration::from_secs(1);
/// Rows moved per mouse-wheel notch.
const WHEEL_ROWS: isize = 3;
/// Most find matches kept (the count shows "+" past it).
pub const FIND_LIMIT: usize = 10_000;

/// Which lines are drawn raw this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modes {
    pub raw_start: usize,
    pub raw_end: usize,
    pub source: bool,
}

impl Modes {
    pub fn of(&self, line: usize) -> LineMode {
        if self.source || (self.raw_start..self.raw_end).contains(&line) {
            LineMode::Raw
        } else {
            LineMode::Rendered
        }
    }
}

/// What [`Editor::prepare`] worked out for the frame being drawn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct View {
    /// Where the note text is drawn (absolute screen cells).
    pub area: Rect,
    /// The document row shown on each screen row, top to bottom.
    pub rows: Vec<ScreenPos>,
    /// The cursor cell relative to `area`, when it is on screen.
    pub cursor: Option<(u16, u16)>,
    pub modes: Modes,
    /// The buffer revision the view was laid out for.
    pub revision: u64,
}

/// A notice for the status line produced by a file operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    Info(String),
    Warn(String),
}

#[derive(Debug)]
pub struct Editor {
    pub path: PathBuf,
    /// `path` for display (home shortened to `~`).
    pub display_path: String,
    pub buffer: Buffer,
    pub doc: Document,
    pub cache: LayoutCache,
    pub stamp: Option<Stamp>,
    /// Why the note must never be written, if so.
    pub read_only: Option<String>,
    /// Ctrl-E: everything raw.
    pub source_mode: bool,
    pub top: ScreenPos,
    /// Keep the cursor on screen (off after wheel scrolling).
    pub follow: bool,
    /// Screen column kept across Up / Down.
    pub goal_x: Option<usize>,
    /// The file changed on disk while we had unsaved edits.
    pub conflict: bool,
    /// What was on disk when the conflict was raised: a newer change warns
    /// again and disarms a pending force-save.
    conflict_stamp: Option<Stamp>,
    /// One Ctrl-S was pressed during a conflict; the next one overwrites.
    /// Any other key or a newer change on disk disarms it.
    pub force_armed: bool,
    /// The file was deleted on disk (Ctrl-S recreates it).
    pub deleted: bool,
    pub last_edit: Option<Instant>,
    last_revision: u64,
    /// The revision whose autosave failed; retried only after another change.
    autosave_failed: Option<u64>,
    words: Vec<u32>,
    pub word_total: u64,
    pub view: View,
    /// Raw / rendered modes pinned while a mouse drag is in progress, so the
    /// line under the press does not switch to raw (or the front matter
    /// collapse) and shift the text under the pointer mid-drag.
    pub held_modes: Option<Modes>,
}

impl Editor {
    /// Opens `path` (a missing file is an empty note created on first save).
    pub fn open(path: &Path) -> Result<Editor, String> {
        let loaded = store::load(path).map_err(|err| format!("{}: {err}", path.display()))?;
        let missing = loaded.stamp.is_none() && loaded.read_only.is_none();
        let mut editor = Editor::from_text(path, &loaded.text, loaded.stamp, loaded.read_only);
        if missing {
            // A note this editor creates ends with a line break, like the
            // new-note template.
            editor.buffer.set_final_newline();
        }
        editor.place_cursor_at_body();
        Ok(editor)
    }

    pub fn from_text(
        path: &Path,
        text: &str,
        stamp: Option<Stamp>,
        read_only: Option<String>,
    ) -> Editor {
        let buffer = Buffer::from_text(text);
        let doc = Document::parse(buffer.lines());
        let words: Vec<u32> = buffer
            .lines()
            .iter()
            .map(|line| text::word_count(line))
            .collect();
        let word_total = words.iter().map(|&count| u64::from(count)).sum();
        Editor {
            path: path.to_path_buf(),
            display_path: display_path(path),
            last_revision: buffer.revision(),
            buffer,
            doc,
            cache: LayoutCache::new(),
            stamp,
            read_only,
            source_mode: false,
            top: ScreenPos::default(),
            follow: true,
            goal_x: None,
            conflict: false,
            conflict_stamp: None,
            force_armed: false,
            deleted: false,
            last_edit: None,
            autosave_failed: None,
            words,
            word_total,
            view: View::default(),
            held_modes: None,
        }
    }

    /// Puts the cursor on the first line after the front matter, so the
    /// front matter starts collapsed.
    pub fn place_cursor_at_body(&mut self) {
        let line = self
            .doc
            .front_matter()
            .map_or(0, |front| front.end_line + 1);
        self.buffer.set_cursor(Pos::new(line, 0), false);
        self.follow = true;
    }

    /// Moves the cursor to the start of `line` and shows it.
    pub fn goto_line(&mut self, line: usize) {
        self.buffer.set_cursor(Pos::new(line, 0), false);
        self.goal_x = None;
        self.follow = true;
    }

    /// Note title: front matter title, first heading, else the file name.
    pub fn title(&self) -> String {
        self.doc.title(self.buffer.lines()).unwrap_or_else(|| {
            self.path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| "untitled".to_owned())
        })
    }

    pub fn is_dirty(&self) -> bool {
        self.buffer.is_dirty()
    }

    /// Brings the parse, layout cache and word count up to date with the
    /// text. Cheap when nothing changed.
    pub fn sync(&mut self) {
        let Some(damage) = self.buffer.take_damage() else {
            return;
        };
        let lines = self.buffer.lines();
        if damage.old_end <= self.words.len() && damage.new_end <= lines.len() {
            let fresh: Vec<u32> = lines[damage.start..damage.new_end]
                .iter()
                .map(|line| text::word_count(line))
                .collect();
            let removed: u64 = self.words[damage.start..damage.old_end]
                .iter()
                .map(|&count| u64::from(count))
                .sum();
            let added: u64 = fresh.iter().map(|&count| u64::from(count)).sum();
            self.words.splice(damage.start..damage.old_end, fresh);
            self.word_total = self.word_total.saturating_sub(removed) + added;
        } else {
            self.words = lines.iter().map(|line| text::word_count(line)).collect();
            self.word_total = self.words.iter().map(|&count| u64::from(count)).sum();
        }
        let changed = self.doc.update(lines, damage);
        self.cache.apply_damage(changed);
    }

    /// Records the time of a text change (for autosave).
    pub fn note_change(&mut self, now: Instant) {
        let revision = self.buffer.revision();
        if revision != self.last_revision {
            self.last_revision = revision;
            self.last_edit = Some(now);
        }
    }

    pub fn modes(&self) -> Modes {
        if let Some(held) = self.held_modes {
            return Modes {
                source: self.source_mode,
                ..held
            };
        }
        let raw = self.doc.raw_range(self.buffer.cursor().line);
        Modes {
            raw_start: raw.start,
            raw_end: raw.end,
            source: self.source_mode,
        }
    }

    /// Whether the cursor is in a code block (Enter keeps indentation only).
    pub fn cursor_in_code(&self) -> bool {
        matches!(
            self.doc
                .line(self.buffer.cursor().line)
                .map(|info| &info.leaf),
            Some(Leaf::Code { .. }) | Some(Leaf::CodeFence { .. })
        )
    }

    // -- layout -----------------------------------------------------------

    fn layout(&mut self, line: usize, mode: LineMode) -> Option<&LineLayout> {
        let info = self.doc.line(line)?;
        Some(self.cache.ensure(line, mode, self.buffer.line(line), info))
    }

    /// The cursor's screen row and column under `modes`.
    fn cursor_screen(&mut self, modes: Modes) -> (ScreenPos, usize) {
        let cursor = self.buffer.cursor();
        let text = self.buffer.line(cursor.line).to_owned();
        match self.layout(cursor.line, modes.of(cursor.line)) {
            Some(layout) => {
                let (row, x) = layout.locate(&text, cursor.col);
                (ScreenPos::new(cursor.line, row), x)
            }
            None => (ScreenPos::new(cursor.line, 0), 0),
        }
    }

    /// Lays out and scrolls for a frame drawn into `area`.
    pub fn prepare(&mut self, area: Rect) {
        self.sync();
        let width = usize::from(area.width).max(1);
        let height = usize::from(area.height);
        self.cache.set_width(width);
        let modes = self.modes();
        let (cursor_pos, cursor_x) = self.cursor_screen(modes);
        let line_count = self.buffer.line_count();
        let Editor {
            cache, buffer, doc, ..
        } = self;
        let mut rows_of = |line: usize| row_count(cache, buffer, doc, modes.of(line), line);
        let top = if self.follow {
            scroll_to_show(self.top, cursor_pos, height, line_count, &mut rows_of)
        } else {
            normalize(self.top, line_count, &mut rows_of)
        };
        let rows = visible_rows(top, height, line_count, &mut rows_of);
        // The cache may have started over while scrolling; lay out what is shown.
        for pos in &rows {
            rows_of(pos.line);
        }
        let cursor = rows
            .iter()
            .position(|pos| *pos == cursor_pos)
            .map(|y| (cursor_x.min(width - 1) as u16, y as u16));
        self.top = top;
        self.view = View {
            area,
            rows,
            cursor,
            modes,
            revision: self.buffer.revision(),
        };
    }

    /// Lays the view out again when the text, cursor block or scroll changed
    /// since the last [`Editor::prepare`] (events handled in one burst before
    /// a redraw), so a click is mapped against the text as it now stands
    /// rather than a mix of old rows and new lines.
    pub fn refresh_view(&mut self) {
        if self.view.rows.is_empty() {
            return;
        }
        let stale = self.view.revision != self.buffer.revision()
            || self.view.modes != self.modes()
            || self.view.rows.first() != Some(&self.top);
        if stale {
            self.prepare(self.view.area);
        }
    }

    /// The source position under an absolute screen cell (from the last
    /// [`Editor::prepare`]), and the source range of the rendered checkbox
    /// there, if any.
    pub fn hit_test(&mut self, column: u16, row: u16) -> Option<(Pos, Option<Range<usize>>)> {
        let area = self.view.area;
        if self.view.rows.is_empty() {
            return None;
        }
        let y = usize::from(row.saturating_sub(area.y));
        let x = usize::from(column.saturating_sub(area.x));
        if y >= self.view.rows.len() {
            // Below the text: the end of the last shown line.
            let line = self.view.rows[self.view.rows.len() - 1].line;
            return Some((Pos::new(line, self.buffer.line(line).len()), None));
        }
        let pos = self.view.rows[y];
        let mode = self.view.modes.of(pos.line);
        let text = self.buffer.line(pos.line).to_owned();
        let layout = self.layout(pos.line, mode)?;
        let checkbox = if mode == LineMode::Rendered {
            layout.checkbox_at(pos.row, x)
        } else {
            None
        };
        let byte = layout.hit(&text, pos.row, x);
        Some((self.buffer.clamp(Pos::new(pos.line, byte)), checkbox))
    }

    /// The source position for dragging a selection to an absolute screen
    /// cell: as [`Editor::hit_test`], except that a selection growing
    /// rightwards ends after the last character left of the pointer, so
    /// hidden markup right after it (a link's `](url)`) is not included.
    pub fn drag_target(&mut self, column: u16, row: u16) -> Option<Pos> {
        let (pos, _) = self.hit_test(column, row)?;
        let anchor = self.buffer.anchor().unwrap_or(self.buffer.cursor());
        let y = usize::from(row.saturating_sub(self.view.area.y));
        let Some(&screen) = self.view.rows.get(y) else {
            return Some(pos);
        };
        if pos <= anchor || self.view.modes.of(screen.line) != LineMode::Rendered {
            return Some(pos);
        }
        let x = usize::from(column.saturating_sub(self.view.area.x));
        let text = self.buffer.line(screen.line).to_owned();
        let layout = self.layout(screen.line, LineMode::Rendered)?;
        let byte = layout.hit_end(&text, screen.row, x);
        let end = self.buffer.clamp(Pos::new(screen.line, byte));
        Some(if end > anchor { end } else { pos })
    }

    // -- motion -----------------------------------------------------------

    /// Up / Down by one screen row (wrap aware), keeping the screen column.
    pub fn move_vertical(&mut self, down: bool, extend: bool) {
        self.sync();
        let cursor = self.buffer.cursor();
        let modes = self.modes();
        let text = self.buffer.line(cursor.line).to_owned();
        let saved_goal = self.goal_x;
        let Some(layout) = self.layout(cursor.line, modes.of(cursor.line)) else {
            return;
        };
        let (row, x) = layout.locate(&text, cursor.col);
        let rows = layout.row_count();
        let goal = saved_goal.unwrap_or(x);
        let within = if down && row + 1 < rows {
            Some(layout.hit(&text, row + 1, goal))
        } else if !down && row > 0 {
            Some(layout.hit(&text, row - 1, goal))
        } else {
            None
        };
        let target = if let Some(byte) = within {
            Pos::new(cursor.line, byte)
        } else if down {
            if cursor.line + 1 < self.buffer.line_count() {
                // The line below becomes the (raw) cursor line.
                let line = cursor.line + 1;
                let below = self.buffer.line(line).to_owned();
                match self.layout(line, LineMode::Raw) {
                    Some(layout) => Pos::new(line, layout.hit(&below, 0, goal)),
                    None => Pos::new(line, 0),
                }
            } else {
                Pos::new(cursor.line, text.len())
            }
        } else if cursor.line > 0 {
            let line = cursor.line - 1;
            let above = self.buffer.line(line).to_owned();
            match self.layout(line, LineMode::Raw) {
                Some(layout) => {
                    let last = layout.row_count().saturating_sub(1);
                    Pos::new(line, layout.hit(&above, last, goal))
                }
                None => Pos::new(line, 0),
            }
        } else {
            Pos::new(0, 0)
        };
        let target = self.buffer.clamp(target);
        self.buffer.set_cursor(target, extend);
        self.goal_x = Some(goal);
        self.follow = true;
    }

    /// PgUp / PgDn: about a screen of rows.
    pub fn move_page(&mut self, down: bool, extend: bool) {
        self.sync();
        let height = usize::from(self.view.area.height).max(2);
        let step = (height - 1) as isize * if down { 1 } else { -1 };
        let modes = self.modes();
        let (cursor_pos, cursor_x) = self.cursor_screen(modes);
        let goal = self.goal_x.unwrap_or(cursor_x);
        let line_count = self.buffer.line_count();
        let Editor {
            cache, buffer, doc, ..
        } = self;
        let mut rows_of = |line: usize| row_count(cache, buffer, doc, modes.of(line), line);
        let target = step_rows(cursor_pos, step, line_count, &mut rows_of);
        let top = step_rows(self.top, step, line_count, &mut rows_of);
        let text = self.buffer.line(target.line).to_owned();
        let byte = match self.layout(target.line, modes.of(target.line)) {
            Some(layout) => layout.hit(&text, target.row, goal),
            None => 0,
        };
        let mut pos = self.buffer.clamp(Pos::new(target.line, byte));
        if target == cursor_pos {
            // Already at the first / last row.
            pos = if down {
                self.buffer.end_pos()
            } else {
                Pos::default()
            };
        }
        self.top = top;
        self.buffer.set_cursor(pos, extend);
        self.goal_x = Some(goal);
        self.follow = true;
    }

    /// Mouse wheel: scrolls the view without moving the cursor.
    pub fn scroll(&mut self, down: bool) {
        self.sync();
        let modes = self.view.modes;
        let line_count = self.buffer.line_count();
        let Editor {
            cache, buffer, doc, ..
        } = self;
        let mut rows_of = |line: usize| row_count(cache, buffer, doc, modes.of(line), line);
        let delta = if down { WHEEL_ROWS } else { -WHEEL_ROWS };
        // Do not scroll past the point where the last row is at the bottom.
        let mut top = step_rows(self.top, delta, line_count, &mut rows_of);
        if down {
            let height = usize::from(self.view.area.height).max(1);
            let last_line = line_count.saturating_sub(1);
            let last = ScreenPos::new(last_line, rows_of(last_line).saturating_sub(1));
            let lowest_top = step_rows(last, -(height as isize - 1), line_count, &mut rows_of);
            if top > lowest_top {
                top = lowest_top.max(self.top);
            }
        }
        self.top = top;
        self.follow = false;
    }

    /// Scrolls so `lines` (the paragraph being read aloud) are on screen,
    /// without moving the cursor. Does nothing when they already show.
    pub fn reveal_lines(&mut self, lines: Range<usize>) {
        if lines.is_empty() {
            return;
        }
        let shown = |line: usize| self.view.rows.iter().any(|pos| pos.line == line);
        let last = lines.end - 1;
        if shown(lines.start) && shown(last) {
            return;
        }
        self.sync();
        let modes = self.view.modes;
        let line_count = self.buffer.line_count();
        let Editor {
            cache, buffer, doc, ..
        } = self;
        let mut rows_of = |line: usize| row_count(cache, buffer, doc, modes.of(line), line);
        // A row of context above the paragraph.
        let start = ScreenPos::new(lines.start.min(line_count.saturating_sub(1)), 0);
        self.top = step_rows(start, -1, line_count, &mut rows_of);
        self.follow = false;
    }

    // -- editing ----------------------------------------------------------

    pub fn newline(&mut self) {
        self.sync();
        let in_code = self.cursor_in_code();
        edit::newline(&mut self.buffer, in_code);
    }

    pub fn toggle_task(&mut self, line: usize) {
        edit::toggle_task(&mut self.buffer, line);
    }

    /// Flips the checkbox whose `[ ]` / `[x]` source is `range` on `line`
    /// (a click on a rendered checkbox).
    pub fn toggle_checkbox(&mut self, line: usize, range: Range<usize>) {
        edit::toggle_checkbox(&mut self.buffer, line, range);
    }

    /// Matches of `query` in the note (smart case), in document order.
    pub fn find_matches(&self, query: &str) -> Vec<(usize, Range<usize>)> {
        let ignore_case = text::smart_case(query);
        let mut out = Vec::new();
        for (index, line) in self.buffer.lines().iter().enumerate() {
            let left = FIND_LIMIT - out.len();
            let mut found = text::find_all(line, query, ignore_case, left);
            // Also what the live preview shows: `Some bold` finds
            // `Some **bold**`. Raw matches win where the two overlap.
            if let Some(info) = self.doc.line(index) {
                for range in find_rendered(line, &info.spans, query, ignore_case, left) {
                    let overlaps = found
                        .iter()
                        .any(|raw| raw.start < range.end && range.start < raw.end);
                    if !overlaps {
                        found.push(range);
                    }
                }
                found.sort_by_key(|range| range.start);
                found.truncate(left);
            }
            out.extend(found.into_iter().map(|range| (index, range)));
            if out.len() >= FIND_LIMIT {
                break;
            }
        }
        out
    }

    // -- files ------------------------------------------------------------

    /// Writes the note, checking the disk as `policy` says: a change on disk
    /// is refused unless forced, a deleted file is written again only on an
    /// explicit save, and a file that is no longer UTF-8 never.
    pub fn save(&mut self, policy: SavePolicy) -> Result<(), String> {
        if let Some(reason) = &self.read_only {
            return Err(format!("read-only: {reason}"));
        }
        let text = self.buffer.to_text();
        match store::save(&self.path, &text, self.stamp.as_ref(), policy) {
            Ok(stamp) => {
                self.stamp = Some(stamp);
                self.buffer.mark_saved();
                self.conflict = false;
                self.conflict_stamp = None;
                self.force_armed = false;
                self.deleted = false;
                self.autosave_failed = None;
                Ok(())
            }
            Err(SaveError::Conflict(stamp)) => {
                self.conflict = true;
                if stamp.is_some() {
                    self.conflict_stamp = stamp;
                }
                Err("changed on disk: Ctrl-S twice to overwrite".to_owned())
            }
            Err(SaveError::Deleted) => {
                self.deleted = true;
                Err("deleted on disk: Ctrl-S writes it again".to_owned())
            }
            Err(SaveError::NotUtf8) => {
                self.read_only = Some(NOT_UTF8_ON_DISK.to_owned());
                Err(format!("not saved: {NOT_UTF8_ON_DISK}"))
            }
            Err(SaveError::Io(err)) => {
                self.autosave_failed = Some(self.buffer.revision());
                Err(format!("save failed: {err}"))
            }
        }
    }

    /// A last-chance save when the process is told to stop (the pane closed)
    /// or the terminal failed: saves normally when that is safe, else writes
    /// the unsaved text to a recovery file next to the note. Returns where the
    /// text went, or `None` when there was nothing to keep.
    pub fn save_before_exit(&mut self) -> Option<PathBuf> {
        if !self.is_dirty() {
            return None;
        }
        if self.read_only.is_none()
            && !self.conflict
            && !self.deleted
            && self.save(SavePolicy::Safe).is_ok()
        {
            return Some(self.path.clone());
        }
        let recovery = recovery_path(&self.path);
        store::write_atomic(&recovery, self.buffer.to_text().as_bytes()).ok()
    }

    /// Whether an autosave should run now.
    pub fn autosave_due(&self, now: Instant) -> bool {
        self.is_dirty()
            && self.read_only.is_none()
            && !self.conflict
            && !self.deleted
            && self.autosave_failed != Some(self.buffer.revision())
            && self
                .last_edit
                .is_some_and(|edit| now.saturating_duration_since(edit) >= AUTOSAVE_DELAY)
    }

    /// When the next autosave is due, if one is pending.
    pub fn autosave_deadline(&self) -> Option<Instant> {
        if self.is_dirty()
            && self.read_only.is_none()
            && !self.conflict
            && !self.deleted
            && self.autosave_failed != Some(self.buffer.revision())
        {
            self.last_edit.map(|edit| edit + AUTOSAVE_DELAY)
        } else {
            None
        }
    }

    /// Checks the file on disk: reloads it when it changed and we have no
    /// unsaved edits, flags a conflict when we do.
    pub fn check_disk(&mut self) -> Option<Notice> {
        let state = match store::disk_state(&self.path, self.stamp.as_ref()) {
            Ok(state) => state,
            Err(_) => return None,
        };
        match state {
            DiskState::Missing => None,
            DiskState::Unchanged | DiskState::Touched(_) => {
                if let DiskState::Touched(stamp) = state {
                    // Same bytes, new metadata: remember it so the next
                    // poll is metadata-only again.
                    self.stamp = Some(stamp);
                }
                if self.deleted {
                    // It came back with the content we know.
                    self.deleted = false;
                    return Some(Notice::Info("back on disk".to_owned()));
                }
                None
            }
            DiskState::Deleted => {
                if self.deleted {
                    return None;
                }
                self.deleted = true;
                Some(Notice::Warn(
                    "deleted on disk: Ctrl-S writes it again".to_owned(),
                ))
            }
            DiskState::Changed { text, stamp } => {
                self.deleted = false;
                match text {
                    Some(text) if !self.is_dirty() => {
                        self.reload(&text, stamp);
                        Some(Notice::Info("reloaded: changed on disk".to_owned()))
                    }
                    Some(_) => {
                        if self.conflict && self.conflict_stamp == Some(stamp) {
                            return None;
                        }
                        let again = self.conflict;
                        self.conflict = true;
                        self.conflict_stamp = Some(stamp);
                        // A pending Ctrl-S confirmed overwriting the older
                        // change, not this one.
                        self.force_armed = false;
                        Some(Notice::Warn(if again {
                            "changed on disk again: Ctrl-S twice to overwrite".to_owned()
                        } else {
                            "changed on disk: Ctrl-S twice to overwrite".to_owned()
                        }))
                    }
                    None => {
                        // Not UTF-8 any more: never write it. A clean note
                        // shows the new content (lossily), read-only.
                        if self.read_only.as_deref() == Some(NOT_UTF8_ON_DISK)
                            && self.conflict_stamp == Some(stamp)
                        {
                            return None;
                        }
                        self.read_only = Some(NOT_UTF8_ON_DISK.to_owned());
                        self.force_armed = false;
                        if !self.is_dirty() {
                            if let Ok(loaded) = store::load(&self.path) {
                                self.reload(&loaded.text, loaded.stamp.unwrap_or(stamp));
                                self.read_only = Some(NOT_UTF8_ON_DISK.to_owned());
                                return Some(Notice::Warn(format!("reloaded: {NOT_UTF8_ON_DISK}")));
                            }
                        }
                        self.conflict = true;
                        self.conflict_stamp = Some(stamp);
                        Some(Notice::Warn(format!(
                            "{NOT_UTF8_ON_DISK}; unsaved edits kept here only"
                        )))
                    }
                }
            }
        }
    }

    fn reload(&mut self, text: &str, stamp: Stamp) {
        self.buffer.reload(text);
        // The damage is the whole text; parse from scratch.
        let _ = self.buffer.take_damage();
        self.doc = Document::parse(self.buffer.lines());
        self.cache.clear();
        self.words = self
            .buffer
            .lines()
            .iter()
            .map(|line| text::word_count(line))
            .collect();
        self.word_total = self.words.iter().map(|&count| u64::from(count)).sum();
        self.stamp = Some(stamp);
        self.conflict = false;
        self.conflict_stamp = None;
        self.force_armed = false;
        self.last_revision = self.buffer.revision();
        self.last_edit = None;
        // The new content is UTF-8 (callers set a non-UTF-8 reason again);
        // a read-only state lifts once the file is writable.
        if store::writable_now(&self.path) {
            self.read_only = None;
        }
    }
}

/// Appended to a note's file name for its recovery file.
pub const RECOVERY_SUFFIX: &str = ".lumbridge-notes-recovery";

/// The read-only reason for a note whose file turned non-UTF-8 on disk.
pub const NOT_UTF8_ON_DISK: &str = "changed on disk to non-UTF-8: read-only";

/// Matches of `query` in the rendered text of a line (its prose and code
/// spans, markers hidden), as source byte ranges of `line`.
fn find_rendered(
    line: &str,
    spans: &[Span],
    query: &str,
    ignore_case: bool,
    limit: usize,
) -> Vec<Range<usize>> {
    // Rendered text, and for each span its start in it.
    let mut shown = String::new();
    let mut pieces: Vec<(usize, &Span)> = Vec::new();
    for span in spans {
        if !matches!(span.kind, SpanKind::Text | SpanKind::Code) {
            continue;
        }
        pieces.push((shown.len(), span));
        shown.push_str(span.text(line));
    }
    if pieces.is_empty() || shown == line {
        // Nothing hidden: the raw matches are the same.
        return Vec::new();
    }
    // The source offset of rendered byte `at` (`end` picks the end of the
    // span for an offset just past it).
    let source_of = |at: usize, end: bool| -> Option<usize> {
        let index = pieces
            .iter()
            .rposition(|(start, _)| if end { *start < at } else { *start <= at })?;
        let (start, span) = pieces[index];
        let offset = at - start;
        if span.is_verbatim() {
            Some((span.source.start + offset).min(span.source.end))
        } else if end && offset >= span.text(line).len() {
            Some(span.source.end)
        } else {
            Some(span.source.start)
        }
    };
    text::find_all(&shown, query, ignore_case, limit)
        .into_iter()
        .filter_map(|range| {
            let start = source_of(range.start, false)?;
            let end = source_of(range.end, true)?;
            (start < end && line.is_char_boundary(start) && line.is_char_boundary(end))
                .then_some(start..end)
        })
        .collect()
}

/// Where unsaved text goes when the note cannot be saved on exit:
/// `<name>.lumbridge-notes-recovery` next to it (not a `.md`, so the notebook
/// does not list it as a note).
pub fn recovery_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "note".into());
    name.push(RECOVERY_SUFFIX);
    path.with_file_name(name)
}

fn row_count(
    cache: &mut LayoutCache,
    buffer: &Buffer,
    doc: &Document,
    mode: LineMode,
    line: usize,
) -> usize {
    match doc.line(line) {
        Some(info) => cache
            .ensure(line, mode, buffer.line(line), info)
            .row_count(),
        None => 0,
    }
}

/// `path` with the home folder shown as `~`.
pub fn display_path(path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|dir| dir.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        if !home.as_os_str().is_empty() {
            if let Ok(rest) = absolute.strip_prefix(&home) {
                return format!("~/{}", rest.display());
            }
        }
    }
    absolute.display().to_string()
}
