//! Terminal setup and teardown, and the event loop.
//!
//! The terminal is restored on normal exit, on error (the guard's `Drop`)
//! and on panic (a panic hook), so a crash never leaves the pane in raw mode.
//! SIGHUP / SIGTERM / SIGINT (Lumbridge TUI closing the pane, `kill`) and terminal
//! I/O errors end the loop through the same path, after keeping any unsaved
//! text (saved, or written to a recovery file).

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;

use lumbridge_notes::app::App;
use lumbridge_notes::escape::EscapeRepair;
use lumbridge_notes::ui;

/// Whether the terminal is in our mode (so restoring is needed).
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Whether keyboard enhancement flags were pushed (and must be popped).
static ENHANCED: AtomicBool = AtomicBool::new(false);
/// Longest wait between checks for a stop signal. Lumbridge TUI sends SIGHUP, then
/// SIGTERM and SIGKILL 250 ms apart, so the save must start well within that.
const SIGNAL_CHECK: Duration = Duration::from_millis(50);
/// How long the event loop gets to save and exit by itself after a stop
/// signal before the watchdog does it.
const EXIT_GRACE: Duration = Duration::from_millis(100);

struct Guard;

impl Guard {
    fn enter() -> io::Result<Guard> {
        enable_raw_mode()?;
        ACTIVE.store(true, Ordering::SeqCst);
        let guard = Guard;
        let mut out = io::stdout();
        execute!(
            out,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            EnableFocusChange
        )?;
        // Lets Ctrl-Backspace, Ctrl-Shift-Z and friends arrive distinctly.
        if matches!(
            crossterm::terminal::supports_keyboard_enhancement(),
            Ok(true)
        ) {
            let pushed = execute!(
                out,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            );
            if pushed.is_ok() {
                ENHANCED.store(true, Ordering::SeqCst);
            }
        }
        Ok(guard)
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        restore();
    }
}

/// Puts the terminal back the way we found it. Safe to call more than once.
fn restore() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut out = io::stdout();
    if ENHANCED.swap(false, Ordering::SeqCst) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        out,
        DisableFocusChange,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        cursor::Show
    );
    let _ = disable_raw_mode();
    let _ = out.flush();
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

/// Sets `stop` when the process is asked to end. Best effort: without the
/// handlers the default action (exit) still applies.
fn register_stop_signals(stop: &Arc<AtomicBool>) {
    use signal_hook::consts::{SIGINT, SIGTERM};
    for signal in [SIGTERM, SIGINT] {
        let _ = signal_hook::flag::register(signal, Arc::clone(stop));
    }
    #[cfg(unix)]
    {
        let _ = signal_hook::flag::register(signal_hook::consts::SIGHUP, Arc::clone(stop));
    }
}

/// Runs the editor until the user quits, a stop signal arrives or the
/// terminal fails. Returns the loop's result and a line to print once the
/// terminal is restored (where unsaved text went, when it could not be saved
/// normally).
pub fn run(app: App) -> (io::Result<()>, Option<String>) {
    install_panic_hook();
    let stop = Arc::new(AtomicBool::new(false));
    register_stop_signals(&stop);
    let guard = match Guard::enter() {
        Ok(guard) => guard,
        Err(err) => return (Err(err), None),
    };
    let shared = Arc::new(Shared {
        app: Mutex::new(app),
        stop,
        exiting: AtomicBool::new(false),
    });
    spawn_watchdog(Arc::clone(&shared));
    let result = event_loop(&shared);
    // A normal quit (Ctrl-Q) already saved, or the user chose to discard.
    let note = if result.is_err() || shared.stop.load(Ordering::SeqCst) {
        claim_exit_save(&shared)
    } else {
        shared.exiting.store(true, Ordering::SeqCst);
        // Stop audio still playing or recording in a child process.
        lock(&shared).stop_assist();
        None
    };
    drop(guard);
    (result, note)
}

/// What the event loop and the stop watchdog share.
struct Shared {
    app: Mutex<App>,
    stop: Arc<AtomicBool>,
    /// Set by whoever runs the exit save (or after a normal quit), so it
    /// happens once.
    exiting: AtomicBool,
}

