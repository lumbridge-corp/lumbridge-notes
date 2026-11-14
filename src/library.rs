//! Note templates and AI prompts, kept as Markdown files in the notebook so
//! the user and agents can add their own:
//!
//! - `<notebook>/.lumbridge/templates/*.md`: front matter `title`,
//!   `description`; the body is the note text, with `{{title}}`, `{{date}}`
//!   and `{{time}}` filled in.
//! - `<notebook>/.lumbridge/prompts/*.md`: front matter `title`, `description`
//!   and `action` (`replace`, `insert`, `append` or `answer`); the body is
//!   the instruction sent with the note (or the selection).
//!
//! The folders are seeded with a starter set the first time they are needed
//! and never overwritten after that.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const LIBRARY_DIR: &str = ".lumbridge";
pub const TEMPLATES_DIR: &str = "templates";
pub const PROMPTS_DIR: &str = "prompts";
const MAX_ITEMS: usize = 200;
const MAX_ITEM_BYTES: u64 = 64 * 1024;

/// What to do with a prompt's answer by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptAction {
    /// Replace the note (or the selection) with the answer.
    Replace,
    /// Insert the answer at the cursor.
    Insert,
    /// Append the answer to the end of the note.
    Append,
    /// Just show the answer.
    #[default]
    Answer,
}

impl PromptAction {
    pub fn label(self) -> &'static str {
        match self {
            PromptAction::Replace => "replace",
            PromptAction::Insert => "insert",
            PromptAction::Append => "append",
            PromptAction::Answer => "answer",
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Header {
    title: String,
    description: String,
    action: Option<PromptAction>,
}

/// A template or a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub title: String,
    pub description: String,
    pub action: PromptAction,
    pub body: String,
    pub path: PathBuf,
}

pub fn templates_dir(root: &Path) -> PathBuf {
    library_dir(root, TEMPLATES_DIR)
}

pub fn prompts_dir(root: &Path) -> PathBuf {
    library_dir(root, PROMPTS_DIR)
}

/// Where the items of `kind` live: `<root>/.lumbridge/<kind>`.
pub fn library_dir(root: &Path, kind: &str) -> PathBuf {
    root.join(LIBRARY_DIR).join(kind)
}

/// The templates in `root`'s notebook, seeding the starter set if the folder
/// does not exist yet.
pub fn templates(root: &Path) -> io::Result<Vec<Item>> {
    seed(&templates_dir(root), STARTER_TEMPLATES)?;
    load(&templates_dir(root))
}

/// The prompts in `root`'s notebook, seeding the starter set if needed.
pub fn prompts(root: &Path) -> io::Result<Vec<Item>> {
    seed(&prompts_dir(root), STARTER_PROMPTS)?;
    load(&prompts_dir(root))
}

/// The items in `dir`, sorted by title.
fn load(dir: &Path) -> io::Result<Vec<Item>> {
    let mut items: Vec<Item> = Vec::new();
    if dir.is_dir() {
        load_into(dir, &mut items)?;
    }
    items.sort_by_key(|item| item.title.to_lowercase());
    Ok(items)
}

fn seed(dir: &Path, starters: &[(&str, &str)]) -> io::Result<()> {
    if dir.exists() {
        return Ok(());
    }
    fs::create_dir_all(dir)?;
    for (name, text) in starters {
        let path = dir.join(name);
        if !path.exists() {
            fs::write(path, text)?;
        }
    }
    Ok(())
}

/// Appends up to [`MAX_ITEMS`] items from `dir` to `items`.
fn load_into(dir: &Path, items: &mut Vec<Item>) -> io::Result<()> {
    let start = items.len();
    for entry in fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || !name.ends_with(".md") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() || meta.len() > MAX_ITEM_BYTES {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        items.push(parse_item(&text, &path));
        if items.len() - start >= MAX_ITEMS {
            break;
        }
    }
    Ok(())
}

/// Splits `+++` TOML front matter from the body.
pub fn parse_item(text: &str, path: &Path) -> Item {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (header, body) = split_front_matter(text);
    let header: Header = header
        .and_then(|header| toml::from_str(header).ok())
        .unwrap_or_default();
    let title = if header.title.trim().is_empty() {
        path.file_stem()
            .map(|stem| stem.to_string_lossy().replace(['-', '_'], " "))
            .unwrap_or_default()
    } else {
        header.title.trim().to_owned()
    };
    Item {
        title,
        description: header.description.trim().to_owned(),
        action: header.action.unwrap_or_default(),
        body: body.trim_start_matches(['\r', '\n']).to_owned(),
        path: path.to_owned(),
    }
}

fn split_front_matter(text: &str) -> (Option<&str>, &str) {
    let Some(rest) = text
        .strip_prefix("+++\n")
        .or_else(|| text.strip_prefix("+++\r\n"))
    else {
        return (None, text);
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "+++" {
            return (Some(&rest[..offset]), &rest[offset + line.len()..]);
        }
        offset += line.len();
    }
    (None, text)
}

/// A template's text with its placeholders filled in.
pub fn fill(body: &str, title: &str, date: &str, time: &str) -> String {
    body.replace("{{title}}", title)
        .replace("{{date}}", date)
        .replace("{{time}}", time)
}

/// Creates a new, empty template or prompt file and returns its path.
pub fn create_item(dir: &Path, title: &str, prompt: bool) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let title = title.trim();
    let title = if title.is_empty() { "untitled" } else { title };
    let base = crate::store::slug(title);
    let text = if prompt {
        format!(
            "+++\ntitle = {}\ndescription = \"\"\naction = \"answer\"  # replace | insert | append | answer\n+++\n\nWrite the instruction for the AI here. It gets the note (or the selected text) after it.\n",
            toml_string(title)
        )
    } else {
        format!(
            "+++\ntitle = {}\ndescription = \"\"\n+++\n\n# {{{{title}}}}\n\n",
            toml_string(title)
        )
    };
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
                io::Write::write_all(&mut file, text.as_bytes())?;
                return Ok(path);
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::other("no free file name"))
}

