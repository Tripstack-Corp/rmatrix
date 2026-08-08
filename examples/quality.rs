//! Visual cost of each quality setting: `cargo run --release --example quality`
//!
//! How far what is on screen sits from the ideal unquantised ramp, and how many
//! distinct brightness levels the trail still shows. Compared against `--levels 8`, which the
//! README already recommends for a full-screen vertical window and which is
//! therefore a look the project has already accepted.

use rmatrix::{Config, Depth, Rain, Renderer, Rgb, Theme};
use std::collections::BTreeSet;

const DT: f32 = 1.0 / 30.0;
const FRAMES: usize = 600;

fn manhattan(a: Rgb, b: Rgb) -> u32 {
    u32::from(a.0.abs_diff(b.0)) + u32::from(a.1.abs_diff(b.1)) + u32::from(a.2.abs_diff(b.2))
}

fn score(w: u16, h: u16, levels: u16, redraw: u16) -> (f64, u32, usize, f64) {
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
    let mut ideal = Theme::from_base((0, 255, 65), false);
    ideal.levels = 0;

    let mut rr = Renderer::new(w, h);
    rr.set_redraw_tolerance(redraw);
    let mut committed: Vec<Option<(char, Rgb)>> = vec![None; w as usize * h as usize];
    let mut sink = Vec::with_capacity(1 << 23);
    for _ in 0..((f32::from(h) / 6.0 / DT * 1.3) as usize) {
        rain.step(DT);
    }
    let _ = rr.draw(&mut sink, &rain, &theme, Depth::True);

    let mut hist = [0u64; 769];
    let mut greens: BTreeSet<u8> = BTreeSet::new();
    let mut bytes = 0usize;
    for _ in 0..FRAMES {
        rain.step(DT);
        sink.clear();
        if let Ok(s) = rr.draw(&mut sink, &rain, &theme, Depth::True) {
            bytes += s.bytes;
        }
        for y in 0..h {
            for x in 0..w {
                let want = rain.color_of(x, y, &theme);
                let slot = &mut committed[y as usize * w as usize + x as usize];
                let keep = match (*slot, want) {
                    (None, None) => true,
                    (Some((a, ac)), Some((b, bc))) => {
                        a == b && manhattan(ac, bc) <= u32::from(redraw)
                    }
                    _ => false,
                };
                if !keep {
                    *slot = want;
                }
                if let (Some((_, on)), Some((_, id))) = (*slot, rain.color_of(x, y, &ideal)) {
                    hist[manhattan(on, id).min(768) as usize] += 1;
                    // The head is near-white; score the trail ramp only.
                    if on.1 <= 235 {
                        greens.insert(on.1);
                    }
                }
            }
        }
    }
    let n: u64 = hist.iter().sum();
    let mean = hist
        .iter()
        .enumerate()
        .map(|(e, c)| e as f64 * *c as f64)
        .sum::<f64>()
        / n.max(1) as f64;
    let mut acc = 0u64;
    let mut p99 = 0u32;
    for (e, c) in hist.iter().enumerate() {
        acc += c;
        if acc as f64 >= n as f64 * 0.99 {
            p99 = e as u32;
            break;
        }
    }
    (mean, p99, greens.len(), bytes as f64 / FRAMES as f64)
}

fn main() {
    let (w, h) = (204u16, 175u16);
    println!("204x175 @30fps, 600 frames. Error is Manhattan RGB against the unquantised ramp.");
    println!(
        "{:>30} {:>9} {:>8} {:>8} {:>10} {:>8}",
        "config", "mean err", "p99 err", "levels", "bytes/f", "vs base"
    );
    let mut base = 0.0;
    for (label, lv, tol) in [
        ("default: levels 24", 24u16, 0u16),
        ("cmatrix, for scale", 3, 0),
        ("README full-screen: levels 8", 8, 0),
        ("levels 4", 4, 0),
        ("governor rung 1 (~1 step)", 24, 11),
        ("governor rung 2 (~2 steps)", 24, 22),
        ("governor rung 3 (~3 steps)", 24, 33),
        ("governor ceiling (4 steps)", 24, 44),
        ("ceiling on top of levels 8", 8, 44),
    ] {
        let (mean, p99, levels, bpf) = score(w, h, lv, tol);
        if base == 0.0 {
            base = bpf;
        }
        println!(
            "{label:>30} {mean:>9.1} {p99:>8} {levels:>8} {bpf:>10.0} {:>7.2}x",
            base / bpf
        );
    }
}
