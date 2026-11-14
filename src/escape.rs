//! Repairs escape sequences that crossterm splits after a lone Esc.
//!
//! When Esc and another special key arrive in the same read (tmux
//! `send-keys`, SSH latency, a fast typist), the bytes are
//! `ESC ESC [ 4 ~`. crossterm 0.29 reports `ESC ESC` as one Esc and then
//! parses `[ 4 ~` as three typed characters, which would land in the note.
//! [`EscapeRepair`] sits between crossterm and the app: after an Esc it holds
//! back the characters that arrive in the same burst while they still look
//! like the tail of an escape sequence, and turns a complete one back into
//! the key (or mouse / focus event) it encodes. Anything else is replayed
//! unchanged. The event loop calls [`EscapeRepair::flush`] as soon as no more
//! input is queued, so a human typing Esc and then `[` is never delayed.

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

/// Longest tail held back; real sequences are far shorter.
const MAX_TAIL: usize = 32;

#[derive(Debug, Default)]
pub struct EscapeRepair {
    /// Characters held back since the last Esc, or `None` when not after an
    /// Esc.
    pending: Option<String>,
}

enum Tail {
    /// A valid prefix of an escape sequence; wait for more.
    Partial,
    /// A complete sequence (its event, when it is one we understand).
    Complete(Option<Event>),
    /// Not an escape sequence.
    Invalid,
}

impl EscapeRepair {
    pub fn new() -> EscapeRepair {
        EscapeRepair::default()
    }

    /// Whether characters may still be held back (call [`flush`] once no
    /// more input is queued).
    ///
    /// [`flush`]: EscapeRepair::flush
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Takes one event from crossterm and returns the events to handle now.
    pub fn feed(&mut self, event: Event) -> Vec<Event> {
        let Some(mut tail) = self.pending.take() else {
            if is_plain_esc(&event) {
                self.pending = Some(String::new());
            }
            return vec![event];
        };
        let Some(ch) = tail_char(&event) else {
            // Not a character: the tail is over.
            let mut out = replay(&tail);
            if is_plain_esc(&event) {
                self.pending = Some(String::new());
            }
            out.push(event);
            return out;
        };
        tail.push(ch);
        match classify(&tail) {
            Tail::Partial if tail.len() < MAX_TAIL => {
                self.pending = Some(tail);
                Vec::new()
            }
            Tail::Complete(event) => event.into_iter().collect(),
            Tail::Partial | Tail::Invalid => replay(&tail),
        }
    }

    /// Releases characters held back (no more input arrived with them).
    pub fn flush(&mut self) -> Vec<Event> {
        match self.pending.take() {
            Some(tail) => replay(&tail),
            None => Vec::new(),
        }
    }
}

fn is_plain_esc(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Esc,
            modifiers,
            kind: KeyEventKind::Press,
            ..
        }) if modifiers.is_empty()
    )
}

/// The character a crossterm event stands for, when it is a plain typed
/// character (uppercase letters carry SHIFT).
fn tail_char(event: &Event) -> Option<char> {
    match event {
        Event::Key(KeyEvent {
            code: KeyCode::Char(ch),
            modifiers,
            kind: KeyEventKind::Press,
            ..
        }) if (*modifiers - KeyModifiers::SHIFT).is_empty() => Some(*ch),
        _ => None,
    }
}

/// The held-back characters as the key events crossterm reported.
fn replay(tail: &str) -> Vec<Event> {
    tail.chars()
        .map(|ch| {
            let modifiers = if ch.is_uppercase() {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            };
            Event::Key(KeyEvent::new(KeyCode::Char(ch), modifiers))
        })
        .collect()
}