fn toml_string(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
}

const STARTER_TEMPLATES: &[(&str, &str)] = &[
    (
        "daily.md",
        "+++\ntitle = \"Daily note\"\ndescription = \"Plan, log and wins for the day\"\n+++\n\n# {{date}}\n\n## Plan\n\n- [ ] \n\n## Log\n\n- {{time}} \n\n## Wins\n\n- \n",
    ),
    (
        "meeting.md",
        "+++\ntitle = \"Meeting\"\ndescription = \"Who, agenda, notes, decisions, actions\"\n+++\n\n# {{title}}\n\n**When:** {{date}} {{time}}\n**Who:** \n\n## Agenda\n\n- \n\n## Notes\n\n- \n\n## Decisions\n\n- \n\n## Actions\n\n- [ ] \n",
    ),
    (
        "idea.md",
        "+++\ntitle = \"Idea\"\ndescription = \"Problem, idea, why now, next step\"\n+++\n\n# {{title}}\n\n## Problem\n\n\n\n## Idea\n\n\n\n## Why now\n\n\n\n## Next step\n\n- [ ] \n",
    ),
    (
        "project.md",
        "+++\ntitle = \"Project\"\ndescription = \"Goal, scope, milestones, risks, tasks\"\n+++\n\n# {{title}}\n\n## Goal\n\n\n\n## Scope\n\n- In: \n- Out: \n\n## Milestones\n\n- [ ] \n\n## Risks\n\n- \n\n## Tasks\n\n- [ ] \n",
    ),
    (
        "agent-brief.md",
        "+++\ntitle = \"Agent brief\"\ndescription = \"A task written for a coding agent to pick up\"\n+++\n\n# {{title}}\n\n## Goal\n\n\n\n## Context\n\n- Repo: \n- Relevant files: \n\n## Requirements\n\n- \n\n## Done when\n\n- [ ] \n\n## Out of scope\n\n- \n",
    ),
];

