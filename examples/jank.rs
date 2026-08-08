//! Jank harness: `cargo run --release --example jank -- --help`
//!
//! Runs the real render loop, in real time, against real stdout — so it must be
//! pointed at a pty whose reader drains at a controlled rate if you want to see
//! what a terminal that cannot keep up does to us. Draining into a `Vec` (which
//! is what `examples/perf.rs` does) never blocks and so never shows the problem.
//!
//! Per-frame timings go to `--log`, not stdout, because stdout is the pty.

use rmatrix::{Backpressure, Config, Depth, Governor, GovernorConfig, Rain, Renderer, Theme};
use std::io::Write;
use std::time::{Duration, Instant};

fn arg(name: &str, default: &str) -> String {
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        if a == name {
            return it.next().unwrap_or_else(|| default.to_string());
        }
    }
    default.to_string()
}

fn flag(name: &str) -> bool {
    std::env::args().any(|a| a == name)
}

fn main() {
    let w: u16 = arg("--w", "204").parse().expect("--w");
    let h: u16 = arg("--h", "175").parse().expect("--h");
    let secs: f64 = arg("--secs", "20").parse().expect("--secs");
    let fps: u32 = arg("--fps", "30").parse().expect("--fps");
    let levels: u16 = arg("--levels", "24").parse().expect("--levels");
    let adaptive = flag("--adaptive");
    let paced = flag("--paced");
    let log = arg("--log", "/dev/null");

    let budget = Duration::from_secs_f64(1.0 / f64::from(fps));
    let dt_sim = 1.0 / fps as f32;

    let mut rain = Rain::new(
        w,
        h,
        Config {
            seed: Some(1),
            ..Config::default()
        },
    );
    let mut theme = Theme::from_base((0, 255, 65), false);
    theme.levels = levels;
    let mut renderer = Renderer::new(w, h);

    // Steady state, not an empty screen: the slowest drops fall at 6 rows/s, so
    // a 175-row window needs ~29 s of simulated time before it is full.
    for _ in 0..((f32::from(h) / 6.0 / dt_sim * 1.3) as usize) {
        rain.step(dt_sim);
    }

    let mut gov = Governor::new(theme.ramp_step(), GovernorConfig::default());

    // The timer sits *under* the BufWriter, so it only ever sees real syscalls.
    let mut out = std::io::BufWriter::with_capacity(1 << 18, Backpressure::new(std::io::stdout()));
    let _ = out.write_all(b"\x1b[2J\x1b[?25l");

    // Wall-clocked, not frame-counted: under a load profile both variants have
    // to see the same schedule, and a slower variant would otherwise run longer
    // and experience a different one.
    let run_for = Duration::from_secs_f64(secs);
    let mut rec: Vec<(u64, u64, u64, u32, u32, u16, u64)> =
        Vec::with_capacity((secs * f64::from(fps)) as usize + 64);

    let start = Instant::now();
    let mut last = start;
    let mut prev_start = start;
    let mut n = 0usize;
    while start.elapsed() < run_for {
        let frame_start = Instant::now();
        // Faithful to main.rs: the budget is counted from *after* the previous
        // draw, so a slow draw pushes the whole cadence out. `--paced` instead
        // holds an absolute cadence, which is the obvious rival fix.
        let until = if paced {
            // Absolute cadence, skipping any deadline already missed — chasing
            // them instead just bursts and blocks.
            let mut k = n as u32 + 1;
            let mut t = start + budget * k;
            while t <= frame_start {
                k += 1;
                t = start + budget * k;
            }
            t
        } else {
            frame_start + budget
        };
        if let Some(d) = until.checked_duration_since(Instant::now()) {
            std::thread::sleep(d);
        }

        let now = Instant::now();
        // How late the OS actually woke us. Under heavy oversubscription this,
        // not anything we do, is what puts frames in the tail.
        let overshoot = now.saturating_duration_since(until);
        let period = now.duration_since(prev_start);
        prev_start = now;
        let dt = (now - last).as_secs_f32().min(0.1);
        last = now;

        rain.step(dt);
        out.get_mut().reset();
        let t0 = Instant::now();
        let stats = renderer
            .draw(&mut out, &rain, &theme, Depth::True)
            .expect("draw");
        let draw = t0.elapsed();
        let blocked = out.get_mut().elapsed();

        if adaptive {
            let q = gov.observe(blocked, budget);
            renderer.set_redraw_tolerance(q.redraw_tolerance);
        }

        rec.push((
            period.as_micros() as u64,
            draw.as_micros() as u64,
            blocked.as_micros() as u64,
            stats.bytes as u32,
            stats.cells_damaged as u32,
            gov.quality().redraw_tolerance,
            overshoot.as_micros() as u64,
        ));
        n += 1;
    }
    let wall = start.elapsed();
    let _ = out.write_all(b"\x1b[?25h\x1b[0m");
    let _ = out.flush();

    let mut f = std::fs::File::create(&log).expect("log");
    writeln!(f, "# wall={:.3}s frames={}", wall.as_secs_f64(), rec.len()).expect("write");
    writeln!(f, "period_us,draw_us,blocked_us,bytes,damaged,tol,late_us").expect("write");
    for (p, d, b, by, dm, r, l) in rec {
        writeln!(f, "{p},{d},{b},{by},{dm},{r},{l}").expect("write");
    }
}
