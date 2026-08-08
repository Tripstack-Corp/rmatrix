//! Frame-time harness: `examples/perf.rs` drains into a `Vec` and so can never
//! reproduce the thing that actually janks. This one writes to whatever stdout
//! is — point it at a pty whose reader drains slowly and you get the real
//! failure: the flush blocks, the frame overruns, `dt` grows, and the next frame
//! has more damage to emit than the last.
//!
//! It mirrors `main.rs`'s loop deliberately: same `BufWriter`, same blocking
//! flush inside `draw`, same `dt` clamp, same pacing. Per-frame timings go to a
//! CSV so the *distribution* can be looked at, not just the mean.
//!
//! Usage:
//!   jank --mode off|auto|<bytes> --secs N --fps F --cols W --rows H --csv PATH

use rmatrix::{Config, Depth, Governor, Rain, Renderer, Theme};
use std::io::Write;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Off,
    Auto,
    Fixed(usize),
}

struct Row {
    interval_us: u128,
    write_us: u128,
    bytes: usize,
    damaged: usize,
    drawn: usize,
    forced: usize,
    dt_us: u128,
    budget: usize,
}

fn arg(name: &str, default: &str) -> String {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

fn main() {
    let mode = match arg("--mode", "auto").as_str() {
        "off" => Mode::Off,
        "auto" => Mode::Auto,
        n => Mode::Fixed(n.parse().expect("--mode takes off, auto, or a byte count")),
    };
    let secs: f64 = arg("--secs", "20").parse().expect("--secs");
    let fps: u32 = arg("--fps", "30").parse().expect("--fps");
    let w: u16 = arg("--cols", "204").parse().expect("--cols");
    let h: u16 = arg("--rows", "175").parse().expect("--rows");
    let csv = arg("--csv", "");
    let period = Duration::from_secs_f64(1.0 / f64::from(fps));

    let mut rain = Rain::new(
        w,
        h,
        Config {
            seed: Some(1),
            ..Config::default()
        },
    );
    let theme = Theme::from_base((0, 255, 65), false);
    let mut rr = Renderer::new(w, h);
    let mut gov = Governor::new(w as usize * h as usize);
    let budget_of = |gov: &Governor| match mode {
        Mode::Off => None,
        Mode::Auto => Some(gov.budget()),
        Mode::Fixed(n) => Some(n),
    };
    rr.set_budget(budget_of(&gov));

    // The slowest drops fall at 6 rows/sec, so a 175-row window needs ~29
    // seconds of simulated time before the screen is full. Warm up in the model
    // only — none of this is drawn or timed.
    let warm = (f64::from(h) / 6.0 * f64::from(fps) * 1.3) as usize;
    for _ in 0..warm {
        rain.step(1.0 / fps as f32);
    }

    let mut out = std::io::BufWriter::with_capacity(1 << 18, std::io::stdout());
    let _ = out.write_all(b"\x1b[?25l\x1b[?7l\x1b[2J");
    let _ = out.flush();

    let frames = (secs * f64::from(fps)) as usize;
    let mut rows: Vec<Row> = Vec::with_capacity(frames);

    // One untimed frame to paint the initial screen, which is a full repaint and
    // would otherwise dominate every percentile.
    let _ = rr.draw(&mut out, &rain, &theme, Depth::True);

    let start = Instant::now();
    let mut last = Instant::now();
    let mut last_frame = Instant::now();
    for i in 0..frames {
        // Pace to the frame period, exactly as the real loop does by polling
        // for input with the remaining time.
        let target = start + period * (i as u32 + 1);
        if let Some(sleep) = target.checked_duration_since(Instant::now()) {
            std::thread::sleep(sleep);
        }

        let now = Instant::now();
        // Clamp so a stall doesn't teleport every drop — and note that this
        // clamp is precisely why a long stall is *visible* rather than merely
        // slow: the drops jump.
        let dt = (now - last).as_secs_f32().min(0.1);
        last = now;
        rain.step(dt);

        let t0 = Instant::now();
        let stats = match rr.draw(&mut out, &rain, &theme, Depth::True) {
            Ok(s) => s,
            Err(_) => break,
        };
        let write = t0.elapsed();
        if mode == Mode::Auto {
            gov.observe(stats.bytes, write, period);
            rr.set_budget(budget_of(&gov));
        }
        rows.push(Row {
            interval_us: (now - last_frame).as_micros(),
            write_us: write.as_micros(),
            bytes: stats.bytes,
            damaged: stats.cells_damaged,
            drawn: stats.cells_drawn,
            forced: stats.cells_forced,
            dt_us: (dt * 1.0e6) as u128,
            budget: budget_of(&gov).unwrap_or(0),
        });
        last_frame = now;
    }

    let _ = out.write_all(b"\x1b[?25h\x1b[?7h");
    let _ = out.flush();
    drop(out);

    if !csv.is_empty() {
        let mut f = std::fs::File::create(&csv).expect("cannot create --csv");
        writeln!(
            f,
            "interval_us,write_us,bytes,damaged,drawn,forced,dt_us,budget"
        )
        .expect("csv header");
        for r in &rows {
            writeln!(
                f,
                "{},{},{},{},{},{},{},{}",
                r.interval_us, r.write_us, r.bytes, r.damaged, r.drawn, r.forced, r.dt_us, r.budget
            )
            .expect("csv row");
        }
    }
}
