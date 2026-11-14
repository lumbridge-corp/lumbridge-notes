# Changelog

## Unreleased

## 0.1.0

First open-source release.

- A terminal Markdown editor with live preview: headings, lists, task boxes,
  tables, quotes and code are drawn formatted, and the line under the cursor
  shows its source. Source mode, undo and redo, find, mouse and a toolbar.
- Notes are plain files: one Markdown file per note with optional TOML front
  matter (`title`, `tags`, `created`). Atomic saves, detection of changes made
  by other programs, and a recovery file when a note cannot be saved on exit.
- A note picker and full-text search over a notebook folder (`~/notes`,
  `--dir` or `$LUMBRIDGE_NOTES_DIR`).
- Templates and AI prompts as Markdown files in `<notebook>/.lumbridge/`, with
  a starter set.
- Optional assistants on servers you name in
  `~/.config/lumbridge-notes/config.toml`: AI prompts and automatic naming
  (`[ai]`), reading aloud (`[speech]`) and dictation (`[dictation]`). Each is
  off until its `url` is set, and says how to set it.
- `scripts/scrub-check` keeps private names, addresses and paths out of the
  repository. `scripts/ci.sh` runs it with format, clippy and tests.
- Settings, environment variables and notebook folders from builds made
  before the first release are no longer read. Use
  `~/.config/lumbridge-notes/`, `LUMBRIDGE_NOTES_*` and
  `<notebook>/.lumbridge/`.
