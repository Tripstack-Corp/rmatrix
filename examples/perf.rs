//! Performance harness: `cargo run --release --example perf`
//!
//! Separates our two costs — simulating the rain, and the bytes we hand the
//! terminal. The second is the one that matters: a terminal emulator has to
//! parse and re-render every escape sequence we emit, so output volume shows up
//! as *its* CPU, not ours.

use rmatrix::{Config, DEFAULT_COLOR_TOLERANCE, Depth, Rain, Renderer, Theme};
use std::time::Instant;

/// The shipped default frame rate. Everything here steps at this rate and
/// reports totals at this rate.
///
/// These MUST agree. An earlier version stepped at 1/60 s while labelling its
/// totals "at 30 fps", which undercounts by roughly 2x: a 30 fps frame carries
/// twice the motion of a 60 fps one, so it damages about twice as many cells.
const FPS: f64 = 30.0;
const FRAMES: usize = 600;
const DT: f32 = 1.0 / FPS as f32;

/// Frames needed before the screen is actually full.
///
/// This matters more than it looks: the slowest drops fall at 6 rows/sec, so a
/// 175-row window takes ~29 seconds of simulated time to reach steady state.
/// Warming up for a fixed 2 seconds measures a half-empty screen and flatters
/// every number by roughly 2x.
fn warmup_frames(h: u16) -> usize {
    // rows / slowest-speed, in frames, with headroom.
    ((f32::from(h) / 6.0 / DT) * 1.3) as usize
}

fn main() {
    println!(
        "{:>10} {:>8} {:>9} {:>9} {:>10} {:>9} {:>8}",
        "size", "cells", "sim/f", "draw/f", "bytes/f", "MB/s@30", "dmg%"
    );
    println!("{}", "-".repeat(70));

    for (w, h) in [(80u16, 24u16), (120, 40), (200, 50), (280, 70), (400, 100)] {
        let mut rain = Rain::new(
            w,
            h,
            Config {
                seed: Some(1),
                ..Config::default()
            },
        );
        let mut rr = Renderer::new(w, h);
        let theme = Theme::from_base((0, 255, 65), false);

        // Warm up so we measure steady state, not an empty screen.
        for _ in 0..warmup_frames(h) {
            rain.step(DT);
        }
        let mut sink = Vec::with_capacity(1 << 22);
        let _ = rr.draw(&mut sink, &rain, &theme, Depth::True);

        let mut sim = std::time::Duration::ZERO;
        let mut draw = std::time::Duration::ZERO;
        let mut bytes = 0usize;

        for _ in 0..FRAMES {
            let t0 = Instant::now();
            rain.step(DT);
            sim += t0.elapsed();

            sink.clear();
            let t1 = Instant::now();
            let _ = rr.draw(&mut sink, &rain, &theme, Depth::True);
            draw += t1.elapsed();
            bytes += sink.len();
        }

        let cells = w as usize * h as usize;
        let bpf = bytes as f64 / FRAMES as f64;
        // Rough damage estimate: an SGR + glyph is ~20 bytes.
        let dmg = (bpf / 20.0) / cells as f64 * 100.0;
        println!(
            "{:>10} {:>8} {:>8.0?} {:>8.0?} {:>10.0} {:>9.2}M {:>7.0}%",
            format!("{w}x{h}"),
            cells,
            sim / FRAMES as u32,
            draw / FRAMES as u32,
            bpf,
            bpf * FPS / 1e6,
            dmg.min(100.0),
        );
    }

    println!();
    levels_sweep();
    println!();
    breakdown();
    println!();
    ab();
    println!();
    fps_sweep();
}