fn lock(shared: &Shared) -> MutexGuard<'_, App> {
    match shared.app.lock() {
        Ok(guard) => guard,
        // A panic elsewhere left it poisoned; the data is still the best copy.
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Runs the exit save unless the other side already did.
fn claim_exit_save(shared: &Shared) -> Option<String> {
    if shared.exiting.swap(true, Ordering::SeqCst) {
        return None;
    }
    lock(shared).save_before_exit()
}

/// When a stop signal arrives and the event loop does not wind down in time
/// (crossterm can spin inside its read once the pane's terminal is gone, and
/// never return to check the signal), saves the note from here and exits.
/// The event loop holds the lock only between waits, so it is free then.
fn spawn_watchdog(shared: Arc<Shared>) {
    let spawned = std::thread::Builder::new()
        .name("lumbridge-notes-stop".to_owned())
        .spawn(move || {
            while !shared.stop.load(Ordering::SeqCst) {
                if shared.exiting.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(SIGNAL_CHECK);
            }
            // Give the event loop the first chance (it restores the
            // terminal on its way out).
            std::thread::sleep(EXIT_GRACE);
            if shared.exiting.load(Ordering::SeqCst) {
                return;
            }
            let note = claim_exit_save(&shared);
            restore();
            if let Some(note) = note {
                eprintln!("lumbridge-notes: {note}");
            }
            std::process::exit(0);
        });
    // Without the thread the event loop still handles signals it sees.
    drop(spawned);
}

fn event_loop(shared: &Shared) -> io::Result<()> {
    let stop = &shared.stop;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    let mut repair = EscapeRepair::new();
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        let deadline = {
            let mut app = lock(shared);
            let size = terminal.size()?;
            app.prepare(Rect::new(0, 0, size.width, size.height));
            terminal.draw(|frame| ui::render(&app, frame))?;
            if !app.osc52.is_empty() {
                let mut out = io::stdout();
                for sequence in app.osc52.drain(..) {
                    out.write_all(sequence.as_bytes())?;
                }
                out.flush()?;
            }
            if app.quit {
                break;
            }
            Instant::now() + app.next_wakeup(Instant::now())
        };
        // Wait without the lock, so the watchdog can save if this hangs.
        if wait_for_event(deadline, stop)? {
            let event = event::read()?;
            dispatch(shared, repair.feed(event));
            // Handle everything already queued (a paste burst, fast typing)
            // before drawing again.
            loop {
                // Never hold the lock while crossterm reads (see the watchdog).
                let quit = lock(shared).quit;
                if quit || stop.load(Ordering::SeqCst) || !poll(Duration::ZERO)? {
                    break;
                }
                let event = event::read()?;
                dispatch(shared, repair.feed(event));
            }
            // Nothing else arrived with them: characters held back after an
            // Esc were typed, not the tail of an escape sequence.
            dispatch(shared, repair.flush());
        }
        lock(shared).tick(Instant::now());
    }
    terminal.show_cursor()?;
    Ok(())
}

/// Hands repaired events to the app.
fn dispatch(shared: &Shared, events: Vec<Event>) {
    if events.is_empty() {
        return;
    }
    let mut app = lock(shared);
    for event in events {
        if app.quit {
            break;
        }
        app.handle_event(event, Instant::now());
    }
}

/// Waits until an event is ready (true), `deadline` passes or a stop signal
/// arrives (false), checking for the signal every [`SIGNAL_CHECK`].
fn wait_for_event(deadline: Instant, stop: &AtomicBool) -> io::Result<bool> {
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if poll(left.min(SIGNAL_CHECK))? {
            return Ok(true);
        }
        if left <= SIGNAL_CHECK {
            return Ok(false);
        }
    }
}

/// `event::poll`, treating a signal interrupting the wait as "no event".
fn poll(timeout: Duration) -> io::Result<bool> {
    match event::poll(timeout) {
        Err(err) if err.kind() == io::ErrorKind::Interrupted => Ok(false),
        other => other,
    }
}
