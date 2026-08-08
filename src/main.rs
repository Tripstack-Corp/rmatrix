//! rmatrix — digital rain for modern terminals.
//!
//! This binary is a thin wrapper: it parses and validates arguments, owns the
//! terminal's raw/alt-screen state, and pumps the event loop. All of the
//! behaviour lives in the library so tests can drive it without a tty.

use anyhow::{Context, Result, bail};
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{
    Attribute, Color, Print, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{
    self, Clear, ClearType, DisableLineWrap, EnableLineWrap, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::{ExecutableCommand, QueueableCommand, cursor};
use rmatrix::charset::{is_wide, is_zero_width};
use rmatrix::writer::{self, FrameWriter};
use rmatrix::{BaseColor, Charset, Config, Depth, DrawStats, Levels, Rain, Renderer, Theme};
use std::io::{Stdout, Write, stdout};
use std::process::ExitCode;
use std::str::FromStr;
use std::time::{Duration, Instant};

#[derive(Parser, Debug, Clone)]
#[command(
    name = "rmatrix",
    version,
    about = "Digital rain for modern terminals",
    after_help = "KEYS:\n  q, Esc, Ctrl-C  quit\n  space           pause\n  1-9             speed\n  r               toggle rainbow\n  c               cycle charset\n  b               toggle bold\n  f               toggle stats overlay"
)]
struct Args {
    /// Colour name, #RRGGBB, or "rainbow"
    #[arg(short = 'C', long, default_value = "green")]
    color: String,

    /// Glyph set
    #[arg(short = 'c', long, value_enum, default_value_t = Charset::Classic)]
    charset: Charset,

    /// Glyphs to use with `--charset custom`
    ///
    /// Every glyph must occupy exactly one terminal column: no control
    /// characters, no double-width or fullwidth characters, and no combining
    /// or zero-width characters. The renderer positions the cursor by counting
    /// columns, so anything else smears the frame.
    #[arg(long, default_value = "")]
    custom: String,

    /// Overall speed multiplier
    #[arg(short = 'S', long, default_value_t = 1.0)]
    speed: f32,

    /// Frame rate cap. Output volume scales linearly with this, and the
    /// terminal — not rmatrix — is what pays for it.
    #[arg(long, default_value_t = 30)]
    fps: u16,

    /// Brightness steps in the trail, or "auto" to size them from the terminal.
    /// Lower means fewer escape sequences for the terminal to parse; 0 disables
    /// quantisation. "auto" re-picks on resize.
    #[arg(long, default_value = "auto")]
    levels: Levels,

    /// Start with the stats overlay visible (toggle with `f`)
    #[arg(long)]
    stats: bool,

    /// Fraction of columns raining at any moment (0.0-1.0)
    #[arg(short = 'd', long, default_value_t = 0.55)]
    density: f32,

    /// Shortest trail, in rows
    #[arg(long, default_value_t = 6.0)]
    tail_min: f32,

    /// Longest trail, in rows
    #[arg(long, default_value_t = 26.0)]
    tail_max: f32,

    /// Glyph churn rate (screens per second); 0 disables
    #[arg(short = 'm', long, default_value_t = 0.35)]
    mutate: f32,

    /// Bold glyphs
    #[arg(short = 'b', long)]
    bold: bool,

    /// Exit on any keypress
    #[arg(short = 's', long)]
    screensaver: bool,

    /// Replay a specific animation
    #[arg(long)]
    seed: Option<u64>,

    /// Force colour depth instead of detecting it
    #[arg(long, value_parser = ["auto", "truecolor", "256", "16"], default_value = "auto")]
    color_depth: String,

    /// Write frames from the simulation thread, blocking on the terminal.
    ///
    /// The pre-writer-thread behaviour, kept so the two I/O paths can be A/B'd
    /// under an identical run. Nobody should want this.
    #[arg(long, hide = true)]
    sync_io: bool,

    /// Run this many loop ticks with timing instrumentation, then exit,
    /// reporting frame-time percentiles on stderr.
    #[arg(long, hide = true, value_name = "TICKS")]
    bench: Option<u32>,

    /// Seconds of rain to simulate before `--bench` starts measuring.
    ///
    /// The slowest drops fall at 6 rows/s, so a 175-row window needs ~29s to
    /// reach steady state; measuring a half-empty screen understates the output
    /// volume by roughly 2x.
    #[arg(long, hide = true, default_value_t = 32.0)]
    bench_warmup: f32,

