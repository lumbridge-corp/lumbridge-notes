//! Application state and input handling (no terminal I/O).
//!
//! [`App::handle_event`] and [`App::tick`] change state; [`App::prepare`]
//! does the per-frame layout math; [`crate::ui::render`] then draws from
//! `&App`.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

use crate::buffer::Pos;
use crate::config::Config;
use crate::edit;
use crate::editor::{Editor, Notice};
use crate::input::LineInput;
use crate::picker::Picker;
use crate::search::Search;
use crate::store::{self, SavePolicy};
use crate::text;
use crate::theme::Theme;
use crate::ui;

/// How often the open file is checked for changes on disk.
pub const DISK_POLL: Duration = Duration::from_secs(1);
/// Copies larger than this skip OSC 52 (terminals cap the sequence).
const OSC52_LIMIT: usize = 100_000;
const INFO_TTL: Duration = Duration::from_secs(3);

mod assist;
pub use assist::{
    Assist, Button, Menu, MenuAction, MenuItem, MenuKind, Panel, PanelButton, ReadScope,
};
const WARN_TTL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub text: String,
    pub level: Level,
    pub at: Instant,
}

#[derive(Debug)]
pub enum Screen {
    Editor,
    Picker(Picker),
    Search(Search),
}

/// Find-in-note state (Ctrl-F).
#[derive(Debug, Default)]
pub struct Find {
    pub input: LineInput,
    /// Matches in document order.
    pub matches: Vec<(usize, Range<usize>)>,
    pub current: Option<usize>,
    /// Where the cursor was when find opened (the search starts there).
    origin: Pos,
}

impl Find {
    /// Matches on `line` (for drawing highlights).
    pub fn matches_on(&self, line: usize) -> &[(usize, Range<usize>)] {
        let start = self.matches.partition_point(|(l, _)| *l < line);
        let end = self.matches.partition_point(|(l, _)| *l <= line);
        &self.matches[start..end]
    }
}

