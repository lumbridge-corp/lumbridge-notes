//! lumbridge-notes: a terminal Markdown note editor with live preview.
//!
//! Core modules (no terminal I/O):
//! - [`buffer`]: text, cursor, selection, undo/redo.
//! - [`edit`]: Markdown-aware edits (list continuation, tasks, indent).
//! - [`markdown`]: per-line parse with rendered spans and source mapping.
//! - [`layout`]: soft wrap and screen <-> source mapping, scrolling.
//! - [`store`]: files, atomic save, external changes, front matter, notebook.
//!
//! The application:
//! - [`editor`]: one open note (text, parse, layout cache, file state, view).
//! - [`app`]: application state and key / mouse handling.
//! - [`picker`] and [`search`]: the note picker and notebook search.
//! - [`ui`]: pure rendering of `&App` into a ratatui frame.
//!
//! Assistants (all optional, configured in [`config`]):
//! - [`services`]: AI, speech and dictation jobs on background threads.
//! - [`library`]: note templates and AI prompts kept in the notebook.
//! - [`narrate`]: a note as text to read aloud.

pub mod app;
pub mod buffer;
pub mod config;
pub mod edit;
pub mod editor;
pub mod escape;
pub mod input;
pub mod layout;
pub mod library;
pub mod live;
pub mod markdown;
pub mod naming;
pub mod narrate;
pub mod picker;
pub mod retitle;
pub mod search;
pub mod services;
pub mod store;
#[cfg(test)]
pub(crate) mod test_support;
pub mod text;
pub mod theme;
pub mod ui;
