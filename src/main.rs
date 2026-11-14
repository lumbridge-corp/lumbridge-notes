//! `lumbridge-notes [PATH] [--dir DIR]`: edit a Markdown note, or pick one from
//! a notebook folder.

mod terminal;

use std::path::PathBuf;
use std::process::ExitCode;

use lumbridge_notes::app::App;
use lumbridge_notes::editor::Editor;
use lumbridge_notes::store;
use lumbridge_notes::theme::Theme;

const USAGE: &str = "\
lumbridge-notes: Markdown notes with live preview

Usage: lumbridge-notes [PATH] [--dir DIR]

  PATH        a note to edit (created on first save), or a folder to pick from
  --dir DIR   notebook folder for the picker and search
              (default: $LUMBRIDGE_NOTES_DIR, else ~/notes)
  -h, --help     show this help
  -V, --version  show the version

Keys: ^S save  ^Q quit  ^O notes  ^N new note  ^G search notes  ^F find
      ^E source mode  ^T toggle task  ^Z undo  ^Y redo  ^C/^X/^V clipboard
      ^K AI menu  ^P templates  ^R read aloud / stop  ^D dictate / stop
      (or click the toolbar buttons along the top)

Assistants: AI prompts and naming, reading aloud and dictation use servers
set in ~/.config/lumbridge-notes/config.toml ($LUMBRIDGE_NOTES_CONFIG):
  [ai]        url = \"http://host:8001/v1\"  model = \"...\"   (OpenAI-style chat)
  [speech]    url = \"http://host:8097/v1\"  voice = \"...\"   (/audio/speech)
  [dictation] url = \"http://host:8791/v1\"                  (/audio/transcriptions)
Templates and prompts are Markdown files in <notebook>/.lumbridge/templates and
<notebook>/.lumbridge/prompts; add your own there.
Colours: NO_COLOR turns them off; LUMBRIDGE_NOTES_THEME=light|dark picks the
palette (else COLORFGBG decides; dark by default).
";

#[derive(Debug, Default, PartialEq, Eq)]
struct Args {
    path: Option<PathBuf>,
    dir: Option<PathBuf>,
    help: bool,
    version: bool,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args::default();
    let mut args = args.into_iter();
    let mut positional_only = false;
    while let Some(arg) = args.next() {
        if positional_only || !arg.starts_with('-') || arg == "-" {
            if parsed.path.is_some() {
                return Err(format!("unexpected argument: {arg}"));
            }
            parsed.path = Some(PathBuf::from(arg));
            continue;
        }
        match arg.as_str() {
            "--" => positional_only = true,
            "-h" | "--help" => parsed.help = true,
            "-V" | "--version" => parsed.version = true,
            "--dir" => match args.next() {
                Some(dir) => parsed.dir = Some(PathBuf::from(dir)),
                None => return Err("--dir needs a folder".to_owned()),
            },
            other => match other.strip_prefix("--dir=") {
                Some(dir) => parsed.dir = Some(PathBuf::from(dir)),
                None => return Err(format!("unknown option: {other}")),
            },
        }
    }
    Ok(parsed)
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(err) => {
            eprintln!("lumbridge-notes: {err}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if args.help {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.version {
        println!("lumbridge-notes {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let app = match startup(args) {
        Ok(app) => app,
        Err(err) => {
            eprintln!("lumbridge-notes: {err}");
            return ExitCode::from(1);
        }
    };
    let (result, note) = terminal::run(app);
    if let Some(note) = note {
        eprintln!("lumbridge-notes: {note}");
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("lumbridge-notes: {err}");
            ExitCode::from(1)
        }
    }
}

/// Builds the initial state from the command line (before the terminal is
/// taken over, so errors print normally).
fn startup(args: Args) -> Result<App, String> {
    let mut app = open(args)?;
    let (config, problem) = lumbridge_notes::config::Config::load();
    app.set_config(config, problem);
    Ok(app)
}

fn open(args: Args) -> Result<App, String> {
    let theme = Theme::from_env();
    let default_dir = store::default_notes_dir();
    let is_default = |dir: &std::path::Path| same_dir(dir, &default_dir);
    match args.path {
        Some(path) if path.is_dir() => {
            let notebook = args.dir.is_some() || is_default(&path);
            let mut app = App::with_picker(path, theme);
            app.notebook = notebook;
            Ok(app)
        }
        Some(path) => {
            let parent = match path.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
                _ => PathBuf::from("."),
            };
            if !parent.is_dir() {
                return Err(format!("{}: folder does not exist", parent.display()));
            }
            let root = store::notebook_root_for(&path, args.dir.as_deref(), &default_dir);
            // Only the configured notebook gets the agent guide on Ctrl-N,
            // never a folder that merely holds the edited file.
            let notebook = args.dir.is_some() || is_default(&root);
            let editor = Editor::open(&path)?;
            let mut app = App::with_editor(root, theme, editor);
            app.notebook = notebook;
            Ok(app)
        }
        None => {
            let root = args.dir.unwrap_or(default_dir);
            std::fs::create_dir_all(&root).map_err(|err| format!("{}: {err}", root.display()))?;
            let mut app = App::with_picker(root, theme);
            app.notebook = true;
            Ok(app)
        }
    }
}

/// Whether two paths name the same folder.
fn same_dir(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_args(args.iter().map(|arg| arg.to_string()))
    }

    #[test]
    fn parses_cli() {
        assert_eq!(parse(&[]).unwrap(), Args::default());
        let args = parse(&["note.md", "--dir", "/n"]).unwrap();
        assert_eq!(args.path, Some(PathBuf::from("note.md")));
        assert_eq!(args.dir, Some(PathBuf::from("/n")));
        assert_eq!(parse(&["--dir=/x"]).unwrap().dir, Some(PathBuf::from("/x")));
        assert!(parse(&["-h"]).unwrap().help);
        assert!(parse(&["--version"]).unwrap().version);
        assert_eq!(
            parse(&["--", "-odd.md"]).unwrap().path,
            Some(PathBuf::from("-odd.md"))
        );
        assert!(parse(&["a", "b"]).is_err());
        assert!(parse(&["--dir"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
    }
}