    /// Override the largest simulation step, in seconds. 0 derives it from
    /// `--fps`; see `max_step`. Exposed only so the clamp can be ablated.
    #[arg(long, hide = true, default_value_t = 0.0)]
    max_step: f32,
}

/// Charsets reachable with the `c` key, in order.
const CYCLE: [Charset; 6] = [
    Charset::Classic,
    Charset::Katakana,
    Charset::Ascii,
    Charset::Alnum,
    Charset::Binary,
    Charset::Greek,
];

/// Everything the loop needs, once the arguments are known-good.
#[derive(Debug)]
struct Settings {
    base: (u8, u8, u8),
    rainbow: bool,
    depth: Depth,
    config: Config,
    frame: Duration,
    /// Largest `dt` handed to the simulation in one tick, in seconds.
    max_step: f32,
}

/// The `dt` clamp, in frame periods.
///
/// Below it the rain moves in exact proportion to elapsed time; beyond it we
/// have been descheduled or suspended, and teleporting a drop is worse than
/// losing the time. Three periods is where a hitch stops being jitter.
const MAX_STEP_FRAMES: f32 = 3.0;

/// Floor for the clamp, so a high `--fps` does not make it hair-trigger. At the
/// default 30 fps the two agree exactly, which is deliberate: this is the value
/// the loop has always used.
const MAX_STEP_FLOOR: f32 = 0.1;

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

/// Pure argument validation, split out so it is testable without a terminal.
fn validate(args: &Args) -> Result<Settings> {
    let BaseColor(base, rainbow) = BaseColor::from_str(&args.color).context("invalid --color")?;

    if !(0.0..=1.0).contains(&args.density) {
        bail!(
            "--density must be between 0.0 and 1.0, got {}",
            args.density
        );
    }
    if !args.speed.is_finite() || args.speed <= 0.0 {
        bail!("--speed must be a positive number, got {}", args.speed);
    }
    if !args.tail_min.is_finite() || args.tail_min <= 0.0 {
        bail!(
            "--tail-min must be a positive number, got {}",
            args.tail_min
        );
    }
    if !args.tail_max.is_finite() || args.tail_max < args.tail_min {
        bail!(
            "--tail-max ({}) must be >= --tail-min ({})",
            args.tail_max,
            args.tail_min
        );
    }
    if !args.mutate.is_finite() || args.mutate < 0.0 {
        bail!("--mutate must be zero or positive, got {}", args.mutate);
    }
    if args.charset == Charset::Custom && args.custom.is_empty() {
        bail!("--charset custom needs --custom <GLYPHS>");
    }
    // `--custom` glyphs reach `Print` unfiltered, unlike the built-in sets,
    // which are tested for these two invariants. A control character would be
    // injected raw into the escape stream; a glyph that is not one column wide
    // breaks the renderer's cursor arithmetic (`self.at = (x + 1, y)`) and every
    // cell drawn after it in that frame lands in the wrong place.
    //
    // Checked whenever the flag is given rather than only under `--charset
    // custom`: the value is nonsense for this flag either way, and silently
    // ignoring it is how a typo survives to the one run where it matters.
    for ch in args.custom.chars() {
        if ch.is_control() {
            bail!(
                "--custom cannot contain control characters, found U+{:04X}",
                ch as u32
            );
        }
        if is_wide(ch) {
            bail!(
                "--custom needs single-column glyphs, but {ch:?} (U+{:04X}) is double-width",
                ch as u32
            );
        }
        if is_zero_width(ch) {
            bail!(
                "--custom needs single-column glyphs, but U+{:04X} is combining or zero-width",
                ch as u32
            );
        }
    }

    let depth = match args.color_depth.as_str() {
        "truecolor" => Depth::True,
        "256" => Depth::Ansi256,
        "16" => Depth::Ansi16,
        _ => Depth::detect(),
    };

    let frame = Duration::from_secs_f64(1.0 / f64::from(args.fps.clamp(1, 240)));
    // A fixed 0.1s clamp is a bug below ~10 fps: the frame period exceeds it, so
    // every step is truncated and the rain runs in slow motion — at `--fps 5`,
    // measurably at exactly half speed. Scaling with the frame period fixes that
    // and leaves the default untouched.
    let max_step = if args.max_step.is_finite() && args.max_step > 0.0 {
        args.max_step
    } else {
        default_max_step(frame)
    };

    Ok(Settings {
        base,
        rainbow,
        depth,
        max_step,
        config: Config {
            speed: args.speed,
            density: args.density,
            tail_min: args.tail_min,
            tail_max: args.tail_max,
            mutate: args.mutate,
            glyphs: args.charset.glyphs(&args.custom),
            seed: args.seed,
        },
        frame,
    })
}

