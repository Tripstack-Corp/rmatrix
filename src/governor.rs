//! Closed-loop quality governor.
//!
//! The complaint this exists to answer is "it gets janky when my computer gets
//! busy". It gets janky because we are not the expensive process — the terminal
//! is. When the machine is loaded the emulator drains the pty more slowly, our
//! `write` blocks, and the whole loop stalls behind it.
//!
//! # The signal
//!
//! *Time spent blocked in `write`*, as a fraction of the frame budget — not
//! frame time. The distinction matters: if we are merely descheduled, or our
//! own diff is slow, frame time rises but sending fewer bytes would not help,
//! and a governor driven by frame time would throw away picture for nothing.
//! Time blocked in `write` rises only when the terminal cannot keep up, which
//! is exactly the condition fewer bytes fixes.
//!
//! It is summarised by a *moving median*, not a mean, and that is load-bearing.
//! Measured at 204x175 against a reader that keeps up easily, blocked time per
//! frame is p50 1.4 ms but p99 11.5 ms and max 102 ms — a pty is bursty even
//! when nothing is wrong. The mean of that (0.07 of a frame) sits close enough
//! to a genuinely saturated terminal to be useless as a discriminator, and an
//! EWMA of it is worse, because one 100 ms outlier drags the estimate over any
//! threshold for several frames. The median does not care:
//!
//! | terminal | median blocked, as a fraction of the frame budget |
//! |---|---|
//! | keeping up  | 0.04 |
//! | 2.0 MB/s    | 0.25 |
//! | 1.6 MB/s    | 0.89 |
//! | 1.2 MB/s    | 1.86 |
//!
//! Two windows, both medians: a short one (9 frames) gates shedding, so five
//! genuinely bad frames move it but one outlier cannot; a long one (61 frames,
//! about two seconds) gates recovery.
//!
//! # The actuator
//!
//! [`Renderer::set_redraw_tolerance`], chosen over `--levels` on measured
//! evidence. At 204x175, moving `levels` mid-flight re-quantises the whole
//! screen and costs 2.0-2.5x a normal frame *on the frame it moves* — an extra
//! spike landing precisely when the terminal is already behind. Raising the
//! redraw tolerance instead costs 0.26-0.5x a normal frame, because it works by
//! declining to repaint cells. It is also continuous, where `levels` is a
//! handful of coarse notches.
//!
//! # Why it is an integrator and not a ladder
//!
//! Blocked time is not proportional to what we send. It is *overflow*: zero
//! while the terminal keeps up, then climbing steeply once it saturates. Against
//! a plant that rectifies like that, any actuator with discrete steps hunts —
//! a step big enough to relieve the overload necessarily takes the load to
//! zero, which reads as "idle", which gives the step back. That is not a tuning
//! problem, it is structural, and an early ladder-shaped draft of this file did
//! exactly that: four rung changes in fourteen seconds, measured.
//!
//! So the tolerance is a continuous state that climbs while we are overloaded
//! and bleeds off while we are not, with a deadband between. It settles at
//! whatever quality the terminal can actually sustain, and the residual ripple
//! is a couple of RGB units — smaller than the pen tolerance we already ship,
//! and far below anything visible.
//!
//! [`Renderer::set_redraw_tolerance`]: crate::render::Renderer::set_redraw_tolerance

use std::io::Write;
use std::time::{Duration, Instant};

/// The knobs the governor is allowed to turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Quality {
    pub redraw_tolerance: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct GovernorConfig {
    /// Fraction of the frame budget spent blocked in `write` above which we
    /// start shedding quality.
    pub degrade_above: f32,
    /// ...and below which we start taking it back. The gap is the deadband the
    /// loop settles inside.
    pub recover_below: f32,
    /// Frames in the short median window, which gates shedding. Odd, so the
    /// median is a real sample. Kept short so a stall is answered in ~150 ms.
    pub fast_window: u32,
    /// Frames in the long median window, which gates recovery. Two seconds, so
    /// "the machine went quiet" has to actually mean quiet.
    pub slow_window: u32,
    /// RGB units of tolerance added per frame while overloaded.
    pub climb: f32,
    /// RGB units given back per frame while idle. Much slower than `climb`:
    /// shedding quality is free and urgent, taking it back makes every stale
    /// cell repaint and is neither.
    pub bleed: f32,
    /// Ceiling on the drift, in whole steps of the theme's own brightness ramp.
    ///
    /// Four steps is the point past which the fade starts to read as banding.
    /// For reference, `--levels 8` — which the README already recommends for a
    /// full-screen window — sits at a p99 on-screen error of 16 RGB units, and
    /// four steps of the default 24-level ramp is ~44.
    pub max_steps: f32,
}

