//! The assistants around the editor: the toolbar buttons, the template and AI
//! menus, the AI answer panel, naming notes, reading aloud and dictation.
//!
//! The work itself runs in [`crate::services`]; this module keeps the state
//! the UI draws and turns finished jobs into edits.

use std::collections::{HashSet, VecDeque};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use super::{App, Bar, Screen};
use crate::buffer::Pos;
use crate::config::Config;
use crate::editor::display_path;
use crate::input::LineInput;
use crate::library::{self, Item, PromptAction};
use crate::live::Live;
use crate::naming;
use crate::narrate;
use crate::retitle::{self, LineEdit};
use crate::services::{self, Cancel, Recording, ServiceEvent, Services};
use crate::store::{self, SavePolicy};
use crate::ui;

/// How long a stopped live dictation may take to deliver its last words.
const LIVE_STOP_GRACE: Duration = Duration::from_secs(6);
/// How often the loop wakes while a job runs (spinner, recording timer).
pub const BUSY_POLL: Duration = Duration::from_millis(100);
/// An untitled note is named by itself once this many words were written
/// into it (beyond what it had when opened, or what a template put there).
const AUTO_NAME_WORDS: u64 = 12;
/// ...and nobody has typed for this long.
const AUTO_NAME_QUIET: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Name,
    Template,
    Ai,
    Read,
    Dictate,
    Save,
    Notes,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKind {
    Template,
    Ai,
    Read,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadScope {
    Cursor,
    Selection,
    Note,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuAction {
    Template(Item),
    NewTemplate,
    Prompt(Item),
    Ask,
    NewPrompt,
    NameNote,
    Read(ReadScope),
    Voice(String),
    /// A heading line in the menu (not selectable).
    Label,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    pub label: String,
    pub detail: String,
    pub action: MenuAction,
}

impl MenuItem {
    fn new(label: impl Into<String>, detail: impl Into<String>, action: MenuAction) -> MenuItem {
        MenuItem {
            label: label.into(),
            detail: detail.into(),
            action,
        }
    }

    pub fn selectable(&self) -> bool {
        self.action != MenuAction::Label
    }
}

/// A dropdown under a toolbar button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    pub kind: MenuKind,
    pub items: Vec<MenuItem>,
    pub selected: usize,
    pub scroll: usize,
}

impl Menu {
    fn new(kind: MenuKind, items: Vec<MenuItem>) -> Menu {
        let selected = items.iter().position(MenuItem::selectable).unwrap_or(0);
        Menu {
            kind,
            items,
            selected,
            scroll: 0,
        }
    }

    fn step(&mut self, delta: isize) {
        let len = self.items.len();
        if len == 0 {
            return;
        }
        let mut index = self.selected;
        for _ in 0..len {
            index = (index as isize + delta).rem_euclid(len as isize) as usize;
            if self.items[index].selectable() {
                self.selected = index;
                return;
            }
        }
    }

    /// Keeps the selection inside `rows` visible rows.
    pub fn prepare(&mut self, rows: usize) {
        let rows = rows.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + rows {
            self.scroll = self.selected + 1 - rows;
        }
        self.scroll = self.scroll.min(self.items.len().saturating_sub(rows));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelButton {
    Replace,
    Insert,
    Append,
    Copy,
    Retry,
    Close,
}

impl PanelButton {
    pub const ALL: [PanelButton; 6] = [
        PanelButton::Replace,
        PanelButton::Insert,
        PanelButton::Append,
        PanelButton::Copy,
        PanelButton::Retry,
        PanelButton::Close,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PanelButton::Replace => "Replace",
            PanelButton::Insert => "Insert",
            PanelButton::Append => "Append",
            PanelButton::Copy => "Copy",
            PanelButton::Retry => "Retry",
            PanelButton::Close => "Close",
        }
    }

    /// The key that presses it in the panel.
    pub fn key(self) -> char {
        match self {
            PanelButton::Replace => 'r',
            PanelButton::Insert => 'i',
            PanelButton::Append => 'a',
            PanelButton::Copy => 'c',
            PanelButton::Retry => 'g',
            PanelButton::Close => 'q',
        }
    }
}

/// What the answer may replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    /// Everything after the front matter.
    Body {
        revision: u64,
    },
    Selection {
        start: Pos,
        end: Pos,
        revision: u64,
    },
}

/// The AI's answer to a prompt, shown under the note.
#[derive(Debug)]
pub struct Panel {
    pub title: String,
    pub instruction: String,
    pub action: PromptAction,
    pub text: String,
    /// Set while the answer is streaming.
    pub job: Option<u64>,
    cancel: Option<Arc<Cancel>>,
    pub error: Option<String>,
    /// First wrapped row shown; `follow` keeps the end in view.
    pub scroll: usize,
    pub follow: bool,
    pub started: Instant,
    target: Target,
    pub part: bool,
}

impl Panel {
    /// The button Enter presses.
    pub fn default_button(&self) -> PanelButton {
        match self.action {
            PromptAction::Replace => PanelButton::Replace,
            PromptAction::Append => PanelButton::Append,
            PromptAction::Insert | PromptAction::Answer => PanelButton::Insert,
        }
    }

    pub fn streaming(&self) -> bool {
        self.job.is_some()
    }
}

#[derive(Debug)]
pub struct Naming {
    pub job: u64,
    cancel: Arc<Cancel>,
    path: PathBuf,
    manual: bool,
}

#[derive(Debug)]
pub struct Speaking {
    pub job: u64,
    cancel: Arc<Cancel>,
    /// Source lines of each chunk.
    pub chunks: Vec<Range<usize>>,
    pub current: Option<usize>,
    pub path: PathBuf,
    pub started: Instant,
}

impl Speaking {
    /// Lines being read now.
    pub fn current_lines(&self) -> Option<Range<usize>> {
        self.current
            .and_then(|index| self.chunks.get(index).cloned())
    }
}

/// A finished live utterance: its text once ready (punctuated or raw), or
/// the punctuation job it waits for.
#[derive(Debug)]
struct Utterance {
    polish: Option<u64>,
    text: Option<String>,
}

#[derive(Debug)]
pub struct Assist {
    pub services: Services,
    pub menu: Option<Menu>,
    pub panel: Option<Panel>,
    pub naming: Option<Naming>,
    pub speaking: Option<Speaking>,
    pub recording: Option<Recording>,
    /// A transcription in flight, and when it started.
    pub transcribing: Option<(u64, Instant)>,
    /// The AI punctuating the last transcript, and when it started.
    pub polishing: Option<(u64, Instant)>,
    /// Live dictation in progress.
    pub live: Option<Live>,
    /// The utterance being spoken right now (shown in the bar).
    pub live_partial: String,
    /// Finished utterances in spoken order, waiting for punctuation before
    /// they go into the note.
    live_queue: VecDeque<Utterance>,
    pub voices: Vec<String>,
    pub voice: String,
    voices_requested: bool,
    /// Notes already named automatically this session (never twice).
    auto_named: HashSet<PathBuf>,
    /// Words in the open note when it was opened (or a template was
    /// applied): only words written since count towards naming it.
    word_baseline: Option<(PathBuf, u64)>,
}

impl Assist {
    pub fn new(config: Config) -> Assist {
        let voice = saved_voice().unwrap_or_else(|| config.speech.voice.clone());
        Assist {
            services: Services::new(config),
            menu: None,
            panel: None,
            naming: None,
            speaking: None,
            recording: None,
            transcribing: None,
            polishing: None,
            live: None,
            live_partial: String::new(),
            live_queue: VecDeque::new(),
            voices: Vec::new(),
            voice,
            voices_requested: false,
            auto_named: HashSet::new(),
            word_baseline: None,
        }
    }

    /// The punctuation job the oldest queued live utterance waits for.
    #[cfg(test)]
    pub fn live_queue_front_polish(&self) -> Option<u64> {
        self.live_queue
            .front()
            .and_then(|utterance| utterance.polish)
    }

    pub fn config(&self) -> &Config {
        &self.services.config
    }

    /// Something is running that the toolbar shows progress for.
    pub fn busy(&self) -> bool {
        self.naming.is_some()
            || self.speaking.is_some()
            || self.recording.is_some()
            || self.transcribing.is_some()
            || self.polishing.is_some()
            || self.live.is_some()
            || !self.live_queue.is_empty()
            || self.panel.as_ref().is_some_and(Panel::streaming)
    }
}

// The chosen voice survives restarts in $XDG_STATE_HOME/lumbridge-notes/voice.
const VOICE_FILE: &str = "voice";

fn state_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| store::home_dir().map(|home| home.join(".local").join("state")))
}

