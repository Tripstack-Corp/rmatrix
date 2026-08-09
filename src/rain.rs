//! The simulation.
//!
//! Model: every cell carries a `glow` that decays continuously. A drop writes
//! glyphs at glow 1.0 as its head descends; the fade behind it is emergent
//! rather than a fixed-length tail buffer, so overlapping drops in one column
//! blend instead of clobbering each other.
//!
//! All motion is expressed in rows/second and integrated against real elapsed
//! time, so the animation runs at the same speed regardless of frame rate.
//!
//! The generator is owned by [`Rain`] and seeded by the caller, so a seed
//! replays an identical animation — that is what makes the sim testable.

use crate::theme::{Rgb, Theme};
use rand::RngExt;
use rand::SeedableRng;
use rand::rngs::StdRng;

/// Below this, a cell is considered dark and gets erased.
const EPS: f32 = 0.012;
/// Guards the `speed / tail` decay solve against a divide-by-nearly-zero.
const MIN_TAIL: f32 = 0.5;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cell {
    pub ch: char,
    pub glow: f32,
    /// Per-second multiplicative decay, captured from the drop that wrote it.
    pub decay: f32,
    pub head: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: ' ',
            glow: 0.0,
            decay: 0.5,
            head: false,
        }
    }
}

#[derive(Debug)]
struct Drop {
    row: f32,
    speed: f32,
    tail: f32,
    /// Index of the cell currently rendered as the head, so we can un-head it.
    head_idx: Option<usize>,
}

#[derive(Debug)]
struct Column {
    drops: Vec<Drop>,
    cooldown: f32,
    /// Vertical gap to leave behind a drop before the next one starts.
    gap: f32,
    hue: f32,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub speed: f32,
    pub density: f32,
    pub tail_min: f32,
    pub tail_max: f32,
    pub mutate: f32,
    pub glyphs: Vec<char>,
    /// `None` draws a seed from the OS; `Some` replays deterministically.
    pub seed: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            speed: 1.0,
            density: 0.55,
            tail_min: 6.0,
            tail_max: 26.0,
            mutate: 0.35,
            glyphs: crate::charset::Charset::Classic.glyphs(""),
            seed: None,
        }
    }
}

pub struct Rain {
    w: u16,
    h: u16,
    cells: Vec<Cell>,
    cols: Vec<Column>,
    cfg: Config,
    rng: StdRng,
    /// Seconds since start; drives the rainbow rotation.
    pub elapsed: f32,
    /// Live speed control, multiplied on top of `cfg.speed`.
    pub speed_mul: f32,
}

impl Rain {
    #[must_use]
    pub fn new(w: u16, h: u16, cfg: Config) -> Rain {
        let rng = match cfg.seed {
            Some(s) => StdRng::seed_from_u64(s),
            None => StdRng::from_rng(&mut rand::rng()),
        };
        let mut r = Rain {
            w: 0,
            h: 0,
            cells: Vec::new(),
            cols: Vec::new(),
            cfg,
            rng,
            elapsed: 0.0,
            speed_mul: 1.0,
        };
        r.resize(w, h);
        r
    }

    #[must_use]
    pub fn width(&self) -> u16 {
        self.w
    }

    #[must_use]
    pub fn height(&self) -> u16 {
        self.h
    }

    /// The multiplier actually applied to elapsed time: the configured
    /// `speed` and the live [`speed_mul`](Self::speed_mul) combined.
    ///
    /// `step` reads it from here rather than multiplying the two inline so a
    /// caller that *displays* the speed cannot disagree with the one that
    /// applies it. The stats overlay is such a caller.
    #[must_use]
    pub fn speed(&self) -> f32 {
        self.speed_mul * self.cfg.speed
    }

    pub fn set_glyphs(&mut self, glyphs: Vec<char>) {
        self.cfg.glyphs = glyphs;
    }

    /// Rebuilt from scratch — cheap, and it sidesteps the whole class of resize
    /// bugs that come from reindexing a live grid.
    pub fn resize(&mut self, w: u16, h: u16) {
        self.w = w;
        self.h = h;
        self.cells = vec![Cell::default(); w as usize * h as usize];
        self.cols = Vec::with_capacity(w as usize);
        for x in 0..w as usize {
            let hue = x as f32 / f32::from(w.max(1));
            self.cols.push(Column {
                drops: Vec::new(),
                // Stagger the start so the first frame isn't one flat wave.
                cooldown: self.rng.random_range(0.0..2.5),
                gap: self.rng.random_range(2.0..14.0),
                hue,
            });
        }
    }

