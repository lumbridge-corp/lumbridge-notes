# Lumbridge Notes

`lumbridge-notes` is a Markdown note editor that runs in a terminal and shows
the formatted note while you type. Each note is one plain file in a folder, so
people, scripts and agents can read and write the same notebook. AI prompts,
reading aloud and dictation are optional and use servers you name.

Part of [Lumbridge](https://lumbridgecorp.com) ([source on GitHub](https://github.com/lumbridge-corp)). In daily use.
Product page: <https://lumbridgecorp.com/notes>. Guide:
<https://lumbridgecorp.com/guide/work>.

## Status

- **Version:** 0.1.0. Interfaces can change until 1.0.
- **Runs on:** Linux. The steps below are tested on Ubuntu 24.04, x86_64.
  macOS builds from the same source; it is not part of the test run yet.
- **Not yet:**
  - No installer, packages or updater: you build from source.
  - Read aloud and dictation look for Linux audio tools by default. On macOS
    you set the play and record commands yourself.
  - Live dictation speaks plain `ws://` only, so it is meant for a private
    network.
  - No sync of its own. Share a notebook with git or a synced folder.

## Install

You need a C linker, git, curl and a current stable Rust toolchain
(1.88 or newer). On Ubuntu 24.04:

```sh
sudo apt update
sudo apt install -y build-essential ca-certificates curl git
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. "$HOME/.cargo/env"

git clone https://github.com/lumbridge-corp/lumbridge-notes.git
cd lumbridge-notes
cargo install --locked --path .
```

This puts one binary, `lumbridge-notes`, in `~/.cargo/bin`. Nothing else is
installed and no service is started. To remove it, run
`cargo uninstall lumbridge-notes`.

All dependencies are crates from crates.io, pinned in `Cargo.lock`.

## Quick start

```sh
lumbridge-notes --version
lumbridge-notes                          # pick from ~/notes (made if missing)
lumbridge-notes --dir ~/work-notes       # another notebook
lumbridge-notes ~/work-notes/plan.md     # edit one note; created on first save
```

With no path you get the picker: type to filter, Enter opens a note. In a
note, the text is drawn formatted and the line under the cursor shows its
Markdown source.

| Key | What it does |
|---|---|
| Ctrl-S | Save |
| Ctrl-Q | Quit |
| Ctrl-O | Note picker |
| Ctrl-N | New note |
| Ctrl-G | Search all notes |
| Ctrl-F | Find in this note |
| Ctrl-E | Source mode |
| Ctrl-T | Toggle a task box |
| Ctrl-Z / Ctrl-Y | Undo / redo |
| Ctrl-C / Ctrl-X / Ctrl-V | Copy / cut / paste |
| Ctrl-P | Templates |
| Ctrl-K | AI menu |
| Ctrl-R | Read aloud, or stop |
| Ctrl-D | Dictate, or stop |

The toolbar along the top has the same actions as buttons. `lumbridge-notes
--help` prints this list.

Editing, preview, the picker, search and templates need no network and no
server.

## Notes are files

- One Markdown file per note. Optional TOML front matter (`title`, `tags`,
  `created`) between `+++` lines. YAML-style `---` front matter is tolerated.
- The notebook is a folder: `~/notes`, or the one named by `--dir` or
  `$LUMBRIDGE_NOTES_DIR`. Subfolders work, up to three levels.
- Templates and AI prompts are Markdown files inside the notebook, in
  `<notebook>/.lumbridge/templates` and `<notebook>/.lumbridge/prompts`. A
  starter set is written the first time they are needed. Add your own.
- A new notebook gets an `AGENTS.md` that explains the format to agents.
- Saves are atomic. A change made by another program is detected. If a note
  cannot be saved on exit, the text goes to `<name>.lumbridge-notes-recovery`
  next to it.

## Configuration

| What | Path |
|---|---|
| Config | `~/.config/lumbridge-notes/config.toml`, or the file named by `$LUMBRIDGE_NOTES_CONFIG` |
| State (the chosen voice) | `~/.local/state/lumbridge-notes/` |
| Notebook | `~/notes`, or `--dir`, or `$LUMBRIDGE_NOTES_DIR` |

The assistants are off until you give them a server. Every section is
optional:

```toml
[ai]
url = "http://models.example.com:8001/v1"   # any OpenAI-compatible chat server
model = "your-model"
api_key_env = "MY_API_KEY"                  # name of a variable that holds a token, if needed
auto_name = true                            # name untitled notes by themselves

[speech]
url = "http://speech.example.com:8097/v1"   # OpenAI-style /audio/speech
voice = "default"

[dictation]
url = "http://speech.example.com:8791/v1"   # OpenAI-style /audio/transcriptions
live = true                                 # words appear while you talk
```

Without an `[ai] url`, the AI menu and buttons say that AI is not set up and
name the config file to edit. The same goes for `[speech]` and `[dictation]`.

The assistants call `curl` for HTTP. Read aloud plays through `pw-play`,
`paplay` or `aplay`, and dictation records with `pw-record`, `parecord` or
`arecord`, whichever is on `PATH`. Set `player`, `recorder` and
`live_recorder` to use other commands.

Colours: `NO_COLOR` turns them off. `LUMBRIDGE_NOTES_THEME=light` or `dark`
picks the palette.

## How it fits the system

Lumbridge Notes works alone. It needs no other Lumbridge part.

| Works with | What passes between them |
|---|---|
| [Lumbridge TUI](https://github.com/lumbridge-corp/lumbridge-tui) | Its notebook drawer opens notes with `lumbridge-notes` in a pane. Both read and write the same note format. |
| [Lumbridge Speak](https://github.com/lumbridge-corp/lumbridge-speak) | Read aloud posts the note's text to an OpenAI-style `/audio/speech` endpoint. |
| [Lumbridge Voice](https://github.com/lumbridge-corp/lumbridge-voice) | Dictation posts recordings to `/audio/transcriptions`, or streams microphone audio and gets words back. |

Any server with the same API works in place of Speak and Voice.

## What it sends and where

Nothing leaves your machine unless you set a server and press its key.
The editor opens no network port. Notes stay in the notebook folder; there is
no database and no account. With an `[ai] url` set, a prompt sends the note or
the selection to that server, and if `auto_name` is on an untitled note is
sent to be named. Read aloud sends the note's text to your speech server.
Dictation sends your recording to your dictation server.

## Docs

- `lumbridge-notes --help`: the command line, keys and settings.
- [Change log](CHANGELOG.md)

## Contributing

Bug reports and small fixes are welcome. Talk to us in an issue before a
large change. See [CONTRIBUTING.md](CONTRIBUTING.md).

```sh
scripts/ci.sh     # what CI runs: scrub check, cargo fmt, clippy, tests
```

`tests/golden/` pins the note format byte for byte. The same files are checked
by Lumbridge TUI's notebook drawer. They are generated outside this repository
and copied in with `scripts/sync-golden.sh`; do not edit them by hand.

## Security

Do not open a public issue for a security problem. Email
security@lumbridgecorp.com. See [SECURITY.md](SECURITY.md).

## Licence

Apache-2.0. See [LICENSE](LICENSE). Copyright 2026 Lumbridge Corp.

Dependencies are Rust crates under their own licences (MIT, Apache-2.0 and
similar).