/// The one-line bar above the status line.
#[derive(Debug)]
pub enum Bar {
    None,
    Find(Find),
    /// Ctrl-N: the new note's title.
    NewNote(LineInput),
    /// "Ask AI…": a question or instruction about the note.
    Ask(LineInput),
    /// The name of a new template (`prompt` false) or AI prompt.
    NewItem {
        prompt: bool,
        input: LineInput,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Motion {
    Left,
    Right,
    WordLeft,
    WordRight,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    DocStart,
    DocEnd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Move(Motion, bool),
    Insert(String),
    /// Bracketed paste: one undo step of its own.
    PasteText(String),
    Backspace,
    Delete,
    DeleteWordBack,
    DeleteWordForward,
    Newline,
    Indent,
    Outdent,
    Undo,
    Redo,
    Copy,
    Cut,
    Paste,
    Save,
    Quit,
    ToggleTask,
    SourceMode,
    Find,
    Picker,
    NewNote,
    Search,
    SelectAll,
    Escape,
    AiMenu,
    TemplateMenu,
    ReadAloud,
    Dictate,
}

impl Command {
    fn mutates(&self) -> bool {
        matches!(
            self,
            Command::Insert(_)
                | Command::PasteText(_)
                | Command::Backspace
                | Command::Delete
                | Command::DeleteWordBack
                | Command::DeleteWordForward
                | Command::Newline
                | Command::Indent
                | Command::Outdent
                | Command::Undo
                | Command::Redo
                | Command::Cut
                | Command::Paste
                | Command::ToggleTask
        )
    }
}

#[derive(Debug)]
pub struct App {
    pub editor: Option<Editor>,
    pub screen: Screen,
    pub bar: Bar,
    /// The notebook folder for the picker, search and new notes.
    pub root: PathBuf,
    /// `root` is the configured notebook (default folder or `--dir`), so a
    /// new note may write the notebook's agent guide into it.
    pub notebook: bool,
    pub theme: Theme,
    pub clipboard: String,
    /// OSC 52 sequences for the terminal; the event loop writes and clears it.
    pub osc52: Vec<String>,
    pub message: Option<Message>,
    pub quit: bool,
    /// A save failed on quit: the next Ctrl-Q quits without saving, as long
    /// as the text is still this revision (an edit since asks again).
    quit_armed: Option<u64>,
    mouse_down: bool,
    last_disk_check: Option<Instant>,
    /// The frame size from the last [`App::prepare`].
    last_area: Option<Rect>,
    /// AI, templates, reading aloud and dictation.
    pub assist: Assist,
    /// When the last [`App::tick`] ran (for elapsed times in the toolbar).
    pub now: Option<Instant>,
    /// The cell under the mouse (buttons light up there).
    pub hover: Option<(u16, u16)>,
}

impl App {
    pub fn new(root: PathBuf, theme: Theme) -> App {
        App {
            editor: None,
            screen: Screen::Editor,
            bar: Bar::None,
            root,
            notebook: false,
            theme,
            clipboard: String::new(),
            osc52: Vec::new(),
            message: None,
            quit: false,
            quit_armed: None,
            mouse_down: false,
            last_disk_check: None,
            last_area: None,
            assist: Assist::new(Config::default()),
            now: None,
            hover: None,
        }
    }

    /// Whether the one-line bar above the status line shows (a prompt, or
    /// the words of a live dictation).
    pub fn bar_shown(&self) -> bool {
        !matches!(self.bar, Bar::None) || self.assist.live.is_some()
    }

    /// Uses `config` for the AI, speech and dictation services.
    pub fn set_config(&mut self, config: Config, problem: Option<String>) {
        self.assist = Assist::new(config);
        if let Some(problem) = problem {
            self.warn(format!("config: {problem}"), Instant::now());
        }
    }

    /// Starts with `editor` open.
    pub fn with_editor(root: PathBuf, theme: Theme, editor: Editor) -> App {
        let mut app = App::new(root, theme);
        if let Some(reason) = &editor.read_only {
            app.message = Some(Message {
                text: format!("read-only: {reason}"),
                level: Level::Warn,
                at: Instant::now(),
            });
        }
        app.editor = Some(editor);
        app
    }

    /// Starts on the note picker.
    pub fn with_picker(root: PathBuf, theme: Theme) -> App {
        let mut app = App::new(root.clone(), theme);
        app.screen = Screen::Picker(Picker::open(root));
        app
    }

    pub fn info(&mut self, text: impl Into<String>, now: Instant) {
        self.say(text, Level::Info, now);
    }

    pub fn warn(&mut self, text: impl Into<String>, now: Instant) {
        self.say(text, Level::Warn, now);
    }

    pub fn error(&mut self, text: impl Into<String>, now: Instant) {
        self.say(text, Level::Error, now);
    }

    fn say(&mut self, text: impl Into<String>, level: Level, now: Instant) {
        self.message = Some(Message {
            text: text.into(),
            level,
            at: now,
        });
    }

    fn notice(&mut self, notice: Notice, now: Instant) {
        match notice {
            Notice::Info(text) => self.info(text, now),
            Notice::Warn(text) => self.warn(text, now),
        }
    }

    // -- events -------------------------------------------------------------

    pub fn handle_event(&mut self, event: Event, now: Instant) {
        // A pending "Ctrl-S again overwrites" confirms only the very next key.
        let disarms = match &event {
            Event::Key(key) => {
                key.kind != KeyEventKind::Release
                    && !(key.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S')))
            }
            Event::Paste(_) => true,
            Event::Mouse(mouse) => matches!(mouse.kind, MouseEventKind::Down(_)),
            _ => false,
        };
        if disarms {
            if let Some(editor) = self.editor.as_mut() {
                editor.force_armed = false;
            }
        }
        // Anything but the mouse ends a drag (its release may never arrive).
        if !matches!(event, Event::Mouse(_)) && self.mouse_down {
            self.end_drag();
        }
        match event {
            Event::Key(key) => {
                if key.kind != KeyEventKind::Release {
                    self.handle_key(key, now);
                }
            }
            Event::Mouse(mouse) => self.handle_mouse(mouse, now),
            Event::Paste(text) => self.handle_paste(&text, now),
            Event::FocusLost => self.save_on_focus_lost(now),
            Event::FocusGained | Event::Resize(..) => {}
        }
        if let Some(editor) = self.editor.as_mut() {
            editor.note_change(now);
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, now: Instant) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let is_quit = ctrl && matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q'));
        if !is_quit {
            self.quit_armed = None;
        }
        if is_quit {
            self.request_quit(now);
            return;
        }
        match self.screen {
            Screen::Picker(_) => self.picker_key(key, now),
            Screen::Search(_) => self.search_key(key, now),
            Screen::Editor if self.assist.menu.is_some() => self.menu_key(key, now),
            Screen::Editor if self.assist.panel.is_some() => self.panel_key(key, now),
            Screen::Editor => match self.bar {
                Bar::Find(_) => self.find_key(key),
                Bar::NewNote(_) => self.new_note_key(key, now),
                Bar::Ask(_) | Bar::NewItem { .. } => self.assist_bar_key(key, now),
                Bar::None => {
                    if let Some(command) = editor_command(key) {
                        self.run(command, now);
                    }
                }
            },
        }
    }

    pub fn handle_paste(&mut self, text: &str, now: Instant) {
        match &mut self.screen {
            Screen::Picker(picker) => {
                if picker.input.insert(text) {
                    picker.refilter();
                }
            }
            Screen::Search(search) => {
                if search.input.insert(text) {
                    search.run();
                }
            }
            Screen::Editor => match &mut self.bar {
                Bar::Find(find) => {
                    if find.input.insert(text) {
                        self.find_update();
                    }
                }
                Bar::NewNote(input) | Bar::Ask(input) | Bar::NewItem { input, .. } => {
                    input.insert(text);
                }
                Bar::None if self.assist.menu.is_some() || self.assist.panel.is_some() => {}
                Bar::None => self.run(Command::PasteText(text.to_owned()), now),
            },
        }
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent, now: Instant) {
        self.hover = Some((mouse.column, mouse.row));
        if mouse.kind == MouseEventKind::Moved {
            // Moving with no button held: a release that never arrived (a
            // click that focused the pane, say) must not keep selecting.
            if self.mouse_down {
                self.end_drag();
            }
            self.hover_menu(mouse.column, mouse.row);
            return;
        }
        match self.screen {
            Screen::Editor => {
                if !self.assist_mouse(mouse, now) {
                    self.editor_mouse(mouse, now);
                }
            }
            Screen::Picker(_) | Screen::Search(_) => self.list_mouse(mouse, now),
        }
    }

    // -- editor commands ----------------------------------------------------

    fn run(&mut self, command: Command, now: Instant) {
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        if command.mutates() {
            if let Some(reason) = &editor.read_only {
                let text = format!("read-only: {reason}");
                self.warn(text, now);
                return;
            }
            // A note about an earlier action ("copied") no longer applies.
            if self
                .message
                .as_ref()
                .is_some_and(|message| message.level == Level::Info)
            {
                self.message = None;
            }
        }
        let keeps_goal = matches!(
            command,
            Command::Move(
                Motion::Up | Motion::Down | Motion::PageUp | Motion::PageDown,
                _
            )
        );
        if !keeps_goal {
            editor.goal_x = None;
        }
        let buffer = &mut editor.buffer;
        match command {
            Command::Move(motion, extend) => {
                match motion {
                    Motion::Left => {
                        // With a selection, Left goes to its start.
                        match buffer.selection() {
                            Some((start, _)) if !extend => buffer.set_cursor(start, false),
                            _ => buffer.move_left(extend),
                        }
                    }
                    Motion::Right => match buffer.selection() {
                        Some((_, end)) if !extend => buffer.set_cursor(end, false),
                        _ => buffer.move_right(extend),
                    },
                    Motion::WordLeft => buffer.move_word_left(extend),
                    Motion::WordRight => buffer.move_word_right(extend),
                    Motion::Home => buffer.move_home(extend),
                    Motion::End => buffer.move_end(extend),
                    Motion::DocStart => buffer.move_doc_start(extend),
                    Motion::DocEnd => buffer.move_doc_end(extend),
                    Motion::Up => editor.move_vertical(false, extend),
                    Motion::Down => editor.move_vertical(true, extend),
                    Motion::PageUp => editor.move_page(false, extend),
                    Motion::PageDown => editor.move_page(true, extend),
                }
                editor.follow = true;
            }
            Command::Insert(text) => {
                buffer.insert_text(&text);
                editor.follow = true;
            }
            Command::PasteText(text) => {
                buffer.paste_text(&text);
                editor.follow = true;
            }
            Command::Backspace => {
                buffer.backspace();
                editor.follow = true;
            }
            Command::Delete => {
                buffer.delete_forward();
                editor.follow = true;
            }
            Command::DeleteWordBack => {
                buffer.delete_word_back();
                editor.follow = true;
            }
            Command::DeleteWordForward => {
                buffer.delete_word_forward();
                editor.follow = true;
            }
            Command::Newline => {
                editor.newline();
                editor.follow = true;
            }
            Command::Indent => {
                edit::indent(buffer);
                editor.follow = true;
            }
            Command::Outdent => {
                edit::outdent(buffer);
                editor.follow = true;
            }
            Command::Undo => {
                if !buffer.undo() {
                    self.info("nothing to undo", now);
                }
                if let Some(editor) = self.editor.as_mut() {
                    editor.follow = true;
                }
            }
            Command::Redo => {
                if !buffer.redo() {
                    self.info("nothing to redo", now);
                }
                if let Some(editor) = self.editor.as_mut() {
                    editor.follow = true;
                }
            }
            Command::ToggleTask => {
                let line = buffer.cursor().line;
                editor.toggle_task(line);
                editor.follow = true;
            }
            Command::SelectAll => buffer.select_all(),
            Command::Escape => {
                if buffer.selection().is_some() {
                    buffer.clear_selection();
                } else if let Some(recording) = self.assist.recording.take() {
                    recording.discard();
                    self.info("dictation discarded", now);
                } else if self.assist.live.is_some() {
                    self.toggle_dictation(now);
                } else if self.assist.speaking.is_some() {
                    self.stop_reading(now);
                }
            }
            Command::Copy | Command::Cut => {
                let cut = command == Command::Cut;
                match buffer.selected_text() {
                    Some(text) => {
                        if cut {
                            buffer.delete_selection();
                            editor.follow = true;
                        }
                        self.copy_to_clipboard(text, now);
                    }
                    None => self.info("nothing selected", now),
                }
            }
            Command::Paste => {
                if self.clipboard.is_empty() {
                    self.info("clipboard is empty (use the terminal's paste)", now);
                } else {
                    let text = self.clipboard.clone();
                    if let Some(editor) = self.editor.as_mut() {
                        editor.buffer.paste_text(&text);
                        editor.follow = true;
                    }
                    self.info("pasted", now);
                }
            }
            Command::SourceMode => {
                editor.source_mode = !editor.source_mode;
                editor.follow = true;
                let text = if editor.source_mode {
                    "source mode"
                } else {
                    "live preview"
                };
                self.info(text, now);
            }
            Command::Save => self.save_command(now),
            Command::Quit => self.request_quit(now),
            Command::Find => {
                let origin = buffer.cursor();
                self.bar = Bar::Find(Find {
                    origin,
                    ..Find::default()
                });
            }
            Command::NewNote => self.bar = Bar::NewNote(LineInput::default()),
            Command::Picker => self.open_picker(now),
            Command::Search => self.open_search(now),
            Command::AiMenu => self.open_menu(MenuKind::Ai, now),
            Command::TemplateMenu => self.open_menu(MenuKind::Template, now),
            Command::ReadAloud => self.toggle_reading(now),
            Command::Dictate => self.toggle_dictation(now),
        }
    }

    fn copy_to_clipboard(&mut self, text: String, now: Instant) {
        if text.len() <= OSC52_LIMIT {
            self.osc52.push(text::osc52(&text));
            self.info("copied", now);
        } else {
            self.info("copied (too big for the system clipboard)", now);
        }
        self.clipboard = text;
    }

    fn save_command(&mut self, now: Instant) {
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        if let Some(reason) = &editor.read_only {
            let text = format!("read-only, not saved: {reason}");
            self.warn(text, now);
            return;
        }
        if editor.conflict && !editor.force_armed {
            editor.force_armed = true;
            self.warn("changed on disk: press Ctrl-S again to overwrite it", now);
            return;
        }
        let force = editor.conflict && editor.force_armed;
        if !force && !editor.is_dirty() && editor.stamp.is_some() && !editor.deleted {
            self.info("already saved", now);
            return;
        }
        // An explicit save writes a note deleted on disk again.
        let policy = if force {
            SavePolicy::Force
        } else {
            SavePolicy::Recreate
        };
        match editor.save(policy) {
            Ok(()) => {
                let text = if force {
                    "saved (overwrote disk)"
                } else {
                    "saved"
                };
                self.info(text, now);
            }
            Err(err) => self.error(err, now),
        }
    }

    fn request_quit(&mut self, now: Instant) {
        let revision = self
            .editor
            .as_ref()
            .map_or(0, |editor| editor.buffer.revision());
        if self.quit_armed == Some(revision) {
            self.quit = true;
            return;
        }
        if self.assist.recording.is_some() || self.assist.live.is_some() {
            // Keep what was said: transcribe first, quit on the next press.
            self.toggle_dictation(now);
            self.quit_armed = Some(revision);
            self.warn(
                "finishing your dictation; Ctrl-Q again quits without it",
                now,
            );
            return;
        }
        if let Some(editor) = self.editor.as_mut() {
            if editor.is_dirty() {
                // Never silently recreate a note deleted on disk, or
                // overwrite one changed there: ask for a second Ctrl-Q.
                if let Err(err) = editor.save(SavePolicy::Safe) {
                    self.quit_armed = Some(revision);
                    self.error(format!("{err}; Ctrl-Q again quits without saving"), now);
                    return;
                }
            }
        }
        self.quit = true;
    }

    fn save_on_focus_lost(&mut self, now: Instant) {
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        if editor.is_dirty() && editor.read_only.is_none() && !editor.conflict && !editor.deleted {
            if let Err(err) = editor.save(SavePolicy::Safe) {
                self.error(err, now);
            }
        }
    }

    /// The process was told to stop (the pane closed) or the terminal
    /// failed: keeps unsaved text, by a normal save when that is safe, else
    /// in a recovery file. Returns a line to print once the terminal is back.
    pub fn save_before_exit(&mut self) -> Option<String> {
        self.stop_assist();
        let editor = self.editor.as_mut()?;
        let path = editor.path.clone();
        match editor.save_before_exit() {
            Some(written) if written == path => None,
            Some(recovery) => Some(format!(
                "unsaved changes to {} written to {}",
                path.display(),
                recovery.display()
            )),
            None if editor.is_dirty() => Some(format!(
                "unsaved changes to {} could not be written",
                path.display()
            )),
            None => None,
        }
    }

    // -- find -----------------------------------------------------------------

    fn find_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Bar::Find(find) = &mut self.bar else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.bar = Bar::None,
            KeyCode::Enter | KeyCode::Down => self.find_step(true),
            KeyCode::Char('f') | KeyCode::Char('g') if ctrl => self.find_step(true),
            KeyCode::Up => self.find_step(false),
            _ => {
                if find.input.handle_key(key) == Some(true) {
                    self.find_update();
                }
            }
        }
    }

    /// Recomputes matches for the query and jumps to the first one at or
    /// after where find started.
    fn find_update(&mut self) {
        let (Some(editor), Bar::Find(find)) = (self.editor.as_mut(), &mut self.bar) else {
            return;
        };
        find.matches = if find.input.text.is_empty() {
            Vec::new()
        } else {
            editor.find_matches(&find.input.text)
        };
        let origin = find.origin;
        find.current = if find.matches.is_empty() {
            None
        } else {
            Some(
                find.matches
                    .iter()
                    .position(|(line, range)| Pos::new(*line, range.start) >= origin)
                    .unwrap_or(0),
            )
        };
        match find.current.and_then(|index| find.matches.get(index)) {
            Some((line, range)) => {
                editor
                    .buffer
                    .set_cursor(Pos::new(*line, range.start), false);
                editor.follow = true;
            }
            None => editor.buffer.set_cursor(origin, false),
        }
    }

    fn find_step(&mut self, forward: bool) {
        let (Some(editor), Bar::Find(find)) = (self.editor.as_mut(), &mut self.bar) else {
            return;
        };
        if find.matches.is_empty() {
            return;
        }
        let len = find.matches.len();
        let next = match find.current {
            Some(index) if forward => (index + 1) % len,
            Some(index) => (index + len - 1) % len,
            None => 0,
        };
        find.current = Some(next);
        if let Some((line, range)) = find.matches.get(next) {
            editor
                .buffer
                .set_cursor(Pos::new(*line, range.start), false);
            editor.follow = true;
        }
    }

    // -- new note prompt ------------------------------------------------------

    fn new_note_key(&mut self, key: KeyEvent, now: Instant) {
        let Bar::NewNote(input) = &mut self.bar else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.bar = Bar::None,
            KeyCode::Enter => {
                let title = input.text.clone();
                self.bar = Bar::None;
                self.create_note(&title, now);
            }
            _ => {
                input.handle_key(key);
            }
        }
    }

