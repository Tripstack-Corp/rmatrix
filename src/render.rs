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

pub struct Renderer {
    prev: Vec<Option<(char, Color)>>,
    w: u16,
    h: u16,
    cur_color: Option<Color>,
    /// Where the terminal cursor sits after the last write, if known.
    at: Option<(u16, u16)>,
}

impl Renderer {
    #[must_use]
    pub fn new(w: u16, h: u16) -> Renderer {
        Renderer {
            prev: vec![None; w as usize * h as usize],
            w,
            h,
            cur_color: None,
            at: None,
        }
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
    ) -> std::io::Result<()> {
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
                if *slot == want {
                    continue;
                }
                *slot = want;

                if self.at != Some((x, y)) {
                    out.queue(cursor::MoveTo(x, y))?;
                }
                match want {
                    Some((ch, color)) => {
                        if self.cur_color != Some(color) {
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
        out.flush()
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
        let mut buf = Vec::new();
        rr.draw(&mut buf, rain, theme, Depth::True)
            .expect("writing to a Vec cannot fail");
        buf
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
}
