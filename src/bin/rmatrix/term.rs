//! Terminal state, and where drawn frames go.
//!
//! Everything in here is about the file descriptor rather than the animation:
//! putting the terminal into raw mode and the alternate screen, putting it back
//! afterwards even if we panic, and handing frames to whoever writes them.

use anyhow::{Context, Result};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::style::{Attribute, SetAttribute};
use crossterm::terminal::{
    self, Clear, ClearType, DisableLineWrap, EnableLineWrap, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::{ExecutableCommand, QueueableCommand, cursor};
use rmatrix::writer::{self, FrameWriter};
use std::io::{Stdout, Write, stdout};
use std::time::Duration;

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

pub(crate) fn setup(bold: bool, screensaver: bool) -> Result<()> {
    terminal::enable_raw_mode().context("entering raw mode")?;
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
    if screensaver {
        out.write_all(&mouse_capture_sequence())?;
        out.flush()?;
    }
    Ok(())
}

/// The bytes that make the terminal report the mouse to us.
///
/// Only the screensaver asks for this. Capturing the mouse takes it away from
/// the terminal, so in normal mode it would stop people selecting text.
pub(crate) fn mouse_capture_sequence() -> Vec<u8> {
    let mut b = Vec::new();
    let _ = b.queue(EnableMouseCapture);
    b
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
    // Unconditional: turning off a mode that was never on is a no-op, and
    // leaving it on after exit makes every mouse twitch type escape codes into
    // the shell. The panic hook has no idea which mode we were in.
    let _ = b.queue(DisableMouseCapture);
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

    /// The DEC private modes a sequence sets (`ESC [ ? n h`) or resets
    /// (`ESC [ ? n l`).
    fn private_modes(bytes: &[u8], suffix: char) -> Vec<String> {
        let text = String::from_utf8_lossy(bytes);
        text.split("\x1b[?")
            .skip(1)
            .filter_map(|rest| {
                let end = rest.find(|c: char| !c.is_ascii_digit() && c != ';')?;
                rest[end..].starts_with(suffix).then_some(&rest[..end])
            })
            .flat_map(|params| params.split(';'))
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn the_teardown_turns_off_every_mouse_mode_the_screensaver_turns_on() {
        // If one of these survives exit, the shell the user returns to receives
        // a burst of escape codes every time the mouse moves, and it stays that
        // way until they run `reset`.
        let enabled = private_modes(&mouse_capture_sequence(), 'h');
        let disabled = private_modes(&restore_sequence(), 'l');
        assert!(!enabled.is_empty(), "the capture sequence enables nothing");
        for mode in &enabled {
            assert!(
                disabled.contains(mode),
                "?{mode} is enabled but never reset"
            );
        }
    }

    #[test]
    fn the_screensaver_hears_the_mouse_move_with_no_button_held() {
        // Without any-motion tracking (?1003) the terminal reports only clicks,
        // drags and scrolls, and nudging the mouse to wake the screen would do
        // nothing.
        assert!(private_modes(&mouse_capture_sequence(), 'h').contains(&"1003".to_owned()));
    }
}