    /// Creates a note in the notebook and opens it with the cursor at its end.
    pub fn create_note(&mut self, title: &str, now: Instant) {
        if !self.save_before_switch(now) {
            return;
        }
        match store::create_note(&self.root, title, SystemTime::now(), self.notebook) {
            Ok(path) => {
                if self.open_note(&path, None, now) {
                    if let Some(editor) = self.editor.as_mut() {
                        // Typing starts on a fresh line below the blank one
                        // under the heading, and still ends with a newline.
                        editor.buffer.open_last_line();
                        editor.buffer.set_final_newline();
                        editor.buffer.move_doc_end(false);
                        editor.follow = true;
                    }
                    self.info("new note", now);
                }
            }
            Err(err) => self.error(format!("could not create note: {err}"), now),
        }
    }

    // -- switching notes ------------------------------------------------------

    /// Saves the open note before showing another one. False (with a message)
    /// when it has unsaved changes that could not be written.
    fn save_before_switch(&mut self, now: Instant) -> bool {
        let Some(editor) = self.editor.as_mut() else {
            return true;
        };
        if !editor.is_dirty() {
            return true;
        }
        match editor.save(SavePolicy::Safe) {
            Ok(()) => true,
            Err(err) => {
                self.error(format!("{err}; staying on this note"), now);
                false
            }
        }
    }

    /// Opens `path` in the editor, optionally at `line`. Returns whether it
    /// is now open.
    pub fn open_note(&mut self, path: &Path, line: Option<usize>, now: Instant) -> bool {
        let same = self
            .editor
            .as_ref()
            .is_some_and(|editor| same_file(&editor.path, path));
        if !same {
            if !self.save_before_switch(now) {
                self.screen = Screen::Editor;
                return false;
            }
            match Editor::open(path) {
                Ok(editor) => {
                    if let Some(reason) = &editor.read_only {
                        let text = format!("read-only: {reason}");
                        self.warn(text, now);
                    }
                    self.editor = Some(editor);
                }
                Err(err) => {
                    self.error(err, now);
                    return false;
                }
            }
        }
        if let Some(editor) = self.editor.as_mut() {
            if let Some(line) = line {
                editor.goto_line(line);
            }
        }
        self.screen = Screen::Editor;
        self.bar = Bar::None;
        true
    }

    fn open_picker(&mut self, now: Instant) {
        if let Some(editor) = self.editor.as_mut() {
            if editor.is_dirty() {
                if let Err(err) = editor.save(SavePolicy::Safe) {
                    self.error(err, now);
                }
            }
        }
        self.bar = Bar::None;
        self.screen = Screen::Picker(Picker::open(self.root.clone()));
    }

    fn open_search(&mut self, now: Instant) {
        if let Some(editor) = self.editor.as_mut() {
            if editor.is_dirty() {
                if let Err(err) = editor.save(SavePolicy::Safe) {
                    self.error(err, now);
                }
            }
        }
        self.bar = Bar::None;
        self.screen = Screen::Search(Search::open(self.root.clone()));
    }

    /// Esc from the picker or search: back to the note, else quit.
    fn leave_list(&mut self, now: Instant) {
        if self.editor.is_some() {
            self.screen = Screen::Editor;
        } else if matches!(self.screen, Screen::Search(_)) {
            self.open_picker(now);
        } else {
            self.request_quit(now);
        }
    }

    // -- picker ---------------------------------------------------------------

    fn picker_key(&mut self, key: KeyEvent, now: Instant) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Screen::Picker(picker) = &mut self.screen else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.leave_list(now),
            KeyCode::Up => picker.move_selection(-1),
            KeyCode::Down => picker.move_selection(1),
            KeyCode::Char('p') if ctrl => picker.move_selection(-1),
            KeyCode::Char('n') if ctrl => {
                let title = picker.query().trim().to_owned();
                self.create_note(&title, now);
            }
            KeyCode::PageUp => picker.move_selection(-10),
            KeyCode::PageDown => picker.move_selection(10),
            KeyCode::Char('g') if ctrl => self.open_search(now),
            KeyCode::Enter => self.picker_open_selected(now),
            _ => {
                if picker.input.handle_key(key) == Some(true) {
                    picker.refilter();
                }
            }
        }
    }

    fn picker_open_selected(&mut self, now: Instant) {
        let Screen::Picker(picker) = &self.screen else {
            return;
        };
        match picker.selected_note() {
            Some(note) => {
                let path = note.path.clone();
                self.open_note(&path, None, now);
            }
            None => {
                let title = picker.query().trim().to_owned();
                if title.is_empty() {
                    self.info("no notes yet: type a title, then Enter", now);
                } else {
                    self.create_note(&title, now);
                }
            }
        }
    }

    // -- search ---------------------------------------------------------------

    fn search_key(&mut self, key: KeyEvent, now: Instant) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Screen::Search(search) = &mut self.screen else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.leave_list(now),
            KeyCode::Up => search.move_selection(-1),
            KeyCode::Down => search.move_selection(1),
            KeyCode::PageUp => search.move_selection(-10),
            KeyCode::PageDown => search.move_selection(10),
            KeyCode::Char('o') if ctrl => self.open_picker(now),
            KeyCode::Enter => self.search_open_selected(now),
            _ => {
                if search.input.handle_key(key) == Some(true) {
                    search.run();
                }
            }
        }
    }

    fn search_open_selected(&mut self, now: Instant) {
        let Screen::Search(search) = &self.screen else {
            return;
        };
        let Some(hit) = search.selected_hit() else {
            return;
        };
        let Some(note) = search.notes.get(hit.note) else {
            return;
        };
        let path = note.path.clone();
        let line = hit.line;
        self.open_note(&path, Some(line), now);
    }

    // -- mouse ------------------------------------------------------------------

    fn editor_mouse(&mut self, mouse: MouseEvent, now: Instant) {
        let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);
        if matches!(mouse.kind, MouseEventKind::Down(_)) {
            // A press without a release in between starts over.
            self.end_drag();
        }
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        if !self.mouse_down {
            editor.refresh_view();
        }
        let area = editor.view.area;
        match mouse.kind {
            MouseEventKind::ScrollUp => editor.scroll(false),
            MouseEventKind::ScrollDown => editor.scroll(true),
            MouseEventKind::Down(MouseButton::Left) => {
                if mouse.row < area.y || mouse.row >= area.y + area.height {
                    return;
                }
                editor.goal_x = None;
                let Some((pos, checkbox)) = editor.hit_test(mouse.column, mouse.row) else {
                    return;
                };
                if let (Some(range), false) = (checkbox, shift) {
                    if let Some(reason) = &editor.read_only {
                        let text = format!("read-only: {reason}");
                        self.warn(text, now);
                    } else {
                        // Exactly the box that was clicked, even when items
                        // nest on one line (`- - [ ] x`).
                        editor.toggle_checkbox(pos.line, range);
                    }
                    return;
                }
                // Keep the layout the press was mapped against until the
                // button is released, so the drag selects what was dragged
                // across on screen.
                editor.held_modes = Some(editor.view.modes);
                editor.buffer.set_cursor(pos, shift);
                editor.follow = true;
                self.mouse_down = true;
                if matches!(self.bar, Bar::Find(_)) {
                    self.bar = Bar::None;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.mouse_down => {
                editor.goal_x = None;
                if mouse.row < area.y {
                    editor.move_vertical(false, true);
                } else if mouse.row >= area.y + area.height {
                    editor.move_vertical(true, true);
                } else if let Some(pos) = editor.drag_target(mouse.column, mouse.row) {
                    editor.buffer.set_cursor(pos, true);
                }
                editor.goal_x = None;
                editor.follow = true;
            }
            MouseEventKind::Up(MouseButton::Left) => self.end_drag(),
            _ => {}
        }
    }

    fn end_drag(&mut self) {
        self.mouse_down = false;
        if let Some(editor) = self.editor.as_mut() {
            editor.held_modes = None;
        }
    }

    fn list_mouse(&mut self, mouse: MouseEvent, now: Instant) {
        let Some(area) = self.list_area() else {
            return;
        };
        match &mut self.screen {
            Screen::Picker(picker) => match mouse.kind {
                MouseEventKind::ScrollUp => picker.move_selection(-3),
                MouseEventKind::ScrollDown => picker.move_selection(3),
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(index) =
                        list_index(area, mouse, picker.scroll, ui::entry_rows(area))
                    {
                        if index < picker.filtered.len() {
                            picker.selected = index;
                            self.picker_open_selected(now);
                        }
                    }
                }
                _ => {}
            },
            Screen::Search(search) => match mouse.kind {
                MouseEventKind::ScrollUp => search.move_selection(-3),
                MouseEventKind::ScrollDown => search.move_selection(3),
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(index) =
                        list_index(area, mouse, search.scroll, ui::entry_rows(area))
                    {
                        if index < search.hits.len() {
                            search.selected = index;
                            self.search_open_selected(now);
                        }
                    }
                }
                _ => {}
            },
            Screen::Editor => {}
        }
    }

    fn list_area(&self) -> Option<Rect> {
        self.last_area.map(|area| ui::list_layout(area).list)
    }

    // -- time -------------------------------------------------------------------

    /// Autosave, external-change polling and message expiry.
    pub fn tick(&mut self, now: Instant) {
        self.now = Some(now);
        self.assist_tick(now);
        if let Some(message) = &self.message {
            let ttl = match message.level {
                Level::Info => INFO_TTL,
                Level::Warn | Level::Error => WARN_TTL,
            };
            if now.saturating_duration_since(message.at) >= ttl {
                self.message = None;
            }
        }
        let due = self
            .last_disk_check
            .is_none_or(|last| now.saturating_duration_since(last) >= DISK_POLL);
        if due {
            self.last_disk_check = Some(now);
            if let Some(notice) = self.editor.as_mut().and_then(Editor::check_disk) {
                self.notice(notice, now);
            }
        }
        if let Some(editor) = self.editor.as_mut() {
            if editor.autosave_due(now) {
                if let Err(err) = editor.save(SavePolicy::Safe) {
                    self.error(err, now);
                }
            }
        }
    }

    /// How long the event loop may wait before [`App::tick`] has work.
    pub fn next_wakeup(&self, now: Instant) -> Duration {
        let mut deadline = self.last_disk_check.map_or(now, |last| last + DISK_POLL);
        if let Some(autosave) = self.editor.as_ref().and_then(Editor::autosave_deadline) {
            deadline = deadline.min(autosave);
        }
        if let Some(message) = &self.message {
            let ttl = match message.level {
                Level::Info => INFO_TTL,
                Level::Warn | Level::Error => WARN_TTL,
            };
            deadline = deadline.min(message.at + ttl);
        }
        if self.assist.busy() {
            deadline = deadline.min(now + assist::BUSY_POLL);
        }
        deadline
            .saturating_duration_since(now)
            .clamp(Duration::from_millis(10), DISK_POLL)
    }

    // -- frame ------------------------------------------------------------------

    /// Layout and scroll math for a frame of size `area` (before drawing).
    pub fn prepare(&mut self, area: Rect) {
        self.last_area = Some(area);
        match &mut self.screen {
            Screen::Editor => {
                let bar = self.bar_shown();
                let panel = self.assist.panel.is_some();
                let layout = ui::editor_layout(area, bar, panel);
                if let Some(editor) = self.editor.as_mut() {
                    editor.prepare(layout.text);
                }
                if let (Some(rect), Some(panel)) = (layout.panel, self.assist.panel.as_mut()) {
                    let max = ui::panel_max_scroll(panel, rect);
                    if panel.follow || panel.scroll >= max {
                        panel.scroll = max;
                        panel.follow = true;
                    }
                }
                let rows = ui::menu_rows(self, area);
                if let Some(menu) = self.assist.menu.as_mut() {
                    menu.prepare(rows);
                }
            }
            Screen::Picker(picker) => {
                let list = ui::list_layout(area).list;
                picker.prepare(usize::from(list.height / ui::entry_rows(list)));
            }
            Screen::Search(search) => {
                let list = ui::list_layout(area).list;
                search.prepare(usize::from(list.height / ui::entry_rows(list)));
            }
        }
    }
}

