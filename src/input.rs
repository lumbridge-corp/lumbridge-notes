//! A one-line text input (find bar, prompts, picker and search queries).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::buffer::{next_grapheme, prev_grapheme};
use crate::layout::str_width;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineInput {
    pub text: String,
    /// Byte offset of the cursor, on a grapheme boundary.
    pub cursor: usize,
}

impl LineInput {
    pub fn new(text: &str) -> LineInput {
        LineInput {
            text: text.to_owned(),
            cursor: text.len(),
        }
    }

    /// Screen width of the text before the cursor.
    pub fn cursor_x(&self) -> usize {
        str_width(self.text.get(..self.cursor).unwrap_or(""))
    }

    pub fn insert(&mut self, text: &str) -> bool {
        // One line only; control characters are dropped.
        let clean: String = text
            .chars()
            .take_while(|&ch| ch != '\n' && ch != '\r')
            .filter(|ch| !ch.is_control() || *ch == '\t')
            .map(|ch| if ch == '\t' { ' ' } else { ch })
            .collect();
        if clean.is_empty() {
            return false;
        }
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
        true
    }

    fn word_start(&self) -> usize {
        let head = &self.text[..self.cursor];
        let trimmed = head.trim_end();
        trimmed.rfind(char::is_whitespace).map_or(0, |index| {
            index + trimmed[index..].chars().next().map_or(1, char::len_utf8)
        })
    }

    /// Handles an editing key. Returns `Some(changed)` when the key was an
    /// input key, `None` when the caller should handle it.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<bool> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('h') | KeyCode::Char('w') if ctrl => Some(self.delete_word()),
            KeyCode::Backspace if ctrl || key.modifiers.contains(KeyModifiers::ALT) => {
                Some(self.delete_word())
            }
            KeyCode::Char('u') if ctrl => {
                let changed = !self.text.is_empty();
                self.text.clear();
                self.cursor = 0;
                Some(changed)
            }
            KeyCode::Char(ch) if !ctrl => {
                let mut buf = [0u8; 4];
                Some(self.insert(ch.encode_utf8(&mut buf)))
            }
            KeyCode::Backspace => {
                if self.cursor == 0 {
                    return Some(false);
                }
                let start = prev_grapheme(&self.text, self.cursor);
                self.text.replace_range(start..self.cursor, "");
                self.cursor = start;
                Some(true)
            }
            KeyCode::Delete => {
                if self.cursor >= self.text.len() {
                    return Some(false);
                }
                let end = next_grapheme(&self.text, self.cursor);
                self.text.replace_range(self.cursor..end, "");
                Some(true)
            }
            KeyCode::Left => {
                self.cursor = prev_grapheme(&self.text, self.cursor);
                Some(false)
            }
            KeyCode::Right => {
                self.cursor = next_grapheme(&self.text, self.cursor);
                Some(false)
            }
            KeyCode::Home => {
                self.cursor = 0;
                Some(false)
            }
            KeyCode::End => {
                self.cursor = self.text.len();
                Some(false)
            }
            _ => None,
        }
    }

    fn delete_word(&mut self) -> bool {
        let start = self.word_start();
        if start == self.cursor {
            return false;
        }
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn edits_one_line() {
        let mut input = LineInput::default();
        for ch in "héllo wörld".chars() {
            input.handle_key(key(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert_eq!(input.text, "héllo wörld");
        assert_eq!(input.cursor_x(), 11);
        assert_eq!(
            input.handle_key(key(KeyCode::Char('w'), KeyModifiers::CONTROL)),
            Some(true)
        );
        assert_eq!(input.text, "héllo ");
        input.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
        input.handle_key(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(input.text, "héll ");
        assert!(input.insert("a\nb"));
        assert_eq!(input.text, "hélla ");
        assert_eq!(
            input.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
        input.handle_key(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(input, LineInput::default());
    }
}
