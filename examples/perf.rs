//! Performance harness: `cargo run --release --example perf`
//!
//! Separates our two costs — simulating the rain, and the bytes we hand the
//! terminal. The second is the one that matters: a terminal emulator has to
//! parse and re-render every escape sequence we emit, so output volume shows up
//! as *its* CPU, not ours.

use rmatrix::{Config, Depth, Rain, Renderer, Theme};
use std::time::Instant;

const FRAMES: usize = 600;
const DT: f32 = 1.0 / 60.0;

fn main() {
    println!(
        "{:>10} {:>8} {:>9} {:>9} {:>10} {:>9} {:>8}",
        "size", "cells", "sim/f", "draw/f", "bytes/f", "B/s@60", "dmg%"
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
        for _ in 0..120 {
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
            "{:>10} {:>8} {:>8.0?} {:>8.0?} {:>10.0} {:>8.1}M {:>7.0}%",
            format!("{w}x{h}"),
            cells,
            sim / FRAMES as u32,
            draw / FRAMES as u32,
            bpf,
            bpf * 60.0 / 1e6,
            dmg.min(100.0),
        );
    }

    println!();
    levels_sweep();
    println!();
    breakdown();
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
        for _ in 0..120 {
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
            bpf * 30.0 / 1e6,
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
    for _ in 0..120 {
        rain.step(DT);
    }
    let mut sink = Vec::with_capacity(1 << 22);
    let _ = rr.draw(&mut sink, &rain, &theme, Depth::True);

    let (mut sgr, mut moves, mut glyphs, mut sgr_n, mut move_n) = (0usize, 0, 0, 0, 0);
    for _ in 0..120 {
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