const STARTER_PROMPTS: &[(&str, &str)] = &[
    (
        "improve-writing.md",
        "+++\ntitle = \"Improve writing\"\ndescription = \"Clearer and tighter, same meaning and structure\"\naction = \"replace\"\n+++\n\nRewrite this to be clearer and more concise. Keep the meaning, the voice, the Markdown structure, the front matter and every fact, link and task. Fix spelling and grammar.\n",
    ),
    (
        "summarize.md",
        "+++\ntitle = \"Summarize\"\ndescription = \"A few bullet points\"\naction = \"answer\"\n+++\n\nSummarize this in 3 to 7 short bullet points, most important first.\n",
    ),
    (
        "extract-tasks.md",
        "+++\ntitle = \"Extract tasks\"\ndescription = \"Every action item as a task list\"\naction = \"append\"\n+++\n\nList every action item, to-do and commitment in this as a Markdown task list (`- [ ] ...`) under a `## Tasks` heading. Keep each one short and concrete. If there are none, say so in one line.\n",
    ),
    (
        "make-a-plan.md",
        "+++\ntitle = \"Turn into a plan\"\ndescription = \"Goal, steps and open questions\"\naction = \"answer\"\n+++\n\nTurn this into a practical plan in Markdown: a one-line goal, numbered steps with tasks (`- [ ]`) under each, and a short list of open questions.\n",
    ),
    (
        "clean-up-dictation.md",
        "+++\ntitle = \"Clean up dictation\"\ndescription = \"Punctuation, paragraphs, no filler words\"\naction = \"replace\"\n+++\n\nThis was dictated. Add punctuation and paragraphs, fix words the speech recogniser got wrong when the intended word is obvious, and remove filler words and false starts. Do not add, remove or reorder ideas.\n",
    ),
    (
        "improve-prompt.md",
        "+++\ntitle = \"Improve as a prompt\"\ndescription = \"Rewrite this as a strong prompt for an AI agent\"\naction = \"replace\"\n+++\n\nThis is a prompt for an AI agent. Rewrite it into a stronger prompt: state the goal first, then the context the agent needs, concrete requirements, constraints, and what done looks like. Keep every requirement and detail from the original, make vague parts specific only where the intent is clear, and list any open questions at the end. Use Markdown headings and lists.\n",
    ),
    (
        "ask.md",
        "+++\ntitle = \"What am I missing?\"\ndescription = \"Gaps, risks and questions\"\naction = \"answer\"\n+++\n\nRead this critically. What is missing, unclear, risky or contradictory? Answer with a short list, most important first.\n",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn seeds_once_and_reads_user_items() {
        let dir = TempDir::new("library");
        let templates = templates(dir.path()).unwrap();
        assert!(templates.iter().any(|item| item.title == "Meeting"));
        let prompts = prompts(dir.path()).unwrap();
        let improve = prompts
            .iter()
            .find(|item| item.title == "Improve writing")
            .unwrap();
        assert_eq!(improve.action, PromptAction::Replace);
        assert!(improve.body.starts_with("Rewrite"));
        // A deleted starter stays deleted; a new file appears.
        fs::remove_file(prompts_dir(dir.path()).join("summarize.md")).unwrap();
        fs::write(
            prompts_dir(dir.path()).join("tone.md"),
            "Make it friendlier.\n",
        )
        .unwrap();
        let prompts = super::prompts(dir.path()).unwrap();
        assert!(!prompts.iter().any(|item| item.title == "Summarize"));
        let tone = prompts.iter().find(|item| item.title == "tone").unwrap();
        assert_eq!(tone.action, PromptAction::Answer);
        assert_eq!(tone.body, "Make it friendlier.\n");
    }

    #[test]
    fn front_matter_is_optional_and_placeholders_fill() {
        let item = parse_item(
            "+++\ntitle = \"T\"\n+++\n\n# {{title}} on {{date}}\n",
            Path::new("/x/t.md"),
        );
        assert_eq!(item.title, "T");
        assert_eq!(
            fill(&item.body, "Plan", "2026-10-16", "19:00"),
            "# Plan on 2026-10-16\n"
        );
        let item = parse_item("+++\nbroken = \n+++\nbody\n", Path::new("/x/my-thing.md"));
        assert_eq!(item.title, "my thing");
        assert_eq!(item.body, "body\n");
        let item = parse_item("no header", Path::new("/x/a.md"));
        assert_eq!(item.body, "no header");
    }

    #[test]
    fn new_items_get_free_names_and_valid_headers() {
        let dir = TempDir::new("library-new");
        let first = create_item(dir.path(), "My \"Prompt\"", true).unwrap();
        let second = create_item(dir.path(), "My \"Prompt\"", true).unwrap();
        assert_ne!(first, second);
        let item = parse_item(&fs::read_to_string(&first).unwrap(), &first);
        assert_eq!(item.title, "My \"Prompt\"");
        let template = create_item(dir.path(), "Weekly", false).unwrap();
        let item = parse_item(&fs::read_to_string(&template).unwrap(), &template);
        assert_eq!(item.body, "# {{title}}\n\n");
    }
}