impl Default for GovernorConfig {
    fn default() -> Self {
        GovernorConfig {
            // Both thresholds sit in the gap between the two regimes in the
            // table above: comfortably over a healthy terminal's 0.04 median,
            // comfortably under a saturated one's 0.25. Setting either by
            // intuition rather than from that distribution is how the first
            // draft ended up with a recover threshold *below* the idle noise
            // floor, which meant it could never recover at all.
            degrade_above: 0.12,
            recover_below: 0.06,
            fast_window: 9,
            slow_window: 61,
            climb: 2.0,
            bleed: 0.25,
            max_steps: 4.0,
        }
    }
}

/// A pure controller: feed it how long the last frame spent blocked, get back
/// the quality to draw the next one at. No clock, no I/O, no randomness, so a
/// given sequence of observations always produces the same sequence of outputs.
#[derive(Clone, Debug)]
pub struct Governor {
    cfg: GovernorConfig,
    step: f32,
    /// The last `slow_window` samples, newest at `head`.
    ring: [f32; MAX_WINDOW],
    head: usize,
    /// Samples seen so far, saturating at `slow_window`. Until the long window
    /// is full there is no evidence for recovery, so recovery waits.
    seen: usize,
    tol: f32,
}

/// Cap on `slow_window`. A couple of seconds of frames is all the history the
/// decision uses, and a fixed array keeps `observe` allocation-free.
const MAX_WINDOW: usize = 61;

impl Governor {
    #[must_use]
    pub fn new(ramp_step: f32, cfg: GovernorConfig) -> Governor {
        Governor {
            cfg,
            step: if ramp_step.is_finite() && ramp_step > 0.0 {
                ramp_step
            } else {
                1.0
            },
            ring: [0.0; MAX_WINDOW],
            head: 0,
            seen: 0,
            tol: 0.0,
        }
    }

    /// Median of the last `n` samples, as a fraction of the frame budget.
    fn median(&self, n: usize) -> f32 {
        // Deliberately *not* narrowed to how many samples we have: the ring
        // starts at zero, so an unfilled window reads as a healthy terminal.
        // Narrowing it instead makes the first frame's median equal the first
        // frame, and one unlucky startup outlier then costs real quality.
        let n = n.clamp(1, MAX_WINDOW);
        let mut buf = [0.0f32; MAX_WINDOW];
        for (i, slot) in buf.iter_mut().enumerate().take(n) {
            // `head` points at the newest sample; walk backwards from it.
            *slot = self.ring[(self.head + MAX_WINDOW - i) % MAX_WINDOW];
        }
        let w = &mut buf[..n];
        w.sort_by(f32::total_cmp);
        w[n / 2]
    }

    /// The current load estimate, as a fraction of the frame budget.
    #[must_use]
    pub fn load(&self) -> f32 {
        self.median(self.cfg.fast_window as usize)
    }

    #[must_use]
    pub fn quality(&self) -> Quality {
        Quality {
            redraw_tolerance: self.tol.round().clamp(0.0, f32::from(u16::MAX)) as u16,
        }
    }

    /// How much quality has been given up, in whole steps of the ramp. Purely
    /// for display — the control state is the continuous tolerance.
    #[must_use]
    pub fn steps_shed(&self) -> f32 {
        self.tol / self.step
    }

    fn ceiling(&self) -> f32 {
        (self.cfg.max_steps * self.step).max(0.0)
    }

    /// Fold in one frame's backpressure and return the quality for the next.
    pub fn observe(&mut self, blocked: Duration, budget: Duration) -> Quality {
        let budget = budget.as_secs_f32();
        // A nonsense budget must not be able to drive the controller.
        let sample = if budget > 0.0 && budget.is_finite() {
            (blocked.as_secs_f32() / budget).clamp(0.0, 4.0)
        } else {
            0.0
        };

        self.head = (self.head + 1) % MAX_WINDOW;
        self.ring[self.head] = sample;
        self.seen = self.seen.saturating_add(1);

        let fast = self.median(self.cfg.fast_window as usize);
        let slow = self.median(self.cfg.slow_window as usize);

        // Shedding answers the short window, recovery the long one. Between the
        // two thresholds nothing moves at all, which is where it settles.
        // Recovery additionally waits for the long window to actually fill, so a
        // fresh start cannot read "no history" as "plenty of headroom".
        if fast > self.cfg.degrade_above {
            self.tol = (self.tol + self.cfg.climb.max(0.0)).min(self.ceiling());
        } else if slow < self.cfg.recover_below && self.seen >= self.cfg.slow_window as usize {
            self.tol = (self.tol - self.cfg.bleed.max(0.0)).max(0.0);
        }
        self.quality()
    }
}