fn saved_voice() -> Option<String> {
    let path = naming::app_file(&state_dir()?, VOICE_FILE);
    let text = std::fs::read_to_string(path).ok()?;
    let voice = text.trim();
    (!voice.is_empty() && voice.len() < 100).then(|| voice.to_owned())
}

fn save_voice(voice: &str) {
    if let Some(path) = state_dir().map(|dir| naming::app_file(&dir, VOICE_FILE)) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, format!("{voice}\n"));
    }
}

/// Index of the first line after the front matter.
fn body_start<S: AsRef<str>>(lines: &[S]) -> usize {
    store::front_matter_lines(lines).map_or(0, |(_, end)| end + 1)
}

/// Where the note's text starts: the first non-blank line after the front
/// matter. When there is none, after one kept blank line under the front
/// matter (so replacing the text never glues it to the closing fence).
fn content_start<S: AsRef<str>>(lines: &[S]) -> usize {
    let start = body_start(lines);
    (start..lines.len())
        .find(|&index| !lines[index].as_ref().trim().is_empty())
        .unwrap_or_else(|| {
            if start > 0 {
                (start + 1).min(lines.len())
            } else {
                start
            }
        })
}

/// The note's text for the AI, without the front matter or a first heading
/// that only repeats the title.
fn text_for_naming<S: AsRef<str>>(lines: &[S], title: &str) -> String {
    let start = content_start(lines);
    let mut rest = lines.get(start..).unwrap_or(&[]).iter().map(AsRef::as_ref);
    let mut text = Vec::new();
    if let Some(first) = rest.next() {
        let repeats = first
            .trim()
            .strip_prefix("# ")
            .is_some_and(|heading| heading.trim().eq_ignore_ascii_case(title.trim()));
        if !repeats {
            text.push(first);
        }
    }
    text.extend(rest);
    text.join("\n")
}

impl App {
    // -- menus ---------------------------------------------------------------

    pub(super) fn open_menu(&mut self, kind: MenuKind, now: Instant) {
        if self.editor.is_none() {
            self.info("open a note first", now);
            return;
        }
        if self
            .assist
            .menu
            .as_ref()
            .is_some_and(|menu| menu.kind == kind)
        {
            self.assist.menu = None;
            return;
        }
        self.bar = Bar::None;
        let items = match self.menu_items(kind) {
            Ok(items) => items,
            Err(err) => {
                self.error(err, now);
                return;
            }
        };
        if kind == MenuKind::Read
            && self.assist.config().speech_ready()
            && !self.assist.voices_requested
        {
            self.assist.voices_requested = true;
            self.assist.services.load_voices();
        }
        self.assist.menu = Some(Menu::new(kind, items));
    }