/// Rolling frame-rate and output-volume meter.
///
/// Output volume is the number that matters: rmatrix's own CPU is negligible
/// next to what the terminal spends parsing the escape sequences we emit.
#[derive(Default)]
struct Meter {
    frames: u32,
    /// Ticks where the terminal was still busy, so the draw was folded into the
    /// next frame instead.
    skipped: u32,
    window: Duration,
    bytes: usize,
    damaged: usize,
    fps: f32,
    bytes_per_frame: f32,
    damage_pct: f32,
    coalesce_pct: f32,
}

impl Meter {
    /// A tick that produced no frame because the writer was still busy. Worth
    /// surfacing: a non-zero figure is the terminal telling you it is the
    /// bottleneck.
    fn skip(&mut self) {
        self.skipped += 1;
    }

    /// Returns true when the averaging window closed and the published figures
    /// changed — the caller uses that to avoid rewriting the title every frame.
    fn record(&mut self, dt: Duration, stats: DrawStats, cells: usize) -> bool {
        self.frames += 1;
        self.window += dt;
        self.bytes += stats.bytes;
        self.damaged += stats.cells_damaged;
        // Re-average about twice a second: often enough to feel live, rarely
        // enough that the digits stay readable.
        if self.window >= Duration::from_millis(500) {
            let secs = self.window.as_secs_f32().max(f32::EPSILON);
            self.fps = self.frames as f32 / secs;
            self.bytes_per_frame = self.bytes as f32 / self.frames as f32;
            self.damage_pct = if cells == 0 {
                0.0
            } else {
                self.damaged as f32 / (self.frames as usize * cells) as f32 * 100.0
            };
            self.coalesce_pct =
                self.skipped as f32 / (self.frames + self.skipped).max(1) as f32 * 100.0;
            *self = Meter {
                fps: self.fps,
                bytes_per_frame: self.bytes_per_frame,
                damage_pct: self.damage_pct,
                coalesce_pct: self.coalesce_pct,
                ..Meter::default()
            };
            return true;
        }
        false
    }

    fn line(&self, levels: u16, auto: bool) -> String {
        format!(
            " {:.0} fps · {:.1} KB/frame · {:.2} MB/s · {:.1}% cells · {:.0}% coalesced · {}{} · q quit ",
            self.fps,
            self.bytes_per_frame / 1024.0,
            self.bytes_per_frame * self.fps / 1.0e6,
            self.damage_pct,
            self.coalesce_pct,
            levels,
            if auto { " auto" } else { "" },
        )
    }

    /// Same figures, for the window title.
    ///
    /// The title is rendered by the OS in the UI font, not by the terminal grid,
    /// which makes it the only place these numbers stay legible under a font
    /// that remaps ASCII to glyphs — exactly the case when pairing `-c ascii`
    /// with Matrix Code NFI.
    fn title(&self, levels: u16, auto: bool) -> String {
        format!(
            "rmatrix — {:.0} fps · {:.1} KB/frame · {:.2} MB/s · {:.1}% cells · {:.0}% coalesced · {} levels{}",
            self.fps,
            self.bytes_per_frame / 1024.0,
            self.bytes_per_frame * self.fps / 1.0e6,
            self.damage_pct,
            self.coalesce_pct,
            levels,
            if auto { " (auto)" } else { "" },
        )
    }
}

