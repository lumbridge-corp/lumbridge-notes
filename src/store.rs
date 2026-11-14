//! Files on disk: loading, atomic saving, external-change detection, front
//! matter, and the notebook folder (new notes, listing).
//!
//! The note format is shared with Lumbridge TUI's notebook drawer
//! (`src/client/notebook.rs`): Markdown with optional TOML front matter
//! between `+++` lines (`title`, `tags`, `created`), YAML-style `---` front
//! matter tolerated, slug file names, and an `AGENTS.md` guide written once
//! into a new notebook folder. Keep the two compatible.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Notes read per listing; a bigger folder shows its newest ones.
pub const MAX_NOTES: usize = 2000;
/// Bytes of each note kept in a listing for preview and search.
pub const MAX_LISTED_BYTES: usize = 256 * 1024;
const MAX_DEPTH: usize = 3;
/// Written once into a new notebook folder so agents working there know the format.
pub const GUIDE_FILE: &str = "AGENTS.md";
const GUIDE: &str = "# Notebook\n\n\
This folder is a notebook: one Markdown note per `.md` file, read by Lumbridge TUI's\n\
notebook drawer and by agents.\n\n\
- Optional TOML front matter between `+++` lines: `title` (string), `tags`\n  \
(list of strings), `created` (RFC 3339 date-time). Unknown keys are kept.\n\
- Without a `title`, the first `# heading` is the title, else the file name.\n\
- Subfolders are fine (up to three levels). Hidden files and folders are skipped.\n\
- Add to notes freely; keep each note about one thing so it is easy to find.\n\n\
## Templates and AI prompts\n\n\
lumbridge-notes reads these from `.lumbridge/` here; add or edit files to\n\
extend them.\n\n\
- `.lumbridge/templates/*.md`: front matter `title`, `description`; the body is the\n  \
note text, with `{{title}}`, `{{date}}` and `{{time}}` filled in.\n\
- `.lumbridge/prompts/*.md`: front matter `title`, `description` and `action`\n  \
(`replace`, `insert`, `append` or `answer`); the body is the instruction sent\n  \
to the AI together with the note or the selected text.\n";

// ---------------------------------------------------------------------------
// Front matter
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FrontMatter {
    pub title: Option<String>,
    pub tags: Vec<String>,
    pub created: Option<String>,
}

impl FrontMatter {
    /// Parses the text between the fences. `fence` is `+++` (TOML) or `---`
    /// (YAML, read line by line for the keys we use). Bad TOML is an empty
    /// front matter, never an error.
    pub fn parse(header: &str, fence: &str) -> FrontMatter {
        if fence == "+++" {
            toml_front(header)
        } else {
            yaml_front(header)
        }
    }

    /// One line for the collapsed front matter: `title · tag1, tag2`.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(title) = &self.title {
            parts.push(title.clone());
        }
        if !self.tags.is_empty() {
            parts.push(self.tags.join(", "));
        }
        if parts.is_empty() {
            "front matter".to_owned()
        } else {
            parts.join(" · ")
        }
    }
}

