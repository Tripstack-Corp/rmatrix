//! rmatrix — digital rain for modern terminals.
//!
//! The binary is deliberately thin: all of the behaviour lives in the library
//! so tests can drive it without a tty. What is left here is the event loop,
//! split across five modules so that no one file has to be read end to end:
//!
//! - [`cli`] — the flags, and `validate`, the only place input is rejected.
//! - [`keys`] — telling a held key apart from a pressed one.
//! - [`term`] — raw mode, the alternate screen, teardown, and where frames go.
//! - [`meter`] — the stats readout behind the `f` key.
//! - [`bench`] — timing instrumentation behind the hidden `--bench` flag.
//!
//! This file owns the loop itself, and nothing else.

mod bench;
mod cli;
mod keys;
mod meter;
mod term;

use anyhow::{Context, Result, bail};
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal;
use rmatrix::{Levels, Rain, Renderer, Theme};
use std::process::ExitCode;
use std::time::Instant;

use bench::{BENCH_PRIME_TICKS, Bench};
use cli::{Args, CYCLE, Settings, validate};
use keys::Repeat;
use meter::{Meter, Readout, draw_overlay, set_title};
use term::{Sink, restore, restore_sequence, setup};

fn main() -> ExitCode {
    let args = Args::parse();
    let settings = match validate(&args) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("rmatrix: {e:#}");
            return ExitCode::from(2);
        }
    };
    match run(&args, settings) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // The loop may have died mid-frame with the terminal still raw.
            restore();
            eprintln!("rmatrix: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args, s: Settings) -> Result<()> {
    // Without this, a panic leaves the terminal raw and on the alt screen.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        default_hook(info);
    }));

    setup(args.bold)?;

    let (tw, th) = terminal::size().context("querying terminal size")?;
    let (mut w, mut h) = (tw.max(1), th.max(1));

    let mut charset_idx = CYCLE.iter().position(|c| *c == args.charset).unwrap_or(0);
    let mut theme = Theme::from_base(s.base, s.rainbow);
    theme.levels = args.levels.resolve(w, h);
    let mut bold = args.bold;
    let mut rain = Rain::new(w, h, s.config);
    let mut renderer = Renderer::new(w, h);

    let mut sink = Sink::new(args.sync_io, 1 << 18);
    // Control sequences that are not part of a frame — a clear on resize, a bold
    // toggle — parked until a frame buffer is free so they stay in order with
    // the drawing they belong to.
    let mut oob: Vec<u8> = Vec::new();
    let mut paused = false;
    let mut show_stats = args.stats;
    let mut meter = Meter::default();
    let mut bench = Bench::default();
    // Terminal auto-repeat is byte-identical to a real press; see keys.rs.
    let mut repeat = Repeat::default();

    if args.bench.is_some() {
        // Reach steady state without drawing: a half-full screen emits about
        // half the bytes and would flatter both I/O paths equally but
        // meaninglessly.
        let step = s.frame.as_secs_f32();
        for _ in 0..(args.bench_warmup.max(0.0) / step) as u32 {
            rain.step(step);
        }
    }

    let mut tick_at = Instant::now();
    let mut last_frame = Instant::now();
    // Absolute cadence: the next tick is scheduled from the last *deadline*, not
    // from when this one happened to finish, so render time does not accumulate
    // into a drift.
    let mut next = tick_at + s.frame;
    let mut ticks = 0u32;
    let (mut sent, mut skipped) = (0u64, 0u64);

    'outer: loop {
        // Drain input until the tick deadline.
        while let Some(remaining) = next.checked_duration_since(Instant::now()) {
            if !event::poll(remaining)? {
                break;
            }
            match event::read()? {
                Event::Key(KeyEvent {
                    code,
                    modifiers,
                    kind,
                    ..
                }) if kind != KeyEventKind::Release => {
                    if modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(code, KeyCode::Char('c'))
                    {
                        break 'outer;
                    }
                    if args.screensaver {
                        break 'outer;
                    }
                    // Quitting is checked before the auto-repeat filter and is
                    // idempotent, so a repeat can never swallow it.
                    if matches!(code, KeyCode::Char('q') | KeyCode::Esc) {
                        break 'outer;
                    }
                    // Everything past here mutates state, and most of it
                    // toggles. See keys.rs: a held key otherwise leaves the
                    // toggle set to the parity of the repeat count.
                    if kind == KeyEventKind::Repeat || !repeat.accept(code, Instant::now()) {
                        continue;
                    }
                    match code {
                        KeyCode::Char(' ') => paused = !paused,
                        KeyCode::Char(d @ '1'..='9') => {
                            rain.speed_mul = f32::from(d as u8 - b'0') / 3.0;
                        }
                        KeyCode::Char('r') => {
                            theme.rainbow = !theme.rainbow;
                            renderer.resize(w, h); // force a full repaint
                        }
                        KeyCode::Char('c') => {
                            charset_idx = (charset_idx + 1) % CYCLE.len();
                            rain.set_glyphs(CYCLE[charset_idx].glyphs(&args.custom));
                        }
                        KeyCode::Char('b') => {
                            bold = !bold;
                            oob.extend_from_slice(if bold { b"\x1b[1m" } else { b"\x1b[22m" });
                            renderer.resize(w, h);
                        }
                        KeyCode::Char('f') => {
                            show_stats = !show_stats;
                            if !show_stats {
                                set_title(&mut oob, "rmatrix")?;
                            }
                            // Repaint so the row the overlay occupied comes back.
                            renderer.resize(w, h);
                        }
                        _ => {}
                    }
                }
                Event::Resize(nw, nh) => {
                    w = nw.max(1);
                    h = nh.max(1);
                    // Re-size the quality to the new window. Going full screen
                    // on a large display can multiply the cell count tenfold,
                    // and a setting that was right before will not be after.
                    theme.levels = args.levels.resolve(w, h);
                    rain.resize(w, h);
                    renderer.resize(w, h);
                    oob.extend_from_slice(b"\x1b[2J");
                }
                _ => {}
            }
        }

        let now = Instant::now();
        next += s.frame;
        if next <= now {
            // A whole period behind. Resync instead of firing a burst of
            // catch-up ticks, which is its own kind of stutter.
            next = now + s.frame;
        }
        // Clamp so a stall (laptop sleep, SIGSTOP) doesn't teleport every drop.
        let dt = (now - tick_at).as_secs_f32().min(s.max_step);
        if args.bench.is_some() && ticks >= BENCH_PRIME_TICKS {
            bench.tick(now - tick_at, dt);
        }
        tick_at = now;

        if !paused {
            rain.step(dt);
        }

        // Draw only when the terminal has finished with the last frame. When it
        // has not, this tick's damage is not lost — the renderer diffs against
        // what it last *emitted*, so the next frame carries it too.
        if let Some(mut buf) = sink.acquire() {
            buf.append(&mut oob);
            let stats = renderer.draw(&mut buf, &rain, &theme, s.depth)?;
            let refreshed = meter.record(now - last_frame, stats, w as usize * h as usize);
            if args.bench.is_some() && ticks >= BENCH_PRIME_TICKS {
                bench.frame(now - last_frame);
            }
            last_frame = now;

            if show_stats {
                let readout = Readout {
                    levels: theme.levels,
                    auto: args.levels == Levels::Auto,
                    speed: rain.speed(),
                };
                draw_overlay(&mut buf, w, &meter, readout)?;
                // Only on refresh: retitling every frame is pointless churn, and
                // some terminals flash the title bar when it changes.
                if refreshed {
                    set_title(&mut buf, &meter.title(readout))?;
                }
                // The overlay wrote colour and moved the cursor behind the
                // renderer's back; without this the next frame paints wrong.
                renderer.forget_cursor_and_color();
            }
            sink.submit(buf)?;
            sent += u64::from(ticks >= BENCH_PRIME_TICKS);
        } else {
            meter.skip();
            skipped += u64::from(ticks >= BENCH_PRIME_TICKS);
        }

        if sink.failed() {
            bail!("the terminal writer stopped");
        }
        ticks = ticks.saturating_add(1);
        if args
            .bench
            .is_some_and(|n| ticks >= n.saturating_add(BENCH_PRIME_TICKS))
        {
            break 'outer;
        }
    }

    if sink.shutdown(restore_sequence()) {
        // The sink emitted the escape sequence itself; only the termios flip is
        // left, and that is ours.
        let _ = terminal::disable_raw_mode();
    } else {
        restore();
    }
    if args.bench.is_some() {
        bench.report(if args.sync_io { "sync" } else { "async" }, sent, skipped);
    }
    Ok(())
}
