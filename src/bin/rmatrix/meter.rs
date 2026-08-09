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

/// The live settings the measured figures are shown against.
///
/// A struct rather than three more positional arguments: both readouts want
/// all of it, so the caller was already building the same list twice a frame,
/// and `(u16, bool, f32)` is exactly the shape that gets transposed one day.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Readout {
    pub(crate) levels: u16,
    /// Whether `levels` was sized from the terminal rather than given.
    pub(crate) auto: bool,
    /// Effective multiplier, from [`rmatrix::Rain::speed`] — never recomputed
    /// here, or the number on screen could disagree with the animation.
    pub(crate) speed: f32,
}

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

    fn line(&self, r: Readout) -> String {
        // Speed sits second rather than at the end because the line is trimmed
        // to the window width and this is the field a narrow terminal can least
        // afford to lose: it is the only feedback the `1`-`9` keys have, and
        // without it the scale being centred on `3` is invisible.
        format!(
            " {:.0} fps · {:.2}x speed · {:.1} KB/frame · {:.2} MB/s · {:.1}% cells · {:.0}% coalesced · {}{} · q quit ",
            self.fps,
            r.speed,
            self.bytes_per_frame / 1024.0,
            self.bytes_per_frame * self.fps / 1.0e6,
            self.damage_pct,
            self.coalesce_pct,
            r.levels,
            if r.auto { " auto" } else { "" },
        )
    }

    /// Same figures, for the window title.
    ///
    /// The title is rendered by the OS in the UI font, not by the terminal grid,
    /// which makes it the only place these numbers stay legible under a font
    /// that remaps ASCII to glyphs — exactly the case when pairing `-c ascii`
    /// with Matrix Code NFI.
    pub(crate) fn title(&self, r: Readout) -> String {
        format!(
            "rmatrix — {:.0} fps · {:.2}x speed · {:.1} KB/frame · {:.2} MB/s · {:.1}% cells · {:.0}% coalesced · {} levels{}",
            self.fps,
            r.speed,
            self.bytes_per_frame / 1024.0,
            self.bytes_per_frame * self.fps / 1.0e6,
            self.damage_pct,
            self.coalesce_pct,
            r.levels,
            if r.auto { " (auto)" } else { "" },
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
pub(crate) fn draw_overlay<W: Write>(out: &mut W, w: u16, meter: &Meter, r: Readout) -> Result<()> {
    let text = meter.line(r);
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

    /// The ambient half of the readout, for tests that only care about figures.
    fn at(levels: u16, auto: bool) -> Readout {
        Readout {
            levels,
            auto,
            speed: 1.0,
        }
    }

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
        assert!(m.line(at(24, true)).contains("fps"));
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
        let t = m.title(at(8, true));
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
    fn both_readouts_name_the_speed_the_simulation_is_actually_using() {
        // Issue #1 reported that rmatrix could not be slowed below its default.
        // It could — `--speed` takes fractions and the `1`-`9` keys are centred
        // on `3` — but nothing on screen said so while running. This is that
        // missing feedback, and it has to show the *effective* multiplier:
        // `--speed 0.4` then key `3` is 0.4x, and reporting either half alone
        // would be wrong in the case people actually hit.
        let m = Meter::default();
        for (speed, want) in [
            (1.0, "1.00x"),
            (1.0 / 3.0, "0.33x"),
            (3.0, "3.00x"),
            (0.05, "0.05x"),
        ] {
            let r = Readout {
                levels: 8,
                auto: true,
                speed,
            };
            assert!(m.line(r).contains(want), "line was {:?}", m.line(r));
            assert!(m.title(r).contains(want), "title was {:?}", m.title(r));
        }
    }

    #[test]
    fn the_speed_field_survives_an_eighty_column_window() {
        // Why it sits second in the line rather than at the end: the overlay is
        // trimmed to the window width, and at 80 columns the tail is already
        // lost. Put speed there and it would be invisible on exactly the narrow
        // terminals whose users are most likely to be hunting for the control.
        let mut m = Meter::default();
        for _ in 0..30 {
            m.record(
                Duration::from_millis(20),
                DrawStats {
                    cells_damaged: 900,
                    bytes: 20_000,
                },
                1000,
            );
        }
        let r = Readout {
            levels: 8,
            auto: true,
            speed: 1.0 / 3.0,
        };
        // The premise: this line does not fit, so ordering decides what is seen.
        assert!(
            m.line(r).chars().count() > 80,
            "line got short enough to fit"
        );

        let mut buf = Vec::new();
        draw_overlay(&mut buf, 80, &m, r).expect("writing to a Vec cannot fail");
        let text = String::from_utf8(buf).expect("the overlay is valid UTF-8");
        assert!(text.contains("0.33x speed"), "80-col overlay was {text:?}");
    }

    #[test]
    fn meter_survives_a_zero_cell_screen() {
        let mut m = Meter::default();
        for _ in 0..30 {
            m.record(Duration::from_millis(50), DrawStats::default(), 0);
        }
        assert_eq!(m.damage_pct, 0.0);
        assert!(m.line(at(24, false)).contains("0.0% cells"));
    }
}
