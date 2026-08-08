//! The stats readout behind the `f` key.
//!
//! Two surfaces for the same figures. The overlay is drawn into the frame; the
//! window title is set with OSC 2 and is the one that stays readable under a
//! font that remaps ASCII to glyphs.

use anyhow::Result;
use crossterm::style::{
    Attribute, Color, Print, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::{QueueableCommand, cursor};
use rmatrix::DrawStats;
use std::io::Write;
use std::time::Duration;

/// Rolling frame-rate and output-volume meter.
///
/// Output volume is the number that matters: rmatrix's own CPU is negligible
/// next to what the terminal spends parsing the escape sequences we emit.
#[derive(Default)]
pub(crate) struct Meter {
    frames: u32,
    /// Ticks where the terminal was still busy, so the draw was folded into the
    /// next frame instead.
    skipped: u32,
    window: Duration,
    bytes: usize,
    damaged: usize,
    fps: f32,
    bytes_per_frame: f32,
    damage_pct: f32,
    coalesce_pct: f32,
}

impl Meter {
    /// A tick that produced no frame because the writer was still busy. Worth
    /// surfacing: a non-zero figure is the terminal telling you it is the
    /// bottleneck.
    pub(crate) fn skip(&mut self) {
        self.skipped += 1;
    }

    /// Returns true when the averaging window closed and the published figures
    /// changed — the caller uses that to avoid rewriting the title every frame.
    pub(crate) fn record(&mut self, dt: Duration, stats: DrawStats, cells: usize) -> bool {
        self.frames += 1;
        self.window += dt;
        self.bytes += stats.bytes;
        self.damaged += stats.cells_damaged;
        // Re-average about twice a second: often enough to feel live, rarely
        // enough that the digits stay readable.
        if self.window >= Duration::from_millis(500) {
            let secs = self.window.as_secs_f32().max(f32::EPSILON);
            self.fps = self.frames as f32 / secs;
            self.bytes_per_frame = self.bytes as f32 / self.frames as f32;
            self.damage_pct = if cells == 0 {
                0.0
            } else {
                self.damaged as f32 / (self.frames as usize * cells) as f32 * 100.0
            };
            self.coalesce_pct =
                self.skipped as f32 / (self.frames + self.skipped).max(1) as f32 * 100.0;
            *self = Meter {
                fps: self.fps,
                bytes_per_frame: self.bytes_per_frame,
                damage_pct: self.damage_pct,
                coalesce_pct: self.coalesce_pct,
                ..Meter::default()
            };
            return true;
        }
        false
    }

    fn line(&self, levels: u16, auto: bool) -> String {
        format!(
            " {:.0} fps · {:.1} KB/frame · {:.2} MB/s · {:.1}% cells · {:.0}% coalesced · {}{} · q quit ",
            self.fps,
            self.bytes_per_frame / 1024.0,
            self.bytes_per_frame * self.fps / 1.0e6,
            self.damage_pct,
            self.coalesce_pct,
            levels,
            if auto { " auto" } else { "" },
        )
    }

    /// Same figures, for the window title.
    ///
    /// The title is rendered by the OS in the UI font, not by the terminal grid,
    /// which makes it the only place these numbers stay legible under a font
    /// that remaps ASCII to glyphs — exactly the case when pairing `-c ascii`
    /// with Matrix Code NFI.
    pub(crate) fn title(&self, levels: u16, auto: bool) -> String {
        format!(
            "rmatrix — {:.0} fps · {:.1} KB/frame · {:.2} MB/s · {:.1}% cells · {:.0}% coalesced · {} levels{}",
            self.fps,
            self.bytes_per_frame / 1024.0,
            self.bytes_per_frame * self.fps / 1.0e6,
            self.damage_pct,
            self.coalesce_pct,
            levels,
            if auto { " (auto)" } else { "" },
        )
    }
}

/// OSC 2. Terminals that don't support it ignore the sequence.
pub(crate) fn set_title<W: Write>(out: &mut W, title: &str) -> Result<()> {
    // Strip anything that would terminate the sequence early.
    let safe: String = title.chars().filter(|c| !c.is_control()).collect();
    write!(out, "\x1b]2;{safe}\x07")?;
    Ok(())
}

/// Painted over the rain each frame, after the renderer has run.
///
/// The caller must follow this with `Renderer::forget_cursor_and_color`: this
/// moves the cursor and sets colours behind the renderer's back.
pub(crate) fn draw_overlay<W: Write>(
    out: &mut W,
    w: u16,
    meter: &Meter,
    levels: u16,
    auto: bool,
) -> Result<()> {
    let text = meter.line(levels, auto);
    let trimmed: String = text.chars().take(w as usize).collect();
    out.queue(cursor::MoveTo(0, 0))?;
    out.queue(SetAttribute(Attribute::Reset))?;
    out.queue(SetBackgroundColor(Color::Rgb { r: 0, g: 40, b: 12 }))?;
    out.queue(SetForegroundColor(Color::Rgb {
        r: 190,
        g: 255,
        b: 200,
    }))?;
    out.queue(Print(trimmed))?;
    out.queue(SetAttribute(Attribute::Reset))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_reports_zero_before_its_first_window_closes() {
        let mut m = Meter::default();
        m.record(
            Duration::from_millis(10),
            DrawStats {
                cells_damaged: 5,
                bytes: 100,
            },
            1000,
        );
        assert_eq!(m.fps, 0.0, "should not publish an average from one sample");
        assert!(m.line(24, true).contains("fps"));
    }

    #[test]
    fn meter_averages_over_its_window() {
        let mut m = Meter::default();
        // 30 frames of 1/30s each = exactly one second, so 30 fps.
        for _ in 0..30 {
            m.record(
                Duration::from_secs_f64(1.0 / 30.0),
                DrawStats {
                    cells_damaged: 100,
                    bytes: 2048,
                },
                1000,
            );
        }
        assert!((m.fps - 30.0).abs() < 1.0, "fps was {}", m.fps);
        assert!((m.bytes_per_frame - 2048.0).abs() < 1.0);
        assert!(
            (m.damage_pct - 10.0).abs() < 0.5,
            "damage was {}",
            m.damage_pct
        );
    }

    #[test]
    fn meter_signals_only_when_its_window_closes() {
        let mut m = Meter::default();
        // Under the 500ms window: no refresh, so no title churn.
        assert!(!m.record(Duration::from_millis(100), DrawStats::default(), 100));
        assert!(!m.record(Duration::from_millis(300), DrawStats::default(), 100));
        // Crossing it publishes.
        assert!(m.record(Duration::from_millis(200), DrawStats::default(), 100));
        // And the window resets, so the next tick is quiet again.
        assert!(!m.record(Duration::from_millis(100), DrawStats::default(), 100));
    }

    #[test]
    fn title_stays_legible_without_control_characters() {
        // The title is the fallback readout when the terminal font remaps
        // ASCII, so it must survive being written raw into an OSC sequence.
        let mut m = Meter::default();
        for _ in 0..30 {
            m.record(
                Duration::from_millis(20),
                DrawStats {
                    cells_damaged: 10,
                    bytes: 500,
                },
                1000,
            );
        }
        let t = m.title(8, true);
        assert!(t.starts_with("rmatrix"));
        assert!(t.contains("fps"), "{t}");
        assert!(
            !t.chars().any(char::is_control),
            "title had a control char: {t:?}"
        );

        let mut buf = Vec::new();
        set_title(&mut buf, "evil\x07title\x1b[0m").expect("writing to a Vec cannot fail");
        assert_eq!(
            buf, b"\x1b]2;eviltitle[0m\x07",
            "control chars leaked into the OSC"
        );
    }

    #[test]
    fn meter_survives_a_zero_cell_screen() {
        let mut m = Meter::default();
        for _ in 0..30 {
            m.record(Duration::from_millis(50), DrawStats::default(), 0);
        }
        assert_eq!(m.damage_pct, 0.0);
        assert!(m.line(24, false).contains("0.0% cells"));
    }
}