fn toml_front(header: &str) -> FrontMatter {
    let Ok(table) = header.parse::<toml::Table>() else {
        return FrontMatter::default();
    };
    let title = table
        .get("title")
        .and_then(|value| value.as_str())
        .map(clean)
        .filter(|title| !title.is_empty());
    let tags = table
        .get("tags")
        .and_then(|value| value.as_array())
        .map(|tags| {
            tags.iter()
                .filter_map(|tag| tag.as_str())
                .map(clean)
                .filter(|tag| !tag.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let created = table.get("created").and_then(|value| match value {
        toml::Value::String(text) => Some(clean(text)),
        toml::Value::Datetime(datetime) => Some(datetime.to_string()),
        _ => None,
    });
    FrontMatter {
        title,
        tags,
        created,
    }
}

fn yaml_front(header: &str) -> FrontMatter {
    let unquote = |value: &str| clean(value.trim().trim_matches(['"', '\'']));
    let mut front = FrontMatter::default();
    for line in header.lines() {
        if let Some(value) = line.strip_prefix("title:") {
            front.title = Some(unquote(value)).filter(|title| !title.is_empty());
        } else if let Some(value) = line.strip_prefix("tags:") {
            front.tags = value
                .trim()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .split(',')
                .map(unquote)
                .filter(|tag| !tag.is_empty())
                .collect();
        } else if let Some(value) = line.strip_prefix("created:") {
            front.created = Some(unquote(value)).filter(|created| !created.is_empty());
        }
    }
    front
}

/// Where the front matter sits in a document split into lines (without line
/// endings): the fence and the index of the closing fence line. Line 0 is the
/// opening fence (a leading BOM is ignored).
///
/// A `---` block also needs to read as YAML (empty, or with a `key:` line):
/// otherwise it is a thematic break and the text up to the next `---` rule is
/// ordinary Markdown, which the editor must not fold away.
pub fn front_matter_lines<S: AsRef<str>>(lines: &[S]) -> Option<(&'static str, usize)> {
    let first = lines.first()?.as_ref();
    let first = first.strip_prefix('\u{feff}').unwrap_or(first);
    let fence = match first.trim_end_matches('\r') {
        "+++" => "+++",
        "---" => "---",
        _ => return None,
    };
    let end = lines
        .iter()
        .skip(1)
        .position(|line| line.as_ref().trim_end_matches('\r') == fence)?
        + 1;
    if fence == "---" && !yaml_block(lines[1..end].iter().map(AsRef::as_ref)) {
        return None;
    }
    Some((fence, end))
}

/// Whether the lines between `---` fences read as YAML front matter: all
/// blank, or at least one `key:` line.
fn yaml_block<'a>(mut lines: impl Iterator<Item = &'a str> + Clone) -> bool {
    lines.clone().all(|line| line.trim().is_empty()) || lines.any(yaml_key_line)
}

/// `key:` or `key: value` at the start of a line (`key` is a plain word).
fn yaml_key_line(line: &str) -> bool {
    let Some((key, rest)) = line.split_once(':') else {
        return false;
    };
    let plain = |ch: char| ch.is_alphanumeric() || matches!(ch, '_' | '-' | ' ' | '.');
    key.starts_with(|ch: char| ch.is_alphanumeric() || ch == '_')
        && key.chars().all(plain)
        && (rest.is_empty() || rest.starts_with([' ', '\t', '\r']))
}

/// Splits `text` into front matter and body (same rules as the drawer).
pub fn parse_note(text: &str) -> (FrontMatter, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for fence in ["+++", "---"] {
        let Some(rest) = text
            .strip_prefix(fence)
            .and_then(|rest| rest.strip_prefix('\n').or(rest.strip_prefix("\r\n")))
        else {
            continue;
        };
        let Some((header, body)) = split_at_fence(rest, fence) else {
            continue;
        };
        if fence == "---" && !yaml_block(header.lines()) {
            continue;
        }
        return (FrontMatter::parse(header, fence), body);
    }
    (FrontMatter::default(), text)
}

fn split_at_fence<'a>(text: &'a str, fence: &str) -> Option<(&'a str, &'a str)> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == fence {
            return Some((&text[..offset], &text[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

/// The first `# heading` when it is the first non-empty line of `body`.
pub fn heading_title(body: &str) -> Option<String> {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .and_then(|line| line.strip_prefix("# "))
        .map(|title| clean(title.trim()))
        .filter(|title| !title.is_empty())
}

/// A note's title: front matter title, else first heading, else file stem.
pub fn note_title(text: &str, path: &Path) -> String {
    let (front, body) = parse_note(text);
    front
        .title
        .or_else(|| heading_title(body))
        .unwrap_or_else(|| {
            path.file_stem()
                .map(|stem| clean(&stem.to_string_lossy()))
                .unwrap_or_default()
        })
}

/// Drops control characters (a title or tag must not move the terminal cursor).
pub fn clean(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_control()).collect()
}

// ---------------------------------------------------------------------------
// New notes
// ---------------------------------------------------------------------------

/// File name for a new note: lowercase words joined by `-`.
pub fn slug(title: &str) -> String {
    let mut slug = String::new();
    for ch in title.chars() {
        if ch.is_alphanumeric() {
            slug.extend(ch.to_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= 60 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "note".to_owned()
    } else {
        slug.to_owned()
    }
}

/// The text of a new note (identical to the drawer's template).
pub fn new_note_text(title: &str, created: &str) -> String {
    let quoted = toml::Value::String(title.to_owned()).to_string();
    format!("+++\ntitle = {quoted}\ntags = []\ncreated = {created}\n+++\n\n# {title}\n\n")
}

/// `time` as an RFC 3339 UTC timestamp with whole seconds, e.g.
/// `2026-10-16T18:40:00Z`.
pub fn rfc3339(time: SystemTime) -> String {
    let secs = match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs() as i64,
        Err(err) => -(err.duration().as_secs() as i64),
    };
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// The calendar date of `time` in the local time zone, e.g. `2026-10-16`
/// (for lists; stored timestamps stay UTC).
pub fn local_date(time: SystemTime) -> String {
    let secs = match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs() as i64,
        Err(err) => -(err.duration().as_secs() as i64),
    };
    let local = secs + utc_offset_secs(secs);
    let (year, month, day) = civil_from_days(local.div_euclid(86_400));
    format!("{year:04}-{month:02}-{day:02}")
}

/// The wall-clock time of `time` in the local time zone, e.g. `19:05`.
pub fn local_time(time: SystemTime) -> String {
    let secs = match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs() as i64,
        Err(err) => -(err.duration().as_secs() as i64),
    };
    let local = (secs + utc_offset_secs(secs)).rem_euclid(86_400);
    format!("{:02}:{:02}", local / 3600, (local % 3600) / 60)
}

/// The local time zone's offset from UTC at `secs` (DST aware), 0 when it
/// cannot be determined.
#[cfg(unix)]
fn utc_offset_secs(secs: i64) -> i64 {
    let Some(time) = libc::time_t::try_from(secs).ok() else {
        return 0;
    };
    // SAFETY: `tm` is plain old data, so all-zero is a valid value, and
    // `localtime_r` only writes into the `tm` we pass (it is the reentrant
    // form; both pointers are valid for the call).
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::localtime_r(&time, &mut tm) };
    if result.is_null() {
        0
    } else {
        // `tm_gmtoff` is a C long: i64 here, i32 on 32-bit targets.
        #[allow(clippy::useless_conversion)]
        i64::from(tm.tm_gmtoff)
    }
}

#[cfg(not(unix))]
fn utc_offset_secs(_secs: i64) -> i64 {
    0
}

/// Howard Hinnant's days-since-epoch to proleptic Gregorian date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Creates the folder if needed and a new note in it; never overwrites an
/// existing file. Returns the new file's path.
///
/// The agent guide (`AGENTS.md`) is written only into a notebook folder:
/// one this call created, or one the caller knows is the configured notebook
/// (`notebook`). An existing folder that merely holds the edited file (a code
/// repository, say) never gets one.
pub fn create_note(
    dir: &Path,
    title: &str,
    now: SystemTime,
    notebook: bool,
) -> io::Result<PathBuf> {
    let existed = dir.is_dir();
    fs::create_dir_all(dir)?;
    let guide = dir.join(GUIDE_FILE);
    if (notebook || !existed) && !guide.exists() {
        fs::write(&guide, GUIDE)?;
    }
    let title = clean(title.trim());
    let title = if title.is_empty() {
        "untitled".to_owned()
    } else {
        title
    };
    let created = rfc3339(now);
    let base = slug(&title);
    for attempt in 0..1000 {
        let name = if attempt == 0 {
            format!("{base}.md")
        } else {
            format!("{base}-{}.md", attempt + 1)
        };
        let path = dir.join(name);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(new_note_text(&title, &created).as_bytes())?;
                return Ok(path);
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::other("no free note file name"))
}

// ---------------------------------------------------------------------------
// Notebook folder
// ---------------------------------------------------------------------------

/// `$LUMBRIDGE_NOTES_DIR` (else the same variable under an old prefix),
/// else `~/notes`.
pub fn default_notes_dir() -> PathBuf {
    notes_dir_from(crate::naming::env_var("DIR").map(PathBuf::from), home_dir())
}

fn notes_dir_from(env_dir: Option<PathBuf>, home: Option<PathBuf>) -> PathBuf {
    match env_dir.filter(|dir| !dir.as_os_str().is_empty()) {
        Some(dir) => dir,
        None => home.unwrap_or_else(|| PathBuf::from(".")).join("notes"),
    }
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|home| !home.as_os_str().is_empty())
}

/// The notebook root used by the picker and search when editing `file`:
/// `explicit` (`--dir`) if given, else the default notebook folder when the
/// file is inside it, else the file's parent folder.
pub fn notebook_root_for(file: &Path, explicit: Option<&Path>, default_dir: &Path) -> PathBuf {
    if let Some(dir) = explicit {
        return dir.to_path_buf();
    }
    let absolute = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let file_abs = match file.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => absolute(parent),
        _ => absolute(Path::new(".")),
    };
    let default_abs = absolute(default_dir);
    if file_abs.starts_with(&default_abs) {
        default_dir.to_path_buf()
    } else {
        match file.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteEntry {
    pub path: PathBuf,
    /// Path relative to the notebook folder.
    pub relative: String,
    pub title: String,
    pub tags: Vec<String>,
    pub modified: SystemTime,
    /// The whole note (front matter included), capped at `MAX_LISTED_BYTES`.
    pub text: String,
    /// `text` was cut at `MAX_LISTED_BYTES`.
    pub truncated: bool,
    /// Byte offset in `text` where the body starts (after front matter).
    pub body_start: usize,
}

impl NoteEntry {
    pub fn body(&self) -> &str {
        self.text.get(self.body_start..).unwrap_or("")
    }
}

fn read_entry(root: &Path, path: &Path) -> Option<NoteEntry> {
    let metadata = fs::metadata(path).ok()?;
    let mut bytes = fs::read(path).ok()?;
    let truncated = bytes.len() > MAX_LISTED_BYTES;
    if truncated {
        bytes.truncate(MAX_LISTED_BYTES);
        // Do not cut a UTF-8 sequence in half.
        if let Err(err) = std::str::from_utf8(&bytes) {
            if err.error_len().is_none() {
                bytes.truncate(err.valid_up_to());
            }
        }
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (front, body) = parse_note(&text);
    let body_start = text.len() - body.len();
    let relative = clean(&path.strip_prefix(root).unwrap_or(path).to_string_lossy());
    let stem = path
        .file_stem()
        .map(|stem| clean(&stem.to_string_lossy()))
        .unwrap_or_default();
    let title = front.title.or_else(|| heading_title(body)).unwrap_or(stem);
    Some(NoteEntry {
        path: path.to_path_buf(),
        relative,
        title,
        tags: front.tags,
        modified: metadata.modified().unwrap_or(UNIX_EPOCH),
        text,
        truncated,
        body_start,
    })
}

fn collect(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || (depth == 0 && name == GUIDE_FILE) {
            continue;
        }
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() && depth + 1 < MAX_DEPTH {
            collect(&path, depth + 1, out);
        } else if kind.is_file() && name.ends_with(".md") {
            out.push(path);
        }
    }
}

/// Every note in `dir`, newest first. A missing folder is an empty notebook.
pub fn list_notes(dir: &Path) -> Vec<NoteEntry> {
    let mut paths = Vec::new();
    collect(dir, 0, &mut paths);
    let mut notes: Vec<NoteEntry> = paths
        .iter()
        .filter_map(|path| read_entry(dir, path))
        .collect();
    notes.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.title.cmp(&b.title)));
    notes.truncate(MAX_NOTES);
    notes
}

// ---------------------------------------------------------------------------
// Load / save
// ---------------------------------------------------------------------------

/// What we knew about the file when we last read or wrote it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub modified: Option<SystemTime>,
    pub len: u64,
    /// Hash of the bytes, so a touched-but-identical file is not "changed".
    pub hash: u64,
}

