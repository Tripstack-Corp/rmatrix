//! Asking the terminal for its own colours, for `--color terminal`.
//!
//! The six chromatic ANSI entries come back over the tty as OSC 4 replies. The
//! hard part is not parsing them but waiting for them: a terminal may answer
//! late, answer some and not others, or not answer at all, and every way of
//! getting that wrong leaves the program stuck or leaves stray bytes behind
//! for the key handler to read as typing.

use rmatrix::Rgb;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Read, Write};
use std::time::{Duration, Instant};

/// Red, yellow, green, cyan, blue, magenta: the chromatic entries. Black,
/// white and the greys are left out because rain in them reads as no colour.
const INDEXES: [u8; 6] = [1, 3, 2, 6, 4, 5];

/// Primary device attributes, sent after the palette queries.
///
/// Every terminal in use answers DA1, and terminals answer in the order they
/// were asked, so its reply arriving means every palette reply that is coming
/// has already come. That is what makes the wait both quick and safe: quick
/// because a terminal that ignores OSC 4 is known the moment it answers DA1,
/// and safe because nothing is still in flight when we stop listening.
const DA1: &[u8] = b"\x1b[c";

/// How long to wait for a terminal that answers nothing at all, not even DA1.
///
/// Any terminal that answers DA1 ends the wait as soon as it does, so this only
/// governs the silent case, where its cost is a slower start. A short timeout
/// is what hurts. A reply that lands after we stop listening is read by
/// crossterm as typing: the `r`, `b`, `c` and `f` in `rgb:cdcd/3131/3131`
/// toggle rainbow, bold, charset and stats, and under `--screensaver` the first
/// byte quits. The first version waited 100 ms, which an ssh round trip to the
/// other side of the world outlasts; a second does not.
const TIMEOUT: Duration = Duration::from_secs(1);

/// How often to look again while waiting.
///
/// The tty is read non-blocking and re-checked on this interval, rather than
/// waited on with poll(2), because macOS poll() does not work on terminals —
/// "The poll() system call currently does not support devices", `man 2 poll`.
/// It reports the tty readable at once, the read that follows blocks, and with
/// a terminal that did not answer, rmatrix sat on a blank screen until a key
/// was pressed and then swallowed the key. A sleep loop is crude, but it runs
/// once, at startup, for at most `TIMEOUT`.
const RECHECK: Duration = Duration::from_millis(2);

/// More than this is not a palette reply; stop rather than buffer it.
const MAX_REPLY: usize = 4096;

/// The terminal's six chromatic colours, or `None` if it does not say.
///
/// Must run in raw mode, so the replies are neither echoed nor held back for a
/// newline, and before crossterm first reads input, so the two never compete
/// for the same bytes.
pub(crate) fn query() -> Option<[Rgb; 6]> {
    let mut tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;
    // Opening /dev/tty creates a new open file description, and non-blocking is
    // a property of the description, so this changes our handle only — never
    // the stdin crossterm reads afterwards.
    rustix::io::ioctl_fionbio(&tty, true).ok()?;
    let mut ask = Vec::new();
    for index in INDEXES {
        write!(ask, "\x1b]4;{index};?\x07").ok()?;
    }
    ask.extend_from_slice(DA1);
    tty.write_all(&ask).ok()?;
    tty.flush().ok()?;
    parse_palette(&read_replies(&mut tty, TIMEOUT))
}

/// Everything the terminal sends until it has answered DA1, or `timeout`
/// passes. `tty` must be non-blocking: `WouldBlock` means nothing yet.
fn read_replies(tty: &mut impl Read, timeout: Duration) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 256];
    while !answered(&bytes) && bytes.len() <= MAX_REPLY && Instant::now() < deadline {
        match tty.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => bytes.extend_from_slice(chunk.get(..n).unwrap_or_default()),
            Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(RECHECK),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    bytes
}

