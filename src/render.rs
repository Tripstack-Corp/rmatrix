//! Damage-tracking renderer.
//!
//! Redrawing every cell each frame is what makes naive terminal animations tear
//! and flicker. We keep the previous frame and emit only the cells that actually
//! changed, coalescing cursor moves and colour changes as we go.

use crate::rain::Rain;
use crate::theme::{Depth, Theme};
use crossterm::style::{Color, Print, SetForegroundColor};
use crossterm::{QueueableCommand, cursor};
use std::io::Write;

/// What one `draw` cost. Surfaced so the caller can show it (see the `f`
/// overlay) — output volume, not our own CPU, is this program's bottleneck.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrawStats {
    pub cells_damaged: usize,
    pub bytes: usize,
}

/// Wraps the real writer just to total up what we emitted.
struct Counting<W> {
    inner: W,
    n: usize,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.n += n;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// How far the pen colour may drift from the ideal before we re-set it, summed
/// across R+G+B. Colour changes are ~16 bytes and dominate output; neighbouring
/// cells in a row are often a hair apart, and re-setting the pen for a
/// difference nobody can see is pure waste. Error never accumulates: every cell
/// is within this of the pen, not of its neighbour.
pub const DEFAULT_COLOR_TOLERANCE: u16 = 12;

pub struct Renderer {
    /// The colour last committed for each cell — see `redraw_tolerance`.
    prev: Vec<Option<(char, Color)>>,
    w: u16,
    h: u16,
    cur_color: Option<Color>,
    tolerance: u16,
    redraw_tolerance: u16,
    /// Where the terminal cursor sits after the last write, if known.
    at: Option<(u16, u16)>,
}

/// Manhattan distance in RGB, for the pen-reuse test. Non-RGB colours (the
/// 256/16 fallbacks) are already coarse, so they compare exactly.
fn within(a: Color, b: Color, tol: u16) -> bool {
    match (a, b) {
        (
            Color::Rgb {
                r: r1,
                g: g1,
                b: b1,
            },
            Color::Rgb {
                r: r2,
                g: g2,
                b: b2,
            },
        ) => {
            u16::from(r1.abs_diff(r2)) + u16::from(g1.abs_diff(g2)) + u16::from(b1.abs_diff(b2))
                <= tol
        }
        _ => a == b,
    }
}

/// Is the colour already committed for this cell close enough to what we now
/// want?
///
/// The glyph must match exactly — a wrong character is not a shade of grey —
/// but the colour only has to be within `tol`. At `tol` 0 this is plain
/// equality, i.e. exact damage tracking.
fn settled(committed: Option<(char, Color)>, want: Option<(char, Color)>, tol: u16) -> bool {
    match (committed, want) {
        (None, None) => true,
        (Some((a, ac)), Some((b, bc))) => a == b && within(ac, bc, tol),
        _ => false,
    }
}

impl Renderer {
    #[must_use]
    pub fn new(w: u16, h: u16) -> Renderer {
        Renderer {
            prev: vec![None; w as usize * h as usize],
            w,
            h,
            cur_color: None,
            tolerance: DEFAULT_COLOR_TOLERANCE,
            redraw_tolerance: 0,
            at: None,
        }
    }

    /// 0 re-sets the pen for any colour change at all.
    pub fn set_color_tolerance(&mut self, tolerance: u16) {
        self.tolerance = tolerance;
    }

    /// How far a cell's ideal colour may drift from what is *already on screen*
    /// before we bother repainting it, summed across R+G+B.
    ///
    /// This is the adaptive-quality knob. Unlike [`Theme::levels`], raising it
    /// costs nothing on the frame it changes: the comparison is against the
    /// screen's real state, so a cell that is already close enough simply stops
    /// generating damage, and lowering it lets cells catch up as they next move.
    /// Error cannot accumulate — every cell stays within this of its ideal, not
    /// of its own previous error. 0 reproduces exact damage tracking.
    ///
    /// [`Theme::levels`]: crate::theme::Theme::levels
    pub fn set_redraw_tolerance(&mut self, tolerance: u16) {
        self.redraw_tolerance = tolerance;
    }

    /// Call after anything else writes to the screen (an overlay, say). The
    /// renderer caches the terminal's current colour and cursor position to skip
    /// redundant escapes; a foreign write invalidates both, and stale caches
    /// paint the next frame in the wrong colour.
    pub fn forget_cursor_and_color(&mut self) {
        self.cur_color = None;
        self.at = None;
    }

    /// Also the way to force a full repaint: dropping the previous frame makes
    /// every cell count as damaged.
    pub fn resize(&mut self, w: u16, h: u16) {
        self.w = w;
        self.h = h;
        self.prev = vec![None; w as usize * h as usize];
        self.cur_color = None;
        self.at = None;
    }