    /// `None` when out of bounds, so a renderer that has drifted out of sync
    /// with the grid degrades to blank cells instead of panicking.
    #[must_use]
    pub fn cell(&self, x: u16, y: u16) -> Option<&Cell> {
        if x >= self.w || y >= self.h {
            return None;
        }
        self.cells.get(y as usize * self.w as usize + x as usize)
    }

    pub fn step(&mut self, dt: f32) {
        if !dt.is_finite() || dt <= 0.0 {
            return;
        }
        self.elapsed += dt;
        let dt_s = (dt * self.speed()).max(0.0);
        if dt_s == 0.0 {
            return;
        }
        self.decay(dt_s);
        self.advance(dt_s);
        self.spawn(dt_s);
        self.churn(dt_s);
    }

    fn decay(&mut self, dt: f32) {
        for c in &mut self.cells {
            if c.glow > 0.0 {
                c.glow *= c.decay.powf(dt);
                if c.glow < EPS {
                    *c = Cell::default();
                }
            }
        }
    }

    fn advance(&mut self, dt: f32) {
        let Rain {
            w,
            h,
            cells,
            cols,
            cfg,
            rng,
            ..
        } = self;
        let (w, h) = (*w as usize, *h);
        for (x, col) in cols.iter_mut().enumerate() {
            for d in col.drops.iter_mut() {
                let prev = d.row;
                d.row += d.speed * dt;
                // Solve decay so glow reaches EPS exactly `tail` rows behind the
                // head, given this drop's speed.
                let decay = EPS.powf(d.speed / d.tail.max(MIN_TAIL));
                for r in (prev.floor() as i32 + 1)..=(d.row.floor() as i32) {
                    // Un-head before the bounds test, not after it. Skipping
                    // this when the new row is off the bottom left the last
                    // on-screen cell flagged `head: true`, and `Theme::color`
                    // returns full head-white whenever that flag is set, whatever
                    // the glow — so a white glyph sat parked on the bottom row
                    // until decay dragged it under EPS, up to `tail / speed`
                    // seconds later.
                    if let Some(p) = d.head_idx.take()
                        && let Some(c) = cells.get_mut(p)
                    {
                        c.head = false;
                    }
                    if r < 0 || r >= i32::from(h) {
                        continue;
                    }
                    let idx = r as usize * w + x;
                    let ch = pick(rng, &cfg.glyphs);
                    if let Some(c) = cells.get_mut(idx) {
                        *c = Cell {
                            ch,
                            glow: 1.0,
                            decay,
                            head: true,
                        };
                        d.head_idx = Some(idx);
                    }
                }
            }
            // By the time a drop is this far gone its trail has already decayed
            // below EPS, so nothing on screen still refers to it. Its head is
            // gone too, and strictly earlier: `tail` is at least MIN_TAIL, so a
            // drop only becomes removable well after its row passed `h`, and the
            // loop above clears `head_idx` on the crossing itself. Dropping a
            // `Drop` therefore cannot strand a flagged cell — which it would,
            // permanently, since nothing else ever clears the flag.
            col.drops.retain(|d| d.row - d.tail < f32::from(h));
        }
    }

    fn spawn(&mut self, dt: f32) {
        let Rain { cols, cfg, rng, .. } = self;
        for col in cols.iter_mut() {
            col.cooldown -= dt;
            let ready = col.drops.last().is_none_or(|d| d.row > d.tail + col.gap);
            if !ready || col.cooldown > 0.0 {
                continue;
            }
            if rng.random::<f32>() > cfg.density {
                // Column sits this one out — keeps the field from filling in.
                col.cooldown = rng.random_range(0.4..3.0);
                continue;
            }
            let hi = cfg.tail_max.max(cfg.tail_min + 0.1);
            col.drops.push(Drop {
                row: -rng.random_range(0.0..6.0),
                speed: rng.random_range(6.0..26.0),
                tail: rng.random_range(cfg.tail_min..hi),
                head_idx: None,
            });
            col.gap = rng.random_range(2.0..14.0);
            col.cooldown = rng.random_range(0.0..0.6);
        }
    }

    /// Churn glyphs in place. Budgeted by area so the cost is frame-rate
    /// independent rather than per-cell-per-frame.
    fn churn(&mut self, dt: f32) {
        let Rain {
            cells, cfg, rng, ..
        } = self;
        if cfg.mutate <= 0.0 || cells.is_empty() {
            return;
        }
        let budget = ((cells.len() as f32) * cfg.mutate * dt) as usize;
        for _ in 0..budget.min(cells.len()) {
            let i = rng.random_range(0..cells.len());
            // Leave the head alone; re-rolling it reads as a stutter.
            if cells[i].glow > EPS && !cells[i].head {
                cells[i].ch = pick(rng, &cfg.glyphs);
            }
        }
    }