    fn menu_items(&self, kind: MenuKind) -> Result<Vec<MenuItem>, String> {
        let root = &self.root;
        let config = self.assist.config();
        let mut items = Vec::new();
        match kind {
            MenuKind::Template => {
                let templates =
                    library::templates(root).map_err(|err| format!("templates: {err}"))?;
                for item in templates {
                    items.push(MenuItem::new(
                        item.title.clone(),
                        item.description.clone(),
                        MenuAction::Template(item),
                    ));
                }
                items.push(MenuItem::new(
                    "+ New template…",
                    "",
                    MenuAction::NewTemplate,
                ));
            }
            MenuKind::Ai => {
                let ready = config.ai_ready();
                let off = if ready { "" } else { "set up [ai] first" };
                items.push(MenuItem::new(
                    "Ask AI…",
                    if ready { "your own prompt" } else { off },
                    MenuAction::Ask,
                ));
                items.push(MenuItem::new(
                    "Name this note",
                    if ready { "title + file name" } else { off },
                    MenuAction::NameNote,
                ));
                items.push(MenuItem::new(
                    "Prompts",
                    "Enter run · i insert text",
                    MenuAction::Label,
                ));
                let prompts = library::prompts(root).map_err(|err| format!("prompts: {err}"))?;
                for item in prompts {
                    let detail = format!("{} · {}", item.action.label(), item.description);
                    let detail = detail.trim_end_matches([' ', '·']).to_owned();
                    items.push(MenuItem::new(
                        item.title.clone(),
                        detail,
                        MenuAction::Prompt(item),
                    ));
                }
                items.push(MenuItem::new("+ New prompt…", "", MenuAction::NewPrompt));
            }
            MenuKind::Read => {
                let ready = config.speech_ready();
                let off = if ready { "" } else { "set up [speech] first" };
                let selection = self
                    .editor
                    .as_ref()
                    .is_some_and(|editor| editor.buffer.selection().is_some());
                items.push(MenuItem::new(
                    "▶ Read from the cursor",
                    off,
                    MenuAction::Read(ReadScope::Cursor),
                ));
                if selection {
                    items.push(MenuItem::new(
                        "▶ Read the selection",
                        off,
                        MenuAction::Read(ReadScope::Selection),
                    ));
                }
                items.push(MenuItem::new(
                    "▶ Read the whole note",
                    off,
                    MenuAction::Read(ReadScope::Note),
                ));
                items.push(MenuItem::new("Voice", "", MenuAction::Label));
                let mut voices = self.assist.voices.clone();
                if !voices.contains(&self.assist.voice) {
                    voices.insert(0, self.assist.voice.clone());
                }
                for voice in voices {
                    let mark = if voice == self.assist.voice {
                        "●"
                    } else {
                        ""
                    };
                    items.push(MenuItem::new(voice.clone(), mark, MenuAction::Voice(voice)));
                }
            }
        }
        Ok(items)
    }