/// Largest simulation step we will take in one frame.
///
/// This exists so a stall — laptop sleep, SIGSTOP, a terminal that stopped
/// draining — doesn't teleport every drop down the screen when the process
/// resumes. It has to sit *above* the normal frame period, or it truncates every
/// ordinary frame instead of only the abnormal ones: a flat 0.1 s clamp is below
/// the frame period under 10 fps, which silently ran `--fps 5` at exactly half
/// speed and `--fps 2` at a fifth.
///
/// Three frames' worth still catches a real stall promptly. The 0.1 s floor
/// makes this bit-identical to the old flat clamp at 30 fps and above, which
/// covers the default and every rate anyone runs in practice. Between 10 and
/// 30 fps it is somewhat more permissive than before (up to 0.3 s), which is the
/// point: "three frames" is a meaningful stall at any rate, "0.1 seconds" is not.
fn default_max_step(frame: Duration) -> f32 {
    (frame.as_secs_f32() * MAX_STEP_FRAMES).max(MAX_STEP_FLOOR)
}

/// OSC 2. Terminals that don't support it ignore the sequence.
fn set_title<W: Write>(out: &mut W, title: &str) -> Result<()> {
    // Strip anything that would terminate the sequence early.
    let safe: String = title.chars().filter(|c| !c.is_control()).collect();
    write!(out, "\x1b]2;{safe}\x07")?;
    Ok(())
}

/// Painted over the rain each frame, after the renderer has run.
fn draw_overlay<W: Write>(
    out: &mut W,
    w: u16,
    meter: &Meter,
    levels: u16,
    auto: bool,
) -> Result<()> {
    let text = meter.line(levels, auto);
    let trimmed: String = text.chars().take(w as usize).collect();
    out.queue(cursor::MoveTo(0, 0))?;
    out.queue(SetAttribute(Attribute::Reset))?;
    out.queue(SetBackgroundColor(Color::Rgb { r: 0, g: 40, b: 12 }))?;
    out.queue(SetForegroundColor(Color::Rgb {
        r: 190,
        g: 255,
        b: 200,
    }))?;
    out.queue(Print(trimmed))?;
    out.queue(SetAttribute(Attribute::Reset))?;
    Ok(())
}

/// Where drawn frames go.
///
/// The loop always renders into a plain `Vec<u8>`, which can neither block nor
/// fail; the only difference between the variants is who hands those bytes to
/// the file descriptor and when.
enum Sink {
    /// A writer thread owns stdout. The loop never blocks on the terminal.
    Threaded(FrameWriter),
    /// The pre-writer-thread path: write and flush inline, on the simulation
    /// thread, and stall there when the terminal is behind. Retained only so
    /// `--sync-io` can measure the thing we are fixing.
    Blocking { free: Option<Vec<u8>>, out: Stdout },
}

impl Sink {
    fn new(sync_io: bool, capacity: usize) -> Sink {
        if sync_io {
            Sink::Blocking {
                free: Some(Vec::with_capacity(capacity)),
                out: stdout(),
            }
        } else {
            Sink::Threaded(FrameWriter::spawn(stdout(), capacity))
        }
    }

    /// A cleared buffer to draw into, or `None` if the terminal is still
    /// swallowing the previous frame. `None` means **do not draw**: skipping the
    /// draw is exactly how frames coalesce without the damage tracker ever
    /// losing track of the screen.
    fn acquire(&mut self) -> Option<Vec<u8>> {
        match self {
            Sink::Threaded(w) => w.acquire(),
            Sink::Blocking { free, .. } => free.take().map(|mut b| {
                b.clear();
                b
            }),
        }
    }

    /// Hand off a drawn frame. Must never silently discard it — every byte the
    /// renderer produced has already been recorded in its `prev` buffer.
    fn submit(&mut self, buf: Vec<u8>) -> Result<()> {
        match self {
            Sink::Threaded(w) => Ok(w.submit(buf)?),
            Sink::Blocking { free, out } => {
                out.write_all(&buf)?;
                out.flush()?;
                *free = Some(buf);
                Ok(())
            }
        }
    }

    fn failed(&self) -> bool {
        match self {
            Sink::Threaded(w) => w.failed(),
            Sink::Blocking { .. } => false,
        }
    }

    /// Emit `last` and stop. Returns true if the sequence definitely reached the
    /// terminal; on false the caller falls back to writing it directly.
    fn shutdown(&mut self, last: Vec<u8>) -> bool {
        match self {
            // A second is far longer than any healthy terminal needs to accept
            // one frame, and short enough that quitting never feels hung.
            Sink::Threaded(w) => w.shutdown(last, Duration::from_secs(1)),
            Sink::Blocking { out, .. } => out.write_all(&last).and_then(|()| out.flush()).is_ok(),
        }
    }
}