/// Whether `bytes` holds a DA1 reply, `ESC [ ? <digits and ;> c`.
fn answered(bytes: &[u8]) -> bool {
    let mut rest = bytes;
    while let Some(at) = rest.windows(3).position(|w| w == b"\x1b[?") {
        rest = rest.get(at + 3..).unwrap_or_default();
        let end = rest.iter().find(|b| !b.is_ascii_digit() && **b != b';');
        if end == Some(&b'c') {
            return true;
        }
    }
    false
}

fn parse_palette(bytes: &[u8]) -> Option<[Rgb; 6]> {
    let mut colors = [None; 6];
    let mut start = 0usize;
    while let Some(offset) = bytes.get(start..)?.windows(4).position(|w| w == b"\x1b]4;") {
        let sequence_start = start + offset + 4;
        let rest = bytes.get(sequence_start..)?;
        let bel = rest.iter().position(|b| *b == b'\x07');
        let st = rest.windows(2).position(|w| w == b"\x1b\\");
        let end = match (bel, st) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        };
        let Some(end) = end else {
            break;
        };
        if let Ok(reply) = std::str::from_utf8(&rest[..end])
            && let Some((index, rgb)) = parse_palette_reply(reply)
            && let Some(slot) = INDEXES.iter().position(|candidate| *candidate == index)
        {
            colors[slot] = Some(rgb);
        }
        start = sequence_start
            + end
            + if rest.get(end) == Some(&b'\x07') {
                1
            } else {
                2
            };
    }
    Some([
        colors[0]?, colors[1]?, colors[2]?, colors[3]?, colors[4]?, colors[5]?,
    ])
}

