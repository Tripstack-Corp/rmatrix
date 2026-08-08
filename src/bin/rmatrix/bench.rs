//! Timing instrumentation behind the hidden `--bench` flag.
//!
//! Exists to compare the two I/O paths under an identical run. Smoothness is a
//! distribution, not an average, so everything here reports spread rather than
//! a single number.

use std::time::Duration;

/// Ticks excluded from `--bench` samples. The first draw after the alt-screen
/// clear repaints every lit cell — an order of magnitude more bytes than a
/// steady-state frame — and that one outlier is not what anybody is looking at.
pub(crate) const BENCH_PRIME_TICKS: u32 = 15;

/// Timing recorder for `--bench`.
#[derive(Default)]
pub(crate) struct Bench {
    /// Wall interval between loop ticks. `dt` is derived from it, so its spread
    /// is the spread of how far the rain moves per simulation step.
    tick_ms: Vec<f64>,
    /// Wall interval between frames that actually reached the terminal.
    frame_ms: Vec<f64>,
    /// Simulated time carried by each of those frames.
    step_ms: Vec<f64>,
    /// Per displayed frame, the share of elapsed wall time the rain failed to
    /// cover, as a percentage. Zero means the drops moved exactly as far as the
    /// clock said they should. A large value is a freeze-then-lurch: the screen
    /// held still for 300ms and then advanced 100ms worth of rain.
    deficit_pct: Vec<f64>,
    pending: f64,
    /// The first displayed frame straddles the boundary into the measured
    /// window: its wall interval reaches back before measurement started but its
    /// accumulated `pending` does not, so the pair is not comparable. Dropped.
    seen_frame: bool,
    sim_ms: f64,
    wall_ms: f64,
}

impl Bench {
    pub(crate) fn tick(&mut self, interval: Duration, dt: f32) {
        let ms = interval.as_secs_f64() * 1000.0;
        self.tick_ms.push(ms);
        self.wall_ms += ms;
        self.sim_ms += f64::from(dt) * 1000.0;
        self.pending += f64::from(dt) * 1000.0;
    }

    pub(crate) fn frame(&mut self, interval: Duration) {
        let ms = interval.as_secs_f64() * 1000.0;
        if self.seen_frame {
            self.frame_ms.push(ms);
            self.step_ms.push(self.pending);
            self.deficit_pct
                .push(((1.0 - self.pending / ms.max(f64::EPSILON)) * 100.0).max(0.0));
        }
        self.seen_frame = true;
        self.pending = 0.0;
    }

    pub(crate) fn report(&self, mode: &str, submitted: u64, coalesced: u64) {
        eprintln!(
            "BENCH mode={mode} ticks={} frames={submitted} coalesced={coalesced}",
            self.tick_ms.len()
        );
        summarise("tick_ms ", &self.tick_ms);
        summarise("frame_ms", &self.frame_ms);
        summarise("step_ms ", &self.step_ms);
        summarise("deficit%", &self.deficit_pct);
        // 1.000 means the rain kept up with the wall clock. Below that it ran in
        // slow motion, which is what a `dt` clamp buys you if it fires often.
        eprintln!(
            "BENCH pacing sim/wall={:.4}",
            self.sim_ms / self.wall_ms.max(f64::EPSILON)
        );
    }
}

/// One line of distribution for a series of frame times.
///
/// Smoothness is a distribution, not an average: `jit` is the mean absolute
/// difference between consecutive samples, which is what an eye actually
/// notices — a steady 80ms looks far better than 33ms alternating with 130ms.
fn summarise(label: &str, v: &[f64]) {
    if v.is_empty() {
        eprintln!("BENCH {label} n=0");
        return;
    }
    let jit = v.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f64>() / (v.len().max(2) - 1) as f64;
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    let sd = (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let q = |p: f64| s[((s.len() - 1) as f64 * p).round() as usize];
    eprintln!(
        "BENCH {label} n={:<5} p50={:>8.2} p95={:>8.2} p99={:>8.2} max={:>9.2} mean={:>8.2} sd={:>8.2} jit={:>8.2}",
        s.len(),
        q(0.50),
        q(0.95),
        q(0.99),
        q(1.0),
        mean,
        sd,
        jit
    );
}