fn setup(bold: bool) -> Result<()> {
    terminal::enable_raw_mode().context("entering raw mode")?;
    let mut out = stdout();
    out.execute(EnterAlternateScreen)?;
    out.execute(DisableLineWrap)?;
    out.execute(cursor::Hide)?;
    out.execute(Clear(ClearType::All))?;
    // Save the window title so the stats meter can borrow it and hand it back.
    out.write_all(b"\x1b[22;2t")?;
    if bold {
        out.execute(SetAttribute(Attribute::Bold))?;
    }
    Ok(())
}

/// The bytes that put the terminal back the way we found it.
///
/// Split out from [`restore`] because the writer thread must be able to emit
/// them as its final act: whoever wrote the last frame has to write the restore
/// too, or the two can race.
fn restore_sequence() -> Vec<u8> {
    let mut b = Vec::new();
    let _ = b.write_all(b"\x1b[23;2t"); // give the window title back
    let _ = b.queue(SetAttribute(Attribute::Reset));
    let _ = b.queue(cursor::Show);
    let _ = b.queue(EnableLineWrap);
    let _ = b.queue(LeaveAlternateScreen);
    b
}

/// Best-effort teardown. Used by the panic hook too, so it must not panic and
/// must be safe to call more than once.
///
/// Safe to call while a writer thread is live: it goes through `Stdout`, whose
/// lock is held for the whole of `write_all`, so this can only land before or
/// after a frame, never inside one.
fn restore() {
    writer::abort_all();
    let mut out = stdout();
    let _ = out.write_all(&restore_sequence());
    let _ = terminal::disable_raw_mode();
    let _ = out.flush();
}

/// Ticks excluded from `--bench` samples. The first draw after the alt-screen
/// clear repaints every lit cell — an order of magnitude more bytes than a
/// steady-state frame — and that one outlier is not what anybody is looking at.
const BENCH_PRIME_TICKS: u32 = 15;

/// Timing recorder for `--bench`.
#[derive(Default)]
struct Bench {
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
    fn tick(&mut self, interval: Duration, dt: f32) {
        let ms = interval.as_secs_f64() * 1000.0;
        self.tick_ms.push(ms);
        self.wall_ms += ms;
        self.sim_ms += f64::from(dt) * 1000.0;
        self.pending += f64::from(dt) * 1000.0;
    }