impl Stamp {
    fn of(metadata: &fs::Metadata, bytes: &[u8]) -> Stamp {
        Stamp {
            modified: metadata.modified().ok(),
            len: metadata.len(),
            hash: hash_bytes(bytes),
        }
    }

    fn same_metadata(&self, metadata: &fs::Metadata) -> bool {
        self.len == metadata.len() && self.modified == metadata.modified().ok()
    }
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    pub text: String,
    /// `None` when the file does not exist yet (it is created on first save).
    pub stamp: Option<Stamp>,
    /// Set when the file must never be written (it is not UTF-8, or it is
    /// read-only); the reason is shown to the user. `text` is then a lossy
    /// rendering when the file is not UTF-8.
    pub read_only: Option<String>,
}

/// The read-only reason shown for a file that is not UTF-8.
pub const NOT_UTF8: &str = "not UTF-8: opened read-only";

/// Reads `path`. A missing file is an empty, writable note.
pub fn load(path: &Path) -> io::Result<Loaded> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(Loaded {
                text: String::new(),
                stamp: None,
                read_only: None,
            })
        }
        Err(err) => return Err(err),
    };
    let metadata = fs::metadata(path)?;
    if metadata.is_dir() {
        return Err(io::Error::other("is a directory"));
    }
    let stamp = Some(Stamp::of(&metadata, &bytes));
    let read_only = read_only_reason(&metadata);
    match String::from_utf8(bytes) {
        Ok(text) => Ok(Loaded {
            text,
            stamp,
            read_only,
        }),
        Err(err) => Ok(Loaded {
            text: String::from_utf8_lossy(err.as_bytes()).into_owned(),
            stamp,
            read_only: Some(NOT_UTF8.to_owned()),
        }),
    }
}