/// Times how long the wrapped writer spends in `write` and `flush`.
///
/// Put this *under* the `BufWriter`, not over it, so it only ever sees real
/// syscalls: `BufWriter::new(Backpressure::new(stdout()))`. What it measures is
/// the pty refusing to accept more because the terminal has not caught up —
/// the governor's entire input.
#[derive(Debug)]
pub struct Backpressure<W> {
    inner: W,
    elapsed: Duration,
}

impl<W> Backpressure<W> {
    pub fn new(inner: W) -> Backpressure<W> {
        Backpressure {
            inner,
            elapsed: Duration::ZERO,
        }
    }

    /// Time spent blocked since the last [`Backpressure::reset`].
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }

    pub fn reset(&mut self) {
        self.elapsed = Duration::ZERO;
    }
}

impl<W: Write> Write for Backpressure<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let t = Instant::now();
        let r = self.inner.write(buf);
        self.elapsed += t.elapsed();
        r
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let t = Instant::now();
        let r = self.inner.flush();
        self.elapsed += t.elapsed();
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUDGET: Duration = Duration::from_micros(33_333);
    /// One step of the default 24-level green ramp.
    const STEP: f32 = 10.9;

    fn gov() -> Governor {
        Governor::new(STEP, GovernorConfig::default())
    }

    fn feed(g: &mut Governor, load: f32, frames: usize) {
        for _ in 0..frames {
            g.observe(BUDGET.mul_f32(load), BUDGET);
        }
    }

    /// Bytes per frame against redraw tolerance, measured at 204x175 by
    /// `examples/sweep.rs`. The plant the closed-loop tests run against; note
    /// the plateaus, which are real — a tolerance below one ramp step changes
    /// nothing at all.
    const PLANT: [(f32, f32); 7] = [
        (0.0, 78314.0),
        (8.0, 78314.0),
        (16.0, 57476.0),
        (24.0, 46087.0),
        (32.0, 42411.0),
        (48.0, 34644.0),
        (64.0, 31650.0),
    ];

    fn demand(tol: f32) -> f32 {
        let mut out = PLANT[PLANT.len() - 1].1;
        for w in PLANT.windows(2) {
            let ((x0, y0), (x1, y1)) = (w[0], w[1]);
            if tol <= x1 {
                let t = ((tol - x0) / (x1 - x0)).clamp(0.0, 1.0);
                out = y0 + (y1 - y0) * t;
                break;
            }
        }
        out
    }

    /// The closed loop, not the controller in isolation. `capacity` is what the
    /// terminal can absorb per frame; blocked time is the overflow past it,
    /// which is the shape that makes a stepped actuator hunt.
    fn closed_loop(capacity: f32, frames: usize) -> Vec<f32> {
        let mut g = gov();
        let mut trace = Vec::with_capacity(frames);
        for _ in 0..frames {
            let load = (demand(g.tol) / capacity - 1.0).max(0.0);
            g.observe(BUDGET.mul_f32(load), BUDGET);
            trace.push(g.tol);
        }
        trace
    }

    #[test]
    fn idle_never_gives_up_quality() {
        let mut g = gov();
        feed(&mut g, 0.0, 600);
        assert_eq!(
            g.quality(),
            Quality {
                redraw_tolerance: 0
            }
        );
    }

    #[test]
    fn a_load_inside_the_deadband_is_left_alone() {
        let mut g = gov();
        feed(&mut g, 0.06, 1200);
        assert_eq!(
            g.quality(),
            Quality {
                redraw_tolerance: 0
            },
            "the deadband is not a deadband"
        );
    }

    #[test]
    fn sustained_overload_climbs_to_the_ceiling_and_stops() {
        let mut g = gov();
        feed(&mut g, 3.0, 600);
        let cap = (GovernorConfig::default().max_steps * STEP).round() as u16;
        assert_eq!(g.quality().redraw_tolerance, cap);
        feed(&mut g, 3.0, 600);
        assert_eq!(
            g.quality().redraw_tolerance,
            cap,
            "climbed past the ceiling"
        );
    }

    #[test]
    fn it_sheds_fast_and_recovers_slow() {
        let mut g = gov();
        let mut to_shed = 0;
        while g.quality().redraw_tolerance == 0 {
            g.observe(BUDGET.mul_f32(3.0), BUDGET);
            to_shed += 1;
            assert!(to_shed < 60, "never reacted to a 3x overload");
        }
        assert!(to_shed <= 10, "took {to_shed} frames to react");

        feed(&mut g, 3.0, 300);
        let deep = g.quality().redraw_tolerance;
        let mut to_recover = 0;
        while g.quality().redraw_tolerance == deep {
            g.observe(Duration::ZERO, BUDGET);
            to_recover += 1;
            assert!(to_recover < 6000, "never recovered");
        }
        assert!(
            to_recover > to_shed * 3,
            "recovery ({to_recover} frames) is not meaningfully slower than \
             shedding ({to_shed} frames)"
        );
    }

    #[test]
    fn it_returns_to_full_quality_when_the_machine_goes_idle() {
        let mut g = gov();
        feed(&mut g, 3.0, 600);
        assert!(g.quality().redraw_tolerance > 0);
        feed(&mut g, 0.0, 3000); // 100 s at 30 fps
        assert_eq!(
            g.quality(),
            Quality {
                redraw_tolerance: 0
            },
            "never came back to full quality"
        );
    }

    #[test]
    fn it_is_deterministic_for_a_fixed_sequence() {
        let seq: Vec<f32> = (0..900)
            .map(|i| match i % 7 {
                0 => 2.4,
                1 => 0.0,
                2 => 0.11,
                3 => 0.019,
                4 => 1.2,
                5 => 0.07,
                _ => 0.8,
            })
            .collect();
        let trace = || {
            let mut g = gov();
            seq.iter()
                .map(|l| g.observe(BUDGET.mul_f32(*l), BUDGET).redraw_tolerance)
                .collect::<Vec<_>>()
        };
        assert_eq!(trace(), trace());
    }

    #[test]
    fn the_closed_loop_settles_without_pumping() {
        // Terminals that can absorb between 90% and 45% of what full quality
        // asks of them. In every case the tolerance must come to rest, and the
        // residual ripple must be far below one step of the ramp — anything
        // bigger is a quality level the viewer can watch breathing.
        for pct in [90u32, 80, 70, 60, 50, 45] {
            let capacity = 78314.0 * pct as f32 / 100.0;
            let trace = closed_loop(capacity, 2400); // 80 s at 30 fps
            let tail = &trace[trace.len() / 2..];
            let lo = tail.iter().copied().fold(f32::MAX, f32::min);
            let hi = tail.iter().copied().fold(f32::MIN, f32::max);
            assert!(
                hi - lo < STEP,
                "capacity {pct}%: tolerance still swinging {:.1} RGB units \
                 ({lo:.1}..{hi:.1}), which is {:.1} steps of the ramp",
                hi - lo,
                (hi - lo) / STEP
            );
        }
    }

    #[test]
    fn the_closed_loop_actually_relieves_the_overload() {
        // Settling is worthless if it settles somewhere useless.
        let capacity = 78314.0 * 0.6;
        let trace = closed_loop(capacity, 2400);
        let settled = trace[trace.len() - 1];
        let before = (demand(0.0) / capacity - 1.0).max(0.0);
        let after = (demand(settled) / capacity - 1.0).max(0.0);
        assert!(
            after < before * 0.25,
            "overload only fell from {before:.2} to {after:.2} of a frame"
        );
        assert!(
            settled <= GovernorConfig::default().max_steps * STEP,
            "settled past the visual ceiling"
        );
    }

    #[test]
    fn a_brief_spike_costs_almost_nothing() {
        // Two bad frames in an otherwise idle run must not be visible, and must
        // not persist.
        let mut g = gov();
        feed(&mut g, 0.0, 120);
        g.observe(BUDGET.mul_f32(4.0), BUDGET);
        g.observe(BUDGET.mul_f32(4.0), BUDGET);
        let mut worst = g.quality().redraw_tolerance;
        for _ in 0..900 {
            g.observe(Duration::ZERO, BUDGET);
            worst = worst.max(g.quality().redraw_tolerance);
        }
        // Bounded by the pen tolerance the renderer already ships as a
        // difference nobody can see, so "costs almost nothing" is measured
        // against the project's own standing claim rather than a number picked
        // to make the test pass.
        assert!(
            worst <= crate::render::DEFAULT_COLOR_TOLERANCE,
            "a two-frame hiccup cost {worst} RGB units, past the {} the renderer \
             already treats as invisible",
            crate::render::DEFAULT_COLOR_TOLERANCE
        );
        assert_eq!(g.quality().redraw_tolerance, 0, "and it never came back");
    }

    /// The measured idle distribution at 204x175: mostly ~0.04 of a frame, but
    /// with a long tail out to 3 whole frames. Deterministic, so this is a
    /// fixture and not a flaky test.
    fn healthy_but_bursty(i: usize) -> f32 {
        match i % 100 {
            0 => 3.06,      // the once-in-a-hundred 100 ms stall
            1..=3 => 0.35,  // p99
            4..=12 => 0.10, // p90
            13..=45 => 0.06,
            _ => 0.04, // p50
        }
    }

    #[test]
    fn a_healthy_but_bursty_terminal_costs_no_quality() {
        // A pty is bursty even when nothing is wrong. An earlier draft summarised
        // this with a mean and an EWMA, and a single 100 ms outlier was enough to
        // drag the estimate over the threshold for several frames running; the
        // tolerance then ratcheted up and — because the recover threshold had
        // been set below the idle floor — never came back down. Both halves of
        // that bug are what this pins.
        let mut g = gov();
        let mut worst = 0u16;
        for i in 0..3000 {
            let tol = g
                .observe(BUDGET.mul_f32(healthy_but_bursty(i)), BUDGET)
                .redraw_tolerance;
            worst = worst.max(tol);
        }
        assert_eq!(worst, 0, "a healthy terminal cost {worst} RGB units");
    }

    #[test]
    fn it_recovers_through_the_idle_noise_floor() {
        // Recovery has to survive contact with the real idle distribution, not
        // just a clean zero.
        let mut g = gov();
        feed(&mut g, 3.0, 600);
        assert!(g.quality().redraw_tolerance > 0);
        for i in 0..3000 {
            g.observe(BUDGET.mul_f32(healthy_but_bursty(i)), BUDGET);
        }
        assert_eq!(
            g.quality().redraw_tolerance,
            0,
            "never recovered against a realistic idle terminal"
        );
    }

    #[test]
    fn the_ceiling_scales_with_the_ramp() {
        // A coarser ramp (fewer --levels) has bigger steps, so the same budget
        // in steps has to allow more raw drift to mean the same thing.
        let mut fine = gov();
        feed(&mut fine, 3.0, 600);
        let mut coarse = Governor::new(STEP * 3.0, GovernorConfig::default());
        feed(&mut coarse, 3.0, 600);
        assert!(coarse.quality().redraw_tolerance > fine.quality().redraw_tolerance);
    }

    #[test]
    fn the_tolerance_only_ever_moves_gradually() {
        // No single frame may jump the quality by a visible amount, in either
        // direction — that is what a viewer would see as a flicker.
        let mut g = gov();
        let mut prev = 0u16;
        for i in 0..2000 {
            let load = if (i / 97) % 2 == 0 { 3.0 } else { 0.0 };
            let tol = g.observe(BUDGET.mul_f32(load), BUDGET).redraw_tolerance;
            assert!(
                tol.abs_diff(prev) <= 2,
                "tolerance jumped {prev} -> {tol} in one frame"
            );
            prev = tol;
        }
    }

    #[test]
    fn hostile_inputs_do_not_move_it() {
        let mut g = Governor::new(f32::NAN, GovernorConfig::default());
        for _ in 0..600 {
            g.observe(Duration::ZERO, Duration::ZERO);
        }
        assert_eq!(g.quality().redraw_tolerance, 0);
        // A zero budget must not read as infinite load.
        let mut g = gov();
        for _ in 0..600 {
            g.observe(Duration::from_secs(1), Duration::ZERO);
        }
        assert_eq!(
            g.quality().redraw_tolerance,
            0,
            "a zero budget drove the controller"
        );
    }

    #[test]
    fn backpressure_times_the_writer_and_resets() {
        struct Slow;
        impl Write for Slow {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                std::thread::sleep(Duration::from_millis(12));
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut w = Backpressure::new(Slow);
        assert_eq!(w.elapsed(), Duration::ZERO);
        w.write_all(b"hello").expect("Slow cannot fail");
        assert!(
            w.elapsed() >= Duration::from_millis(10),
            "{:?}",
            w.elapsed()
        );
        w.reset();
        assert_eq!(w.elapsed(), Duration::ZERO);
    }
}