    pub fn draw<W: Write>(
        &mut self,
        out: &mut W,
        rain: &Rain,
        theme: &Theme,
        depth: Depth,
    ) -> std::io::Result<DrawStats> {
        let mut out = Counting { inner: out, n: 0 };
        let mut damaged = 0usize;

        // Row-major. Column-major looks tempting — a column is one drop's fade,
        // so its colours are coherent and the pen could be reused — but it
        // measures ~11% worse. At ~7% damage, lit cells are sparse in *both*
        // axes, so neighbour coherence almost never applies, and scanning by
        // column trades cheap same-row `MoveRight` hops for absolute moves
        // (4.7 -> 8.2 bytes each). Measured, not assumed; see examples/perf.rs.
        for y in 0..self.h {
            for x in 0..self.w {
                let want = rain
                    .color_of(x, y, theme)
                    .map(|(ch, rgb)| (ch, depth.to_color(rgb)));
                let idx = y as usize * self.w as usize + x as usize;
                // Bounds-checked rather than indexed: `prev` is only resized
                // alongside w/h, and a mismatch should drop a frame, not abort.
                let Some(slot) = self.prev.get_mut(idx) else {
                    continue;
                };
                // A cell whose ideal colour has not drifted `redraw_tolerance`
                // from the one we last committed to is left alone: repainting
                // costs a cursor move, an SGR and a glyph, and at small deltas
                // nobody can tell. At tolerance 0 this is plain equality, i.e.
                // exactly the damage tracking we have always done.
                if settled(*slot, want, self.redraw_tolerance) {
                    continue;
                }
                *slot = want;
                damaged += 1;

                // Cursor positioning is a fifth of all output, so the cheap
                // cases are worth spelling out.
                match self.at {
                    Some((cx, cy)) if cx == x && cy == y => {}
                    // Printing left us one cell to the right and we want the row
                    // below: backspace + line feed is 2 bytes against ~10 for an
                    // absolute move. Safe from scrolling because cy is at most
                    // h-2 here, and raw mode clears OPOST so LF is a bare feed.
                    Some((cx, cy)) if cx == x + 1 && cy + 1 == y => {
                        out.write_all(b"\x08\n")?;
                    }
                    Some((cx, cy)) if cy == y && x > cx => {
                        out.queue(cursor::MoveRight(x - cx))?;
                    }
                    _ => {
                        out.queue(cursor::MoveTo(x, y))?;
                    }
                }

                match want {
                    Some((ch, color)) => {
                        if !self
                            .cur_color
                            .is_some_and(|pen| within(pen, color, self.tolerance))
                        {
                            out.queue(SetForegroundColor(color))?;
                            self.cur_color = Some(color);
                        }
                        out.queue(Print(ch))?;
                    }
                    None => {
                        out.queue(Print(' '))?;
                    }
                }

                // Line wrap is disabled, so the cursor stalls in the last column
                // rather than moving on — don't track it there.
                self.at = if x + 1 < self.w {
                    Some((x + 1, y))
                } else {
                    None
                };
            }
        }
        out.flush()?;
        Ok(DrawStats {
            cells_damaged: damaged,
            bytes: out.n,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rain::{Config, Rain};

    fn scene() -> (Rain, Theme) {
        let mut r = Rain::new(
            20,
            10,
            Config {
                seed: Some(42),
                ..Config::default()
            },
        );
        for _ in 0..90 {
            r.step(1.0 / 60.0);
        }
        (r, Theme::from_base((0, 255, 65), false))
    }

    fn draw(rr: &mut Renderer, rain: &Rain, theme: &Theme) -> Vec<u8> {
        draw_stats(rr, rain, theme).0
    }

    fn draw_stats(rr: &mut Renderer, rain: &Rain, theme: &Theme) -> (Vec<u8>, DrawStats) {
        let mut buf = Vec::new();
        let s = rr
            .draw(&mut buf, rain, theme, Depth::True)
            .expect("writing to a Vec cannot fail");
        (buf, s)
    }

    #[test]
    fn first_frame_emits_the_lit_cells() {
        let (rain, theme) = scene();
        let mut rr = Renderer::new(20, 10);
        let out = draw(&mut rr, &rain, &theme);
        assert!(!out.is_empty(), "nothing was emitted");
        assert!(
            out.windows(7).any(|w| w.starts_with(b"\x1b[38;2;")),
            "no truecolor SGR"
        );
    }

    #[test]
    fn an_unchanged_frame_emits_nothing() {
        // This is the whole point of the damage tracking.
        let (rain, theme) = scene();
        let mut rr = Renderer::new(20, 10);
        let _ = draw(&mut rr, &rain, &theme);
        let second = draw(&mut rr, &rain, &theme);
        assert!(
            second.is_empty(),
            "redundant redraw emitted {} bytes",
            second.len()
        );
    }

    #[test]
    fn resize_forces_a_full_repaint() {
        let (rain, theme) = scene();
        let mut rr = Renderer::new(20, 10);
        let _ = draw(&mut rr, &rain, &theme);
        assert!(draw(&mut rr, &rain, &theme).is_empty());
        rr.resize(20, 10);
        assert!(
            !draw(&mut rr, &rain, &theme).is_empty(),
            "repaint was not forced"
        );
    }

    #[test]
    fn renderer_larger_than_the_grid_does_not_panic() {
        let (rain, theme) = scene();
        let mut rr = Renderer::new(200, 100);
        let out = draw(&mut rr, &rain, &theme);
        // The in-grid cells still paint; cells past the grid read as dark, and
        // since they started dark the damage tracker correctly emits nothing
        // for them rather than blanking the whole 200x100 field.
        assert!(
            !out.is_empty(),
            "the in-bounds region should still have painted"
        );
        assert!(
            draw(&mut rr, &rain, &theme).is_empty(),
            "second pass should be clean"
        );
    }

    #[test]
    fn zero_sized_renderer_is_a_no_op() {
        let (rain, theme) = scene();
        let mut rr = Renderer::new(0, 0);
        assert!(draw(&mut rr, &rain, &theme).is_empty());
    }

    #[test]
    fn stats_report_damage_and_bytes() {
        let (rain, theme) = scene();
        let mut rr = Renderer::new(20, 10);
        let (buf, s) = draw_stats(&mut rr, &rain, &theme);
        assert_eq!(
            s.bytes,
            buf.len(),
            "byte count disagrees with what was written"
        );
        assert!(s.cells_damaged > 0);
        assert!(s.cells_damaged <= 20 * 10);

        let (_, s2) = draw_stats(&mut rr, &rain, &theme);
        assert_eq!(
            s2,
            DrawStats {
                cells_damaged: 0,
                bytes: 0
            },
            "clean frame is not free"
        );
    }

    #[test]
    fn quantising_the_ramp_cuts_damage() {
        // The performance premise: fewer brightness steps means a cell holds its
        // colour across more frames, so fewer cells are damaged per frame.
        fn damage_over(levels: u16) -> usize {
            let mut rain = Rain::new(
                60,
                30,
                Config {
                    seed: Some(5),
                    ..Config::default()
                },
            );
            let mut theme = Theme::from_base((0, 255, 65), false);
            theme.levels = levels;
            let mut rr = Renderer::new(60, 30);
            let mut total = 0;
            for _ in 0..240 {
                rain.step(1.0 / 60.0);
                total += draw_stats(&mut rr, &rain, &theme).1.cells_damaged;
            }
            total
        }
        let coarse = damage_over(8);
        let fine = damage_over(0); // unquantised
        assert!(
            coarse * 2 < fine,
            "quantising barely helped: {coarse} damaged vs {fine} unquantised"
        );
    }

    #[test]
    fn pen_tolerance_saves_bytes_without_changing_damage() {
        // Reusing the pen for an imperceptible colour delta must reduce output
        // but must never change *which* cells we consider damaged.
        fn run(tol: u16) -> (usize, usize) {
            let mut rain = Rain::new(
                80,
                40,
                Config {
                    seed: Some(9),
                    ..Config::default()
                },
            );
            let theme = Theme::from_base((0, 255, 65), false);
            let mut rr = Renderer::new(80, 40);
            rr.set_color_tolerance(tol);
            let (mut bytes, mut dmg) = (0, 0);
            for _ in 0..300 {
                rain.step(1.0 / 60.0);
                let s = draw_stats(&mut rr, &rain, &theme).1;
                bytes += s.bytes;
                dmg += s.cells_damaged;
            }
            (bytes, dmg)
        }
        let (loose_bytes, loose_dmg) = run(DEFAULT_COLOR_TOLERANCE);
        let (strict_bytes, strict_dmg) = run(0);
        assert_eq!(loose_dmg, strict_dmg, "tolerance changed the damage set");
        assert!(
            loose_bytes < strict_bytes,
            "tolerance cost bytes instead of saving them: {loose_bytes} vs {strict_bytes}"
        );
    }

    #[test]
    fn near_identical_colors_are_within_tolerance() {
        let a = Color::Rgb {
            r: 0,
            g: 200,
            b: 50,
        };
        assert!(within(
            a,
            Color::Rgb {
                r: 0,
                g: 204,
                b: 52
            },
            12
        ));
        assert!(!within(
            a,
            Color::Rgb {
                r: 0,
                g: 230,
                b: 50
            },
            12
        ));
        assert!(within(a, a, 0), "a colour must always match itself");
        // Palette colours are already coarse; they compare exactly.
        assert!(within(Color::AnsiValue(40), Color::AnsiValue(40), 99));
        assert!(!within(Color::AnsiValue(40), Color::AnsiValue(41), 99));
    }

    #[test]
    fn forgetting_the_cache_reemits_color() {
        let (rain, theme) = scene();
        let mut rr = Renderer::new(20, 10);
        let _ = draw(&mut rr, &rain, &theme);
        assert!(draw(&mut rr, &rain, &theme).is_empty());
        // No cells changed, so this alone must not repaint anything...
        rr.forget_cursor_and_color();
        assert!(draw(&mut rr, &rain, &theme).is_empty());
        // ...but the next real change must not assume a stale colour.
        rr.resize(20, 10);
        let out = draw(&mut rr, &rain, &theme);
        assert!(out.windows(7).any(|w| w.starts_with(b"\x1b[38;2;")));
    }
}