/// `levels = 0` is the unquantised ramp, i.e. the original behaviour.
fn levels_sweep() {
    let (w, h) = (200u16, 50u16);
    println!("brightness levels vs output, at {w}x{h}:");
    println!(
        "{:>8} {:>11} {:>10} {:>9} {:>10}",
        "levels", "bytes/f", "MB/s@30", "dmg%", "vs none"
    );

    let mut baseline = 0.0f64;
    for levels in [0u16, 64, 32, 24, 16, 8] {
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
        let mut rr = Renderer::new(w, h);
        for _ in 0..warmup_frames(h) {
            rain.step(DT);
        }
        let mut sink = Vec::with_capacity(1 << 22);
        let _ = rr.draw(&mut sink, &rain, &theme, Depth::True);

        let (mut bytes, mut dmg) = (0usize, 0usize);
        for _ in 0..FRAMES {
            rain.step(DT);
            sink.clear();
            if let Ok(s) = rr.draw(&mut sink, &rain, &theme, Depth::True) {
                bytes += s.bytes;
                dmg += s.cells_damaged;
            }
        }
        let bpf = bytes as f64 / FRAMES as f64;
        if levels == 0 {
            baseline = bpf;
        }
        println!(
            "{:>8} {:>11.0} {:>9.2}M {:>8.1}% {:>9.2}x",
            if levels == 0 {
                "none".to_string()
            } else {
                levels.to_string()
            },
            bpf,
            bpf * FPS / 1e6,
            dmg as f64 / FRAMES as f64 / (w as usize * h as usize) as f64 * 100.0,
            baseline / bpf,
        );
    }
}

/// What are those bytes actually made of?
fn breakdown() {
    let (w, h) = (200u16, 50u16);
    let mut rain = Rain::new(
        w,
        h,
        Config {
            seed: Some(1),
            ..Config::default()
        },
    );
    let mut rr = Renderer::new(w, h);
    let theme = Theme::from_base((0, 255, 65), false);
    for _ in 0..warmup_frames(h) {
        rain.step(DT);
    }
    let mut sink = Vec::with_capacity(1 << 22);
    let _ = rr.draw(&mut sink, &rain, &theme, Depth::True);

    let (mut sgr, mut moves, mut glyphs, mut sgr_n, mut move_n) = (0usize, 0, 0, 0, 0);
    for _ in 0..FRAMES {
        rain.step(DT);
        sink.clear();
        let _ = rr.draw(&mut sink, &rain, &theme, Depth::True);
        let mut i = 0;
        while i < sink.len() {
            if sink[i] == 0x1b {
                let end = sink[i..]
                    .iter()
                    .position(|b| b.is_ascii_alphabetic())
                    .unwrap_or(1);
                let seq = &sink[i..i + end + 1];
                if seq.ends_with(b"m") {
                    sgr += seq.len();
                    sgr_n += 1;
                } else {
                    moves += seq.len();
                    move_n += 1;
                }
                i += end + 1;
            } else {
                glyphs += 1;
                i += 1;
            }
        }
    }
    let total = (sgr + moves + glyphs) as f64;
    println!("byte breakdown at 200x50 (120 frames):");
    println!(
        "  colour SGR : {:>9} bytes ({:>4.1}%)  {:>7} sequences, {:.1} B each",
        sgr,
        sgr as f64 / total * 100.0,
        sgr_n,
        sgr as f64 / sgr_n.max(1) as f64
    );
    println!(
        "  cursor move: {:>9} bytes ({:>4.1}%)  {:>7} sequences, {:.1} B each",
        moves,
        moves as f64 / total * 100.0,
        move_n,
        moves as f64 / move_n.max(1) as f64
    );
    println!(
        "  glyphs     : {:>9} bytes ({:>4.1}%)",
        glyphs,
        glyphs as f64 / total * 100.0
    );
}