/// Why a file with these permissions must not be written, if so.
fn read_only_reason(metadata: &fs::Metadata) -> Option<String> {
    metadata
        .permissions()
        .readonly()
        .then(|| "read-only file: opened read-only".to_owned())
}

/// Whether `path` is now a writable file (it was fixed or made writable
/// on disk); used to lift a read-only state after a reload.
pub fn writable_now(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| read_only_reason(&metadata).is_none())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskState {
    /// Same as our stamp.
    Unchanged,
    /// Touched with identical content: only the metadata moved. Holds the
    /// fresh stamp, so the next check matches on metadata alone again.
    Touched(Stamp),
    /// Someone else changed it; holds the new text and stamp. `text` is
    /// `None` when the new content is not UTF-8.
    Changed { text: Option<String>, stamp: Stamp },
    /// It existed when we read it and is gone now.
    Deleted,
    /// It did not exist and still does not.
    Missing,
}

/// Compares the file with what we last read or wrote (`stamp`). Cheap when
/// nothing changed: only metadata is read.
pub fn disk_state(path: &Path, stamp: Option<&Stamp>) -> io::Result<DiskState> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(if stamp.is_some() {
                DiskState::Deleted
            } else {
                DiskState::Missing
            })
        }
        Err(err) => return Err(err),
    };
    if let Some(stamp) = stamp {
        if stamp.same_metadata(&metadata) {
            return Ok(DiskState::Unchanged);
        }
    }
    let bytes = fs::read(path)?;
    let new = Stamp::of(&metadata, &bytes);
    if let Some(stamp) = stamp {
        if stamp.hash == new.hash && stamp.len == new.len {
            return Ok(DiskState::Touched(new));
        }
    }
    Ok(DiskState::Changed {
        text: String::from_utf8(bytes).ok(),
        stamp: new,
    })
}

/// How careful a save is about what happened on disk since the last read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavePolicy {
    /// Refuse when the file changed or was deleted on disk (autosave, quit,
    /// switching notes).
    Safe,
    /// As `Safe`, but writes a deleted file again (an explicit Ctrl-S).
    Recreate,
    /// Overwrite whatever is on disk (Ctrl-S twice after a conflict), unless
    /// the file there is not UTF-8: that is never overwritten.
    Force,
}

