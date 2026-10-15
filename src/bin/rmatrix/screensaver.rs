//! What ends `--screensaver` mode.
//!
//! A screensaver is dismissed by touching anything, so any key press or any
//! mouse report ends it — including plain movement with no button held, which
//! is what someone returning to the machine actually does first. Mouse reports
//! only arrive because `term::setup` asks for them in this mode; see
//! `term::mouse_capture_sequence`.

use crossterm::event::{Event, KeyEventKind};
use std::time::Duration;

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
}