    fn frame(&mut self, interval: Duration) {
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

    fn report(&self, mode: &str, submitted: u64, coalesced: u64) {
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
                    match code {
                        KeyCode::Char('q') | KeyCode::Esc => break 'outer,
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
                draw_overlay(
                    &mut buf,
                    w,
                    &meter,
                    theme.levels,
                    args.levels == Levels::Auto,
                )?;
                // Only on refresh: retitling every frame is pointless churn, and
                // some terminals flash the title bar when it changes.
                if refreshed {
                    set_title(
                        &mut buf,
                        &meter.title(theme.levels, args.levels == Levels::Auto),
                    )?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Args {
        Args::parse_from(["rmatrix"])
    }

    #[test]
    fn defaults_are_valid() {
        assert!(validate(&args()).is_ok());
    }

    #[test]
    fn cli_parses_the_documented_flags() {
        let a = Args::parse_from([
            "rmatrix",
            "-C",
            "#ff0000",
            "-c",
            "katakana",
            "-S",
            "2.0",
            "-d",
            "0.9",
            "-b",
            "-s",
            "--fps",
            "30",
            "--seed",
            "12",
            "--tail-min",
            "2",
            "--tail-max",
            "9",
        ]);
        assert_eq!(a.color, "#ff0000");
        assert_eq!(a.charset, Charset::Katakana);
        assert_eq!(a.seed, Some(12));
        assert!(a.bold && a.screensaver);
        let s = validate(&a).expect("should validate");
        assert_eq!(s.base, (255, 0, 0));
        assert_eq!(s.frame, Duration::from_secs_f64(1.0 / 30.0));
    }

    #[test]
    fn rainbow_sets_the_flag_not_a_literal_colour() {
        let mut a = args();
        a.color = "rainbow".into();
        assert!(validate(&a).expect("valid").rainbow);
    }

    #[test]
    fn bad_density_is_rejected() {
        for d in [-0.1, 1.1, f32::NAN] {
            let mut a = args();
            a.density = d;
            assert!(validate(&a).is_err(), "density {d} should be rejected");
        }
    }

    #[test]
    fn bad_tails_are_rejected() {
        let mut a = args();
        (a.tail_min, a.tail_max) = (20.0, 5.0);
        assert!(
            validate(&a).is_err(),
            "tail-max below tail-min should be rejected"
        );

        let mut a = args();
        a.tail_min = 0.0;
        assert!(validate(&a).is_err(), "zero tail-min should be rejected");
    }

    #[test]
    fn nonpositive_speed_is_rejected() {
        for sp in [0.0, -1.0, f32::INFINITY] {
            let mut a = args();
            a.speed = sp;
            assert!(validate(&a).is_err(), "speed {sp} should be rejected");
        }
    }

    #[test]
    fn custom_charset_requires_glyphs() {
        let mut a = args();
        a.charset = Charset::Custom;
        assert!(
            validate(&a).is_err(),
            "custom charset without --custom should be rejected"
        );
        a.custom = "ab".into();
        assert!(validate(&a).is_ok());
    }

    #[test]
    fn custom_glyphs_that_would_desync_the_renderer_are_rejected() {
        // The built-in charsets are tested for these invariants; `--custom` is
        // the one path that reaches `Print` without them. A control character
        // lands in the middle of the escape stream, and anything that is not one
        // column wide breaks the cursor arithmetic in `render.rs`, which shifts
        // every cell drawn after it.
        for (bad, what) in [
            ("a\u{7}b", "a control character"),
            ("ab\u{1b}", "an escape"),
            ("日", "a fullwidth ideograph"),
            ("Ａ", "fullwidth ASCII"),
            ("🌧", "a pictograph"),
            ("e\u{0301}", "a combining acute"),
            ("a\u{200D}b", "a zero-width joiner"),
        ] {
            let mut a = args();
            a.charset = Charset::Custom;
            a.custom = bad.into();
            assert!(
                validate(&a).is_err(),
                "{bad:?} contains {what} and should be rejected"
            );
        }
    }

    #[test]
    fn single_column_custom_glyphs_are_accepted() {
        // Halfwidth katakana is the case that must not be caught by the width
        // check: it sits directly above the fullwidth block it is screening for.
        for good in ["abc", "ｱｲｳ", "01", "αβγ", "!@#"] {
            let mut a = args();
            a.charset = Charset::Custom;
            a.custom = good.into();
            assert!(validate(&a).is_ok(), "{good:?} should be accepted");
        }
    }

    #[test]
    fn a_rejected_custom_glyph_says_which_flag_and_which_character() {
        for bad in ["a\u{7}b", "日", "a\u{200D}b"] {
            let mut a = args();
            a.charset = Charset::Custom;
            a.custom = bad.into();
            let e = format!("{:#}", validate(&a).expect_err("should reject"));
            assert!(e.contains("--custom"), "error did not name the flag: {e}");
            assert!(
                e.contains("U+"),
                "error did not identify the character: {e}"
            );
        }
    }

    #[test]
    fn unknown_colour_is_rejected_with_context() {
        let mut a = args();
        a.color = "chartreuse".into();
        let e = validate(&a).expect_err("should reject");
        assert!(
            format!("{e:#}").contains("--color"),
            "error lost its context: {e:#}"
        );
    }

    #[test]
    fn the_step_clamp_never_throttles_the_frame_rate_asked_for() {
        // A fixed 0.1s clamp shorter than the frame period truncates every step,
        // and the rain silently runs slow — at `--fps 5` it measured at exactly
        // half speed. The clamp must never sit below one frame period.
        for fps in [1u16, 5, 10, 15, 24, 30, 60, 120, 240] {
            let a = Args::parse_from(["rmatrix", "--fps", &fps.to_string()]);
            let s = validate(&a).expect("valid");
            assert!(
                s.max_step >= s.frame.as_secs_f32(),
                "at {fps} fps the clamp {} is under the frame period {:?}",
                s.max_step,
                s.frame
            );
        }
    }

    #[test]
    fn the_step_clamp_is_unchanged_at_the_default_frame_rate() {
        // 3 x 1/30s is exactly the 0.1s the loop has always used, so nobody on
        // defaults sees a different animation.
        assert!((validate(&args()).expect("valid").max_step - 0.1).abs() < 1e-6);
        // Faster frame rates keep the floor rather than shrinking with it.
        let a = Args::parse_from(["rmatrix", "--fps", "120"]);
        assert!((validate(&a).expect("valid").max_step - 0.1).abs() < 1e-6);
        // Slower ones scale up.
        let a = Args::parse_from(["rmatrix", "--fps", "5"]);
        assert!((validate(&a).expect("valid").max_step - 0.6).abs() < 1e-6);
    }

    #[test]
    fn fps_is_clamped_rather_than_dividing_by_zero() {
        let mut a = args();
        a.fps = 0;
        assert_eq!(validate(&a).expect("valid").frame, Duration::from_secs(1));
        a.fps = u16::MAX;
        assert_eq!(
            validate(&a).expect("valid").frame,
            Duration::from_secs_f64(1.0 / 240.0)
        );
    }

    #[test]
    fn colour_depth_can_be_forced() {
        for (flag, want) in [
            ("truecolor", Depth::True),
            ("256", Depth::Ansi256),
            ("16", Depth::Ansi16),
        ] {
            let mut a = args();
            a.color_depth = flag.into();
            assert_eq!(validate(&a).expect("valid").depth, want);
        }
    }

    #[test]
    fn every_cycled_charset_yields_glyphs() {
        for cs in CYCLE {
            assert!(!cs.glyphs("").is_empty(), "{cs:?} produced no glyphs");
        }
    }

    #[test]
    fn cli_definition_is_internally_consistent() {
        use clap::CommandFactory;
        Args::command().debug_assert();
    }

    #[test]
    fn perf_defaults_favour_the_terminal() {
        // These two defaults exist to keep output volume down; if someone raises
        // them casually, this test should make them think about it first.
        let a = args();
        assert_eq!(a.fps, 30, "fps default drives output volume linearly");
        assert_eq!(
            a.levels,
            Levels::Auto,
            "quality should size itself to the window"
        );
        assert!(!a.stats, "the overlay should be opt-in");
    }

    #[test]
    fn levels_and_stats_are_settable() {
        let a = Args::parse_from(["rmatrix", "--levels", "8", "--stats", "--fps", "120"]);
        assert_eq!(a.levels, Levels::Fixed(8));
        assert!(a.stats);
        assert!(validate(&a).is_ok());

        // 0 is the documented "no quantisation" escape hatch.
        let a = Args::parse_from(["rmatrix", "--levels", "0"]);
        assert_eq!(a.levels, Levels::Fixed(0));
        assert!(validate(&a).is_ok());
    }

    #[test]
    fn auto_levels_track_the_window_but_a_fixed_setting_does_not() {
        // The whole point of `auto`: a stock terminal and a full-screen vertical
        // one are an order of magnitude apart in cells and should not share a
        // quality setting.
        let auto = Args::parse_from(["rmatrix"]).levels;
        assert!(auto.resolve(80, 24) > auto.resolve(204, 175));

        let fixed = Args::parse_from(["rmatrix", "--levels", "20"]).levels;
        assert_eq!(fixed.resolve(80, 24), fixed.resolve(204, 175));
    }

    #[test]
    fn bad_levels_are_rejected_by_the_parser() {
        for bad in ["twelve", "-3", "auto2", "1.5"] {
            assert!(
                Args::try_parse_from(["rmatrix", "--levels", bad]).is_err(),
                "--levels {bad} should be rejected"
            );
        }
    }

    #[test]
    fn meter_reports_zero_before_its_first_window_closes() {
        let mut m = Meter::default();
        m.record(
            Duration::from_millis(10),
            DrawStats {
                cells_damaged: 5,
                bytes: 100,
            },
            1000,
        );
        assert_eq!(m.fps, 0.0, "should not publish an average from one sample");
        assert!(m.line(24, true).contains("fps"));
    }

    #[test]
    fn meter_averages_over_its_window() {
        let mut m = Meter::default();
        // 30 frames of 1/30s each = exactly one second, so 30 fps.
        for _ in 0..30 {
            m.record(
                Duration::from_secs_f64(1.0 / 30.0),
                DrawStats {
                    cells_damaged: 100,
                    bytes: 2048,
                },
                1000,
            );
        }
        assert!((m.fps - 30.0).abs() < 1.0, "fps was {}", m.fps);
        assert!((m.bytes_per_frame - 2048.0).abs() < 1.0);
        assert!(
            (m.damage_pct - 10.0).abs() < 0.5,
            "damage was {}",
            m.damage_pct
        );
    }

    #[test]
    fn the_step_clamp_never_truncates_an_ordinary_frame() {
        // The bug this pins: a flat 0.1s clamp sits *below* the frame period
        // under 10 fps, so every frame was truncated and the rain ran slow.
        for fps in [1u16, 2, 5, 10, 24, 30, 60, 120, 240] {
            let frame = Duration::from_secs_f64(1.0 / f64::from(fps));
            let clamp = default_max_step(frame);
            assert!(
                clamp >= frame.as_secs_f32(),
                "--fps {fps}: clamp {clamp} truncates a normal {:?} frame",
                frame
            );
        }
    }

    #[test]
    fn the_step_clamp_is_unchanged_at_thirty_fps_and_above() {
        // 3 x (1/30) is exactly 0.1, so the floor takes over at and above the
        // default rate and behaviour is identical to the old flat clamp.
        for fps in [30u16, 60, 120, 240] {
            let frame = Duration::from_secs_f64(1.0 / f64::from(fps));
            assert_eq!(
                default_max_step(frame),
                0.1,
                "--fps {fps} changed behaviour"
            );
        }
        // Below 30 it is deliberately looser — three frames rather than a flat
        // tenth of a second — but never tighter.
        for fps in [10u16, 24] {
            let frame = Duration::from_secs_f64(1.0 / f64::from(fps));
            assert!(
                default_max_step(frame) >= 0.1,
                "--fps {fps} got tighter, not looser"
            );
        }
    }

    #[test]
    fn the_step_clamp_still_catches_a_real_stall() {
        // It must not become so loose that a genuine stall teleports the rain.
        for fps in [5u16, 30, 60] {
            let frame = Duration::from_secs_f64(1.0 / f64::from(fps));
            let clamp = default_max_step(frame);
            assert!(clamp <= 0.6, "--fps {fps}: clamp {clamp} is too permissive");
            // A ten-frame stall is abnormal and must still be clamped.
            assert!(clamp < frame.as_secs_f32() * 10.0);
        }
    }

    #[test]
    fn meter_signals_only_when_its_window_closes() {
        let mut m = Meter::default();
        // Under the 500ms window: no refresh, so no title churn.
        assert!(!m.record(Duration::from_millis(100), DrawStats::default(), 100));
        assert!(!m.record(Duration::from_millis(300), DrawStats::default(), 100));
        // Crossing it publishes.
        assert!(m.record(Duration::from_millis(200), DrawStats::default(), 100));
        // And the window resets, so the next tick is quiet again.
        assert!(!m.record(Duration::from_millis(100), DrawStats::default(), 100));
    }

    #[test]
    fn title_stays_legible_without_control_characters() {
        // The title is the fallback readout when the terminal font remaps
        // ASCII, so it must survive being written raw into an OSC sequence.
        let mut m = Meter::default();
        for _ in 0..30 {
            m.record(
                Duration::from_millis(20),
                DrawStats {
                    cells_damaged: 10,
                    bytes: 500,
                },
                1000,
            );
        }
        let t = m.title(8, true);
        assert!(t.starts_with("rmatrix"));
        assert!(t.contains("fps"), "{t}");
        assert!(
            !t.chars().any(char::is_control),
            "title had a control char: {t:?}"
        );

        let mut buf = Vec::new();
        set_title(&mut buf, "evil\x07title\x1b[0m").expect("writing to a Vec cannot fail");
        assert_eq!(
            buf, b"\x1b]2;eviltitle[0m\x07",
            "control chars leaked into the OSC"
        );
    }

    #[test]
    fn meter_survives_a_zero_cell_screen() {
        let mut m = Meter::default();
        for _ in 0..30 {
            m.record(Duration::from_millis(50), DrawStats::default(), 0);
        }
        assert_eq!(m.damage_pct, 0.0);
        assert!(m.line(24, false).contains("0.0% cells"));
    }
}
