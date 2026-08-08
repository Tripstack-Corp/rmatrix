//! Telling a held key apart from a pressed one.
//!
//! A terminal delivers auto-repeat as bytes indistinguishable from a real
//! keypress. `KeyEventKind::Repeat` exists in crossterm but only arrives under
//! the kitty keyboard protocol, which we do not enable, so on every terminal
//! this program actually runs on the only signal available is timing.
//!
//! That matters because most of the keys here *toggle*. Holding space used to
//! deliver a stream of toggles and leave `paused` set to the parity of however
//! many repeats the terminal happened to send — a coin flip, and half the time
//! the coin came up "frozen screen". Holding `r`, `b` or `f` was worse: each one
//! forces a full repaint, and at 204x175 that took output from 0.9 MB/s to
//! 4.8 MB/s, which is well past what the terminal can absorb.

use crossterm::event::KeyCode;
use std::time::{Duration, Instant};

/// Two presses of the same key closer together than this are auto-repeat.
///
/// It has to sit *above* the repeat interval or a held key still gets through:
/// macOS defaults to 90 ms and bottoms out at 30 ms on the fastest slider
/// setting, and X11 typically repeats at 33 ms. 150 ms clears all of those.
///
/// The cost is the other side of the same guess: deliberate taps faster than
/// about seven a second are read as a held key and dropped. For a pause toggle
/// and a charset cycle that is a trade worth making — nobody taps pause at 7 Hz
/// on purpose, and the alternative is the coin flip.
const REPEAT_WINDOW: Duration = Duration::from_millis(150);

/// Filters terminal auto-repeat out of a key stream.
#[derive(Default)]
pub(crate) struct Repeat {
    last: Option<(KeyCode, Instant)>,
}

impl Repeat {
    /// Whether this keypress should be acted on.
    ///
    /// Quit keys must be checked *before* this: swallowing a quit because the
    /// user happened to press it twice is never the right call, and quitting is
    /// idempotent so repeats cost nothing.
    pub(crate) fn accept(&mut self, code: KeyCode, now: Instant) -> bool {
        let held =
            matches!(self.last, Some((c, t)) if c == code && now.duration_since(t) < REPEAT_WINDOW);
        // The timestamp advances even when the event is suppressed. Only
        // refreshing it on acceptance would let every second repeat through: at
        // a 90 ms repeat against a 150 ms window, the third event would measure
        // 180 ms from the last *accepted* one and be taken for a fresh press.
        self.last = Some((code, now));
        !held
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n` presses of `code` spaced `gap` apart, counting how many get through.
    fn accepted(code: KeyCode, n: usize, gap: Duration) -> usize {
        let mut r = Repeat::default();
        let t0 = Instant::now();
        (0..n)
            .filter(|i| r.accept(code, t0 + gap * u32::try_from(*i).expect("small count")))
            .count()
    }

    #[test]
    fn a_held_key_acts_once_however_long_it_is_held() {
        // The bug: every repeat toggled `paused`, so holding space left it set
        // to the parity of the repeat count. Measured on a pty, an odd number of
        // presses left the rain frozen and an even number left it running, with
        // nothing else distinguishing them.
        for gap_ms in [15u64, 30, 90, 149] {
            for n in [1usize, 2, 3, 4, 5, 20, 21, 100] {
                assert_eq!(
                    accepted(KeyCode::Char(' '), n, Duration::from_millis(gap_ms)),
                    1,
                    "{n} presses {gap_ms}ms apart should act once"
                );
            }
        }
    }

    #[test]
    fn the_window_clears_every_auto_repeat_rate_we_expect_to_meet() {
        // If REPEAT_WINDOW ever drops below a platform's repeat interval the
        // filter silently stops working, and the failure looks like the original
        // bug rather than like a broken constant.
        for (platform, interval_ms) in [
            ("macOS default", 90u64),
            ("macOS fastest slider", 30),
            ("macOS defaults-write floor", 15),
            ("X11 typical", 33),
        ] {
            assert!(
                REPEAT_WINDOW > Duration::from_millis(interval_ms),
                "{platform} repeats every {interval_ms}ms, inside the window"
            );
        }
    }

    #[test]
    fn deliberate_presses_still_get_through() {
        // The filter is only worth having if it leaves ordinary use alone.
        for gap_ms in [150u64, 200, 400, 1000] {
            assert_eq!(
                accepted(KeyCode::Char(' '), 5, Duration::from_millis(gap_ms)),
                5,
                "presses {gap_ms}ms apart are a human, not auto-repeat"
            );
        }
    }

    #[test]
    fn a_different_key_is_never_suppressed() {
        // Auto-repeat only ever repeats one key, so a change of key is always a
        // real press however fast it arrives.
        let mut r = Repeat::default();
        let t0 = Instant::now();
        let fast = Duration::from_millis(5);
        for (i, code) in [
            KeyCode::Char('b'),
            KeyCode::Char('f'),
            KeyCode::Char('r'),
            KeyCode::Char('c'),
            KeyCode::Esc,
        ]
        .into_iter()
        .enumerate()
        {
            assert!(
                r.accept(code, t0 + fast * u32::try_from(i).expect("small count")),
                "{code:?} followed a different key and must be accepted"
            );
        }
    }

    #[test]
    fn releasing_and_pressing_again_works() {
        // Hold, let go, press again: the second press must register, or the
        // filter would make the key feel broken after any held press.
        let mut r = Repeat::default();
        let t0 = Instant::now();
        assert!(r.accept(KeyCode::Char(' '), t0));
        for i in 1..30u32 {
            assert!(!r.accept(KeyCode::Char(' '), t0 + Duration::from_millis(30) * i));
        }
        let after_release = t0 + Duration::from_millis(30 * 29) + REPEAT_WINDOW;
        assert!(r.accept(KeyCode::Char(' '), after_release));
    }
}