#[derive(Debug)]
pub enum SaveError {
    /// The file changed on disk since `expected`; nothing was written.
    /// Holds the stamp of what is there now.
    Conflict(Option<Stamp>),
    /// The file was deleted on disk; nothing was written.
    Deleted,
    /// The file on disk is not UTF-8 (any more); nothing was written.
    NotUtf8,
    Io(io::Error),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::Conflict(_) => write!(f, "file changed on disk"),
            SaveError::Deleted => write!(f, "file deleted on disk"),
            SaveError::NotUtf8 => write!(f, "file on disk is not UTF-8"),
            SaveError::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for SaveError {}

impl From<io::Error> for SaveError {
    fn from(err: io::Error) -> Self {
        SaveError::Io(err)
    }
}

/// Saves `text` to `path` atomically, checking the disk against `expected`
/// as `policy` says. Returns the new stamp.
pub fn save(
    path: &Path,
    text: &str,
    expected: Option<&Stamp>,
    policy: SavePolicy,
) -> Result<Stamp, SaveError> {
    if policy == SavePolicy::Force {
        match fs::read(path) {
            Ok(bytes) if std::str::from_utf8(&bytes).is_err() => return Err(SaveError::NotUtf8),
            _ => {}
        }
    } else {
        match disk_state(path, expected)? {
            DiskState::Unchanged | DiskState::Touched(_) | DiskState::Missing => {}
            DiskState::Deleted => {
                if policy == SavePolicy::Safe {
                    return Err(SaveError::Deleted);
                }
            }
            DiskState::Changed { text: None, .. } => return Err(SaveError::NotUtf8),
            DiskState::Changed { stamp, .. } => return Err(SaveError::Conflict(Some(stamp))),
        }
    }
    let target = write_atomic(path, text.as_bytes())?;
    let metadata = fs::metadata(&target)?;
    Ok(Stamp::of(&metadata, text.as_bytes()))
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
/// Bytes of the note's name kept in its temp file's name (the whole temp
/// name must stay under the usual 255-byte limit).
const TEMP_NAME_KEEP: usize = 100;

/// Writes `bytes` to `path` via a temp file in the same folder, fsync and
/// rename, keeping the original file's permissions (and, on Unix, its owner
/// and group). A symlink is followed, even a dangling one, so the link itself
/// survives. A file without write permission is refused. On Unix a file with
/// several hard links, or whose owner cannot be kept, is rewritten in place
/// instead, so the other names and the owner stay intact. Returns the path
/// actually written.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    let target = resolve_target(path)?;
    let dir = match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "note".to_owned());
    let original = fs::metadata(&target).ok();
    if original.is_some() {
        // Renaming over a file only needs the folder to be writable; do not
        // let that bypass the file's own permissions.
        if let Err(err) = fs::OpenOptions::new().write(true).open(&target) {
            if err.kind() == io::ErrorKind::PermissionDenied {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "read-only file (no write permission)",
                ));
            }
        }
    }
    #[cfg(unix)]
    if original.as_ref().is_some_and(unix::shared_inode) {
        write_in_place(&target, bytes)?;
        return Ok(target);
    }

    let short = truncate_at_char(&name, TEMP_NAME_KEEP);
    let mut temp = None;
    for _ in 0..100 {
        let candidate = dir.join(format!(
            ".{short}.{}-{}-{}.tmp",
            crate::naming::APP,
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        match create_temp(&candidate, original.is_some()) {
            Ok(file) => {
                temp = Some((candidate, file));
                break;
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    let Some((temp_path, file)) = temp else {
        return Err(io::Error::other("no free temp file name"));
    };
    let result = finish_temp(file, &temp_path, original.as_ref(), bytes);
    match result {
        Ok(TempOutcome::Ready) => {
            let renamed = fs::rename(&temp_path, &target);
            if renamed.is_err() {
                let _ = fs::remove_file(&temp_path);
            }
            renamed?;
            sync_dir(&dir);
            Ok(target)
        }
        Ok(TempOutcome::KeepOwner) => {
            // The temp file could not take the original's owner: rewrite the
            // original in place rather than hand the note to another user.
            let _ = fs::remove_file(&temp_path);
            write_in_place(&target, bytes)?;
            Ok(target)
        }
        Err(err) => {
            let _ = fs::remove_file(&temp_path);
            Err(err)
        }
    }
}

enum TempOutcome {
    /// Written and synced; rename it over the target.
    Ready,
    /// The original's owner cannot be kept; write in place instead.
    KeepOwner,
}

/// Gives the temp file the original's mode (before any bytes are in it) and
/// owner, then writes and syncs it.
fn finish_temp(
    mut file: fs::File,
    temp_path: &Path,
    original: Option<&fs::Metadata>,
    bytes: &[u8],
) -> io::Result<TempOutcome> {
    if let Some(original) = original {
        file.set_permissions(original.permissions())?;
        #[cfg(unix)]
        if !unix::keep_owner(&file, temp_path, original) {
            return Ok(TempOutcome::KeepOwner);
        }
    }
    #[cfg(not(unix))]
    let _ = temp_path;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(TempOutcome::Ready)
}

/// Creates the temp file. When it replaces an existing note (`private`), it
/// starts readable by its owner only (Unix) and gets the note's own mode
/// before any text is written, so a private note is never briefly readable
/// by others. A new note gets the usual mode (umask applies).
fn create_temp(path: &Path, private: bool) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    options.open(path)
}

/// Overwrites `target` in place (truncate, write, fsync). Not atomic, but
/// keeps the inode: its owner, hard links, ACLs and extended attributes.
fn write_in_place(target: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(target)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// The file a save should write: `path` with symlinks resolved. A dangling
/// symlink resolves to the path it points at (created by the save).
fn resolve_target(path: &Path) -> io::Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(real) => return Ok(real),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let mut current = path.to_path_buf();
    for _ in 0..40 {
        match fs::read_link(&current) {
            Ok(next) => {
                current = match current.parent() {
                    Some(parent) if next.is_relative() => parent.join(next),
                    _ => next,
                };
            }
            Err(_) => return Ok(current),
        }
    }
    Err(io::Error::other("too many levels of symbolic links"))
}

/// The longest prefix of `text` of at most `max` bytes, on a char boundary.
fn truncate_at_char(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    /// The file has other hard links that a rename would split off.
    pub fn shared_inode(metadata: &fs::Metadata) -> bool {
        metadata.nlink() > 1
    }

    /// Gives `file` the owner and group of `original`; false when that is
    /// not allowed (only root may give a file away).
    pub fn keep_owner(file: &fs::File, path: &Path, original: &fs::Metadata) -> bool {
        let Ok(mine) = fs::metadata(path) else {
            return false;
        };
        if mine.uid() == original.uid() && mine.gid() == original.gid() {
            return true;
        }
        std::os::unix::fs::fchown(file, Some(original.uid()), Some(original.gid())).is_ok()
    }
}

/// Makes the rename durable. Best effort: not every platform can open a folder.
#[cfg(unix)]
fn sync_dir(dir: &Path) {
    if let Ok(handle) = fs::File::open(dir) {
        let _ = handle.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let path = std::env::temp_dir().join(format!(
                "lumbridge-notes-test-{tag}-{}-{}",
                std::process::id(),
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn parses_toml_yaml_and_bare_front_matter() {
        let (front, body) = parse_note(
            "+++\ntitle = \"desk down\"\ntags = [\"fleet\", \"desk\"]\ncreated = 2026-10-16T18:40:00Z\n+++\nbody\n",
        );
        assert_eq!(front.title.as_deref(), Some("desk down"));
        assert_eq!(front.tags, vec!["fleet", "desk"]);
        assert_eq!(front.created.as_deref(), Some("2026-10-16T18:40:00Z"));
        assert_eq!(body, "body\n");

        let (front, body) = parse_note("---\ntitle: 'Y'\ntags: [a, \"b\"]\n---\r\nrest");
        assert_eq!(front.title.as_deref(), Some("Y"));
        assert_eq!(front.tags, vec!["a", "b"]);
        assert_eq!(body, "rest");

        let (front, body) = parse_note("# Just a heading\n");
        assert_eq!(front, FrontMatter::default());
        assert_eq!(body, "# Just a heading\n");

        // Unclosed or broken front matter.
        let (front, body) = parse_note("+++\ntitle = \n");
        assert_eq!(front, FrontMatter::default());
        assert_eq!(body, "+++\ntitle = \n");
        let (front, _) = parse_note("+++\ntitle = = bad\n+++\n");
        assert_eq!(front, FrontMatter::default());
    }

    #[test]
    fn front_matter_lines_and_summary() {
        let lines = ["+++", "title = \"T\"", "+++", "# x"];
        assert_eq!(front_matter_lines(&lines), Some(("+++", 2)));
        assert_eq!(
            front_matter_lines(&["\u{feff}---", "a: b", "---"]),
            Some(("---", 2))
        );
        assert_eq!(front_matter_lines(&["+++", "title"]), None);
        assert_eq!(front_matter_lines(&["text"]), None);
        assert_eq!(front_matter_lines::<&str>(&[]), None);
        let front = FrontMatter::parse("title = \"T\"\ntags = [\"a\", \"b\"]", "+++");
        assert_eq!(front.summary(), "T · a, b");
        assert_eq!(FrontMatter::default().summary(), "front matter");
    }

    #[test]
    fn titles_fall_back_to_heading_then_stem() {
        let path = Path::new("/n/my-note.md");
        assert_eq!(note_title("+++\ntitle = \"A\"\n+++\n# B\n", path), "A");
        assert_eq!(note_title("\n\n# B\ntext", path), "B");
        assert_eq!(note_title("text\n# B", path), "my-note");
        assert_eq!(note_title("# evil\u{1b}[2J\n", path), "evil[2J");
    }

    #[test]
    fn slug_template_and_timestamps() {
        assert_eq!(slug("Desk :8080 down!"), "desk-8080-down");
        assert_eq!(slug("   "), "note");
        assert_eq!(slug("Ünïcode Title"), "ünïcode-title");
        assert_eq!(
            new_note_text("Say \"hi\"", "2026-10-16T18:40:00Z"),
            "+++\ntitle = 'Say \"hi\"'\ntags = []\ncreated = 2026-10-16T18:40:00Z\n+++\n\n# Say \"hi\"\n\n"
        );
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(1_792_175_400)),
            "2026-10-16T18:30:00Z"
        );
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00Z"
        );
        // The template's front matter round-trips through the parser.
        let text = new_note_text("T", "2026-10-16T18:40:00Z");
        let (front, body) = parse_note(&text);
        assert_eq!(front.title.as_deref(), Some("T"));
        assert_eq!(front.created.as_deref(), Some("2026-10-16T18:40:00Z"));
        assert_eq!(body, "\n# T\n\n");
    }

    #[test]
    fn creates_notes_without_overwriting_and_lists_newest_first() {
        let dir = TempDir::new("create");
        let root = dir.0.join("notebook");
        let now = UNIX_EPOCH + Duration::from_secs(1_792_175_400);
        let first = create_note(&root, "Hello World", now, false).unwrap();
        let second = create_note(&root, "Hello World", now, false).unwrap();
        assert_eq!(first.file_name().unwrap(), "hello-world.md");
        assert_eq!(second.file_name().unwrap(), "hello-world-2.md");
        assert!(root.join(GUIDE_FILE).exists());
        let blank = create_note(&root, "  ", now, false).unwrap();
        assert!(fs::read_to_string(&blank)
            .unwrap()
            .contains("title = \"untitled\""));

        fs::create_dir_all(root.join("sub/.hidden")).unwrap();
        fs::write(root.join("sub/deep.md"), "# Deep\n").unwrap();
        fs::write(root.join("sub/.hidden/x.md"), "# Hidden\n").unwrap();
        fs::write(root.join("not-a-note.txt"), "x").unwrap();
        // Recovery and temp files are never listed as notes.
        for scratch in [
            "hello-world.md.lumbridge-notes-recovery",
            ".hello-world.md.lumbridge-notes-1-0.tmp",
        ] {
            fs::write(root.join(scratch), "# Scratch\n").unwrap();
        }
        let notes = list_notes(&root);
        let titles: Vec<&str> = notes.iter().map(|note| note.title.as_str()).collect();
        assert_eq!(notes.len(), 4, "{titles:?}");
        assert!(titles.contains(&"Deep"));
        assert!(!titles.contains(&"Hidden"));
        let deep = notes.iter().find(|note| note.title == "Deep").unwrap();
        assert_eq!(
            deep.relative,
            Path::new("sub").join("deep.md").to_string_lossy()
        );
        let hello = notes.iter().find(|note| note.path == first).unwrap();
        assert_eq!(hello.body(), "\n# Hello World\n\n");
        assert!(list_notes(&dir.0.join("missing")).is_empty());
    }

    #[test]
    fn notebook_dir_resolution() {
        assert_eq!(
            notes_dir_from(Some(PathBuf::from("/x")), Some(PathBuf::from("/h"))),
            PathBuf::from("/x")
        );
        assert_eq!(
            notes_dir_from(Some(PathBuf::new()), Some(PathBuf::from("/h"))),
            PathBuf::from("/h/notes")
        );
        assert_eq!(
            notes_dir_from(None, Some(PathBuf::from("/h"))),
            PathBuf::from("/h/notes")
        );

        let dir = TempDir::new("root");
        let notebook = dir.0.join("notes");
        fs::create_dir_all(notebook.join("sub")).unwrap();
        let elsewhere = dir.0.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        assert_eq!(
            notebook_root_for(&notebook.join("sub/a.md"), None, &notebook),
            notebook
        );
        assert_eq!(
            notebook_root_for(&elsewhere.join("a.md"), None, &notebook),
            elsewhere
        );
        assert_eq!(
            notebook_root_for(&elsewhere.join("a.md"), Some(&dir.0), &notebook),
            dir.0
        );
    }

    #[test]
    fn load_missing_utf8_and_non_utf8() {
        let dir = TempDir::new("load");
        let missing = load(&dir.0.join("nope.md")).unwrap();
        assert_eq!(missing.text, "");
        assert!(missing.stamp.is_none());
        assert!(missing.read_only.is_none());

        let good = dir.0.join("good.md");
        fs::write(&good, "héllo\r\n").unwrap();
        let loaded = load(&good).unwrap();
        assert_eq!(loaded.text, "héllo\r\n");
        assert!(loaded.read_only.is_none());

        let bad = dir.0.join("bad.md");
        fs::write(&bad, [b'a', 0xff, b'b']).unwrap();
        let loaded = load(&bad).unwrap();
        assert!(loaded.read_only.is_some());
        assert_eq!(loaded.text, "a\u{fffd}b");
        assert!(load(&dir.0).is_err());
    }

    #[test]
    fn atomic_save_keeps_permissions_and_leaves_no_temp_files() {
        let dir = TempDir::new("save");
        let path = dir.0.join("note.md");
        // Creating a new file.
        let stamp = save(&path, "one\n", None, SavePolicy::Safe).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\n");
        assert_eq!(stamp.len, 4);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            let stamp = load(&path).unwrap().stamp;
            save(&path, "two\n", stamp.as_ref(), SavePolicy::Safe).unwrap();
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let leftovers: Vec<_> = fs::read_dir(&dir.0)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        // Missing parent folder is an error, not a panic.
        assert!(save(&dir.0.join("no/such/dir.md"), "x", None, SavePolicy::Safe).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_save_follows_symlinks() {
        let dir = TempDir::new("link");
        let real = dir.0.join("real.md");
        fs::write(&real, "a").unwrap();
        let link = dir.0.join("link.md");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_atomic(&link, b"b").unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&real).unwrap(), "b");
    }

    #[test]
    fn detects_external_changes_and_refuses_to_clobber() {
        let dir = TempDir::new("external");
        let path = dir.0.join("note.md");
        let stamp = save(&path, "mine\n", None, SavePolicy::Safe).unwrap();
        assert_eq!(
            disk_state(&path, Some(&stamp)).unwrap(),
            DiskState::Unchanged
        );

        // Same bytes rewritten (mtime may change): not a change, and the
        // fresh stamp makes the next check a metadata-only one again.
        fs::write(&path, "mine\n").unwrap();
        let stamp = match disk_state(&path, Some(&stamp)).unwrap() {
            DiskState::Unchanged => stamp,
            DiskState::Touched(fresh) => {
                assert_eq!(fresh.hash, stamp.hash);
                assert_eq!(
                    disk_state(&path, Some(&fresh)).unwrap(),
                    DiskState::Unchanged
                );
                fresh
            }
            other => panic!("expected unchanged, got {other:?}"),
        };

        fs::write(&path, "theirs, longer\n").unwrap();
        let DiskState::Changed {
            text,
            stamp: theirs,
        } = disk_state(&path, Some(&stamp)).unwrap()
        else {
            panic!("expected a change");
        };
        assert_eq!(text.as_deref(), Some("theirs, longer\n"));
        assert!(matches!(
            save(&path, "mine again\n", Some(&stamp), SavePolicy::Safe),
            Err(SaveError::Conflict(Some(now))) if now == theirs
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), "theirs, longer\n");
        // With the new stamp (after a reload) or with force, it saves.
        save(&path, "merged\n", Some(&theirs), SavePolicy::Safe).unwrap();
        fs::write(&path, "x").unwrap();
        save(&path, "forced\n", Some(&theirs), SavePolicy::Force).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "forced\n");

        fs::remove_file(&path).unwrap();
        assert_eq!(
            disk_state(&path, Some(&theirs)).unwrap(),
            DiskState::Deleted
        );
        assert_eq!(disk_state(&path, None).unwrap(), DiskState::Missing);
        // A deleted note is only written again on request.
        assert!(matches!(
            save(&path, "back\n", Some(&theirs), SavePolicy::Safe),
            Err(SaveError::Deleted)
        ));
        assert!(!path.exists());
        save(&path, "back\n", Some(&theirs), SavePolicy::Recreate).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "back\n");
    }

    #[test]
    fn a_file_that_turned_non_utf8_is_never_overwritten() {
        let dir = TempDir::new("utf8");
        let path = dir.0.join("note.md");
        let stamp = save(&path, "hello\n", None, SavePolicy::Safe).unwrap();
        fs::write(&path, b"x\xffy\nz").unwrap();
        for policy in [SavePolicy::Safe, SavePolicy::Recreate, SavePolicy::Force] {
            assert!(matches!(
                save(&path, "hello\n", Some(&stamp), policy),
                Err(SaveError::NotUtf8)
            ));
        }
        assert_eq!(fs::read(&path).unwrap(), b"x\xffy\nz");
    }

    #[test]
    fn yaml_fences_need_yaml_inside() {
        // A note that opens with a thematic break is not front matter.
        let lines = [
            "---",
            "",
            "# Real heading",
            "",
            "body",
            "",
            "---",
            "",
            "end",
        ];
        assert_eq!(front_matter_lines(&lines), None);
        let (front, body) = parse_note("---\n\n# Real heading\n\n---\nend");
        assert_eq!(front, FrontMatter::default());
        assert!(body.starts_with("---\n"));
        // Real YAML front matter, an empty block, and TOML are.
        assert_eq!(
            front_matter_lines(&["---", "title: x", "tags: [a]", "---"]),
            Some(("---", 3))
        );
        assert_eq!(
            front_matter_lines(&["---", "---", "text"]),
            Some(("---", 1))
        );
        assert_eq!(
            front_matter_lines(&["+++", "# toml comment", "+++"]),
            Some(("+++", 2))
        );
        // A URL is not a key.
        assert_eq!(front_matter_lines(&["---", "http://x", "---"]), None);
    }

    #[cfg(unix)]
    #[test]
    fn read_only_files_are_refused_not_replaced() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("ro");
        let path = dir.0.join("note.md");
        fs::write(&path, "old\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        let loaded = load(&path).unwrap();
        assert!(loaded.read_only.is_some(), "a 0444 note opens read-only");
        let err = save(&path, "new\n", loaded.stamp.as_ref(), SavePolicy::Safe).unwrap_err();
        assert!(err.to_string().contains("read-only"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "old\n");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(writable_now(&path));
    }

    #[cfg(unix)]
    #[test]
    fn hard_links_stay_linked_and_dangling_symlinks_survive() {
        use std::os::unix::fs::MetadataExt;
        let dir = TempDir::new("links");
        let a = dir.0.join("a.md");
        let b = dir.0.join("b.md");
        fs::write(&a, "old").unwrap();
        fs::hard_link(&a, &b).unwrap();
        write_atomic(&a, b"new").unwrap();
        assert_eq!(fs::read_to_string(&b).unwrap(), "new");
        assert_eq!(fs::metadata(&a).unwrap().nlink(), 2);

        let link = dir.0.join("link.md");
        std::os::unix::fs::symlink("target-missing.md", &link).unwrap();
        write_atomic(&link, b"hi").unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_to_string(dir.0.join("target-missing.md")).unwrap(),
            "hi"
        );
    }

    #[test]
    fn long_file_names_still_save() {
        let dir = TempDir::new("long");
        let name = format!("{}.md", "日".repeat(82));
        assert!(name.len() > 240 && name.len() <= 255);
        let path = dir.0.join(name);
        fs::write(&path, "a").unwrap();
        let stamp = load(&path).unwrap().stamp;
        save(&path, "b", stamp.as_ref(), SavePolicy::Safe).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "b");
    }

    #[test]
    fn the_guide_goes_only_into_notebook_folders() {
        let dir = TempDir::new("guide");
        let now = UNIX_EPOCH;
        // An existing folder that is not the notebook (a repository, say).
        let repo = dir.0.join("repo");
        fs::create_dir_all(&repo).unwrap();
        create_note(&repo, "todo", now, false).unwrap();
        assert!(!repo.join(GUIDE_FILE).exists());
        // The configured notebook, and a folder the note created.
        create_note(&dir.0, "x", now, true).unwrap();
        assert!(dir.0.join(GUIDE_FILE).exists());
        let fresh = dir.0.join("fresh");
        create_note(&fresh, "y", now, false).unwrap();
        assert!(fresh.join(GUIDE_FILE).exists());
    }
}
