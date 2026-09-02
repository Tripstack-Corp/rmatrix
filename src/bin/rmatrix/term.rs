//! Terminal state, and where drawn frames go.
//!
//! Everything in here is about the file descriptor rather than the animation:
//! putting the terminal into raw mode and the alternate screen, putting it back
//! afterwards even if we panic, and handing frames to whoever writes them.

use anyhow::{Context, Result};
use crossterm::style::{Attribute, SetAttribute};
use crossterm::terminal::{
    self, Clear, ClearType, DisableLineWrap, EnableLineWrap, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::{ExecutableCommand, QueueableCommand, cursor};
use rmatrix::writer::{self, FrameWriter};
use rmatrix::Rgb;
use rustix::event::{PollFd, PollFlags, poll};
use rustix::time::Timespec;
use std::fs::{File, OpenOptions};
use std::io::{Read, Stdout, Write, stdout};
use std::time::{Duration, Instant};

const PALETTE_INDEXES: [u8; 6] = [1, 3, 2, 6, 4, 5];
const PALETTE_TIMEOUT: Duration = Duration::from_millis(100);

/// Where drawn frames go.
///
/// The loop always renders into a plain `Vec<u8>`, which can neither block nor
/// fail; the only difference between the variants is who hands those bytes to
/// the file descriptor and when.
pub(crate) enum Sink {
    /// A writer thread owns stdout. The loop never blocks on the terminal.
    Threaded(FrameWriter),
    /// The pre-writer-thread path: write and flush inline, on the simulation
    /// thread, and stall there when the terminal is behind. Retained only so
    /// `--sync-io` can measure the thing we are fixing.
    Blocking { free: Option<Vec<u8>>, out: Stdout },
}

impl Sink {
    pub(crate) fn new(sync_io: bool, capacity: usize) -> Sink {
        if sync_io {
            Sink::Blocking {
                free: Some(Vec::with_capacity(capacity)),
                out: stdout(),
            }
        } else {
            Sink::Threaded(FrameWriter::spawn(stdout(), capacity))
        }
    }

    /// A cleared buffer to draw into, or `None` if the terminal is still
    /// swallowing the previous frame. `None` means **do not draw**: skipping the
    /// draw is exactly how frames coalesce without the damage tracker ever
    /// losing track of the screen.
    pub(crate) fn acquire(&mut self) -> Option<Vec<u8>> {
        match self {
            Sink::Threaded(w) => w.acquire(),
            Sink::Blocking { free, .. } => free.take().map(|mut b| {
                b.clear();
                b
            }),
        }
    }

    /// Hand off a drawn frame. Must never silently discard it — every byte the
    /// renderer produced has already been recorded in its `prev` buffer.
    pub(crate) fn submit(&mut self, buf: Vec<u8>) -> Result<()> {
        match self {
            Sink::Threaded(w) => Ok(w.submit(buf)?),
            Sink::Blocking { free, out } => {
                out.write_all(&buf)?;
                out.flush()?;
                *free = Some(buf);
                Ok(())
            }
        }
    }

    pub(crate) fn failed(&self) -> bool {
        match self {
            Sink::Threaded(w) => w.failed(),
            Sink::Blocking { .. } => false,
        }
    }

    /// Emit `last` and stop. Returns true if the sequence definitely reached the
    /// terminal; on false the caller falls back to writing it directly.
    pub(crate) fn shutdown(&mut self, last: Vec<u8>) -> bool {
        match self {
            // A second is far longer than any healthy terminal needs to accept
            // one frame, and short enough that quitting never feels hung.
            Sink::Threaded(w) => w.shutdown(last, Duration::from_secs(1)),
            Sink::Blocking { out, .. } => out.write_all(&last).and_then(|()| out.flush()).is_ok(),
        }
    }
}

/// Ask the terminal for its six chromatic ANSI palette entries.
///
/// OSC 4 replies arrive on the controlling tty, not stdout. This runs before
/// crossterm starts its event reader so the two never compete for input. Failure
/// is deliberately silent: palette queries are optional terminal functionality.
pub(crate) fn query_palette() -> Option<[Rgb; 6]> {
    let mut tty = OpenOptions::new().read(true).write(true).open("/dev/tty").ok()?;
    for index in PALETTE_INDEXES {
        write!(tty, "\x1b]4;{index};?\x07").ok()?;
    }
    tty.flush().ok()?;
    read_palette(&mut tty, PALETTE_TIMEOUT)
}

fn read_palette(tty: &mut File, timeout: Duration) -> Option<[Rgb; 6]> {
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::with_capacity(256);
    let mut chunk = [0u8; 256];
    loop {
        if let Some(palette) = parse_palette(&bytes) {
            return Some(palette);
        }
        let remaining = deadline.checked_duration_since(Instant::now())?;
        let timeout = Timespec::try_from(remaining).ok()?;
        let mut fds = [PollFd::new(tty, PollFlags::IN)];
        if poll(&mut fds, Some(&timeout)).ok()? == 0 {
            return None;
        }
        let n = tty.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..n]);
        if bytes.len() > 4096 {
            return None;
        }
    }
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
            && let Some(slot) = PALETTE_INDEXES.iter().position(|candidate| *candidate == index)
        {
            colors[slot] = Some(rgb);
        }
        start = sequence_start + end + if rest.get(end) == Some(&b'\x07') { 1 } else { 2 };
    }
    Some([
        colors[0]?,
        colors[1]?,
        colors[2]?,
        colors[3]?,
        colors[4]?,
        colors[5]?,
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
    if component.is_empty() || component.len() > 4 || !component.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(component, 16).ok()?;
    let max = (1u32 << (component.len() * 4)) - 1;
    Some(((value * 255 + max / 2) / max) as u8)
}

pub(crate) fn setup(bold: bool, query_colors: bool) -> Result<Option<[Rgb; 6]>> {
    terminal::enable_raw_mode().context("entering raw mode")?;
    let palette = query_colors.then(query_palette).flatten();
    let mut out = stdout();
    out.execute(EnterAlternateScreen)?;
    out.execute(DisableLineWrap)?;
    out.execute(cursor::Hide)?;
    out.execute(Clear(ClearType::All))?;
    // Save the window title so the stats meter can borrow it and hand it back.
    out.write_all(b"\x1b[22;2t")?;
    if bold {
        out.execute(SetAttribute(Attribute::Bold))?;
    }
    Ok(palette)
}

/// The bytes that put the terminal back the way we found it.
///
/// Split out from [`restore`] because the writer thread must be able to emit
/// them as its final act: whoever wrote the last frame has to write the restore
/// too, or the two can race.
pub(crate) fn restore_sequence() -> Vec<u8> {
    let mut b = Vec::new();
    let _ = b.write_all(b"\x1b[23;2t"); // give the window title back
    let _ = b.queue(SetAttribute(Attribute::Reset));
    let _ = b.queue(cursor::Show);
    let _ = b.queue(EnableLineWrap);
    let _ = b.queue(LeaveAlternateScreen);
    b
}

/// Best-effort teardown. Used by the panic hook too, so it must not panic and
/// must be safe to call more than once.
///
/// Safe to call while a writer thread is live: it goes through `Stdout`, whose
/// lock is held for the whole of `write_all`, so this can only land before or
/// after a frame, never inside one.
pub(crate) fn restore() {
    writer::abort_all();
    let mut out = stdout();
    let _ = out.write_all(&restore_sequence());
    let _ = terminal::disable_raw_mode();
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(parse_palette_reply("2;rgb:ff/80/00"), Some((2, (255, 128, 0))));
        assert_eq!(parse_palette_reply("3;rgb:fff/800/000"), Some((3, (255, 128, 0))));
        assert_eq!(parse_palette_reply("4;rgb:ffff/8080/0000"), Some((4, (255, 128, 0))));
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
