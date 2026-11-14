//! Giving a note a new title: the front matter `title`, the heading that
//! repeats it, and the file name when it was made from the old title.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::store;

/// One change to the note's lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineEdit {
    /// Replace line `0` with `1`.
    Replace(usize, String),
    /// Insert these lines before line `0`.
    Insert(usize, Vec<String>),
}

/// Whether a note still has a placeholder title.
pub fn is_untitled(title: &str, path: &Path) -> bool {
    let title = title.trim().to_lowercase();
    title.is_empty() || title == "untitled" || untitled_stem(path)
}

fn untitled_stem(path: &Path) -> bool {
    let Some(stem) = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_lowercase())
    else {
        return false;
    };
    match stem.strip_prefix("untitled") {
        Some("") => true,
        Some(rest) => rest
            .strip_prefix('-')
            .is_some_and(|n| !n.is_empty() && n.chars().all(|ch| ch.is_ascii_digit())),
        None => false,
    }
}

fn toml_string(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
}

fn yaml_string(text: &str) -> String {
    let plain = !text.is_empty()
        && !text.contains([':', '#', '"', '\'', '[', ']', '{', '}', ','])
        && !text.starts_with(['-', '?', '!', '&', '*', '|', '>', '%', '@', '`', ' '])
        && !text.ends_with(' ');
    if plain {
        text.to_owned()
    } else {
        format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// The edits that make `new` the note's title. `old` is the current title.
pub fn edits<S: AsRef<str>>(lines: &[S], old: &str, new: &str) -> Vec<LineEdit> {
    let mut edits = Vec::new();
    let front = store::front_matter_lines(lines);
    let body_start = front.map_or(0, |(_, end)| end + 1);
    let mut titled = false;
    if let Some((fence, end)) = front {
        for (index, line) in lines.iter().enumerate().take(end).skip(1) {
            let line = line.as_ref();
            let key = if fence == "+++" { '=' } else { ':' };
            let Some((name, _)) = line.split_once(key) else {
                continue;
            };
            if name.trim() == "title" && !line.starts_with([' ', '\t']) {
                let value = if fence == "+++" {
                    format!("title = {}", toml_string(new))
                } else {
                    format!("title: {}", yaml_string(new))
                };
                edits.push(LineEdit::Replace(index, value));
                titled = true;
                break;
            }
        }
        if !titled {
            let value = if fence == "+++" {
                format!("title = {}", toml_string(new))
            } else {
                format!("title: {}", yaml_string(new))
            };
            edits.push(LineEdit::Insert(1, vec![value]));
            titled = true;
        }
    }
    // The first heading, when it is the first text of the body.
    let first = lines
        .iter()
        .enumerate()
        .skip(body_start)
        .find(|(_, line)| !line.as_ref().trim().is_empty());
    match first {
        Some((index, line)) if line.as_ref().trim_start().starts_with("# ") => {
            let heading = line.as_ref().trim_start()[2..].trim();
            let old = old.trim();
            if heading.eq_ignore_ascii_case(old)
                || heading.eq_ignore_ascii_case("untitled")
                || !titled
            {
                edits.push(LineEdit::Replace(index, format!("# {new}")));
            }
        }
        _ if !titled => {
            edits.push(LineEdit::Insert(
                body_start,
                vec![format!("# {new}"), String::new()],
            ));
        }
        _ => {}
    }
    edits
}

/// Where the file should move for the new title: only when its name was
/// made from the old title (or is a placeholder), so hand-picked names stay.
pub fn new_path(path: &Path, old: &str, new: &str) -> Option<PathBuf> {
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    let old_slug = store::slug(old);
    let followed = untitled_stem(path)
        || stem == old_slug
        || stem
            .strip_prefix(&old_slug)
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|n| !n.is_empty() && n.chars().all(|ch| ch.is_ascii_digit()));
    let new_slug = store::slug(new);
    if !followed || new_slug == stem {
        return None;
    }
    let ext = path
        .extension()
        .map_or_else(|| "md".to_owned(), |ext| ext.to_string_lossy().into_owned());
    Some(path.with_file_name(format!("{new_slug}.{ext}")))
}

/// Moves `from` to `to`, or `to` with a `-2`, `-3`... suffix if taken; never
/// replaces an existing file. Returns where it went.
pub fn rename_no_clobber(from: &Path, to: &Path) -> io::Result<PathBuf> {
    let stem = to
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "note".to_owned());
    let ext = to
        .extension()
        .map_or_else(String::new, |ext| format!(".{}", ext.to_string_lossy()));
    for attempt in 0..1000 {
        let candidate = if attempt == 0 {
            to.to_path_buf()
        } else {
            to.with_file_name(format!("{stem}-{}{ext}", attempt + 1))
        };
        // A hard link fails if the name is taken, so nothing is replaced even
        // when another program creates the file meanwhile.
        match fs::hard_link(from, &candidate) {
            Ok(()) => {
                fs::remove_file(from)?;
                return Ok(candidate);
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => {
                // No hard links here (some filesystems): check, then rename.
                if candidate.exists() {
                    continue;
                }
                fs::rename(from, &candidate)?;
                return Ok(candidate);
            }
        }
    }
    Err(io::Error::other("no free file name"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn apply(lines: &[&str], edits: &[LineEdit]) -> Vec<String> {
        let mut out: Vec<String> = lines.iter().map(|line| (*line).to_owned()).collect();
        for edit in edits.iter().rev() {
            match edit {
                LineEdit::Replace(index, text) => out[*index] = text.clone(),
                LineEdit::Insert(index, new) => {
                    for (offset, line) in new.iter().enumerate() {
                        out.insert(index + offset, line.clone());
                    }
                }
            }
        }
        out
    }

    #[test]
    fn retitles_front_matter_and_the_matching_heading() {
        let note = [
            "+++",
            "title = \"untitled\"",
            "tags = []",
            "+++",
            "",
            "# untitled",
            "",
            "text",
        ];
        let got = apply(&note, &edits(&note, "untitled", "Desk \"voice\" notes"));
        assert_eq!(got[1], "title = 'Desk \"voice\" notes'");
        assert_eq!(got[5], "# Desk \"voice\" notes");
        assert_eq!(got[7], "text");
        // A heading the user wrote themselves stays.
        let note = [
            "+++",
            "title = \"untitled\"",
            "+++",
            "",
            "# My own heading",
            "x",
        ];
        let got = apply(&note, &edits(&note, "untitled", "New"));
        assert_eq!(got[1], "title = \"New\"");
        assert_eq!(got[4], "# My own heading");
    }

    #[test]
    fn yaml_and_bare_notes_get_titles_too() {
        let note = ["---", "tags: [a]", "---", "body"];
        let got = apply(&note, &edits(&note, "x", "A: b"));
        assert_eq!(got, ["---", "title: \"A: b\"", "tags: [a]", "---", "body"]);
        let note = ["just text", "more"];
        let got = apply(&note, &edits(&note, "x", "Plain"));
        assert_eq!(got, ["# Plain", "", "just text", "more"]);
        let note = ["# old", "text"];
        let got = apply(&note, &edits(&note, "old", "New"));
        assert_eq!(got, ["# New", "text"]);
    }

    #[test]
    fn files_follow_the_title_only_when_named_after_it() {
        let p = Path::new("/n/untitled-3.md");
        assert_eq!(
            new_path(p, "untitled", "Big Plan"),
            Some(PathBuf::from("/n/big-plan.md"))
        );
        assert_eq!(
            new_path(Path::new("/n/old-title.md"), "Old title", "New"),
            Some(PathBuf::from("/n/new.md"))
        );
        assert_eq!(
            new_path(Path::new("/n/old-title-2.md"), "Old title", "New"),
            Some(PathBuf::from("/n/new.md"))
        );
        assert_eq!(
            new_path(Path::new("/n/my-name.md"), "Old title", "New"),
            None
        );
        assert_eq!(new_path(Path::new("/n/new.md"), "untitled", "New"), None);
        assert!(is_untitled("untitled", Path::new("/n/x.md")));
        assert!(is_untitled("Real", Path::new("/n/untitled-2.md")));
        assert!(!is_untitled("Real", Path::new("/n/untitled-notes.md")));
    }

    #[test]
    fn renames_never_replace_existing_files() {
        let dir = TempDir::new("retitle");
        let from = dir.path().join("untitled.md");
        let taken = dir.path().join("plan.md");
        fs::write(&from, "mine").unwrap();
        fs::write(&taken, "theirs").unwrap();
        let got = rename_no_clobber(&from, &taken).unwrap();
        assert_eq!(got, dir.path().join("plan-2.md"));
        assert_eq!(fs::read_to_string(&taken).unwrap(), "theirs");
        assert_eq!(fs::read_to_string(&got).unwrap(), "mine");
        assert!(!from.exists());
    }
}