fn parse_palette_reply(reply: &str) -> Option<(u8, Rgb)> {
    let (index, spec) = reply.split_once(';')?;
    let index = index.parse().ok()?;
    let components = spec.strip_prefix("rgb:")?;
    let mut parts = components.split('/');
    let r = scale_hex(parts.next()?)?;
    let g = scale_hex(parts.next()?)?;
    let b = scale_hex(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    Some((index, (r, g, b)))
}

fn scale_hex(component: &str) -> Option<u8> {
    if component.is_empty()
        || component.len() > 4
        || !component.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    let value = u32::from_str_radix(component, 16).ok()?;
    let max = (1u32 << (component.len() * 4)) - 1;
    Some(((value * 255 + max / 2) / max) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io;

    /// A non-blocking tty that delivers `chunks` one read at a time, then has
    /// nothing more to say: `None` in the script is a read that would block.
    struct Tty(VecDeque<Option<&'static [u8]>>);

    impl Read for Tty {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.0.pop_front() {
                Some(Some(chunk)) => {
                    buf[..chunk.len()].copy_from_slice(chunk);
                    Ok(chunk.len())
                }
                _ => Err(ErrorKind::WouldBlock.into()),
            }
        }
    }

    fn tty(script: &[Option<&'static [u8]>]) -> Tty {
        Tty(script.iter().copied().collect())
    }

    const PALETTE: &[u8] = b"\x1b]4;1;rgb:cdcd/3131/3131\x07\x1b]4;3;rgb:e5e5/e5e5/1010\x07\
                             \x1b]4;2;rgb:0d0d/bcbc/7979\x07\x1b]4;6;rgb:1111/a8a8/cdcd\x07\
                             \x1b]4;4;rgb:2424/7272/c8c8\x07\x1b]4;5;rgb:bcbc/3f3f/bcbc\x07";

    #[test]
    fn the_wait_ends_as_soon_as_the_terminal_answers_device_attributes() {
        // Replies straggling in over several reads, with gaps, then DA1. The
        // wait has to collect all of it and then stop at once, not sit out
        // the timeout.
        let mut t = tty(&[
            None,
            Some(&PALETTE[..40]),
            None,
            None,
            Some(&PALETTE[40..]),
            Some(b"\x1b[?62;22;52c"),
        ]);
        let started = Instant::now();
        let replies = read_replies(&mut t, Duration::from_secs(30));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "sat out the timeout"
        );
        assert_eq!(
            parse_palette(&replies),
            Some([
                (205, 49, 49),
                (229, 229, 16),
                (13, 188, 121),
                (17, 168, 205),
                (36, 114, 200),
                (188, 63, 188),
            ])
        );
    }

    #[test]
    fn a_terminal_that_ignores_palette_queries_is_known_by_its_da1_reply() {
        // Answering DA1 and nothing else means no palette is coming. Knowing
        // that at once is what keeps startup instant on such terminals.
        let mut t = tty(&[Some(b"\x1b[?1;2c")]);
        let started = Instant::now();
        let replies = read_replies(&mut t, Duration::from_secs(30));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "sat out the timeout"
        );
        assert_eq!(parse_palette(&replies), None);
    }

    #[test]
    fn a_terminal_that_says_nothing_is_given_up_on_at_the_timeout() {
        // The bug this replaced: with a silent terminal on macOS the wait never
        // ended until a key was pressed, and then it ate the key.
        let mut t = tty(&[]);
        let timeout = Duration::from_millis(30);
        let started = Instant::now();
        let replies = read_replies(&mut t, timeout);
        let took = started.elapsed();
        assert!(replies.is_empty());
        assert!(took >= timeout, "gave up after {took:?}");
        assert!(
            took < timeout + Duration::from_secs(1),
            "held on for {took:?}"
        );
    }

    #[test]
    fn only_a_complete_da1_reply_ends_the_wait() {
        assert!(answered(b"\x1b[?62;22;52c"));
        assert!(answered(b"noise\x1b]4;1;rgb:ff/00/00\x07\x1b[?1;2c"));
        // Still arriving.
        assert!(!answered(b"\x1b[?62;22"));
        // Other replies that start the same way. A mouse report can arrive
        // during the wait under --screensaver, and a keyboard-protocol reply
        // ends in `u`.
        assert!(!answered(b"\x1b[<35;10;5M"));
        assert!(!answered(b"\x1b[?1u"));
    }

    #[test]
    fn osc_palette_replies_parse_in_any_order_with_bel_or_st() {
        let replies = b"noise\x1b]4;5;rgb:aaaa/1111/bbbb\x1b\\\x1b]4;1;rgb:ff/00/00\x07\
                        \x1b]4;4;rgb:1234/5678/9abc\x07\x1b]4;2;rgb:00/ff/00\x07\
                        \x1b]4;6;rgb:00/ffff/ffff\x1b\\\x1b]4;3;rgb:ffff/8000/0000\x07";
        assert_eq!(
            parse_palette(replies),
            Some([
                (255, 0, 0),
                (255, 128, 0),
                (0, 255, 0),
                (0, 255, 255),
                (18, 86, 154),
                (170, 17, 187),
            ])
        );
    }

    #[test]
    fn osc_rgb_components_scale_from_one_to_four_hex_digits() {
        assert_eq!(parse_palette_reply("1;rgb:f/8/0"), Some((1, (255, 136, 0))));
        assert_eq!(
            parse_palette_reply("2;rgb:ff/80/00"),
            Some((2, (255, 128, 0)))
        );
        assert_eq!(
            parse_palette_reply("3;rgb:fff/800/000"),
            Some((3, (255, 128, 0)))
        );
        assert_eq!(
            parse_palette_reply("4;rgb:ffff/8080/0000"),
            Some((4, (255, 128, 0)))
        );
    }

    #[test]
    fn incomplete_or_malformed_palette_replies_use_the_fallback() {
        assert_eq!(parse_palette(b"\x1b]4;1;rgb:ff/00/00\x07"), None);
        for bad in [
            "1;rgba:ff/00/00",
            "1;rgb:gg/00/00",
            "1;rgb:ff/00",
            "1;rgb:ff/00/00/ff",
            "x;rgb:ff/00/00",
        ] {
            assert_eq!(parse_palette_reply(bad), None, "accepted {bad:?}");
        }
    }
}
