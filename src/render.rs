//! Damage-tracking renderer with a bounded, prioritised per-frame budget.
//!
//! Redrawing every cell each frame is what makes naive terminal animations tear
//! and flicker. We keep the previous frame and emit only the cells that actually
//! changed, coalescing cursor moves and colour changes as we go.
//!
//! Damage alone still leaves the worst case unbounded: a burst of drop heads
//! costs whatever it costs, the terminal falls behind, our blocking flush
//! stalls, and the next frame's `dt` is huge — which generates *more* damage.
//! That feedback loop is the jank. So a frame also gets a byte budget, and when
//! damage exceeds it we spend the budget on what the eye actually tracks and
//! defer the rest (see [`salience`]). Deferral is safe only because a skipped
//! cell is *not* recorded in `prev`, so it stays damaged until it is really
//! drawn, and because debt ageing plus a hard deadline bound how long that takes.

use crate::rain::Rain;
use crate::theme::{Depth, Rgb, Theme};
use crossterm::style::{Color, Print, SetForegroundColor};
use crossterm::{QueueableCommand, cursor};
use std::io::Write;
use std::time::Duration;

/// What one `draw` cost. Surfaced so the caller can show it (see the `f`
/// overlay) — output volume, not our own CPU, is this program's bottleneck.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrawStats {
    /// Cells whose content differs from what the terminal is believed to show.
    pub cells_damaged: usize,
    /// Of those, the ones actually emitted this frame.
    pub cells_drawn: usize,
    /// Emitted past the budget because they hit the staleness deadline.
    pub cells_forced: usize,
    pub bytes: usize,
}