/// The list entry under a click; entries are `rows_per` rows tall.
fn list_index(area: Rect, mouse: MouseEvent, scroll: usize, rows_per: u16) -> Option<usize> {
    if mouse.row < area.y
        || mouse.row >= area.y + area.height
        || mouse.column < area.x
        || mouse.column >= area.x + area.width
    {
        return None;
    }
    Some(scroll + usize::from((mouse.row - area.y) / rows_per))
}

fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Maps a key in the editor to a command.
fn editor_command(key: KeyEvent) -> Option<Command> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let motion = |motion: Motion| Some(Command::Move(motion, shift));
    match key.code {
        KeyCode::Left if ctrl || alt => motion(Motion::WordLeft),
        KeyCode::Right if ctrl || alt => motion(Motion::WordRight),
        KeyCode::Left => motion(Motion::Left),
        KeyCode::Right => motion(Motion::Right),
        KeyCode::Up => motion(Motion::Up),
        KeyCode::Down => motion(Motion::Down),
        KeyCode::Home if ctrl => motion(Motion::DocStart),
        KeyCode::End if ctrl => motion(Motion::DocEnd),
        KeyCode::Home => motion(Motion::Home),
        KeyCode::End => motion(Motion::End),
        KeyCode::PageUp => motion(Motion::PageUp),
        KeyCode::PageDown => motion(Motion::PageDown),
        KeyCode::Backspace if ctrl || alt => Some(Command::DeleteWordBack),
        KeyCode::Backspace => Some(Command::Backspace),
        KeyCode::Delete if ctrl || alt => Some(Command::DeleteWordForward),
        KeyCode::Delete => Some(Command::Delete),
        KeyCode::Enter => Some(Command::Newline),
        KeyCode::Tab => Some(Command::Indent),
        KeyCode::BackTab => Some(Command::Outdent),
        KeyCode::Esc => Some(Command::Escape),
        // AltGr arrives as Ctrl+Alt on some terminals: treat it as text.
        KeyCode::Char(ch) if ctrl && alt => Some(Command::Insert(ch.to_string())),
        KeyCode::Char(ch) if ctrl => match ch.to_ascii_lowercase() {
            'h' | 'w' => Some(Command::DeleteWordBack),
            'z' if shift => Some(Command::Redo),
            'z' => Some(Command::Undo),
            'y' => Some(Command::Redo),
            'c' => Some(Command::Copy),
            'x' => Some(Command::Cut),
            'v' => Some(Command::Paste),
            's' => Some(Command::Save),
            'q' => Some(Command::Quit),
            't' => Some(Command::ToggleTask),
            'e' => Some(Command::SourceMode),
            'f' => Some(Command::Find),
            'o' => Some(Command::Picker),
            'n' => Some(Command::NewNote),
            'g' => Some(Command::Search),
            'a' => Some(Command::SelectAll),
            'k' => Some(Command::AiMenu),
            'p' => Some(Command::TemplateMenu),
            'r' => Some(Command::ReadAloud),
            'd' => Some(Command::Dictate),
            _ => None,
        },
        KeyCode::Char(_) if alt => None,
        KeyCode::Char(ch) => Some(Command::Insert(ch.to_string())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use crossterm::event::KeyEventState;
    use std::fs;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    fn ctrl(ch: char) -> Event {
        key(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    fn plain(code: KeyCode) -> Event {
        key(code, KeyModifiers::NONE)
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    struct Harness {
        app: App,
        now: Instant,
        _dir: TempDir,
        path: PathBuf,
    }

    impl Harness {
        /// An app editing `note.md` (written with `text`) in a temp notebook.
        fn new(text: &str) -> Harness {
            let dir = TempDir::new("app");
            let path = dir.path().join("note.md");
            fs::write(&path, text).unwrap();
            let editor = Editor::open(&path).unwrap();
            let app = App::with_editor(dir.path().to_path_buf(), Theme::new(true), editor);
            let mut harness = Harness {
                app,
                now: Instant::now(),
                _dir: dir,
                path,
            };
            harness.frame(80, 24);
            harness
        }

        fn root(&self) -> PathBuf {
            self._dir.path().to_path_buf()
        }

        fn send(&mut self, event: Event) {
            self.app.handle_event(event, self.now);
            self.frame(80, 24);
        }

        fn frame(&mut self, width: u16, height: u16) {
            self.app.prepare(Rect::new(0, 0, width, height));
        }

        fn typed(&mut self, text: &str) {
            for ch in text.chars() {
                self.send(plain(KeyCode::Char(ch)));
            }
        }

        fn editor(&self) -> &Editor {
            self.app.editor.as_ref().unwrap()
        }

        fn editor_mut(&mut self) -> &mut Editor {
            self.app.editor.as_mut().unwrap()
        }

        fn text(&self) -> String {
            self.editor().buffer.to_text()
        }

        fn set_cursor(&mut self, line: usize, col: usize) {
            self.editor_mut()
                .buffer
                .set_cursor(Pos::new(line, col), false);
            self.frame(80, 24);
        }

        fn advance(&mut self, millis: u64) {
            self.now += Duration::from_millis(millis);
            self.app.tick(self.now);
        }

        fn disk(&self) -> String {
            fs::read_to_string(&self.path).unwrap()
        }

        /// The screen cell where source position `pos` is drawn.
        fn cell_of(&mut self, line: usize, col: usize) -> (u16, u16) {
            let editor = self.app.editor.as_mut().unwrap();
            let view = editor.view.clone();
            let mode = view.modes.of(line);
            let text = editor.buffer.line(line).to_owned();
            let layout = editor.cache.get(line, mode).unwrap();
            let (row, x) = layout.locate(&text, col);
            let y = view
                .rows
                .iter()
                .position(|pos| pos.line == line && pos.row == row)
                .unwrap();
            (view.area.x + x as u16, view.area.y + y as u16)
        }
    }

    #[test]
    fn typing_enter_continues_lists_and_ctrl_t_toggles() {
        let mut h = Harness::new("- one");
        h.set_cursor(0, 5);
        h.send(plain(KeyCode::Enter));
        h.typed("two");
        assert_eq!(h.text(), "- one\n- two");
        h.send(ctrl('t'));
        assert_eq!(h.text(), "- one\n- [ ] two");
        h.send(ctrl('t'));
        assert_eq!(h.text(), "- one\n- [x] two");
        h.send(plain(KeyCode::Enter));
        assert_eq!(h.text(), "- one\n- [x] two\n- [ ] ");
        // Enter on the empty item ends the list.
        h.send(plain(KeyCode::Enter));
        assert_eq!(h.text(), "- one\n- [x] two\n");
        h.send(plain(KeyCode::Tab));
        assert_eq!(h.text(), "- one\n- [x] two\n    ");
    }

    #[test]
    fn tab_indents_list_items_and_shift_tab_outdents() {
        let mut h = Harness::new("- a\n- b");
        h.set_cursor(1, 3);
        h.send(plain(KeyCode::Tab));
        assert_eq!(h.text(), "- a\n  - b");
        h.send(plain(KeyCode::BackTab));
        assert_eq!(h.text(), "- a\n- b");
    }

    #[test]
    fn undo_and_redo_by_typing_runs() {
        let mut h = Harness::new("");
        h.typed("hello world");
        h.send(ctrl('z'));
        assert_eq!(h.text(), "hello ");
        h.send(ctrl('z'));
        assert_eq!(h.text(), "");
        h.send(ctrl('y'));
        assert_eq!(h.text(), "hello ");
        h.send(key(
            KeyCode::Char('Z'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert_eq!(h.text(), "hello world");
        h.send(ctrl('w'));
        assert_eq!(h.text(), "hello ");
        h.send(key(KeyCode::Backspace, KeyModifiers::CONTROL));
        assert_eq!(h.text(), "");
    }

    #[test]
    fn selection_copy_cut_paste_and_osc52() {
        let mut h = Harness::new("alpha beta");
        h.set_cursor(0, 0);
        h.send(key(
            KeyCode::Right,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert_eq!(h.editor().buffer.selected_text().as_deref(), Some("alpha"));
        h.send(ctrl('c'));
        assert_eq!(h.app.clipboard, "alpha");
        assert_eq!(h.app.osc52, vec![text::osc52("alpha")]);
        h.send(ctrl('x'));
        assert_eq!(h.text(), " beta");
        h.send(key(KeyCode::End, KeyModifiers::NONE));
        h.send(ctrl('v'));
        assert_eq!(h.text(), " betaalpha");
        h.send(ctrl('a'));
        assert_eq!(
            h.editor().buffer.selected_text().as_deref(),
            Some(" betaalpha")
        );
        h.send(plain(KeyCode::Esc));
        assert!(h.editor().buffer.selection().is_none());
        // Bracketed paste inserts text (newlines too).
        h.send(Event::Paste("x\r\ny".to_owned()));
        assert_eq!(h.text(), " betaalphax\ny");
    }

    #[test]
    fn autosave_waits_for_a_quiet_second() {
        let mut h = Harness::new("start\n");
        h.set_cursor(0, 5);
        h.typed("!");
        h.advance(500);
        assert_eq!(h.disk(), "start\n");
        assert!(h.editor().is_dirty());
        h.advance(600);
        assert_eq!(h.disk(), "start!\n");
        assert!(!h.editor().is_dirty());
    }

    #[test]
    fn ctrl_s_saves_and_keeps_crlf() {
        let mut h = Harness::new("a\r\nb\r\n");
        h.set_cursor(1, 1);
        h.send(plain(KeyCode::Enter));
        h.typed("c");
        h.send(ctrl('s'));
        assert_eq!(h.disk(), "a\r\nb\r\nc\r\n");
        assert_eq!(
            h.app.message.as_ref().map(|m| m.text.as_str()),
            Some("saved")
        );
    }

    #[test]
    fn quit_saves_first_and_needs_a_second_press_when_saving_fails() {
        let mut h = Harness::new("x");
        h.typed("y");
        h.send(ctrl('q'));
        assert!(h.app.quit);
        assert_eq!(h.disk(), "yx");

        // A note whose folder has gone away cannot be saved.
        let dir = TempDir::new("gone");
        let path = dir.path().join("sub").join("n.md");
        let editor = Editor::from_text(&path, "", None, None);
        let mut app = App::with_editor(dir.path().to_path_buf(), Theme::new(false), editor);
        let now = Instant::now();
        app.handle_event(plain(KeyCode::Char('z')), now);
        app.handle_event(ctrl('q'), now);
        assert!(!app.quit);
        assert_eq!(app.message.as_ref().map(|m| m.level), Some(Level::Error));
        app.handle_event(ctrl('q'), now);
        assert!(app.quit);
    }

    #[test]
    fn external_changes_reload_when_clean_and_conflict_when_dirty() {
        let mut h = Harness::new("one\n");
        fs::write(&h.path, "one\ntwo\n").unwrap();
        h.advance(1100);
        assert_eq!(h.text(), "one\ntwo\n");
        assert!(!h.editor().is_dirty());

        h.set_cursor(0, 0);
        h.typed("A");
        fs::write(&h.path, "outside edit\n").unwrap();
        h.advance(1100);
        assert!(h.editor().conflict);
        // Autosave does not overwrite while in conflict.
        h.advance(2000);
        assert_eq!(h.disk(), "outside edit\n");
        h.send(ctrl('s'));
        assert_eq!(h.disk(), "outside edit\n");
        assert!(h.editor().force_armed);
        h.send(ctrl('s'));
        assert_eq!(h.disk(), "Aone\ntwo\n");
        assert!(!h.editor().conflict);
    }

    #[test]
    fn read_only_notes_refuse_edits() {
        let dir = TempDir::new("ro");
        let path = dir.path().join("latin.md");
        fs::write(&path, b"caf\xe9\n").unwrap();
        let editor = Editor::open(&path).unwrap();
        let mut app = App::with_editor(dir.path().to_path_buf(), Theme::new(true), editor);
        let now = Instant::now();
        app.handle_event(plain(KeyCode::Char('x')), now);
        app.handle_event(ctrl('s'), now);
        app.handle_event(ctrl('q'), now);
        assert!(app.quit);
        assert_eq!(fs::read(&path).unwrap(), b"caf\xe9\n");
    }

    #[test]
    fn find_jumps_to_matches_and_cycles() {
        let mut h = Harness::new("foo\nbar Foo\nfoo end");
        h.set_cursor(1, 0);
        h.send(ctrl('f'));
        h.typed("foo");
        let Bar::Find(find) = &h.app.bar else {
            panic!("find bar not open");
        };
        assert_eq!(find.matches.len(), 3);
        assert_eq!(find.current, Some(1));
        assert_eq!(h.editor().buffer.cursor(), Pos::new(1, 4));
        h.send(plain(KeyCode::Enter));
        assert_eq!(h.editor().buffer.cursor(), Pos::new(2, 0));
        h.send(plain(KeyCode::Down));
        assert_eq!(h.editor().buffer.cursor(), Pos::new(0, 0));
        h.send(plain(KeyCode::Up));
        assert_eq!(h.editor().buffer.cursor(), Pos::new(2, 0));
        h.send(plain(KeyCode::Esc));
        assert!(matches!(h.app.bar, Bar::None));
        assert_eq!(h.editor().buffer.cursor(), Pos::new(2, 0));
    }

    #[test]
    fn picker_filters_opens_and_creates() {
        let mut h = Harness::new("# First\n");
        fs::write(h.root().join("groceries.md"), "# Groceries\n- milk\n").unwrap();
        h.send(ctrl('o'));
        assert!(matches!(h.app.screen, Screen::Picker(_)));
        h.typed("groc");
        h.send(plain(KeyCode::Enter));
        assert!(matches!(h.app.screen, Screen::Editor));
        assert!(h.editor().path.ends_with("groceries.md"));

        // Ctrl-N in the picker creates a note named after the query.
        h.send(ctrl('o'));
        h.typed("Trip plan");
        h.send(ctrl('n'));
        let path = h.root().join("trip-plan.md");
        assert!(path.exists());
        assert!(h.editor().path.ends_with("trip-plan.md"));
        assert_eq!(h.editor().title(), "Trip plan");
        assert_eq!(h.editor().buffer.cursor(), h.editor().buffer.end_pos());

        // Esc from the picker goes back to the open note.
        h.send(ctrl('o'));
        h.send(plain(KeyCode::Esc));
        assert!(matches!(h.app.screen, Screen::Editor));
    }

    #[test]
    fn ctrl_n_prompts_for_a_title() {
        let mut h = Harness::new("x");
        h.send(ctrl('n'));
        assert!(matches!(h.app.bar, Bar::NewNote(_)));
        h.typed("Meeting notes");
        h.send(plain(KeyCode::Enter));
        assert!(h.editor().path.ends_with("meeting-notes.md"));
        let text = fs::read_to_string(h.root().join("meeting-notes.md")).unwrap();
        assert!(text.starts_with("+++\ntitle = \"Meeting notes\"\ntags = []\ncreated = "));
    }

    #[test]
    fn search_opens_the_note_at_the_matching_line() {
        let mut h = Harness::new("# Here\n");
        fs::write(h.root().join("other.md"), "a\nb\nthe needle line\n").unwrap();
        h.send(ctrl('g'));
        h.typed("needle");
        let Screen::Search(search) = &h.app.screen else {
            panic!("search not open");
        };
        assert_eq!(search.hits.len(), 1);
        h.send(plain(KeyCode::Enter));
        assert!(h.editor().path.ends_with("other.md"));
        assert_eq!(h.editor().buffer.cursor(), Pos::new(2, 0));
    }

    #[test]
    fn clicking_places_the_cursor_and_toggles_checkboxes() {
        let mut h = Harness::new("intro\n\n- [ ] task\n\nSome **bold** text");
        h.set_cursor(0, 0);
        // The checkbox on the rendered task line.
        let (x, y) = h.cell_of(2, 2);
        h.send(mouse(MouseEventKind::Down(MouseButton::Left), x, y));
        assert_eq!(h.text(), "intro\n\n- [x] task\n\nSome **bold** text");
        assert_eq!(h.editor().buffer.cursor(), Pos::new(0, 0));
        // A click on rendered "text" maps back past the hidden markers.
        let (x, y) = h.cell_of(4, 13);
        h.send(mouse(MouseEventKind::Down(MouseButton::Left), x + 1, y));
        h.send(mouse(MouseEventKind::Up(MouseButton::Left), x + 1, y));
        assert_eq!(h.editor().buffer.cursor(), Pos::new(4, 14));
        // Drag selects.
        let (x0, y0) = h.cell_of(4, 0);
        h.send(mouse(MouseEventKind::Down(MouseButton::Left), x0, y0));
        h.send(mouse(MouseEventKind::Drag(MouseButton::Left), x0 + 4, y0));
        assert_eq!(h.editor().buffer.selected_text().as_deref(), Some("Some"));
    }

    #[test]
    fn up_and_down_move_by_wrapped_rows() {
        let long = "word ".repeat(30);
        let mut h = Harness::new(&format!("top\n{long}\nend"));
        h.set_cursor(0, 0);
        h.frame(30, 24);
        h.send(plain(KeyCode::Down));
        h.frame(30, 24);
        assert_eq!(h.editor().buffer.cursor().line, 1);
        assert_eq!(h.editor().buffer.cursor().col, 0);
        h.app.handle_event(plain(KeyCode::Down), h.now);
        h.frame(30, 24);
        let cursor = h.editor().buffer.cursor();
        assert_eq!(cursor.line, 1);
        assert!(cursor.col > 0, "moved within the wrapped line: {cursor:?}");
        for _ in 0..10 {
            h.app.handle_event(plain(KeyCode::Down), h.now);
            h.frame(30, 24);
        }
        assert_eq!(h.editor().buffer.cursor().line, 2);
        h.app.handle_event(plain(KeyCode::Up), h.now);
        h.frame(30, 24);
        assert_eq!(h.editor().buffer.cursor().line, 1);
    }

    #[test]
    fn up_leaves_a_wrapped_line_and_down_skips_no_row() {
        // At 12 columns the text area is 12 wide (no margin below 30).
        let mut h = Harness::new("top\naaaa bbbbbbbbbb\nend");
        h.frame(12, 10);
        let end = h.editor().buffer.line(1).len();
        h.editor_mut().buffer.set_cursor(Pos::new(1, end), false);
        h.frame(12, 10);
        let mut seen = Vec::new();
        for _ in 0..3 {
            h.app.handle_event(plain(KeyCode::Up), h.now);
            h.frame(12, 10);
            seen.push(h.editor().buffer.cursor());
        }
        assert_eq!(seen[0].line, 1);
        assert!(seen[0].col < 5, "row 0 of the wrapped line: {seen:?}");
        assert_eq!(seen[1].line, 0, "{seen:?}");
        // Down from the top line visits both rows of the wrapped line.
        h.editor_mut().buffer.set_cursor(Pos::new(0, 3), false);
        h.editor_mut().goal_x = None;
        h.frame(12, 10);
        h.app.handle_event(plain(KeyCode::Down), h.now);
        h.frame(12, 10);
        assert_eq!(h.editor().buffer.cursor(), Pos::new(1, 3));
        h.app.handle_event(plain(KeyCode::Down), h.now);
        h.frame(12, 10);
        let cursor = h.editor().buffer.cursor();
        assert!(cursor.line == 1 && cursor.col >= 5, "{cursor:?}");
    }

    #[test]
    fn wheel_scrolls_without_moving_the_cursor_and_typing_follows() {
        let text: String = (0..100).map(|i| format!("line {i}\n\n")).collect();
        let mut h = Harness::new(&text);
        h.set_cursor(0, 0);
        h.send(mouse(MouseEventKind::ScrollDown, 10, 10));
        h.send(mouse(MouseEventKind::ScrollDown, 10, 10));
        assert_eq!(h.editor().buffer.cursor(), Pos::new(0, 0));
        assert!(h.editor().top.line > 0);
        assert!(h.editor().view.cursor.is_none());
        h.typed("x");
        assert_eq!(h.editor().top.line, 0);
        assert!(h.editor().view.cursor.is_some());
        // PgDn moves the cursor about a screen.
        h.send(plain(KeyCode::PageDown));
        assert!(h.editor().buffer.cursor().line >= 15);
        h.send(key(KeyCode::End, KeyModifiers::CONTROL));
        assert_eq!(h.editor().buffer.cursor(), h.editor().buffer.end_pos());
        assert!(h.editor().view.cursor.is_some());
    }

    #[test]
    fn source_mode_shows_every_line_raw() {
        let mut h = Harness::new("# Title\n\nbody");
        h.set_cursor(2, 0);
        assert_eq!(
            h.editor().view.modes.of(0),
            crate::layout::LineMode::Rendered
        );
        h.send(ctrl('e'));
        assert_eq!(h.editor().view.modes.of(0), crate::layout::LineMode::Raw);
    }

    #[test]
    fn a_bracketed_paste_is_its_own_undo_step() {
        let mut h = Harness::new("");
        h.typed("abc");
        h.send(Event::Paste("PASTED-TEXT".to_owned()));
        h.typed("d");
        h.send(ctrl('z'));
        assert_eq!(h.text(), "abcPASTED-TEXT");
        h.send(ctrl('z'));
        assert_eq!(h.text(), "abc");
    }

    #[test]
    fn edits_after_a_failed_quit_need_a_fresh_attempt() {
        let dir = TempDir::new("requit");
        let sub = dir.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        let path = sub.join("n.md");
        let editor = Editor::from_text(&path, "", None, None);
        let mut app = App::with_editor(dir.path().to_path_buf(), Theme::new(false), editor);
        let now = Instant::now();
        app.handle_event(plain(KeyCode::Char('a')), now);
        fs::remove_dir_all(&sub).unwrap();
        app.handle_event(ctrl('q'), now);
        assert!(!app.quit, "save failed, quit armed");
        // A paste (or a checkbox click) changes the note: the next Ctrl-Q
        // tries to save again instead of discarding the new text.
        app.handle_event(Event::Paste("more".to_owned()), now);
        fs::create_dir_all(&sub).unwrap();
        app.handle_event(ctrl('q'), now);
        assert!(app.quit);
        assert_eq!(fs::read_to_string(&path).unwrap(), "amore");
    }

    #[test]
    fn a_force_save_is_disarmed_by_other_keys_and_newer_disk_changes() {
        let mut h = Harness::new("one\n");
        h.set_cursor(0, 0);
        h.typed("A");
        fs::write(&h.path, "outside 1\n").unwrap();
        h.advance(1100);
        assert!(h.editor().conflict);
        h.send(ctrl('s'));
        assert!(h.editor().force_armed);
        // Typing disarms; Ctrl-S arms again.
        h.typed("B");
        assert!(!h.editor().force_armed);
        h.send(ctrl('s'));
        assert!(h.editor().force_armed);
        // Another change on disk warns again and disarms.
        fs::write(&h.path, "outside 2, important\n").unwrap();
        h.advance(1100);
        assert!(!h.editor().force_armed);
        assert!(h.app.message.as_ref().unwrap().text.contains("again"));
        h.send(ctrl('s'));
        assert_eq!(h.disk(), "outside 2, important\n");
        h.send(ctrl('s'));
        assert_eq!(h.disk(), "ABone\n");
    }

    #[test]
    fn a_note_deleted_on_disk_is_not_recreated_by_quit() {
        let mut h = Harness::new("x\n");
        h.typed("y");
        fs::remove_file(&h.path).unwrap();
        h.send(ctrl('q'));
        assert!(!h.app.quit);
        assert!(!h.path.exists());
        assert!(h.editor().deleted);
        // Switching notes does not write it either.
        h.send(ctrl('o'));
        assert!(!h.path.exists());
        h.send(plain(KeyCode::Esc));
        // Ctrl-S does.
        h.send(ctrl('s'));
        assert_eq!(h.disk(), "yx\n");
        h.send(ctrl('q'));
        assert!(h.app.quit);
    }

    #[test]
    fn a_note_back_on_disk_with_the_same_text_autosaves_again() {
        let mut h = Harness::new("one\n");
        fs::remove_file(&h.path).unwrap();
        h.advance(1100);
        assert!(h.editor().deleted);
        fs::write(&h.path, "one\n").unwrap();
        h.advance(1100);
        assert!(!h.editor().deleted);
        h.set_cursor(0, 3);
        h.typed("!");
        h.advance(1100);
        h.advance(1100);
        assert_eq!(h.disk(), "one!\n");
    }

    #[test]
    fn a_note_that_turns_non_utf8_is_never_overwritten() {
        // Clean: reloaded read-only.
        let mut h = Harness::new("hello\n");
        fs::write(&h.path, b"x\xffy\nz").unwrap();
        h.advance(1100);
        assert!(h.editor().read_only.is_some());
        h.send(ctrl('s'));
        h.send(ctrl('s'));
        assert_eq!(fs::read(&h.path).unwrap(), b"x\xffy\nz");
        // Fixed on disk: editable again.
        fs::write(&h.path, "fixed\n").unwrap();
        h.advance(1100);
        assert!(h.editor().read_only.is_none());
        assert_eq!(h.text(), "fixed\n");

        // Dirty: unsaved edits kept, nothing written, even when forced.
        let mut h = Harness::new("hello\n");
        h.typed("A");
        fs::write(&h.path, b"x\xffy").unwrap();
        h.advance(1100);
        h.send(ctrl('s'));
        h.send(ctrl('s'));
        assert_eq!(fs::read(&h.path).unwrap(), b"x\xffy");
        assert_eq!(h.text(), "Ahello\n");
    }

    #[test]
    fn exit_on_a_signal_keeps_unsaved_text() {
        let mut h = Harness::new("one\n");
        h.typed("A");
        assert_eq!(h.app.save_before_exit(), None);
        assert_eq!(h.disk(), "Aone\n");
        // In conflict it goes to a recovery file instead of over the disk.
        h.typed("B");
        fs::write(&h.path, "theirs\n").unwrap();
        h.advance(1100);
        assert!(h.editor().conflict, "{:?}", h.app.message);
        let note = h.app.save_before_exit().unwrap();
        assert!(note.contains("lumbridge-notes-recovery"), "{note}");
        assert_eq!(h.disk(), "theirs\n");
        let recovery = crate::editor::recovery_path(&h.path);
        assert_eq!(fs::read_to_string(recovery).unwrap(), "ABone\n");
    }

    #[test]
    fn clicking_a_nested_checkbox_toggles_that_box() {
        let mut h = Harness::new("intro\n\n- - [ ] x");
        h.set_cursor(0, 0);
        let (x, y) = h.cell_of(2, 4);
        h.send(mouse(MouseEventKind::Down(MouseButton::Left), x, y));
        assert_eq!(h.text(), "intro\n\n- - [x] x");
    }

    #[test]
    fn new_notes_beside_a_file_outside_the_notebook_write_no_guide() {
        let mut h = Harness::new("# x\n");
        assert!(!h.app.notebook);
        h.send(ctrl('n'));
        h.typed("todo");
        h.send(plain(KeyCode::Enter));
        assert!(h.root().join("todo.md").exists());
        assert!(!h.root().join(store::GUIDE_FILE).exists());
    }

    #[test]
    fn esc_in_a_picker_without_a_note_quits() {
        let dir = TempDir::new("picker");
        let mut app = App::with_picker(dir.path().to_path_buf(), Theme::new(true));
        let now = Instant::now();
        app.handle_event(plain(KeyCode::Char('n')), now);
        app.handle_event(plain(KeyCode::Esc), now);
        assert!(app.quit);
    }

    #[test]
    fn a_drag_selects_what_was_dragged_across_on_screen() {
        // The pressed line stays rendered for the drag; `[` is hidden.
        let mut h = Harness::new("intro\n\nA [link text](http://x) end");
        h.set_cursor(0, 0);
        let (x, y) = h.cell_of(2, 3);
        h.send(mouse(MouseEventKind::Down(MouseButton::Left), x, y));
        h.send(mouse(MouseEventKind::Drag(MouseButton::Left), x + 9, y));
        assert_eq!(
            h.editor().buffer.selected_text().as_deref(),
            Some("link text")
        );
        h.send(mouse(MouseEventKind::Up(MouseButton::Left), x + 9, y));
        assert!(h.editor().held_modes.is_none());
        assert_eq!(
            h.editor().buffer.selected_text().as_deref(),
            Some("link text")
        );

        // Pressing below an expanded front matter collapses it only after
        // the drag, so the rows under the pointer stay put.
        let mut h = Harness::new("+++\ntitle = \"x\"\ntags = []\n+++\nfirst\nSome **bold** words");
        h.set_cursor(1, 0);
        let (x, y) = h.cell_of(5, 0);
        h.send(mouse(MouseEventKind::Down(MouseButton::Left), x, y));
        h.send(mouse(MouseEventKind::Drag(MouseButton::Left), x + 4, y));
        assert_eq!(h.editor().buffer.selected_text().as_deref(), Some("Some"));
        // A key ends the drag (its release may never come).
        h.send(plain(KeyCode::Right));
        assert!(h.editor().held_modes.is_none());
    }

    #[test]
    fn a_click_after_queued_edits_maps_to_the_current_text() {
        let mut h = Harness::new("a\n- [ ] one\n- [ ] two\n- [ ] three");
        h.set_cursor(0, 0);
        let (x, y) = h.cell_of(3, 2);
        // Enter and the click arrive in one burst, before a redraw: line 0
        // splits, so "three" is now one row lower.
        h.app.handle_event(plain(KeyCode::Enter), h.now);
        h.send(mouse(MouseEventKind::Down(MouseButton::Left), x, y + 1));
        assert_eq!(h.text(), "\na\n- [ ] one\n- [ ] two\n- [x] three");
    }

    #[test]
    fn new_files_and_new_notes_end_with_a_newline() {
        let dir = TempDir::new("fresh");
        let path = dir.path().join("fresh.md");
        let editor = Editor::open(&path).unwrap();
        let mut app = App::with_editor(dir.path().to_path_buf(), Theme::new(true), editor);
        let now = Instant::now();
        for ch in "hello fresh".chars() {
            app.handle_event(plain(KeyCode::Char(ch)), now);
        }
        app.handle_event(ctrl('s'), now);
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello fresh\n");
        // A file read from disk keeps its missing final newline.
        let mut h = Harness::new("no newline");
        h.set_cursor(0, 10);
        h.typed("!");
        h.send(ctrl('s'));
        assert_eq!(h.disk(), "no newline!");

        // Ctrl-N: typing starts below the blank line under the heading.
        let mut h = Harness::new("x");
        h.send(ctrl('n'));
        h.typed("Picker made");
        h.send(plain(KeyCode::Enter));
        let path = h.root().join("picker-made.md");
        let before = fs::read_to_string(&path).unwrap();
        assert!(before.ends_with("# Picker made\n\n"), "{before:?}");
        assert!(!h.editor().is_dirty());
        h.typed("1. one");
        h.send(ctrl('s'));
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(after, format!("{before}1. one\n"));
    }

    #[test]
    fn find_matches_the_rendered_text_too() {
        let mut h = Harness::new("Some **bold** text\nSome bold raw");
        h.set_cursor(1, 0);
        h.send(ctrl('f'));
        h.typed("Some bold");
        let Bar::Find(find) = &h.app.bar else {
            panic!("find bar");
        };
        assert_eq!(find.matches, vec![(0, 0..11), (1, 0..9)]);
        h.send(plain(KeyCode::Esc));
        h.send(ctrl('f'));
        h.typed("bold text");
        let Bar::Find(find) = &h.app.bar else {
            panic!("find bar");
        };
        assert_eq!(find.matches, vec![(0, 7..18)]);
    }

    #[test]
    fn status_messages_follow_the_latest_action() {
        let dir = TempDir::new("ro-msg");
        let path = dir.path().join("latin.md");
        fs::write(&path, b"caf\xe9\n").unwrap();
        let editor = Editor::open(&path).unwrap();
        let mut app = App::with_editor(dir.path().to_path_buf(), Theme::new(true), editor);
        app.handle_event(ctrl('s'), Instant::now());
        let message = app.message.as_ref().unwrap();
        assert!(
            message.text.starts_with("read-only, not saved"),
            "{message:?}"
        );

        let mut h = Harness::new("cut me");
        h.send(ctrl('a'));
        h.send(ctrl('x'));
        assert_eq!(h.app.message.as_ref().unwrap().text, "copied");
        h.send(ctrl('v'));
        assert_eq!(h.app.message.as_ref().unwrap().text, "pasted");
        h.typed("!");
        assert!(h.app.message.is_none());
    }

    // -- assistants -------------------------------------------------------------

    use crate::services::ServiceEvent;

    const UNTITLED: &str = "+++\ntitle = \"untitled\"\ntags = []\n+++\n\n# untitled\n\n";

    /// A harness whose assistants are all set up but start no real jobs.
    fn assisted(text: &str) -> Harness {
        let mut harness = Harness::new(text);
        let config: Config = toml::from_str(
            "[ai]\nurl = \"http://ai/v1\"\nauto_name = false\n[speech]\nurl = \"http://tts/v1\"\nvoice = \"sam\"\n[dictation]\nurl = \"http://stt/v1\"\n",
        )
        .unwrap();
        harness.app.set_config(config, None);
        harness.app.assist.services.dry_run = true;
        harness.app.assist.voice = "sam".to_owned();
        harness
    }

    fn click_button(harness: &mut Harness, button: Button) {
        let row = Rect::new(0, 0, 80, 1);
        let tool = ui::toolbar(&harness.app, row, ui::assist_title_width(&harness.app))
            .into_iter()
            .find(|tool| tool.button == button)
            .unwrap_or_else(|| panic!("no {button:?} button"));
        harness.send(mouse(
            MouseEventKind::Down(MouseButton::Left),
            tool.rect.x + 1,
            0,
        ));
        harness.send(mouse(
            MouseEventKind::Up(MouseButton::Left),
            tool.rect.x + 1,
            0,
        ));
    }

    fn pick(harness: &mut Harness, label: &str) {
        for _ in 0..50 {
            let menu = harness.app.assist.menu.as_ref().expect("menu open");
            if menu.items[menu.selected].label == label {
                harness.send(plain(KeyCode::Enter));
                return;
            }
            harness.send(plain(KeyCode::Down));
        }
        panic!("no menu entry {label}");
    }

    #[test]
    fn a_template_fills_a_blank_note_and_is_inserted_into_a_full_one() {
        let mut harness = assisted(UNTITLED);
        click_button(&mut harness, Button::Template);
        assert_eq!(
            harness.app.assist.menu.as_ref().unwrap().kind,
            MenuKind::Template
        );
        pick(&mut harness, "Meeting");
        let text = harness.text();
        assert!(text.starts_with("+++\ntitle = \"untitled\""), "{text}");
        assert!(
            text.contains("# untitled\n\n**When:**") && text.contains("## Agenda"),
            "{text}"
        );
        assert_eq!(text.matches("# untitled").count(), 1, "{text}");
        assert!(
            text.contains("+++\n\n# untitled"),
            "blank line kept: {text}"
        );
        // One undo step takes it back.
        harness.send(ctrl('z'));
        assert_eq!(harness.text(), UNTITLED);
        // A note with text gets the template as a block under the cursor.
        let mut harness = assisted("# Mine\n\nsome text\n");
        harness.set_cursor(2, 0);
        harness.send(ctrl('p'));
        pick(&mut harness, "Idea");
        let text = harness.text();
        assert!(
            text.starts_with("# Mine\n\nsome text\n\n## Problem"),
            "{text}"
        );
    }

    #[test]
    fn an_ai_answer_streams_into_the_panel_and_replaces_the_body() {
        let mut harness = assisted("+++\ntitle = \"T\"\n+++\n\nteh draft\n");
        harness.send(ctrl('k'));
        pick(&mut harness, "Improve writing");
        let job = harness.app.assist.panel.as_ref().unwrap().job.unwrap();
        let services = &harness.app.assist.services;
        services.inject(ServiceEvent::Delta {
            job,
            text: "```markdown\nThe ".into(),
        });
        services.inject(ServiceEvent::Delta {
            job,
            text: "draft.\n```".into(),
        });
        harness.advance(10);
        assert!(harness.app.assist.panel.as_ref().unwrap().streaming());
        // Using it before the answer is complete does nothing.
        harness.send(plain(KeyCode::Enter));
        assert!(harness.text().contains("teh draft"));
        harness.app.assist.services.inject(ServiceEvent::Done {
            job,
            result: Ok(()),
        });
        harness.advance(10);
        harness.send(plain(KeyCode::Enter));
        assert!(harness.app.assist.panel.is_none());
        assert_eq!(harness.text(), "+++\ntitle = \"T\"\n+++\n\nThe draft.\n");
        harness.send(ctrl('z'));
        assert_eq!(harness.text(), "+++\ntitle = \"T\"\n+++\n\nteh draft\n");
    }

    #[test]
    fn ask_ai_works_on_the_selection_and_answers_insert_or_append() {
        let mut harness = assisted("one\ntwo\n");
        harness
            .editor_mut()
            .buffer
            .set_selection(Pos::new(1, 0), Pos::new(1, 3));
        harness.send(ctrl('k'));
        pick(&mut harness, "Ask AI…");
        assert!(matches!(harness.app.bar, Bar::Ask(_)));
        harness.typed("translate");
        harness.send(plain(KeyCode::Enter));
        let panel = harness.app.assist.panel.as_ref().unwrap();
        assert!(panel.part && panel.title == "Ask AI");
        let job = panel.job.unwrap();
        harness.app.assist.services.inject(ServiceEvent::Delta {
            job,
            text: "deux".into(),
        });
        harness.app.assist.services.inject(ServiceEvent::Done {
            job,
            result: Ok(()),
        });
        harness.advance(10);
        harness.send(plain(KeyCode::Char('a')));
        assert_eq!(harness.text(), "one\ntwo\n\ndeux\n");
        // A failed request shows its error and leaves the note alone.
        harness.send(ctrl('k'));
        pick(&mut harness, "Summarize");
        let job = harness.app.assist.panel.as_ref().unwrap().job.unwrap();
        harness.app.assist.services.inject(ServiceEvent::Done {
            job,
            result: Err("down".into()),
        });
        harness.advance(10);
        assert_eq!(
            harness.app.assist.panel.as_ref().unwrap().error.as_deref(),
            Some("down")
        );
        harness.send(plain(KeyCode::Esc));
        assert!(harness.app.assist.panel.is_none());
    }

    #[test]
    fn preset_prompt_text_can_be_inserted_instead_of_run() {
        let mut harness = assisted("# Note\n\n");
        harness.set_cursor(2, 0);
        harness.send(ctrl('k'));
        for _ in 0..50 {
            let menu = harness.app.assist.menu.as_ref().unwrap();
            if menu.items[menu.selected].label == "Improve as a prompt" {
                break;
            }
            harness.send(plain(KeyCode::Down));
        }
        harness.send(plain(KeyCode::Char('i')));
        assert!(harness.app.assist.panel.is_none());
        assert!(
            harness.text().contains("This is a prompt for an AI agent."),
            "{}",
            harness.text()
        );
    }

    #[test]
    fn naming_retitles_the_note_and_renames_its_file() {
        let mut harness = assisted(UNTITLED);
        let untitled = harness.root().join("untitled.md");
        fs::rename(&harness.path, &untitled).unwrap();
        harness.app.editor = Some(Editor::open(&untitled).unwrap());
        harness.set_cursor(6, 0);
        harness.typed("we should plan the lumbridge notes launch next week");
        harness.advance(1500);
        click_button(&mut harness, Button::Name);
        let job = harness.app.assist.naming.as_ref().unwrap().job;
        harness.app.assist.services.inject(ServiceEvent::Title {
            job,
            result: Ok("Launch Plan".into()),
        });
        harness.advance(10);
        let moved = harness.root().join("launch-plan.md");
        assert_eq!(harness.editor().path, moved);
        assert!(!untitled.exists());
        let text = fs::read_to_string(&moved).unwrap();
        assert!(
            text.contains("title = \"Launch Plan\"") && text.contains("# Launch Plan"),
            "{text}"
        );
        assert!(text.contains("launch next week"));
    }

    #[test]
    fn untitled_notes_are_named_once_they_have_words_and_a_pause() {
        let mut harness = assisted(UNTITLED);
        harness.app.assist.services.config.ai.auto_name = true;
        harness.set_cursor(6, 0);
        harness.typed("short");
        harness.advance(5000);
        assert!(harness.app.assist.naming.is_none(), "too few words");
        harness.typed(" and now a good many more words so that it reads like a real note");
        harness.advance(1500);
        assert!(harness.app.assist.naming.is_none(), "not quiet yet");
        harness.advance(5000);
        let job = harness.app.assist.naming.as_ref().expect("auto naming").job;
        harness.app.assist.services.inject(ServiceEvent::Title {
            job,
            result: Err("down".into()),
        });
        harness.advance(5000);
        // Tried once; not again after a failure.
        assert!(harness.app.assist.naming.is_none());
    }

    #[test]
    fn dictation_lands_at_the_cursor_with_spaces() {
        let mut harness = assisted("I said\n");
        harness.set_cursor(0, 6);
        harness.app.assist.transcribing = Some((99, harness.now));
        harness
            .app
            .assist
            .services
            .inject(ServiceEvent::Transcript {
                job: 99,
                result: Ok("hello there".into()),
            });
        harness.advance(10);
        assert_eq!(harness.text(), "I said hello there\n");
        assert!(harness.app.assist.transcribing.is_none());
    }

    #[test]
    fn reading_marks_the_current_paragraph_and_esc_stops() {
        let mut harness = assisted("# Title\n\nFirst paragraph here.\n\nSecond one.\n");
        harness.set_cursor(0, 0);
        harness.send(ctrl('r'));
        let job = harness.app.assist.speaking.as_ref().expect("reading").job;
        harness
            .app
            .assist
            .services
            .inject(ServiceEvent::Speaking { job, chunk: 0 });
        harness.advance(10);
        let lines = harness
            .app
            .assist
            .speaking
            .as_ref()
            .unwrap()
            .current_lines()
            .unwrap();
        assert_eq!(lines.start, 0);
        harness.send(plain(KeyCode::Esc));
        assert!(harness.app.assist.speaking.is_none());
        // Without a speech server it says how to set one up.
        let mut harness = Harness::new("text\n");
        harness.send(ctrl('r'));
        let message = harness.app.message.as_ref().unwrap();
        assert!(message.text.contains("[speech] url"), "{}", message.text);
    }

    #[test]
    fn toolbar_buttons_save_and_open_the_notes_list() {
        let mut harness = assisted("x\n");
        harness.typed("y");
        click_button(&mut harness, Button::Save);
        assert!(!harness.editor().is_dirty());
        click_button(&mut harness, Button::Notes);
        assert!(matches!(harness.app.screen, Screen::Picker(_)));
    }

    #[test]
    fn template_words_do_not_trigger_naming_but_written_ones_do() {
        let mut harness = assisted("+++\ntitle = \"untitled\"\n+++\n");
        harness.app.assist.services.config.ai.auto_name = true;
        harness.advance(10);
        harness.send(ctrl('p'));
        pick(&mut harness, "Meeting");
        assert!(
            harness
                .text()
                .starts_with("+++\ntitle = \"untitled\"\n+++\n\n# untitled\n"),
            "{}",
            harness.text()
        );
        harness.advance(1500);
        harness.advance(5000);
        assert!(
            harness.app.assist.naming.is_none(),
            "only template words so far"
        );
        harness.typed("Sam and Alex talked about shipping the notes toolbar and narration voices");
        harness.advance(1500);
        harness.advance(5000);
        assert!(harness.app.assist.naming.is_some());
    }

    #[test]
    fn naming_ignores_a_heading_that_only_repeats_the_title() {
        let mut harness = assisted("+++\ntitle = \"Old\"\n+++\n\n# Old\n\nthe real words here\n");
        click_button(&mut harness, Button::Name);
        let job = harness.app.assist.naming.as_ref().unwrap().job;
        harness.app.assist.services.inject(ServiceEvent::Title {
            job,
            result: Ok("Old".into()),
        });
        harness.advance(10);
        assert!(harness
            .app
            .message
            .as_ref()
            .unwrap()
            .text
            .starts_with("kept the title"));
        assert!(!harness.editor().is_dirty());
    }

    #[test]
    fn dictation_is_punctuated_or_falls_back_to_the_raw_words() {
        let mut harness = assisted("Note:\n");
        harness.set_cursor(0, 5);
        harness.app.assist.transcribing = Some((7, harness.now));
        let raw = "okay so we ship it tomorrow morning";
        harness
            .app
            .assist
            .services
            .inject(ServiceEvent::Transcript {
                job: 7,
                result: Ok(raw.into()),
            });
        harness.advance(10);
        let (job, _) = harness.app.assist.polishing.expect("punctuating");
        assert_eq!(harness.text(), "Note:\n", "nothing until punctuated");
        harness.app.assist.services.inject(ServiceEvent::Polished {
            job,
            raw: raw.into(),
            result: Ok("Okay, so we ship it tomorrow morning.".into()),
        });
        harness.advance(10);
        assert_eq!(
            harness.text(),
            "Note: Okay, so we ship it tomorrow morning.\n"
        );
        // A failed cleanup still keeps what was said.
        harness.app.assist.transcribing = Some((8, harness.now));
        harness
            .app
            .assist
            .services
            .inject(ServiceEvent::Transcript {
                job: 8,
                result: Ok(raw.into()),
            });
        harness.advance(10);
        let (job, _) = harness.app.assist.polishing.unwrap();
        harness.app.assist.services.inject(ServiceEvent::Polished {
            job,
            raw: raw.into(),
            result: Err("x".into()),
        });
        harness.advance(10);
        assert!(
            harness
                .text()
                .ends_with("morning. okay so we ship it tomorrow morning\n"),
            "{}",
            harness.text()
        );
        // Short takes go straight in.
        harness.app.assist.transcribing = Some((9, harness.now));
        harness
            .app
            .assist
            .services
            .inject(ServiceEvent::Transcript {
                job: 9,
                result: Ok("yes".into()),
            });
        harness.advance(10);
        assert!(harness.app.assist.polishing.is_none());
        assert!(harness.text().ends_with(" yes\n"));
    }

    #[test]
    fn moving_the_mouse_without_a_button_ends_a_lost_drag() {
        let mut harness = Harness::new("one two three\nfour five six\n");
        let area = harness.editor().view.area;
        harness.send(mouse(
            MouseEventKind::Down(MouseButton::Left),
            area.x + 1,
            area.y,
        ));
        // The release never arrives (Lumbridge TUI took it); the mouse just moves.
        harness.send(mouse(MouseEventKind::Moved, area.x + 5, area.y + 1));
        harness.send(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            area.x + 8,
            area.y + 1,
        ));
        assert!(harness.editor().buffer.selection().is_none());
    }

    #[test]
    fn hovering_a_menu_moves_its_highlight() {
        let mut harness = assisted("x\n");
        harness.send(ctrl('k'));
        let area = Rect::new(0, 0, 80, 24);
        let rect = ui::menu_rect(&harness.app, area).unwrap();
        harness.send(mouse(MouseEventKind::Moved, rect.x + 2, rect.y + 2));
        assert_eq!(harness.app.assist.menu.as_ref().unwrap().selected, 1);
        // Outside the box nothing changes.
        harness.send(mouse(
            MouseEventKind::Moved,
            rect.x + rect.width + 2,
            rect.y + 1,
        ));
        assert_eq!(harness.app.assist.menu.as_ref().unwrap().selected, 1);
    }

    #[test]
    fn the_ai_panel_renders_its_answer_as_markdown() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut harness = assisted("# Note\n\nsome text here\n");
        harness.send(ctrl('k'));
        pick(&mut harness, "Summarize");
        let job = harness.app.assist.panel.as_ref().unwrap().job.unwrap();
        let answer = "## Points\n\n- first **bold** point\n- [ ] a task\n\n```\ncode\n```";
        harness.app.assist.services.inject(ServiceEvent::Delta {
            job,
            text: answer.into(),
        });
        harness.app.assist.services.inject(ServiceEvent::Done {
            job,
            result: Ok(()),
        });
        harness.advance(10);
        let mut terminal = Terminal::new(TestBackend::new(80, 32)).unwrap();
        harness.app.prepare(Rect::new(0, 0, 80, 32));
        terminal
            .draw(|frame| ui::render(&harness.app, frame))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..32)
            .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>() + "\n")
            .collect();
        assert!(
            screen.contains("Points") && !screen.contains("## Points"),
            "{screen}"
        );
        assert!(screen.contains("• first bold point"), "{screen}");
        assert!(screen.contains("☐ a task"), "{screen}");
        assert!(screen.contains("Summarize · note · done"), "{screen}");
    }

    #[test]
    fn live_dictation_shows_words_and_inserts_utterances_in_order() {
        use crate::live::Live;
        let mut harness = assisted("Notes:\n");
        harness.set_cursor(0, 6);
        harness.app.assist.live = Some(Live::detached(40));
        assert!(harness.app.bar_shown());
        let services = &harness.app.assist.services;
        services.inject(ServiceEvent::LivePartial {
            job: 40,
            text: "first thing we".into(),
        });
        harness.advance(10);
        assert_eq!(harness.app.assist.live_partial, "first thing we");
        // Two utterances: the first waits for punctuation, the second is
        // short and ready at once, but must not jump ahead.
        let services = &harness.app.assist.services;
        services.inject(ServiceEvent::LiveFinal {
            job: 40,
            text: "first thing we ship it".into(),
        });
        services.inject(ServiceEvent::LiveFinal {
            job: 40,
            text: "then rest".into(),
        });
        harness.advance(10);
        assert!(harness.app.assist.live_partial.is_empty());
        assert_eq!(harness.text(), "Notes:\n", "waiting for the first");
        let job = harness.app.assist.live_queue_front_polish().unwrap();
        harness.app.assist.services.inject(ServiceEvent::Polished {
            job,
            raw: "first thing we ship it".into(),
            result: Ok("First thing: we ship it.".into()),
        });
        harness.advance(10);
        assert_eq!(
            harness.text(),
            "Notes: First thing: we ship it. then rest\n"
        );
        // Stopping waits for the server to end the session.
        harness.send(ctrl('d'));
        assert!(harness.app.assist.live.as_ref().unwrap().stopping.is_some());
        harness.app.assist.services.inject(ServiceEvent::LiveEnded {
            job: 40,
            result: Ok(()),
        });
        harness.advance(10);
        assert!(harness.app.assist.live.is_none() && !harness.app.bar_shown());
    }

    #[test]
    fn a_live_dictation_that_never_closes_is_given_up() {
        let mut harness = assisted("x\n");
        harness.app.assist.live = Some(crate::live::Live::detached(41));
        harness.send(plain(KeyCode::Esc));
        assert!(harness.app.assist.live.as_ref().unwrap().stopping.is_some());
        harness.advance(7000);
        assert!(harness.app.assist.live.is_none());
    }
}