    pub(super) fn menu_key(&mut self, key: KeyEvent, now: Instant) {
        let Some(menu) = self.assist.menu.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.assist.menu = None,
            KeyCode::Up => menu.step(-1),
            KeyCode::Down | KeyCode::Tab => menu.step(1),
            KeyCode::BackTab => menu.step(-1),
            KeyCode::PageUp => menu.step(-5),
            KeyCode::PageDown => menu.step(5),
            KeyCode::Enter => {
                let index = menu.selected;
                self.activate_menu_item(index, false, now);
            }
            KeyCode::Char('i') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let index = menu.selected;
                self.activate_menu_item(index, true, now);
            }
            // The menu's own shortcut closes it again.
            KeyCode::Char('k' | 'p' | 'r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.assist.menu = None;
            }
            _ => {}
        }
    }

    /// Runs the menu entry at `index`. `insert` puts a prompt's text into the
    /// note instead of running it.
    fn activate_menu_item(&mut self, index: usize, insert: bool, now: Instant) {
        let Some(menu) = self.assist.menu.as_ref() else {
            return;
        };
        let Some(item) = menu.items.get(index).cloned() else {
            return;
        };
        if !item.selectable() {
            return;
        }
        self.assist.menu = None;
        match item.action {
            MenuAction::Template(template) => self.apply_template(&template, now),
            MenuAction::NewTemplate => {
                self.bar = Bar::NewItem {
                    prompt: false,
                    input: LineInput::default(),
                }
            }
            MenuAction::Prompt(prompt) if insert => self.insert_block(&prompt.body, now),
            MenuAction::Prompt(prompt) => {
                self.start_prompt(&prompt.title, &prompt.body, prompt.action, now)
            }
            MenuAction::Ask => {
                if self.require_ai(now) {
                    self.bar = Bar::Ask(LineInput::default());
                }
            }
            MenuAction::NewPrompt => {
                self.bar = Bar::NewItem {
                    prompt: true,
                    input: LineInput::default(),
                }
            }
            MenuAction::NameNote => self.start_naming(true, now),
            MenuAction::Read(scope) => self.start_reading(scope, now),
            MenuAction::Voice(voice) => {
                save_voice(&voice);
                self.info(format!("voice: {voice}"), now);
                self.assist.voice = voice;
            }
            MenuAction::Label => {}
        }
    }

    /// Points the open menu's highlight at the entry under the mouse.
    pub(super) fn hover_menu(&mut self, column: u16, row: u16) {
        let Some(area) = self.last_area else {
            return;
        };
        let inside =
            ui::menu_rect(self, area).is_some_and(|rect| rect.contains((column, row).into()));
        if !inside {
            return;
        }
        if let Some(index) = ui::menu_index(self, area, row) {
            if let Some(menu) = self.assist.menu.as_mut() {
                if menu.items.get(index).is_some_and(MenuItem::selectable) {
                    menu.selected = index;
                }
            }
        }
    }

    pub(super) fn assist_bar_key(&mut self, key: KeyEvent, now: Instant) {
        let input = match &mut self.bar {
            Bar::Ask(input) | Bar::NewItem { input, .. } => input,
            _ => return,
        };
        match key.code {
            KeyCode::Esc => self.bar = Bar::None,
            KeyCode::Enter => {
                let text = input.text.trim().to_owned();
                let bar = std::mem::replace(&mut self.bar, Bar::None);
                if text.is_empty() {
                    return;
                }
                match bar {
                    Bar::Ask(_) => self.start_prompt("Ask AI", &text, PromptAction::Answer, now),
                    Bar::NewItem { prompt, .. } => self.create_library_item(&text, prompt, now),
                    _ => {}
                }
            }
            _ => {
                input.handle_key(key);
            }
        }
    }

    fn create_library_item(&mut self, title: &str, prompt: bool, now: Instant) {
        let dir = if prompt {
            library::prompts_dir(&self.root)
        } else {
            library::templates_dir(&self.root)
        };
        match library::create_item(&dir, title, prompt) {
            Ok(path) => {
                if self.open_note(&path, None, now) {
                    if let Some(editor) = self.editor.as_mut() {
                        editor.buffer.move_doc_end(false);
                        editor.follow = true;
                    }
                    let what = if prompt { "prompt" } else { "template" };
                    self.info(
                        format!("new {what}: edit it here, it is saved as you type"),
                        now,
                    );
                }
            }
            Err(err) => self.error(format!("could not create it: {err}"), now),
        }
    }

    // -- toolbar ---------------------------------------------------------------

    pub(super) fn press(&mut self, button: Button, now: Instant) {
        match button {
            Button::Name => self.start_naming(true, now),
            Button::Template => self.open_menu(MenuKind::Template, now),
            Button::Ai => self.open_menu(MenuKind::Ai, now),
            Button::Read => {
                if self.assist.speaking.is_some() {
                    self.stop_reading(now);
                } else {
                    self.open_menu(MenuKind::Read, now);
                }
            }
            Button::Dictate => self.toggle_dictation(now),
            Button::Save => self.save_command(now),
            Button::Notes => self.open_picker(now),
            Button::Quit => self.request_quit(now),
        }
    }

    /// Mouse events for the toolbar, menus and panel. True when handled.
    pub(super) fn assist_mouse(&mut self, mouse: MouseEvent, now: Instant) -> bool {
        let Some(area) = self.last_area else {
            return false;
        };
        let down = matches!(mouse.kind, MouseEventKind::Down(_));
        let right = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right));
        if self.assist.menu.is_some() {
            let Some(rect) = ui::menu_rect(self, area) else {
                self.assist.menu = None;
                return true;
            };
            let inside = rect.contains((mouse.column, mouse.row).into());
            match mouse.kind {
                MouseEventKind::ScrollUp if inside => {
                    if let Some(menu) = self.assist.menu.as_mut() {
                        menu.step(-1);
                    }
                }
                MouseEventKind::ScrollDown if inside => {
                    if let Some(menu) = self.assist.menu.as_mut() {
                        menu.step(1);
                    }
                }
                _ if down && inside => {
                    if let Some(index) = ui::menu_index(self, area, mouse.row) {
                        self.activate_menu_item(index, right, now);
                    }
                }
                _ if down => {
                    self.assist.menu = None;
                    // A click on another toolbar button opens that one.
                    if let Some(button) = ui::toolbar_hit(self, area, mouse.column, mouse.row) {
                        self.press(button, now);
                    }
                }
                _ => {}
            }
            return true;
        }
        if down {
            if let Some(button) = ui::toolbar_hit(self, area, mouse.column, mouse.row) {
                self.press(button, now);
                return true;
            }
        }
        if self.assist.panel.is_some() {
            let layout = ui::editor_layout(area, self.bar_shown(), true);
            let Some(rect) = layout.panel else {
                return false;
            };
            if !rect.contains((mouse.column, mouse.row).into()) {
                return false;
            }
            match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll_panel(-3),
                MouseEventKind::ScrollDown => self.scroll_panel(3),
                _ if down => {
                    if let Some(button) = ui::panel_button_hit(self, rect, mouse.column, mouse.row)
                    {
                        self.panel_button(button, now);
                    }
                }
                _ => {}
            }
            return true;
        }
        false
    }

    // -- AI prompts ------------------------------------------------------------

    fn require_ai(&mut self, now: Instant) -> bool {
        if self.assist.config().ai_ready() {
            return true;
        }
        self.warn(setup_hint("[ai] url"), now);
        false
    }

    fn start_prompt(&mut self, title: &str, instruction: &str, action: PromptAction, now: Instant) {
        if !self.require_ai(now) {
            return;
        }
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        editor.sync();
        let revision = editor.buffer.revision();
        let (text, target, part) = match editor.buffer.selection() {
            Some((start, end)) => (
                editor.buffer.text_range(start, end),
                Target::Selection {
                    start,
                    end,
                    revision,
                },
                true,
            ),
            None => {
                let lines = editor.buffer.lines();
                let start = body_start(lines);
                (
                    lines.get(start..).unwrap_or(&[]).join("\n"),
                    Target::Body { revision },
                    false,
                )
            }
        };
        if text.trim().is_empty() {
            self.info("the note is empty: write something first", now);
            return;
        }
        if let Some(old) = self.assist.panel.take() {
            if let Some(cancel) = old.cancel {
                cancel.cancel();
            }
        }
        let (job, cancel) = self.assist.services.run_prompt(instruction, &text, part);
        self.assist.panel = Some(Panel {
            title: title.to_owned(),
            instruction: instruction.to_owned(),
            action,
            text: String::new(),
            job: Some(job),
            cancel: Some(cancel),
            error: None,
            scroll: 0,
            follow: true,
            started: now,
            target,
            part,
        });
    }

    pub(super) fn panel_key(&mut self, key: KeyEvent, now: Instant) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if let KeyCode::Char('c') = key.code {
                self.panel_button(PanelButton::Copy, now);
            }
            return;
        }
        match key.code {
            KeyCode::Esc => self.panel_button(PanelButton::Close, now),
            KeyCode::Enter => {
                if let Some(button) = self.assist.panel.as_ref().map(Panel::default_button) {
                    self.panel_button(button, now);
                }
            }
            KeyCode::Up => self.scroll_panel(-1),
            KeyCode::Down => self.scroll_panel(1),
            KeyCode::PageUp => self.scroll_panel(-10),
            KeyCode::PageDown => self.scroll_panel(10),
            KeyCode::Char(ch) => {
                if let Some(button) = PanelButton::ALL.into_iter().find(|b| b.key() == ch) {
                    self.panel_button(button, now);
                }
            }
            _ => {}
        }
    }

    fn scroll_panel(&mut self, delta: isize) {
        if let Some(panel) = self.assist.panel.as_mut() {
            panel.scroll = panel.scroll.saturating_add_signed(delta);
            // Scrolling up stops following the stream; the draw clamps the
            // end and turns following back on there.
            panel.follow = false;
        }
    }

    fn panel_button(&mut self, button: PanelButton, now: Instant) {
        let Some(panel) = self.assist.panel.as_ref() else {
            return;
        };
        match button {
            PanelButton::Close => {
                if let Some(panel) = self.assist.panel.take() {
                    if let Some(cancel) = panel.cancel {
                        cancel.cancel();
                    }
                }
                return;
            }
            PanelButton::Retry => {
                let (title, instruction, action) =
                    (panel.title.clone(), panel.instruction.clone(), panel.action);
                self.start_prompt(&title, &instruction, action, now);
                return;
            }
            _ => {}
        }
        if panel.streaming() {
            self.info("still writing: wait for the answer, or Esc to stop", now);
            return;
        }
        let answer = services::unfence(&panel.text).trim_end().to_owned();
        if answer.trim().is_empty() {
            self.info("no answer to use", now);
            return;
        }
        let target = panel.target;
        if button == PanelButton::Copy {
            self.copy_to_clipboard(answer, now);
            return;
        }
        if let Some(reason) = self
            .editor
            .as_ref()
            .and_then(|editor| editor.read_only.clone())
        {
            self.warn(format!("read-only: {reason}"), now);
            return;
        }
        let done = match button {
            PanelButton::Replace => self.replace_target(target, &answer, now),
            PanelButton::Insert => {
                self.insert_block(&answer, now);
                true
            }
            PanelButton::Append => {
                self.append_block(&answer);
                true
            }
            _ => false,
        };
        if done {
            self.assist.panel = None;
            let what = match button {
                PanelButton::Replace => "replaced",
                PanelButton::Append => "appended",
                _ => "inserted",
            };
            self.info(format!("{what} (Ctrl-Z undoes it)"), now);
        }
    }

    fn replace_target(&mut self, target: Target, answer: &str, now: Instant) -> bool {
        let Some(editor) = self.editor.as_mut() else {
            return false;
        };
        let current = editor.buffer.revision();
        let revision = match target {
            Target::Body { revision } | Target::Selection { revision, .. } => revision,
        };
        if revision != current {
            self.warn("the note changed since you asked: use Insert or Retry", now);
            return false;
        }
        let buffer = &mut editor.buffer;
        let (start, end) = match target {
            Target::Selection { start, end, .. } => (start, end),
            Target::Body { .. } => {
                // Keep the blank lines between the front matter and the text.
                let start = content_start(buffer.lines());
                (Pos::new(start, 0), buffer.end_pos())
            }
        };
        buffer.begin_compound();
        let end = buffer.replace(start, end, answer);
        buffer.set_cursor(end, false);
        buffer.end_compound();
        if matches!(target, Target::Body { .. }) {
            buffer.set_final_newline();
        }
        editor.follow = true;
        true
    }

    /// Inserts `text` as its own block below the cursor line (or at the
    /// cursor on an empty line), as one undo step.
    pub(super) fn insert_block(&mut self, text: &str, now: Instant) {
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        if let Some(reason) = &editor.read_only {
            let text = format!("read-only: {reason}");
            self.warn(text, now);
            return;
        }
        let text = text.trim_matches('\n');
        let buffer = &mut editor.buffer;
        let cursor = buffer.selection().map_or(buffer.cursor(), |(_, end)| end);
        let line_len = buffer.line(cursor.line).len();
        let empty_line = buffer.line(cursor.line).trim().is_empty();
        buffer.begin_compound();
        let end = if empty_line {
            buffer.replace(
                Pos::new(cursor.line, 0),
                Pos::new(cursor.line, line_len),
                text,
            )
        } else {
            let at = Pos::new(cursor.line, line_len);
            buffer.replace(at, at, &format!("\n\n{text}"))
        };
        buffer.set_cursor(end, false);
        buffer.end_compound();
        editor.follow = true;
    }

    fn append_block(&mut self, text: &str) {
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        let buffer = &mut editor.buffer;
        let end = buffer.end_pos();
        let last_blank = buffer.line(end.line).trim().is_empty();
        let prev_blank = end.line == 0 || buffer.line(end.line - 1).trim().is_empty();
        let lead = match (last_blank, prev_blank) {
            (true, true) => "",
            (true, false) => "\n",
            _ => "\n\n",
        };
        buffer.begin_compound();
        let at = buffer.replace(end, end, &format!("{lead}{}", text.trim_matches('\n')));
        buffer.set_cursor(at, false);
        buffer.end_compound();
        buffer.set_final_newline();
        editor.follow = true;
    }

    // -- templates -------------------------------------------------------------

    fn apply_template(&mut self, template: &Item, now: Instant) {
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        if let Some(reason) = &editor.read_only {
            let text = format!("read-only: {reason}");
            self.warn(text, now);
            return;
        }
        editor.sync();
        let title = editor.title();
        let time = SystemTime::now();
        let text = library::fill(
            &template.body,
            &title,
            &store::local_date(time),
            &store::local_time(time),
        );
        let lines = editor.buffer.lines();
        let start = body_start(lines);
        // Blank apart from a heading that repeats the title: fill the note.
        let blank = lines.iter().skip(start).all(|line| {
            let line = line.trim();
            line.is_empty()
                || line
                    .strip_prefix("# ")
                    .is_some_and(|heading| heading.trim().eq_ignore_ascii_case(title.trim()))
        });
        if blank {
            let start = content_start(lines);
            let buffer = &mut editor.buffer;
            buffer.begin_compound();
            let end = buffer.end_pos();
            let text = text.trim_matches('\n');
            let (from, text) = if start >= buffer.line_count() {
                // Only front matter: start after its closing fence, with a
                // blank line between.
                (end, format!("\n\n{text}"))
            } else {
                (Pos::new(start, 0), text.to_owned())
            };
            buffer.replace(from, end, &text);
            // The cursor goes to the first empty slot the template leaves
            // (`- [ ] `, `**Who:** `), else the end.
            let slot = buffer
                .lines()
                .iter()
                .enumerate()
                .skip(start)
                .find(|(_, line)| line.ends_with(' ') || line.trim() == "-")
                .map(|(index, line)| Pos::new(index, line.len()));
            let at = slot.unwrap_or_else(|| buffer.end_pos());
            buffer.set_cursor(at, false);
            buffer.end_compound();
            buffer.set_final_newline();
            editor.follow = true;
        } else {
            // Mid-note, a heading that repeats the note's title is dropped.
            let mut lines = text
                .lines()
                .skip_while(|line| line.trim().is_empty())
                .peekable();
            let repeats = lines.peek().is_some_and(|first| {
                first
                    .trim()
                    .strip_prefix("# ")
                    .is_some_and(|heading| heading.trim().eq_ignore_ascii_case(title.trim()))
            });
            let text = if repeats {
                lines
                    .skip(1)
                    .skip_while(|line| line.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                text
            };
            self.insert_block(&text, now);
        }
        // The template's own words do not count towards naming the note.
        self.assist.word_baseline = None;
        self.info(format!("template: {}", template.title), now);
    }

    // -- naming ----------------------------------------------------------------

    pub(super) fn start_naming(&mut self, manual: bool, now: Instant) {
        if self.assist.naming.is_some() {
            if manual {
                self.info("already naming this note", now);
            }
            return;
        }
        if manual && !self.require_ai(now) {
            return;
        }
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        if let Some(reason) = &editor.read_only {
            let text = format!("read-only: {reason}");
            if manual {
                self.warn(text, now);
            }
            return;
        }
        editor.sync();
        let body = text_for_naming(editor.buffer.lines(), &editor.title());
        if crate::text::word_count(&body) < 3 {
            if manual {
                self.info("write a few words first, then name it", now);
            }
            return;
        }
        let path = editor.path.clone();
        let (job, cancel) = self.assist.services.name_note(&body);
        self.assist.naming = Some(Naming {
            job,
            cancel,
            path,
            manual,
        });
        if manual {
            self.info("naming…", now);
        }
    }

    fn apply_title(&mut self, title: &str, manual: bool, now: Instant) {
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        if editor.read_only.is_some() || editor.conflict || editor.deleted {
            self.warn("not renamed: the note changed on disk or is read-only", now);
            return;
        }
        editor.sync();
        let old = editor.title();
        if old.trim() == title.trim() {
            if manual {
                self.info(format!("kept the title: {title}"), now);
            }
            return;
        }
        let edits = retitle::edits(editor.buffer.lines(), &old, title);
        let buffer = &mut editor.buffer;
        let mut cursor = buffer.cursor();
        buffer.begin_compound();
        // From the bottom up so earlier line numbers stay valid.
        let mut ordered = edits.clone();
        ordered.sort_by_key(|edit| match edit {
            LineEdit::Replace(index, _) | LineEdit::Insert(index, _) => std::cmp::Reverse(*index),
        });
        for edit in ordered {
            match edit {
                LineEdit::Replace(index, text) => {
                    let len = buffer.line(index).len();
                    buffer.replace(Pos::new(index, 0), Pos::new(index, len), &text);
                    if cursor.line == index {
                        cursor.col = cursor.col.min(text.len());
                    }
                }
                LineEdit::Insert(index, lines) => {
                    let mut text = lines.join("\n");
                    text.push('\n');
                    buffer.replace(Pos::new(index, 0), Pos::new(index, 0), &text);
                    if cursor.line >= index {
                        cursor.line += lines.len();
                    }
                }
            }
        }
        let cursor = buffer.clamp(cursor);
        buffer.set_cursor(cursor, false);
        buffer.end_compound();
        if let Err(err) = editor.save(SavePolicy::Safe) {
            self.error(format!("named, but not saved: {err}"), now);
            return;
        }
        let mut message = format!("named: {title}");
        if let Some(to) = retitle::new_path(&editor.path, &old, title) {
            match retitle::rename_no_clobber(&editor.path, &to) {
                Ok(moved) => {
                    let file = moved
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    message = format!("named: {title} → {file}");
                    self.assist.auto_named.insert(moved.clone());
                    editor.display_path = display_path(&moved);
                    editor.path = moved;
                }
                Err(err) => message = format!("named: {title} (file not renamed: {err})"),
            }
        }
        if !manual {
            message.push_str(" · Ctrl-Z undoes the title");
        }
        self.info(message, now);
    }

    /// Names an untitled note once it has some text and the typing paused.
    fn maybe_auto_name(&mut self, now: Instant) {
        let config = self.assist.config();
        if !config.ai_ready() || !config.ai.auto_name || self.assist.naming.is_some() {
            return;
        }
        if !matches!(self.screen, Screen::Editor) {
            return;
        }
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        editor.sync();
        let baseline = match &self.assist.word_baseline {
            Some((path, words)) if *path == editor.path => *words,
            _ => {
                self.assist.word_baseline = Some((editor.path.clone(), editor.word_total));
                return;
            }
        };
        if editor.read_only.is_some()
            || editor.conflict
            || editor.deleted
            || editor.is_dirty()
            || editor.word_total < baseline + AUTO_NAME_WORDS
            || self.assist.auto_named.contains(&editor.path)
            || editor
                .last_edit
                .is_some_and(|at| now.saturating_duration_since(at) < AUTO_NAME_QUIET)
        {
            return;
        }
        if !retitle::is_untitled(&editor.title(), &editor.path) {
            return;
        }
        let path = editor.path.clone();
        self.assist.auto_named.insert(path);
        self.start_naming(false, now);
    }

    // -- reading aloud -----------------------------------------------------------

    pub(super) fn toggle_reading(&mut self, now: Instant) {
        if self.assist.speaking.is_some() {
            self.stop_reading(now);
        } else {
            let scope = if self
                .editor
                .as_ref()
                .is_some_and(|editor| editor.buffer.selection().is_some())
            {
                ReadScope::Selection
            } else {
                ReadScope::Cursor
            };
            self.start_reading(scope, now);
        }
    }

    pub(super) fn stop_reading(&mut self, now: Instant) {
        if let Some(speaking) = self.assist.speaking.take() {
            speaking.cancel.cancel();
            self.info("stopped reading", now);
        }
    }

    fn start_reading(&mut self, scope: ReadScope, now: Instant) {
        if !self.assist.config().speech_ready() {
            self.warn(setup_hint("[speech] url"), now);
            return;
        }
        if let Some(old) = self.assist.speaking.take() {
            old.cancel.cancel();
        }
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        editor.sync();
        let count = editor.buffer.line_count();
        let range = match (scope, editor.buffer.selection()) {
            (ReadScope::Selection, Some((start, end))) => start.line..end.line + 1,
            (ReadScope::Note, _) => 0..count,
            _ => {
                // From the cursor's block, or the top when it sits in the
                // front matter.
                let line = editor.buffer.cursor().line;
                let start = body_start(editor.buffer.lines());
                line.max(start)..count
            }
        };
        let chunks = narrate::chunks(&editor.doc, editor.buffer.lines(), range);
        if chunks.is_empty() {
            self.info("nothing to read here", now);
            return;
        }
        let path = editor.path.clone();
        let lines = chunks.iter().map(|chunk| chunk.lines.clone()).collect();
        let texts = chunks.into_iter().map(|chunk| chunk.text).collect();
        let voice = self.assist.voice.clone();
        let (job, cancel) = self.assist.services.speak(texts, &voice);
        self.assist.speaking = Some(Speaking {
            job,
            cancel,
            chunks: lines,
            current: None,
            path,
            started: now,
        });
        self.info(format!("reading with {voice}… (Esc or ■ stops)"), now);
    }

    // -- dictation -----------------------------------------------------------------

    pub(super) fn toggle_dictation(&mut self, now: Instant) {
        if let Some(live) = self.assist.live.as_mut() {
            if live.stopping.is_none() {
                live.stop(now);
                self.info("finishing the last words…", now);
            }
            return;
        }
        if let Some(recording) = self.assist.recording.take() {
            let job = self.assist.services.transcribe(recording);
            self.assist.transcribing = Some((job, now));
            self.info("transcribing…", now);
            return;
        }
        if self.assist.transcribing.is_some() || self.assist.polishing.is_some() {
            self.info("still transcribing the last take", now);
            return;
        }
        if !self.assist.config().dictation_ready() {
            self.warn(setup_hint("[dictation] url"), now);
            return;
        }
        match self.editor.as_ref() {
            None => {
                self.info("open a note first", now);
                return;
            }
            Some(editor) if editor.read_only.is_some() => {
                self.warn("read-only note", now);
                return;
            }
            Some(_) => {}
        }
        let mut fallback = None;
        if self.assist.config().dictation.live && !self.assist.services.dry_run {
            match self.assist.services.start_live() {
                Ok(live) => {
                    self.assist.live = Some(live);
                    self.assist.live_partial.clear();
                    self.info(
                        "listening… words appear as you talk (■ or Ctrl-D stops)",
                        now,
                    );
                    return;
                }
                Err(err) => fallback = Some(err),
            }
        }
        match self.assist.services.record() {
            Ok(recording) => {
                self.assist.recording = Some(recording);
                match fallback {
                    Some(err) => self.warn(format!("{err}; recording a take instead"), now),
                    None => self.info("listening… click ■ or Ctrl-D to stop", now),
                }
            }
            Err(err) => self.error(err, now),
        }
    }

    /// A live utterance finished: queue it (punctuated first when set up).
    fn live_final(&mut self, text: &str, now: Instant) {
        self.assist.live_partial.clear();
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let config = self.assist.config();
        let punctuate =
            config.dictation.punctuate && config.ai_ready() && crate::text::word_count(text) >= 4;
        let utterance = if punctuate {
            Utterance {
                polish: Some(self.assist.services.polish_dictation(text)),
                text: None,
            }
        } else {
            Utterance {
                polish: None,
                text: Some(text.to_owned()),
            }
        };
        self.assist.live_queue.push_back(utterance);
        self.flush_live_queue(now);
    }

    /// Inserts finished utterances in spoken order.
    fn flush_live_queue(&mut self, now: Instant) {
        while self
            .assist
            .live_queue
            .front()
            .is_some_and(|utterance| utterance.text.is_some())
        {
            if let Some(text) = self.assist.live_queue.pop_front().and_then(|u| u.text) {
                self.insert_dictation(&text, now);
            }
        }
    }

    fn insert_dictation(&mut self, text: &str, now: Instant) {
        let text = text.trim();
        if text.is_empty() {
            self.info("heard nothing", now);
            return;
        }
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        if editor.read_only.is_some() {
            return;
        }
        let buffer = &mut editor.buffer;
        let cursor = buffer.cursor();
        let before = buffer.line(cursor.line).get(..cursor.col).unwrap_or("");
        let after = buffer.line(cursor.line).get(cursor.col..).unwrap_or("");
        let mut insert = String::new();
        if before.chars().last().is_some_and(|ch| !ch.is_whitespace()) {
            insert.push(' ');
        }
        insert.push_str(text);
        if after.chars().next().is_some_and(|ch| !ch.is_whitespace()) {
            insert.push(' ');
        }
        buffer.paste_text(&insert);
        editor.follow = true;
        let words = crate::text::word_count(text);
        self.info(format!("dictated {words} words"), now);
    }

    // -- background results ------------------------------------------------------

    pub(super) fn assist_tick(&mut self, now: Instant) {
        for event in self.assist.services.drain() {
            self.service_event(event, now);
        }
        let limit = Duration::from_secs(self.assist.config().dictation.max_seconds.max(5));
        if self
            .assist
            .recording
            .as_ref()
            .is_some_and(|recording| now.saturating_duration_since(recording.started) >= limit)
        {
            self.toggle_dictation(now);
        }
        if let Some(live) = self.assist.live.as_mut() {
            if now.saturating_duration_since(live.started) >= limit {
                live.stop(now);
            }
            // The server never sent its close after a stop: give up.
            if live
                .stopping
                .is_some_and(|at| now.saturating_duration_since(at) >= LIVE_STOP_GRACE)
            {
                if let Some(live) = self.assist.live.take() {
                    live.abort();
                }
                self.assist.live_partial.clear();
                self.warn("dictation ended without its last words", now);
            }
        }
        self.maybe_auto_name(now);
    }

    fn service_event(&mut self, event: ServiceEvent, now: Instant) {
        match event {
            ServiceEvent::Title { job, result } => {
                let Some(naming) = self.assist.naming.take_if(|naming| naming.job == job) else {
                    return;
                };
                let same = self
                    .editor
                    .as_ref()
                    .is_some_and(|editor| editor.path == naming.path);
                match result {
                    Ok(title) if same => self.apply_title(&title, naming.manual, now),
                    Ok(_) => {}
                    Err(err) if naming.manual => self.error(format!("naming failed: {err}"), now),
                    Err(_) => {}
                }
            }
            ServiceEvent::Delta { job, text } => {
                if let Some(panel) = self.assist.panel.as_mut().filter(|p| p.job == Some(job)) {
                    panel.text.push_str(&text);
                }
            }
            ServiceEvent::Done { job, result } => {
                if let Some(panel) = self.assist.panel.as_mut().filter(|p| p.job == Some(job)) {
                    panel.job = None;
                    panel.cancel = None;
                    if let Err(err) = result {
                        panel.error = Some(err);
                    }
                }
            }
            ServiceEvent::Voices(result) => match result {
                Ok(voices) if !voices.is_empty() => {
                    self.assist.voices = voices;
                    if self
                        .assist
                        .menu
                        .as_ref()
                        .is_some_and(|m| m.kind == MenuKind::Read)
                    {
                        if let Ok(items) = self.menu_items(MenuKind::Read) {
                            if let Some(menu) = self.assist.menu.as_mut() {
                                let selected = menu.selected;
                                *menu = Menu::new(MenuKind::Read, items);
                                menu.selected = selected.min(menu.items.len().saturating_sub(1));
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(err) => self.warn(format!("voices: {err}"), now),
            },
            ServiceEvent::Speaking { job, chunk } => {
                if let Some(speaking) = self.assist.speaking.as_mut().filter(|s| s.job == job) {
                    speaking.current = Some(chunk);
                    let lines = speaking.chunks.get(chunk).cloned();
                    let path = speaking.path.clone();
                    if let (Some(lines), Some(editor)) = (lines, self.editor.as_mut()) {
                        if editor.path == path {
                            editor.reveal_lines(lines);
                        }
                    }
                }
            }
            ServiceEvent::SpeechDone { job, result } => {
                if self.assist.speaking.as_ref().is_some_and(|s| s.job == job) {
                    self.assist.speaking = None;
                    match result {
                        Ok(()) => self.info("finished reading", now),
                        Err(err) => self.error(format!("reading failed: {err}"), now),
                    }
                }
            }
            ServiceEvent::Transcript { job, result } => {
                if self.assist.transcribing.is_some_and(|(id, _)| id == job) {
                    self.assist.transcribing = None;
                    match result {
                        Ok(text) => {
                            let config = self.assist.config();
                            let punctuate = config.dictation.punctuate
                                && config.ai_ready()
                                && crate::text::word_count(&text) >= 4;
                            if punctuate {
                                let job = self.assist.services.polish_dictation(&text);
                                self.assist.polishing = Some((job, now));
                            } else {
                                self.insert_dictation(&text, now);
                            }
                        }
                        Err(err) => self.error(format!("dictation failed: {err}"), now),
                    }
                }
            }
            ServiceEvent::Polished { job, raw, result }
                if self.assist.live_queue.iter().any(|u| u.polish == Some(job)) =>
            {
                if let Some(utterance) = self
                    .assist
                    .live_queue
                    .iter_mut()
                    .find(|u| u.polish == Some(job))
                {
                    utterance.text = Some(result.unwrap_or(raw));
                }
                self.flush_live_queue(now);
            }
            ServiceEvent::LivePartial { job, text } => {
                if self
                    .assist
                    .live
                    .as_ref()
                    .is_some_and(|live| live.job == job)
                {
                    self.assist.live_partial = text;
                }
            }
            ServiceEvent::LiveFinal { job, text } => {
                if self
                    .assist
                    .live
                    .as_ref()
                    .is_some_and(|live| live.job == job)
                {
                    self.live_final(&text, now);
                }
            }
            ServiceEvent::LiveEnded { job, result } => {
                if self
                    .assist
                    .live
                    .as_ref()
                    .is_some_and(|live| live.job == job)
                {
                    self.assist.live = None;
                    self.assist.live_partial.clear();
                    if let Err(err) = result {
                        self.error(format!("dictation stopped: {err}"), now);
                    }
                }
            }
            ServiceEvent::Polished { job, raw, result } => {
                if self.assist.polishing.is_some_and(|(id, _)| id == job) {
                    self.assist.polishing = None;
                    match result {
                        Ok(text) => self.insert_dictation(&text, now),
                        Err(err) => {
                            self.insert_dictation(&raw, now);
                            if self
                                .message
                                .as_ref()
                                .is_some_and(|m| m.text.starts_with("dictated"))
                            {
                                let words = crate::text::word_count(&raw);
                                self.info(
                                    format!("dictated {words} words (not punctuated: {err})"),
                                    now,
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// Stops every job and child process (on quit).
    pub fn stop_assist(&mut self) {
        if let Some(speaking) = self.assist.speaking.take() {
            speaking.cancel.cancel();
        }
        if let Some(recording) = self.assist.recording.take() {
            recording.discard();
        }
        if let Some(live) = self.assist.live.take() {
            live.abort();
        }
        if let Some(naming) = self.assist.naming.take() {
            naming.cancel.cancel();
        }
        if let Some(panel) = self.assist.panel.as_mut() {
            if let Some(cancel) = panel.cancel.take() {
                cancel.cancel();
            }
        }
    }
}

fn setup_hint(key: &str) -> String {
    let path =
        Config::path().map_or_else(|| "the config file".to_owned(), |path| display_path(&path));
    format!("not set up: add {key} to {path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn naming_text_skips_front_matter_and_the_title_heading() {
        let lines = ["+++", "title = \"X\"", "+++", "", "# X", "", "body words"];
        assert_eq!(text_for_naming(&lines, "X"), "\nbody words");
        let lines = ["# Other", "body"];
        assert_eq!(text_for_naming(&lines, "X"), "# Other\nbody");
        assert_eq!(content_start(&["+++", "a = 1", "+++"]), 3);
        assert_eq!(content_start(&["+++", "a = 1", "+++", "", ""]), 4);
        assert_eq!(content_start(&["", "text"]), 1);
    }
}