impl DrawStats {
    /// Damaged cells held back for a later frame.
    #[must_use]
    pub fn cells_deferred(&self) -> usize {
        self.cells_damaged - self.cells_drawn
    }
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

/// Score added per frame a cell has spent waiting. At `MAX_DEFER - 1` frames
/// this already exceeds the largest possible salience (255), so a cell nearing
/// its deadline outranks every freshly damaged cell except a drop head.
const AGE_GAIN: u16 = 40;

/// Hard staleness deadline, in frames. A cell deferred this many consecutive
/// frames is drawn whether or not the budget allows it. Busting the budget
/// occasionally is a far better failure than a cell that sits wrong on screen:
/// this is what turns "eventually" into a number.
///
/// Note that correctness rests on this alone, *not* on the score ordering —
/// which is what frees the score to be purely about what looks best.
pub const MAX_DEFER: u8 = 8;

/// Drop heads sit above the entire ageing range on purpose. Ageing exists to
/// stop dim cells rotting, but a head is the leading edge of a drop and stalling
/// one reads as the whole drop stuttering — far worse than a dim cell being a
/// few frames out of date. Letting a 7-frames-old tail cell outrank a fresh head
/// measurably starved heads (55 of 145 drawn) before this existed.
const HEAD_SCORE: u16 = 256 + (MAX_DEFER as u16) * AGE_GAIN;

/// Scores run from 0 to [`HEAD_SCORE`]; a cell past its deadline is forced
/// rather than scored, so `debt` never exceeds `MAX_DEFER` here.
const SCORE_BUCKETS: usize = HEAD_SCORE as usize + 1;

/// Bytes charged per cell before any frame has been measured. Refined online
/// from what output actually costs, so the estimate self-corrects for colour
/// depth, glyph width, and the way a sparse frame inflates cursor moves.
const INITIAL_CELL_COST: f32 = 20.0;

/// One damaged cell, resolved once and then emitted (or not) in a second pass.
#[derive(Clone, Copy)]
struct Damage {
    idx: u32,
    want: Option<(char, Color)>,
    luma: u8,
    score: u16,
    /// Past its staleness deadline: draw regardless of budget.
    forced: bool,
}

/// Which damaged cells this frame may emit.
struct Plan {
    /// Cells scoring above this are admitted outright.
    threshold: u16,
    /// How many cells *at* the threshold may be admitted, in scan order.
    partial: usize,
    /// Hard stop: once this many bytes are out, only forced cells continue.
    cap: usize,
}

impl Plan {
    /// Everything fits — the pre-budget behaviour, byte for byte.
    fn unbounded() -> Plan {
        Plan {
            threshold: 0,
            partial: usize::MAX,
            cap: usize::MAX,
        }
    }
}

pub struct Renderer {
    prev: Vec<Option<(char, Color)>>,
    /// Brightness of what we believe is on screen, kept beside `prev` so the
    /// priority function can measure how big a change it is being asked to make
    /// without reverse-engineering it out of a `Color`.
    prev_luma: Vec<u8>,
    /// Consecutive frames each cell has been damaged but not drawn.
    debt: Vec<u8>,
    /// This frame's damage, in scan order. Reused to keep draw allocation-free.
    damage: Vec<Damage>,
    /// Score histogram, so admission is a counting sort rather than a real one.
    hist: Vec<u32>,
    /// Rolling estimate of bytes per emitted cell.
    cell_cost: f32,
    /// Bytes this frame may spend on cells that are not past their deadline.
    budget: Option<usize>,
    w: u16,
    h: u16,
    cur_color: Option<Color>,
    tolerance: u16,
    /// Where the terminal cursor sits after the last write, if known.
    at: Option<(u16, u16)>,
}

/// Green-weighted luma, 0..=255. The rain is overwhelmingly green, so this is
/// close enough to perceived brightness to order cells by.
fn luma((r, g, b): Rgb) -> u8 {
    ((u16::from(r) * 2 + u16::from(g) * 5 + u16::from(b)) / 8) as u8
}

/// How much a viewer would miss this cell if we skipped it, 0..=255.
///
/// Two factors, because visibility is both *how much changes* and *where the
/// eye is looking*:
///
/// * `delta` — the brightness step being asked for. A cell appearing or
///   vanishing is a full-magnitude change; a dim tail cell sliding one
///   quantisation step is nearly invisible; a glyph churning under an unchanged
///   colour is zero, which is exactly right (churn is pure noise, and killing it
///   outright only saves 4% of bytes, so it is cheap to defer and no loss).
/// * `bias` — absolute brightness. Bright cells hold the eye even when the step
///   is small, dim ones do not.
///
/// Heads short-circuit to [`HEAD_SCORE`], above even the oldest deferred cell.
/// They are near-white and are the one thing that moves every single frame, so
/// the eye tracks them and nothing else. They are also cheap — roughly one cell
/// per active column per frame — so pinning them at the top crowds out little.
fn salience(want: u8, prev: u8, head: bool) -> u16 {
    if head {
        return HEAD_SCORE;
    }
    let delta = u16::from(want.abs_diff(prev));
    let bias = u16::from(want.max(prev)) / 4;
    (delta + bias).min(255)
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

impl Renderer {
    #[must_use]
    pub fn new(w: u16, h: u16) -> Renderer {
        let cells = w as usize * h as usize;
        Renderer {
            prev: vec![None; cells],
            prev_luma: vec![0; cells],
            debt: vec![0; cells],
            damage: Vec::new(),
            hist: vec![0; SCORE_BUCKETS],
            cell_cost: INITIAL_CELL_COST,
            budget: None,
            w,
            h,
            cur_color: None,
            tolerance: DEFAULT_COLOR_TOLERANCE,
            at: None,
        }
    }

    /// 0 re-sets the pen for any colour change at all.
    pub fn set_color_tolerance(&mut self, tolerance: u16) {
        self.tolerance = tolerance;
    }

    /// Cap the bytes one frame may spend on cells that are not yet past their
    /// staleness deadline. `None` restores the unbounded behaviour; a budget
    /// larger than the frame's damage is also a no-op, so the output is
    /// byte-identical whenever there is room to spare.
    pub fn set_budget(&mut self, budget: Option<usize>) {
        self.budget = budget;
    }

    /// Bytes currently charged per emitted cell, learned from previous frames.
    /// Exposed for harnesses that want to convert a cell budget into a byte one.
    #[must_use]
    pub fn cell_cost(&self) -> f32 {
        self.cell_cost
    }

    /// Longest any cell has been damaged-but-undrawn, in frames. The invariant
    /// the design rests on is that this never reaches [`MAX_DEFER`] + 1.
    #[must_use]
    pub fn max_debt(&self) -> u8 {
        self.debt.iter().copied().max().unwrap_or(0)
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
        let cells = w as usize * h as usize;
        self.w = w;
        self.h = h;
        self.prev = vec![None; cells];
        self.prev_luma = vec![0; cells];
        self.debt = vec![0; cells];
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
        self.collect(rain, theme, depth);
        let plan = self.plan();
        self.emit(out, &plan)
    }

    /// Pass one: find every damaged cell and score it. Deliberately writes
    /// nothing — `prev` may only be advanced for cells we really emit, and at
    /// this point we do not yet know which those are.
    fn collect(&mut self, rain: &Rain, theme: &Theme, depth: Depth) {
        self.damage.clear();
        self.hist.iter_mut().for_each(|b| *b = 0);

        // Row-major. Column-major looks tempting — a column is one drop's fade,
        // so its colours are coherent and the pen could be reused — but it
        // measures ~11% worse. At ~7% damage, lit cells are sparse in *both*
        // axes, so neighbour coherence almost never applies, and scanning by
        // column trades cheap same-row `MoveRight` hops for absolute moves
        // (4.7 -> 8.2 bytes each). Measured, not assumed; see examples/perf.rs.
        for y in 0..self.h {
            for x in 0..self.w {
                let sample = rain.sample(x, y, theme);
                let want = sample.map(|s| (s.ch, depth.to_color(s.rgb)));
                let idx = y as usize * self.w as usize + x as usize;
                // Bounds-checked rather than indexed: `prev` is only resized
                // alongside w/h, and a mismatch should drop a frame, not abort.
                let Some(slot) = self.prev.get(idx) else {
                    continue;
                };
                if *slot == want {
                    // Nothing owed here. Clear any debt, or a cell that drifted
                    // back to what is already on screen would come back later
                    // carrying a deadline it no longer deserves.
                    self.debt[idx] = 0;
                    continue;
                }
                let want_luma = sample.map_or(0, |s| luma(s.rgb));
                let debt = self.debt[idx];
                let forced = debt >= MAX_DEFER;
                let head = sample.is_some_and(|s| s.head);
                let score = if head {
                    HEAD_SCORE
                } else {
                    salience(want_luma, self.prev_luma[idx], false) + u16::from(debt) * AGE_GAIN
                };
                if !forced {
                    self.hist[score as usize] += 1;
                }
                self.damage.push(Damage {
                    idx: idx as u32,
                    want,
                    luma: want_luma,
                    score,
                    forced,
                });
            }
        }
    }

    /// Pass two: decide how far down the priority order this frame can afford
    /// to go. A counting sort over the score histogram, so no allocation and no
    /// comparison sort of a few thousand cells.
    fn plan(&self) -> Plan {
        let Some(budget) = self.budget else {
            return Plan::unbounded();
        };
        let cost = self.cell_cost.max(1.0);
        if self.damage.len() as f32 * cost <= budget as f32 {
            return Plan::unbounded();
        }
        let forced = self.damage.iter().filter(|d| d.forced).count();
        // Always leave room for at least one discretionary cell, whatever the
        // deadline traffic: forward progress is what the fairness proof needs.
        let free = (budget as f32 - forced as f32 * cost).max(cost);
        let allow = (free / cost) as usize;

        let mut acc = 0usize;
        for b in (0..SCORE_BUCKETS).rev() {
            let n = self.hist[b] as usize;
            if acc + n > allow {
                return Plan {
                    threshold: b as u16,
                    partial: allow - acc,
                    cap: budget,
                };
            }
            acc += n;
        }
        Plan::unbounded()
    }

    /// Pass three: emit the admitted cells, still in scan order so the cursor
    /// coalescing keeps working, and advance `prev` for those cells only.
    fn emit<W: Write>(&mut self, out: &mut W, plan: &Plan) -> std::io::Result<DrawStats> {
        let mut out = Counting { inner: out, n: 0 };
        let mut partial = plan.partial;
        let mut drawn = 0usize;
        let mut forced = 0usize;

        for i in 0..self.damage.len() {
            let d = self.damage[i];
            let selected = if d.score > plan.threshold {
                true
            } else if d.score == plan.threshold && partial > 0 {
                partial -= 1;
                true
            } else {
                false
            };
            // The plan sized the frame from an *estimate* of what a cell costs;
            // the `out.n` test is the hard stop that makes the byte bound true
            // rather than approximate.
            let admit = d.forced || (selected && out.n < plan.cap);
            if !admit {
                self.debt[d.idx as usize] = self.debt[d.idx as usize].saturating_add(1);
                continue;
            }
            if d.forced {
                forced += 1;
            }
            drawn += 1;

            let x = d.idx % u32::from(self.w);
            let y = d.idx / u32::from(self.w);
            let (x, y) = (x as u16, y as u16);

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

            match d.want {
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

            // Only now is the cell really on screen, so only now may the damage
            // tracker believe it. Recording it any earlier — at the point we
            // decided it changed — is what would make a deferred cell stale
            // forever.
            self.prev[d.idx as usize] = d.want;
            self.prev_luma[d.idx as usize] = d.luma;
            self.debt[d.idx as usize] = 0;

            // Line wrap is disabled, so the cursor stalls in the last column
            // rather than moving on — don't track it there.
            self.at = if x + 1 < self.w {
                Some((x + 1, y))
            } else {
                None
            };
        }
        out.flush()?;

        // Learn what a cell really costs. A sparse frame spends more per cell
        // (fewer same-row hops, less pen reuse), so this must track, not assume.
        if drawn > 0 {
            let per = out.n as f32 / drawn as f32;
            self.cell_cost = (self.cell_cost * 0.85 + per * 0.15).clamp(4.0, 96.0);
        }
        Ok(DrawStats {
            cells_damaged: self.damage.len(),
            cells_drawn: drawn,
            cells_forced: forced,
            bytes: out.n,
        })
    }
}

/// A write eating this much of the frame period means we overshot outright.
const OVERSHOOT: f32 = 0.9;
/// The load the loop aims to hover at. This is the whole trick: with a target
/// *between* the extremes, the budget creeps down above it and up below it and
/// settles without ever having to overshoot to find the edge.
const TARGET_LOAD: f32 = 0.6;
/// Anything within this much of the target is close enough. Without a dead band
/// a creep-up/creep-down rule can only ever hunt, never sit still.
const DEAD_BAND: f32 = 0.85;
/// How hard an outright overshoot cuts.
const CUT: f32 = 0.7;
const CREEP_UP: f32 = 1.03;
const CREEP_DOWN: f32 = 0.98;

/// Turns "how long did the terminal make our last write block" into a per-frame
/// byte budget.
///
/// A fixed budget would have to be tuned per terminal, per font, and per
/// machine, and would be wrong the moment any of those changed. The tempting
/// alternative — divide bytes by write time to get the terminal's throughput —
/// is measurably wrong: part of every write lands in the tty buffer for free, so
/// an under-budget frame reports a rate two or three times the truth, the budget
/// jumps up, and the next frame overshoots. That oscillation was worth 95 KB
/// frames against a 65 KB steady state.
///
/// So this does not model the terminal at all. It watches one number — the
/// fraction of the frame spent blocked in `write` — and creeps the budget toward
/// the value that holds that fraction at [`TARGET_LOAD`]. While the consumer
/// keeps up, no budget is imposed and output is unchanged.
#[derive(Debug)]
pub struct Governor {
    budget: f32,
    /// The last three observed loads. The controller runs off their median, not
    /// the newest: under CPU contention a frame can block because we were
    /// descheduled, not because the terminal is slow, and cutting the budget
    /// does nothing about that. Measured, on a 14-core machine with 10 spinners:
    /// reacting to raw load throttled to 1.16 MB/s against a consumer that could
    /// take 2.0, and pushed p99 frame time to 95 ms. A median rejects the
    /// isolated spike and keeps the sustained signal.
    recent: [f32; 3],
    ceiling: f32,
    floor: f32,
    throttling: bool,
}

/// Median of three, branch-light and allocation-free.
fn median3([a, b, c]: [f32; 3]) -> f32 {
    a.max(b).min(a.min(b).max(c))
}

impl Governor {
    /// `cells` sets the ceiling: a full repaint is the most any frame could
    /// possibly want, so a budget at or above it is the same as no budget.
    #[must_use]
    pub fn new(cells: usize) -> Governor {
        let ceiling = (cells as f32 * 24.0).max(4096.0);
        Governor {
            budget: ceiling,
            recent: [0.0; 3],
            ceiling,
            floor: 2048.0,
            throttling: false,
        }
    }

    #[must_use]
    pub fn budget(&self) -> usize {
        self.budget as usize
    }

    /// Whether the budget is currently biting.
    #[must_use]
    pub fn throttling(&self) -> bool {
        self.throttling
    }

    /// Feed back one frame: bytes written, how long the blocking write took,
    /// and the frame period being paced to.
    pub fn observe(&mut self, bytes: usize, write: Duration, period: Duration) {
        self.recent = [
            self.recent[1],
            self.recent[2],
            write.as_secs_f32() / period.as_secs_f32().max(1e-4),
        ];
        let load = median3(self.recent);
        if load > OVERSHOOT {
            // Anchor the cut to what we actually sent, not to the budget: the
            // budget may have been sitting far above this frame's demand, and
            // scaling *that* down would take many frames to bite.
            self.budget = (bytes as f32 * CUT).clamp(self.floor, self.ceiling);
            self.throttling = true;
        } else if self.throttling {
            let creep = if load > TARGET_LOAD {
                CREEP_DOWN
            } else if load < TARGET_LOAD * DEAD_BAND {
                CREEP_UP
            } else {
                1.0
            };
            self.budget = (self.budget * creep).clamp(self.floor, self.ceiling);
            // Back at the ceiling with nothing blocking: the terminal has room
            // again, so stop imposing anything at all.
            self.throttling = self.budget < self.ceiling;
        }
        // Not throttling and not overshooting: stay out of the way entirely.
    }

    /// Keep what we have learned about the terminal, rescale what depends on the
    /// window.
    pub fn resize(&mut self, cells: usize) {
        let (budget, throttling, recent) = (self.budget, self.throttling, self.recent);
        *self = Governor::new(cells);
        self.recent = recent;
        if throttling {
            self.budget = budget.clamp(self.floor, self.ceiling);
            self.throttling = self.budget < self.ceiling;
        }
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
        assert_eq!(s2, DrawStats::default(), "clean frame is not free");
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

    /// Just enough of a terminal to check ourselves against: cursor moves, the
    /// backspace/line-feed shortcut, truecolor SGR, and printing. Everything the
    /// renderer emits and nothing else, so an unexpected escape is a test
    /// failure rather than a silent no-op.
    struct FakeTerm {
        w: usize,
        h: usize,
        cells: Vec<(char, Option<Color>)>,
        x: usize,
        y: usize,
        pen: Option<Color>,
    }

    impl FakeTerm {
        fn new(w: u16, h: u16) -> FakeTerm {
            FakeTerm {
                w: w as usize,
                h: h as usize,
                cells: vec![(' ', None); w as usize * h as usize],
                x: 0,
                y: 0,
                pen: None,
            }
        }

        fn feed(&mut self, buf: &[u8]) {
            let mut i = 0;
            while i < buf.len() {
                match buf[i] {
                    0x08 => {
                        self.x = self.x.saturating_sub(1);
                        i += 1;
                    }
                    b'\n' => {
                        self.y += 1;
                        i += 1;
                    }
                    0x1b => {
                        assert_eq!(buf[i + 1], b'[', "only CSI sequences are expected");
                        let end = i
                            + 2
                            + buf[i + 2..]
                                .iter()
                                .position(u8::is_ascii_alphabetic)
                                .expect("unterminated CSI");
                        let args: Vec<u32> = std::str::from_utf8(&buf[i + 2..end])
                            .expect("CSI parameters are ASCII")
                            .split(';')
                            .map(|p| p.parse().unwrap_or(0))
                            .collect();
                        match buf[end] {
                            b'H' => {
                                self.y = args[0].saturating_sub(1) as usize;
                                self.x = args[1].saturating_sub(1) as usize;
                            }
                            b'C' => self.x += args[0] as usize,
                            b'm' => {
                                assert_eq!(args[..2], [38, 2], "unexpected SGR {args:?}");
                                self.pen = Some(Color::Rgb {
                                    r: args[2] as u8,
                                    g: args[3] as u8,
                                    b: args[4] as u8,
                                });
                            }
                            other => panic!("unexpected CSI final byte {}", other as char),
                        }
                        i = end + 1;
                    }
                    b => {
                        let len = if b < 0x80 {
                            1
                        } else if b >= 0xf0 {
                            4
                        } else if b >= 0xe0 {
                            3
                        } else {
                            2
                        };
                        let ch = std::str::from_utf8(&buf[i..i + len])
                            .expect("valid UTF-8")
                            .chars()
                            .next()
                            .expect("one char");
                        assert!(self.y < self.h && self.x < self.w, "wrote out of bounds");
                        self.cells[self.y * self.w + self.x] = (ch, self.pen);
                        // Line wrap is disabled, so the cursor stalls.
                        self.x = (self.x + 1).min(self.w - 1);
                        i += len;
                    }
                }
            }
        }

        /// What the renderer *believes* is on screen, as this terminal sees it.
        fn agrees_with(&self, prev: &[Option<(char, Color)>]) -> Option<usize> {
            (0..self.cells.len()).find(|&i| {
                let (ch, pen) = self.cells[i];
                match prev[i] {
                    None => ch != ' ',
                    Some((want_ch, want_color)) => ch != want_ch || pen != Some(want_color),
                }
            })
        }
    }

    /// The invariant the whole design rests on. Under a budget far too small to
    /// keep up, everything we emit must still leave the renderer's belief about
    /// the screen exactly true, and no cell may sit wrong for longer than the
    /// deadline.
    #[test]
    fn a_starved_renderer_never_lies_about_the_screen() {
        let (w, h) = (60u16, 40u16);
        let mut rain = Rain::new(
            w,
            h,
            Config {
                seed: Some(17),
                density: 0.9,
                ..Config::default()
            },
        );
        let theme = Theme::from_base((0, 255, 65), false);
        let mut rr = Renderer::new(w, h);
        // Exact colours, so the fake terminal can compare them exactly.
        rr.set_color_tolerance(0);
        // Roughly a twentieth of what a frame wants: sustained, deep starvation.
        rr.set_budget(Some(700));

        let mut term = FakeTerm::new(w, h);
        let mut buf = Vec::new();
        let mut worst_debt = 0u8;
        let mut ever_deferred = 0usize;

        for frame in 0..900 {
            rain.step(1.0 / 30.0);
            buf.clear();
            let stats = rr
                .draw(&mut buf, &rain, &theme, Depth::True)
                .expect("writing to a Vec cannot fail");
            term.feed(&buf);
            ever_deferred += stats.cells_deferred();
            worst_debt = worst_debt.max(rr.max_debt());
            assert!(
                rr.max_debt() <= MAX_DEFER,
                "frame {frame}: a cell went {} frames undrawn",
                rr.max_debt()
            );
            if let Some(i) = term.agrees_with(&rr.prev) {
                panic!(
                    "frame {frame}: renderer believes cell {i} is {:?}, screen shows {:?}",
                    rr.prev[i], term.cells[i]
                );
            }
        }
        assert!(
            ever_deferred > 100_000,
            "the budget never actually bit ({ever_deferred} deferrals)"
        );
        assert!(worst_debt > 0, "nothing was ever deferred");

        // Freeze the simulation: every deferred cell must now drain away, and
        // the deadline says it takes at most MAX_DEFER frames.
        for frame in 0..=u32::from(MAX_DEFER) {
            buf.clear();
            let stats = rr
                .draw(&mut buf, &rain, &theme, Depth::True)
                .expect("writing to a Vec cannot fail");
            term.feed(&buf);
            assert!(
                term.agrees_with(&rr.prev).is_none(),
                "frame {frame}: screen and belief diverged while draining"
            );
            if stats.cells_damaged == 0 {
                break;
            }
            assert!(
                frame < u32::from(MAX_DEFER),
                "still {} cells outstanding after {MAX_DEFER} idle frames",
                stats.cells_damaged
            );
        }

        // ...and having drained, the screen is exactly the unbudgeted image.
        let mut reference = Renderer::new(w, h);
        reference.set_color_tolerance(0);
        let mut refbuf = Vec::new();
        reference
            .draw(&mut refbuf, &rain, &theme, Depth::True)
            .expect("writing to a Vec cannot fail");
        assert_eq!(rr.prev, reference.prev, "converged to the wrong picture");
    }

    #[test]
    fn a_budget_bounds_the_worst_frame() {
        let (w, h) = (204u16, 175u16);
        let mut rain = Rain::new(
            w,
            h,
            Config {
                seed: Some(1),
                ..Config::default()
            },
        );
        let theme = Theme::from_base((0, 255, 65), false);

        fn run(rain: &mut Rain, theme: &Theme, w: u16, h: u16, budget: Option<usize>) -> usize {
            let mut rr = Renderer::new(w, h);
            rr.set_budget(budget);
            let mut buf = Vec::with_capacity(1 << 22);
            let mut worst = 0usize;
            for _ in 0..400 {
                rain.step(1.0 / 30.0);
                buf.clear();
                let s = rr
                    .draw(&mut buf, rain, theme, Depth::True)
                    .expect("writing to a Vec cannot fail");
                worst = worst.max(s.bytes);
            }
            worst
        }

        let mut warm = Rain::new(
            w,
            h,
            Config {
                seed: Some(1),
                ..Config::default()
            },
        );
        for _ in 0..1200 {
            warm.step(1.0 / 30.0);
        }
        let unbounded = run(&mut rain, &theme, w, h, None);
        let bounded = run(&mut warm, &theme, w, h, Some(20_000));
        assert!(
            bounded < unbounded / 2,
            "budget did not bound the frame: {bounded} vs {unbounded}"
        );
    }

    /// With room to spare the budgeted path must be the old path, byte for byte
    /// — not merely similar. The constants were captured from the renderer
    /// before the budget existed.
    #[test]
    fn a_slack_budget_changes_nothing() {
        fn fingerprint(w: u16, h: u16, seed: u64, frames: usize, budget: Option<usize>) -> u64 {
            let mut rain = Rain::new(
                w,
                h,
                Config {
                    seed: Some(seed),
                    ..Config::default()
                },
            );
            let theme = Theme::from_base((0, 255, 65), false);
            let mut rr = Renderer::new(w, h);
            rr.set_budget(budget);
            let mut fp: u64 = 0xcbf2_9ce4_8422_2325;
            let mut buf = Vec::with_capacity(1 << 22);
            for _ in 0..frames {
                rain.step(1.0 / 30.0);
                buf.clear();
                rr.draw(&mut buf, &rain, &theme, Depth::True)
                    .expect("writing to a Vec cannot fail");
                for b in &buf {
                    fp ^= u64::from(*b);
                    fp = fp.wrapping_mul(0x0000_0100_0000_01b3);
                }
            }
            fp
        }
        // Captured from the pre-budget renderer.
        assert_eq!(fingerprint(204, 175, 1, 400, None), 0x6391_4c4f_3d51_4118);
        assert_eq!(fingerprint(80, 24, 7, 300, None), 0xcb83_fa08_5b58_12d0);
        // A budget bigger than any frame needs must be indistinguishable.
        assert_eq!(
            fingerprint(204, 175, 1, 400, Some(1 << 24)),
            0x6391_4c4f_3d51_4118
        );
        assert_eq!(
            fingerprint(80, 24, 7, 300, Some(1 << 24)),
            0xcb83_fa08_5b58_12d0
        );
    }

    #[test]
    fn the_budget_is_spent_on_heads_first() {
        // The premise of the priority function: under pressure, the bright
        // leading edge gets drawn and the dim tail waits.
        let (w, h) = (100u16, 60u16);
        let mut rain = Rain::new(
            w,
            h,
            Config {
                seed: Some(23),
                density: 0.9,
                ..Config::default()
            },
        );
        let theme = Theme::from_base((0, 255, 65), false);
        let mut rr = Renderer::new(w, h);
        // Around half of what a frame wants: hard enough that most of the tail
        // waits, not so hard that the deadline is the only thing running.
        rr.set_budget(Some(6_000));
        let mut buf = Vec::new();
        for _ in 0..400 {
            rain.step(1.0 / 30.0);
            buf.clear();
            rr.draw(&mut buf, &rain, &theme, Depth::True)
                .expect("writing to a Vec cannot fail");
        }

        // One more starved frame: count how many of the heads it wanted to draw
        // it actually drew, against the same figure for dim cells.
        rain.step(1.0 / 30.0);
        let mut heads = (0usize, 0usize);
        let mut dim = (0usize, 0usize);
        {
            let before: Vec<Option<(char, Color)>> = rr.prev.clone();
            buf.clear();
            rr.draw(&mut buf, &rain, &theme, Depth::True)
                .expect("writing to a Vec cannot fail");
            for y in 0..h {
                for x in 0..w {
                    let i = y as usize * w as usize + x as usize;
                    let want = rain
                        .sample(x, y, &theme)
                        .map(|s| (s.ch, Depth::True.to_color(s.rgb), s.head));
                    let target = want.map(|(c, col, _)| (c, col));
                    if before[i] == target {
                        continue; // undamaged
                    }
                    let bucket = match want {
                        Some((_, _, true)) => &mut heads,
                        _ => &mut dim,
                    };
                    bucket.1 += 1;
                    if rr.prev[i] == target {
                        bucket.0 += 1;
                    }
                }
            }
        }
        assert!(heads.1 > 0 && dim.1 > 0, "nothing to compare");
        // Measured here: 87 of 87 heads drawn, against 280 of 2233 other
        // damaged cells. That gap is the entire point of the priority function.
        let head_rate = heads.0 as f64 / heads.1 as f64;
        let dim_rate = dim.0 as f64 / dim.1 as f64;
        assert!(
            head_rate > 0.95,
            "heads were starved: {}/{} drawn",
            heads.0,
            heads.1
        );
        assert!(
            head_rate > dim_rate * 2.0,
            "priority did nothing: heads {head_rate:.2} vs others {dim_rate:.2}"
        );
    }

    #[test]
    fn salience_orders_the_way_the_eye_does() {
        // A head outranks even a cell that has waited to its deadline.
        assert_eq!(salience(200, 0, true), HEAD_SCORE);
        assert!(HEAD_SCORE > 255 + u16::from(MAX_DEFER - 1) * AGE_GAIN);
        // Appearing or vanishing beats a small step at the same brightness.
        assert!(salience(180, 0, false) > salience(180, 160, false));
        // A dim cell's step matters less than the same step when bright.
        assert!(salience(40, 20, false) < salience(220, 200, false));
        // Pure glyph churn — colour unchanged — is the cheapest thing to defer.
        assert!(salience(90, 90, false) < salience(90, 40, false));
        assert!(salience(255, 0, false) <= 255, "must fit the histogram");
    }

    #[test]
    fn the_governor_only_throttles_once_a_write_blocks() {
        let period = Duration::from_millis(33);
        let mut g = Governor::new(204 * 175);
        let wide_open = g.budget();
        assert!(!g.throttling());

        // Instant writes teach it nothing, so it stays out of the way — this is
        // what keeps the output identical on a terminal that keeps up.
        for _ in 0..50 {
            g.observe(50_000, Duration::from_micros(200), period);
        }
        assert_eq!(g.budget(), wide_open);

        // A single blocked write is not evidence — it could just as easily be
        // this process losing the CPU for a moment.
        g.observe(50_000, Duration::from_millis(50), period);
        assert!(!g.throttling(), "one spike must not throttle");
        for _ in 0..30 {
            g.observe(50_000, Duration::from_micros(200), period);
        }
        assert_eq!(
            g.budget(),
            wide_open,
            "an isolated spike changed the budget"
        );

        // A consumer that consistently makes 50 KB take a frame and a half.
        for _ in 0..3 {
            g.observe(50_000, Duration::from_millis(50), period);
        }
        assert!(g.throttling());
        assert!(
            g.budget() < 50_000,
            "an overshoot must cut below what it just sent, got {}",
            g.budget()
        );

        // Once throttling, it settles rather than hunting: hold the load in the
        // dead band and the budget must stop moving, however long for.
        let in_band = period.mul_f32(TARGET_LOAD * 0.92);
        for _ in 0..8 {
            g.observe(g.budget(), in_band, period);
        }
        let settled = g.budget();
        for _ in 0..500 {
            g.observe(settled, in_band, period);
        }
        assert_eq!(g.budget(), settled, "budget hunted instead of settling");
        assert!(g.throttling(), "should still be throttling");

        // Consumer recovers: the budget climbs back to wide open, and once there
        // it stops constraining anything.
        for _ in 0..400 {
            g.observe(20_000, Duration::from_micros(100), period);
        }
        assert!(!g.throttling());
        assert_eq!(g.budget(), wide_open);
    }

    #[test]
    fn the_governor_never_collapses_to_nothing() {
        let period = Duration::from_millis(33);
        let mut g = Governor::new(80 * 24);
        for _ in 0..500 {
            g.observe(1, Duration::from_secs(1), period);
        }
        assert!(g.budget() >= 2048, "budget bottomed out at {}", g.budget());
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