    /// The glyph and colour to draw at `(x, y)`, or `None` if the cell is dark.
    #[must_use]
    pub fn color_of(&self, x: u16, y: u16, theme: &Theme) -> Option<(char, Rgb)> {
        let c = self.cell(x, y)?;
        if c.glow <= 0.0 {
            return None;
        }
        let hue = self.cols.get(x as usize).map_or(0.0, |c| c.hue) + self.elapsed * 0.08;
        Some((c.ch, theme.color(c.glow, c.head, hue)))
    }
}

fn pick(rng: &mut StdRng, glyphs: &[char]) -> char {
    if glyphs.is_empty() {
        return ' ';
    }
    glyphs[rng.random_range(0..glyphs.len())]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(seed: u64) -> Config {
        Config {
            seed: Some(seed),
            ..Config::default()
        }
    }

    /// A comparable snapshot of every cell.
    fn snapshot(r: &Rain) -> Vec<Cell> {
        (0..r.height())
            .flat_map(|y| (0..r.width()).map(move |x| (x, y)))
            .map(|(x, y)| *r.cell(x, y).expect("in bounds by construction"))
            .collect()
    }

    /// Advance a grid and return a comparable snapshot of every cell.
    fn run(w: u16, h: u16, cfg: Config, frames: usize) -> Vec<Cell> {
        let mut r = Rain::new(w, h, cfg);
        for _ in 0..frames {
            r.step(1.0 / 60.0);
        }
        snapshot(&r)
    }

    #[test]
    fn same_seed_replays_identically() {
        let a = run(40, 20, cfg(7), 120);
        let b = run(40, 20, cfg(7), 120);
        assert_eq!(a, b);
    }

    #[test]
    fn different_seeds_diverge() {
        let a = run(40, 20, cfg(7), 120);
        let b = run(40, 20, cfg(8), 120);
        assert_ne!(a, b);
    }

    #[test]
    fn the_reported_speed_is_the_one_step_applies() {
        // The stats overlay prints `Rain::speed`. The moment that stops
        // agreeing with the multiplier inside `step`, the readout becomes a
        // plausible lie — the worst kind, because nothing looks broken. Both
        // now read the same accessor; this pins that they keep doing so.
        let mut r = Rain::new(
            40,
            20,
            Config {
                speed: 0.5,
                ..cfg(7)
            },
        );
        r.speed_mul = 0.5;
        assert!((r.speed() - 0.25).abs() < 1e-6, "speed() was {}", r.speed());

        // Driven, not merely read: at zero effective speed the grid must not
        // move, while `elapsed` keeps tracking real time because it drives the
        // rainbow rotation rather than the rain.
        r.speed_mul = 0.0;
        let before = snapshot(&r);
        r.step(1.0);
        assert_eq!(snapshot(&r), before, "zero speed must freeze the rain");
        assert!((r.elapsed - 1.0).abs() < 1e-6, "elapsed was {}", r.elapsed);
    }

    #[test]
    fn speed_only_ever_scales_the_clock() {
        // Half speed for twice as long lands exactly where full speed for half
        // the time does. That equivalence is what lets `--speed 0.4` be sold as
        // "the same rain, slower" rather than a different animation.
        //
        // One step each, rather than a loop: both runs then draw from the RNG
        // the same number of times, so the seeds stay comparable. Stepping at
        // different dt would change the draw count and compare two different
        // realisations — see CLAUDE.md under Performance.
        let mut slow = Rain::new(
            40,
            20,
            Config {
                speed: 0.5,
                ..cfg(11)
            },
        );
        let mut fast = Rain::new(40, 20, cfg(11));
        slow.step(2.0);
        fast.step(1.0);
        assert_eq!(snapshot(&slow), snapshot(&fast));
    }

    #[test]
    fn rain_actually_lights_cells() {
        let lit = run(40, 20, cfg(1), 120)
            .iter()
            .filter(|c| c.glow > 0.0)
            .count();
        assert!(lit > 0, "nothing was drawn");
    }

    #[test]
    fn glyph_selection_is_uniform() {
        // Guards the sampler: a biased pick makes the rain look repetitive.
        let mut rng = StdRng::seed_from_u64(99);
        let glyphs = crate::charset::Charset::Katakana.glyphs("");
        let mut counts: HashMap<char, usize> = HashMap::new();
        let n = 56_000;
        for _ in 0..n {
            *counts.entry(pick(&mut rng, &glyphs)).or_default() += 1;
        }
        assert_eq!(counts.len(), glyphs.len(), "some glyphs never appeared");
        let expected = n as f64 / glyphs.len() as f64;
        for (g, c) in &counts {
            let dev = (*c as f64 - expected).abs() / expected;
            assert!(
                dev < 0.20,
                "glyph {g:?} deviates {:.1}% from uniform",
                dev * 100.0
            );
        }
    }

    #[test]
    fn a_dark_cell_never_keeps_a_stale_glyph() {
        // The renderer blanks cells by glow alone, so a dark cell that kept its
        // glyph would leave a permanent smudge on screen.
        let (w, h) = (20u16, 20u16);
        let mut r = Rain::new(
            w,
            h,
            Config {
                density: 1.0,
                ..cfg(3)
            },
        );
        let mut ever_lit = false;
        for _ in 0..600 {
            r.step(1.0 / 60.0);
            for y in 0..h {
                for x in 0..w {
                    let c = r.cell(x, y).expect("in bounds");
                    if c.glow > 0.0 {
                        ever_lit = true;
                    } else {
                        assert_eq!(c.ch, ' ', "dark cell at ({x},{y}) kept {:?}", c.ch);
                        assert!(!c.head, "dark cell at ({x},{y}) is still a head");
                    }
                }
            }
        }
        assert!(ever_lit, "nothing ever rendered");
    }

    #[test]
    fn a_trail_fades_through_many_levels() {
        // This is the whole visual premise: cmatrix has 3 brightness steps, we
        // want a continuum. A trail collapsing to one level would still "work".
        let mut r = Rain::new(
            1,
            60,
            Config {
                density: 1.0,
                mutate: 0.0,
                ..cfg(21)
            },
        );
        let mut best = 0usize;
        for _ in 0..600 {
            r.step(1.0 / 60.0);
            let levels: std::collections::BTreeSet<u32> = (0..60)
                .filter_map(|y| r.cell(0, y))
                .filter(|c| c.glow > 0.0)
                .map(|c| (c.glow * 1000.0) as u32)
                .collect();
            best = best.max(levels.len());
        }
        assert!(
            best >= 5,
            "trail only reached {best} distinct brightness levels"
        );
    }

    /// Longest run of consecutive frames for which `cell` was flagged as a head.
    ///
    /// A head legitimately occupies one cell only until its drop crosses into
    /// the next row. The slowest drop falls 6 rows/s, so at 60 fps that is ten
    /// frames of stepping plus the frame it was written in — eleven. Anything
    /// beyond that is a flag nobody is going to clear.
    fn longest_head_run(w: u16, h: u16, cfg: Config, frames: usize) -> u32 {
        let mut r = Rain::new(w, h, cfg);
        let mut run = vec![0u32; w as usize * h as usize];
        let mut worst = 0u32;
        for _ in 0..frames {
            r.step(1.0 / 60.0);
            for y in 0..h {
                for x in 0..w {
                    let i = y as usize * w as usize + x as usize;
                    if r.cell(x, y).expect("in bounds by construction").head {
                        run[i] += 1;
                        worst = worst.max(run[i]);
                    } else {
                        run[i] = 0;
                    }
                }
            }
        }
        worst
    }

    #[test]
    fn a_head_that_falls_off_screen_stops_being_a_head() {
        // The bug: `advance` hit `continue` on the out-of-bounds row and skipped
        // the un-heading branch, so the last on-screen cell kept `head: true`.
        // `Theme::color` paints a head full white regardless of glow, leaving a
        // white glyph parked on the bottom row until decay finished — with the
        // 40-row tail used here, several hundred frames.
        let h = 20u16;
        let worst = longest_head_run(
            1,
            h,
            Config {
                density: 1.0,
                mutate: 0.0,
                tail_min: 40.0,
                tail_max: 40.5,
                ..cfg(13)
            },
            1800,
        );
        assert!(worst > 0, "no head ever reached the grid");
        assert!(
            worst <= 12,
            "a cell stayed flagged as the head for {worst} frames"
        );
    }

    #[test]
    fn no_head_outlives_the_drop_that_wrote_it() {
        // The other way a flag could be stranded: `advance` retires a drop once
        // its trail is off screen, and a drop retired while `head_idx` still
        // pointed at a live cell would leave that cell white forever. It cannot
        // happen — the head is cleared on the crossing, which is strictly
        // earlier — and this pins the ordering across a busy grid rather than
        // one contrived column.
        let worst = longest_head_run(
            40,
            30,
            Config {
                density: 1.0,
                tail_min: 40.0,
                tail_max: 41.0,
                ..cfg(17)
            },
            1200,
        );
        assert!(worst > 0, "nothing ever rendered");
        assert!(
            worst <= 12,
            "a cell stayed flagged as the head for {worst} frames"
        );
    }

    #[test]
    fn zero_density_never_spawns() {
        let lit = run(
            30,
            15,
            Config {
                density: 0.0,
                ..cfg(5)
            },
            300,
        )
        .iter()
        .filter(|c| c.glow > 0.0)
        .count();
        assert_eq!(lit, 0);
    }

    #[test]
    fn degenerate_sizes_do_not_panic() {
        for (w, h) in [(0, 0), (1, 1), (0, 20), (20, 0)] {
            let mut r = Rain::new(w, h, cfg(2));
            for _ in 0..30 {
                r.step(1.0 / 60.0);
            }
            assert!(r.cell(w, h).is_none());
        }
    }

    #[test]
    fn out_of_bounds_reads_are_none_not_panics() {
        let r = Rain::new(4, 4, cfg(2));
        assert!(r.cell(4, 0).is_none());
        assert!(r.cell(0, 4).is_none());
        assert!(r.cell(u16::MAX, u16::MAX).is_none());
    }

    #[test]
    fn hostile_dt_values_are_ignored() {
        let mut r = Rain::new(20, 10, cfg(4));
        for dt in [f32::NAN, f32::INFINITY, -1.0, 0.0, -f32::INFINITY] {
            r.step(dt);
        }
        let before = r.elapsed;
        assert_eq!(before, 0.0, "a bogus dt advanced the clock");
        r.step(1.0);
        assert!(r.elapsed > 0.0);
    }

    #[test]
    fn a_long_stall_does_not_panic() {
        let mut r = Rain::new(60, 30, cfg(6));
        r.step(600.0); // e.g. the machine slept
        assert_eq!(r.width(), 60);
    }

    #[test]
    fn resize_clears_and_reshapes() {
        let mut r = Rain::new(40, 20, cfg(9));
        for _ in 0..120 {
            r.step(1.0 / 60.0);
        }
        r.resize(15, 7);
        assert_eq!((r.width(), r.height()), (15, 7));
        for y in 0..7 {
            for x in 0..15 {
                assert_eq!(r.cell(x, y).expect("in bounds").glow, 0.0);
            }
        }
        // And it keeps running afterwards.
        for _ in 0..120 {
            r.step(1.0 / 60.0);
        }
        assert!((0..7).any(|y| (0..15).any(|x| r.cell(x, y).is_some_and(|c| c.glow > 0.0))));
    }

    #[test]
    fn empty_glyph_set_falls_back_to_blank_not_panic() {
        let mut r = Rain::new(
            10,
            10,
            Config {
                glyphs: vec![],
                density: 1.0,
                ..cfg(11)
            },
        );
        for _ in 0..120 {
            r.step(1.0 / 60.0);
        }
        assert!((0..10).all(|y| (0..10).all(|x| r.cell(x, y).expect("in bounds").ch == ' ')));
    }

    /// Longest unbroken run of lit cells in a one-column grid. A column can hold
    /// several drops at once, so total-lit would conflate them; the contiguous
    /// run is the thing a viewer actually perceives as "the tail".
    fn longest_trail(tail_min: f32, tail_max: f32, seed: u64) -> usize {
        let h = 80u16;
        let mut r = Rain::new(
            1,
            h,
            Config {
                density: 1.0,
                mutate: 0.0,
                tail_min,
                tail_max,
                ..cfg(seed)
            },
        );
        let mut best = 0usize;
        for _ in 0..900 {
            r.step(1.0 / 60.0);
            let mut run = 0usize;
            for y in 0..h {
                if r.cell(0, y).is_some_and(|c| c.glow > 0.0) {
                    run += 1;
                    best = best.max(run);
                } else {
                    run = 0;
                }
            }
        }
        best
    }

    #[test]
    fn tail_config_controls_trail_length() {
        let short = longest_trail(3.0, 3.5, 12);
        let long = longest_trail(30.0, 32.0, 12);
        assert!(
            short > 0 && long > 0,
            "nothing rendered (short={short}, long={long})"
        );
        assert!(
            long > short * 2,
            "--tail had little effect: short={short} rows, long={long} rows"
        );
    }
}
