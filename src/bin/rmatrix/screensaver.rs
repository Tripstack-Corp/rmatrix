//! What ends `--screensaver` mode.
//!
//! A screensaver is dismissed by touching anything, so any key press or any
//! mouse report ends it — including plain movement with no button held, which
//! is what someone returning to the machine actually does first. Mouse reports
//! only arrive because `term::setup` asks for them in this mode; see
//! `term::mouse_capture_sequence`.
//!
//! Waking it with the mouse leaves a tail behind, which [`drain_mouse_tail`]
//! swallows before the shell gets the terminal back.

use crossterm::event::{self, Event, KeyEventKind};
use std::time::{Duration, Instant};

/// Input this soon after startup does not end the screensaver.
///
/// Any-motion tracking reports the smallest twitch of a hand still resting on
/// the trackpad, and keys typed just before launch sit in the tty's input
/// queue and are read on the first poll. Either would close the screensaver
/// before it had drawn a frame. A quarter of a second is too short to notice
/// when dismissing it on purpose.
pub(crate) const GRACE: Duration = Duration::from_millis(250);

/// Whether `event`, arriving `since_start` after launch, ends the screensaver.
pub(crate) fn dismisses(event: &Event, since_start: Duration) -> bool {
    since_start >= GRACE
        && match event {
            Event::Key(key) => key.kind != KeyEventKind::Release,
            Event::Mouse(_) => true,
            Event::FocusGained | Event::FocusLost | Event::Paste(_) | Event::Resize(..) => false,
        }
}

/// How long input has to stay silent before the gesture is taken to be over.
///
/// The teardown turns mouse capture off, but the terminal only acts on that
/// once it has parsed everything queued ahead of it, and a terminal drawing
/// this program at full screen is usually behind. Every report it sends in
/// that gap lands in the shell's input as `35;12;7M` and wrecks the next
/// command line. On a pty that reports motion every 8 ms until it parses the
/// `?1003l`, a terminal one frame behind (16 ms) leaked one report and one
/// 50 ms behind leaked five; with this drain, none in either case.
///
/// Waiting for silence rather than a fixed time is what makes it safe: while
/// the hand is still moving, reports keep arriving until the terminal catches
/// up, so the gap only opens once nothing more can come. 100 ms covers a
/// terminal up to 100 ms behind, which is several frames of the worst case
/// measured, and it costs nothing visible — the shell is already back on
/// screen while it runs.
const QUIET: Duration = Duration::from_millis(100);

/// The longest the drain may hold the exit open, should the input never go
/// quiet — a terminal that never acts on `?1003l` while the mouse keeps moving.
const DRAIN_CAP: Duration = Duration::from_millis(500);

/// Read and discard input until the mouse gesture that woke the screensaver is
/// over. Must run after mouse capture is turned off and before raw mode is,
/// or the tail is echoed onto the screen as well as typed into the shell.
///
/// Only called after a *mouse* wake. Keys typed after a keypress wake carry no
/// tail, belong to whoever is typing, and are left for the shell.
pub(crate) fn drain_mouse_tail() {
    drain(DRAIN_CAP, |quiet| {
        matches!(event::poll(quiet), Ok(true)) && event::read().is_ok()
    });
}

/// The stopping rule behind [`drain_mouse_tail`], with the input source passed
/// in so it is testable without a terminal. `discard_one(timeout)` waits up to
/// `timeout` for one event and reports whether it consumed one.
fn drain(cap: Duration, mut discard_one: impl FnMut(Duration) -> bool) {
    let end = Instant::now() + cap;
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        if !discard_one(QUIET.min(left)) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventState, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

    const AWAKE: Duration = Duration::from_secs(5);

    fn mouse(kind: MouseEventKind) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: 10,
            row: 4,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn key(code: KeyCode, kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind,
            state: KeyEventState::NONE,
        })
    }

    #[test]
    fn every_kind_of_mouse_activity_dismisses_the_screensaver() {
        // Inside tmux with `mouse on`, these are the gestures tmux forwards. A
        // screensaver that ignored any of them would sit there while the user
        // waved at it.
        for kind in [
            MouseEventKind::Moved,
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::ScrollUp,
            MouseEventKind::ScrollDown,
            MouseEventKind::ScrollLeft,
            MouseEventKind::ScrollRight,
        ] {
            assert!(dismisses(&mouse(kind), AWAKE), "{kind:?} should dismiss");
        }
    }

    #[test]
    fn any_key_press_dismisses_the_screensaver() {
        for code in [
            KeyCode::Char('a'),
            KeyCode::Char(' '),
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::F(5),
        ] {
            assert!(dismisses(&key(code, KeyEventKind::Press), AWAKE));
            assert!(dismisses(&key(code, KeyEventKind::Repeat), AWAKE));
        }
    }

    #[test]
    fn a_key_release_alone_does_not_dismiss_the_screensaver() {
        // Releases only arrive under the kitty keyboard protocol, where the key
        // that launched rmatrix can be released after it starts. That release
        // is the tail of the launch, not someone coming back.
        assert!(!dismisses(
            &key(KeyCode::Enter, KeyEventKind::Release),
            AWAKE
        ));
    }

    #[test]
    fn resizing_or_refocusing_the_window_does_not_dismiss_the_screensaver() {
        // A tiling window manager or tmux re-layout can resize an idle pane,
        // and nobody touched anything.
        for event in [Event::Resize(80, 24), Event::FocusGained, Event::FocusLost] {
            assert!(!dismisses(&event, AWAKE), "{event:?} should not dismiss");
        }
    }

    #[test]
    fn input_during_the_startup_grace_is_ignored() {
        let moved = mouse(MouseEventKind::Moved);
        let pressed = key(KeyCode::Char('a'), KeyEventKind::Press);
        for since_start in [
            Duration::ZERO,
            Duration::from_millis(100),
            GRACE - Duration::from_millis(1),
        ] {
            assert!(!dismisses(&moved, since_start), "{since_start:?}");
            assert!(!dismisses(&pressed, since_start), "{since_start:?}");
        }
        assert!(dismisses(&moved, GRACE));
        assert!(dismisses(&pressed, GRACE));
    }

    #[test]
    fn the_drain_swallows_the_tail_and_stops_at_the_first_quiet_gap() {
        // The tail of a gesture is a burst of reports and then silence. Every
        // report in the burst has to be eaten — each one that escapes is typed
        // into the shell — and the silence has to end it, or the drain eats
        // whatever the user types next as well.
        let mut calls = 0;
        drain(Duration::from_secs(5), |timeout| {
            assert!(timeout <= QUIET, "waited {timeout:?} for one event");
            calls += 1;
            calls <= 3
        });
        assert_eq!(calls, 4, "three reports, then one silent wait");
    }

    #[test]
    fn a_mouse_that_never_goes_quiet_cannot_hold_the_exit_open() {
        // A terminal that never acts on ?1003l, with the hand still moving,
        // sends reports forever. Exiting has to win regardless.
        let cap = Duration::from_millis(30);
        let started = Instant::now();
        drain(cap, |timeout| {
            std::thread::sleep(timeout.min(Duration::from_millis(1)));
            true
        });
        let took = started.elapsed();
        assert!(took >= cap, "gave up after {took:?}, before the cap");
        assert!(took < cap + Duration::from_secs(1), "held on for {took:?}");
    }
}
