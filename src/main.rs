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
use rmatrix::{BaseColor, Charset, Config, Depth, DrawStats, Levels, Rain, Renderer, Theme};
use std::io::{Write, stdout};
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
}

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

    let depth = match args.color_depth.as_str() {
        "truecolor" => Depth::True,
        "256" => Depth::Ansi256,
        "16" => Depth::Ansi16,
        _ => Depth::detect(),
    };

    Ok(Settings {
        base,
        rainbow,
        depth,
        config: Config {
            speed: args.speed,
            density: args.density,
            tail_min: args.tail_min,
            tail_max: args.tail_max,
            mutate: args.mutate,
            glyphs: args.charset.glyphs(&args.custom),
            seed: args.seed,
        },
        frame: Duration::from_secs_f64(1.0 / f64::from(args.fps.clamp(1, 240))),
    })
}

/// Rolling frame-rate and output-volume meter.
///
/// Output volume is the number that matters: rmatrix's own CPU is negligible
/// next to what the terminal spends parsing the escape sequences we emit.
#[derive(Default)]
struct Meter {
    frames: u32,
    window: Duration,
    bytes: usize,
    damaged: usize,
    fps: f32,
    bytes_per_frame: f32,
    damage_pct: f32,
}

impl Meter {
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
            *self = Meter {
                fps: self.fps,
                bytes_per_frame: self.bytes_per_frame,
                damage_pct: self.damage_pct,
                ..Meter::default()
            };
            return true;
        }
        false
    }

    fn line(&self, levels: u16, auto: bool) -> String {
        format!(
            " {:.0} fps · {:.1} KB/frame · {:.2} MB/s · {:.1}% cells · {}{} · q quit ",
            self.fps,
            self.bytes_per_frame / 1024.0,
            self.bytes_per_frame * self.fps / 1.0e6,
            self.damage_pct,
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
            "rmatrix — {:.0} fps · {:.1} KB/frame · {:.2} MB/s · {:.1}% cells · {} levels{}",
            self.fps,
            self.bytes_per_frame / 1024.0,
            self.bytes_per_frame * self.fps / 1.0e6,
            self.damage_pct,
            levels,
            if auto { " (auto)" } else { "" },
        )
    }
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

/// Best-effort teardown. Used by the panic hook too, so it must not panic and
/// must be safe to call more than once.
fn restore() {
    let mut out = stdout();
    let _ = out.write_all(b"\x1b[23;2t"); // give the window title back
    let _ = out.execute(SetAttribute(Attribute::Reset));
    let _ = out.execute(cursor::Show);
    let _ = out.execute(EnableLineWrap);
    let _ = out.execute(LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
    let _ = out.flush();
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

    let mut out = std::io::BufWriter::with_capacity(1 << 18, stdout());
    let mut last = Instant::now();
    let mut last_frame = Instant::now();
    let mut paused = false;
    let mut show_stats = args.stats;
    let mut meter = Meter::default();

    'outer: loop {
        let frame_start = Instant::now();

        // Drain input until it is time to render the next frame.
        while let Some(remaining) = s.frame.checked_sub(frame_start.elapsed()) {
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
                            out.write_all(if bold { b"\x1b[1m" } else { b"\x1b[22m" })?;
                            renderer.resize(w, h);
                        }
                        KeyCode::Char('f') => {
                            show_stats = !show_stats;
                            if !show_stats {
                                set_title(&mut out, "rmatrix")?;
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
                    out.write_all(b"\x1b[2J")?;
                }
                _ => {}
            }
        }

        let now = Instant::now();
        // Clamp so a stall (laptop sleep, SIGSTOP) doesn't teleport every drop.
        let dt = (now - last).as_secs_f32().min(0.1);
        last = now;

        if !paused {
            rain.step(dt);
        }
        let stats = renderer.draw(&mut out, &rain, &theme, s.depth)?;
        let refreshed = meter.record(now - last_frame, stats, w as usize * h as usize);
        last_frame = now;

        if show_stats {
            draw_overlay(
                &mut out,
                w,
                &meter,
                theme.levels,
                args.levels == Levels::Auto,
            )?;
            // Only on refresh: retitling every frame is pointless churn, and
            // some terminals flash the title bar when it changes.
            if refreshed {
                set_title(
                    &mut out,
                    &meter.title(theme.levels, args.levels == Levels::Auto),
                )?;
            }
            // The overlay wrote colour and moved the cursor behind the
            // renderer's back; without this the next frame paints wrong.
            renderer.forget_cursor_and_color();
            out.flush()?;
        }
    }

    drop(out);
    restore();
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