/// Isolates two things I guessed at and had to measure: whether reusing the pen
/// for imperceptible colour deltas pays, and what glyph churn costs.
fn ab() {
    let (w, h) = (204u16, 175u16); // a full-screen vertical window
    println!("at {w}x{h} (full-screen vertical), 600 frames:");
    println!("{:>34} {:>11} {:>10}", "variant", "bytes/f", "MB/s@30");
    for (label, tol, mutate, levels, density, tail) in [
        (
            "profile as shipped",
            DEFAULT_COLOR_TOLERANCE,
            0.35,
            24u16,
            0.75,
            40.0,
        ),
        ("  + no pen tolerance", 0, 0.35, 24, 0.75, 40.0),
        (
            "  + no glyph churn (-m 0)",
            DEFAULT_COLOR_TOLERANCE,
            0.0,
            24,
            0.75,
            40.0,
        ),
        (
            "same look, levels 12",
            DEFAULT_COLOR_TOLERANCE,
            0.35,
            12,
            0.75,
            40.0,
        ),
        (
            "same look, levels 8",
            DEFAULT_COLOR_TOLERANCE,
            0.35,
            8,
            0.75,
            40.0,
        ),
        (
            "default density/tail",
            DEFAULT_COLOR_TOLERANCE,
            0.35,
            24,
            0.55,
            26.0,
        ),
        (
            "  + levels 12",
            DEFAULT_COLOR_TOLERANCE,
            0.35,
            12,
            0.55,
            26.0,
        ),
        (
            "  + levels 12, -m 0.1",
            DEFAULT_COLOR_TOLERANCE,
            0.1,
            12,
            0.55,
            26.0,
        ),
    ] {
        let mut rain = Rain::new(
            w,
            h,
            Config {
                seed: Some(1),
                mutate,
                density,
                tail_max: tail,
                ..Config::default()
            },
        );
        let mut theme = Theme::from_base((0, 255, 65), false);
        theme.levels = levels;
        let mut rr = Renderer::new(w, h);
        rr.set_color_tolerance(tol);
        for _ in 0..warmup_frames(h) {
            rain.step(DT);
        }
        let mut sink = Vec::with_capacity(1 << 23);
        let _ = rr.draw(&mut sink, &rain, &theme, Depth::True);
        let mut bytes = 0usize;
        for _ in 0..FRAMES {
            rain.step(DT);
            sink.clear();
            if let Ok(s) = rr.draw(&mut sink, &rain, &theme, Depth::True) {
                bytes += s.bytes;
            }
        }
        let bpf = bytes as f64 / FRAMES as f64;
        println!("{:>34} {:>11.0} {:>9.2}M", label, bpf, bpf * FPS / 1e6);
    }
}

/// Output vs frame rate, measured honestly.
///
/// The rest of this harness historically stepped the simulation at 1/60 s while
/// labelling its totals "at 30 fps". That undercounts: a 30 fps frame carries
/// twice the motion of a 60 fps one, so it damages more cells. Here each row
/// steps at its own frame period, which is the only way the MB/s column means
/// anything.
fn fps_sweep() {
    let (w, h) = (204u16, 175u16);
    println!("output vs frame rate at {w}x{h}, stepping at each rate's own dt:");
    println!(
        "{:>6} {:>11} {:>10} {:>10} {:>12}",
        "fps", "bytes/f", "MB/s", "vs 30fps", "if linear"
    );

    let mut at30 = 0.0f64;
    for fps in [30u16, 60, 24, 15, 10] {
        let dt = 1.0 / f32::from(fps);
        let mut rain = Rain::new(
            w,
            h,
            Config {
                seed: Some(1),
                density: 0.75,
                tail_max: 40.0,
                ..Config::default()
            },
        );
        let mut theme = Theme::from_base((0, 255, 65), false);
        theme.levels = 8;
        let mut rr = Renderer::new(w, h);
        // Warm up in simulated time, not frames, so every rate fills equally.
        let warm = (f32::from(h) / 6.0 * 1.3 / dt) as usize;
        for _ in 0..warm {
            rain.step(dt);
        }
        let mut sink = Vec::with_capacity(1 << 23);
        let _ = rr.draw(&mut sink, &rain, &theme, Depth::True);

        let frames = 300;
        let mut bytes = 0usize;
        for _ in 0..frames {
            rain.step(dt);
            sink.clear();
            if let Ok(s) = rr.draw(&mut sink, &rain, &theme, Depth::True) {
                bytes += s.bytes;
            }
        }
        let bpf = bytes as f64 / frames as f64;
        let mbs = bpf * f64::from(fps) / 1e6;
        if fps == 30 {
            at30 = mbs;
        }
        let linear = at30 * f64::from(fps) / 30.0;
        println!(
            "{:>6} {:>11.0} {:>9.2}M {:>9.2}x {:>11.2}M",
            fps,
            bpf,
            mbs,
            mbs / at30,
            linear
        );
    }
}