fn classify(tail: &str) -> Tail {
    let mut chars = tail.chars();
    match chars.next() {
        Some('O') => match chars.next() {
            None => Tail::Partial,
            Some(final_char) if chars.next().is_none() => match ss3_key(final_char) {
                Some(code) => Tail::Complete(Some(key(code, KeyModifiers::NONE))),
                None => Tail::Invalid,
            },
            Some(_) => Tail::Invalid,
        },
        Some('[') => {
            let body = &tail[1..];
            let Some(last) = body.chars().last() else {
                return Tail::Partial;
            };
            let params = &body[..body.len() - last.len_utf8()];
            if !params
                .chars()
                .all(|ch| ch.is_ascii_digit() || ch == ';' || ch == '<')
            {
                return Tail::Invalid;
            }
            if last.is_ascii_digit() || last == ';' || last == '<' {
                return Tail::Partial;
            }
            // Only sequences we decode are swallowed; anything else (Esc
            // then `[link]` typed in one burst) is replayed as typed.
            match csi_event(params, last) {
                Some(event) => Tail::Complete(Some(event)),
                None => Tail::Invalid,
            }
        }
        _ => Tail::Invalid,
    }
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

fn ss3_key(final_char: char) -> Option<KeyCode> {
    Some(match final_char {
        'A' => KeyCode::Up,
        'B' => KeyCode::Down,
        'C' => KeyCode::Right,
        'D' => KeyCode::Left,
        'H' => KeyCode::Home,
        'F' => KeyCode::End,
        'P' => KeyCode::F(1),
        'Q' => KeyCode::F(2),
        'R' => KeyCode::F(3),
        'S' => KeyCode::F(4),
        _ => return None,
    })
}

/// xterm modifier parameter (1 + bits: shift 1, alt 2, ctrl 4).
fn modifiers_from(param: Option<&str>) -> KeyModifiers {
    let bits = param
        .and_then(|value| value.parse::<u8>().ok())
        .map_or(0, |value| value.saturating_sub(1));
    let mut modifiers = KeyModifiers::NONE;
    if bits & 1 != 0 {
        modifiers |= KeyModifiers::SHIFT;
    }
    if bits & 2 != 0 {
        modifiers |= KeyModifiers::ALT;
    }
    if bits & 4 != 0 {
        modifiers |= KeyModifiers::CONTROL;
    }
    modifiers
}

fn csi_event(params: &str, final_char: char) -> Option<Event> {
    if let Some(mouse) = params.strip_prefix('<') {
        return sgr_mouse(mouse, final_char);
    }
    if params.contains('<') {
        return None;
    }
    let mut parts = params.split(';');
    let first = parts.next().filter(|part| !part.is_empty());
    let modifiers = modifiers_from(parts.next());
    let code = match final_char {
        'A' => KeyCode::Up,
        'B' => KeyCode::Down,
        'C' => KeyCode::Right,
        'D' => KeyCode::Left,
        'H' => KeyCode::Home,
        'F' => KeyCode::End,
        'P' => KeyCode::F(1),
        'Q' => KeyCode::F(2),
        'R' => KeyCode::F(3),
        'S' => KeyCode::F(4),
        'Z' => return Some(key(KeyCode::BackTab, KeyModifiers::SHIFT)),
        'I' if params.is_empty() => return Some(Event::FocusGained),
        'O' if params.is_empty() => return Some(Event::FocusLost),
        '~' => match first?.parse::<u8>().ok()? {
            1 | 7 => KeyCode::Home,
            2 => KeyCode::Insert,
            3 => KeyCode::Delete,
            4 | 8 => KeyCode::End,
            5 => KeyCode::PageUp,
            6 => KeyCode::PageDown,
            number @ 11..=15 => KeyCode::F(number - 10),
            number @ 17..=21 => KeyCode::F(number - 11),
            number @ 23..=24 => KeyCode::F(number - 12),
            _ => return None,
        },
        _ => return None,
    };
    Some(key(code, modifiers))
}

/// `CSI < button ; column ; row (M|m)` (1-based coordinates).
fn sgr_mouse(params: &str, final_char: char) -> Option<Event> {
    if final_char != 'M' && final_char != 'm' {
        return None;
    }
    let mut parts = params.split(';').map(|part| part.parse::<u16>().ok());
    let code = parts.next()??;
    let column = parts.next()??.checked_sub(1)?;
    let row = parts.next()??.checked_sub(1)?;
    if parts.next().is_some() {
        return None;
    }
    let mut modifiers = KeyModifiers::NONE;
    if code & 4 != 0 {
        modifiers |= KeyModifiers::SHIFT;
    }
    if code & 8 != 0 {
        modifiers |= KeyModifiers::ALT;
    }
    if code & 16 != 0 {
        modifiers |= KeyModifiers::CONTROL;
    }
    let button = match code & 3 {
        0 => MouseButton::Left,
        1 => MouseButton::Middle,
        _ => MouseButton::Right,
    };
    let kind = if code & 64 != 0 {
        match code & 3 {
            0 => MouseEventKind::ScrollUp,
            1 => MouseEventKind::ScrollDown,
            2 => MouseEventKind::ScrollLeft,
            _ => MouseEventKind::ScrollRight,
        }
    } else if code & 32 != 0 {
        if code & 3 == 3 {
            MouseEventKind::Moved
        } else {
            MouseEventKind::Drag(button)
        }
    } else if final_char == 'm' {
        MouseEventKind::Up(button)
    } else if code & 3 == 3 {
        MouseEventKind::Up(MouseButton::Left)
    } else {
        MouseEventKind::Down(button)
    };
    Some(Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn esc() -> Event {
        key(KeyCode::Esc, KeyModifiers::NONE)
    }

    fn chars(text: &str) -> Vec<Event> {
        replay(text)
    }

    /// Feeds what crossterm reports for one read, then flushes.
    fn run(events: Vec<Event>) -> Vec<Event> {
        let mut repair = EscapeRepair::new();
        let mut out = Vec::new();
        for event in events {
            out.extend(repair.feed(event));
        }
        out.extend(repair.flush());
        out
    }

    #[test]
    fn esc_then_end_in_one_read() {
        // ESC ESC [ 4 ~ comes out of crossterm as Esc, '[', '4', '~'.
        let mut events = vec![esc()];
        events.extend(chars("[4~"));
        assert_eq!(
            run(events),
            vec![esc(), key(KeyCode::End, KeyModifiers::NONE)]
        );
    }

    #[test]
    fn esc_then_arrows_and_modifiers() {
        let mut events = vec![esc()];
        events.extend(chars("[D"));
        assert_eq!(
            run(events),
            vec![esc(), key(KeyCode::Left, KeyModifiers::NONE)]
        );
        let mut events = vec![esc()];
        events.extend(chars("[1;5C"));
        assert_eq!(
            run(events),
            vec![esc(), key(KeyCode::Right, KeyModifiers::CONTROL)]
        );
        let mut events = vec![esc()];
        events.extend(chars("OH"));
        assert_eq!(
            run(events),
            vec![esc(), key(KeyCode::Home, KeyModifiers::NONE)]
        );
        let mut events = vec![esc()];
        events.extend(chars("[3~"));
        assert_eq!(
            run(events),
            vec![esc(), key(KeyCode::Delete, KeyModifiers::NONE)]
        );
    }

    #[test]
    fn esc_then_mouse_click() {
        let mut events = vec![esc()];
        events.extend(chars("[<0;10;5M"));
        assert_eq!(
            run(events),
            vec![
                esc(),
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 9,
                    row: 4,
                    modifiers: KeyModifiers::NONE,
                })
            ]
        );
    }

    #[test]
    fn typed_text_after_esc_is_kept() {
        let mut events = vec![esc()];
        events.extend(chars("[link]"));
        assert_eq!(run(events), {
            let mut expected = vec![esc()];
            expected.extend(chars("[link]"));
            expected
        });
        let mut events = vec![esc()];
        events.extend(chars("hello"));
        let mut expected = vec![esc()];
        expected.extend(chars("hello"));
        assert_eq!(run(events), expected);
        // A lone '[' typed right after Esc is released by the flush.
        assert_eq!(run(vec![esc(), chars("[")[0].clone()]), {
            let mut expected = vec![esc()];
            expected.extend(chars("["));
            expected
        });
    }

    #[test]
    fn other_events_pass_through() {
        let enter = key(KeyCode::Enter, KeyModifiers::NONE);
        let mut events = vec![esc()];
        events.extend(chars("["));
        events.push(enter.clone());
        let mut expected = vec![esc()];
        expected.extend(chars("["));
        expected.push(enter.clone());
        assert_eq!(run(events), expected);
        assert_eq!(run(chars("[4~")), chars("[4~"));
        // Two Escs, then a key: both Escs are kept.
        let mut events = vec![esc(), esc()];
        events.extend(chars("[A"));
        assert_eq!(
            run(events),
            vec![esc(), esc(), key(KeyCode::Up, KeyModifiers::NONE)]
        );
    }
}
