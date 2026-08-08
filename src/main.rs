//! rmatrix — digital rain for modern terminals.
//!
//! This binary is a thin wrapper: it parses and validates arguments, owns the
//! terminal's raw/alt-screen state, and pumps the event loop. All of the
//! behaviour lives in the library so tests can drive it without a tty.

use anyhow::{Context, Result, bail};
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, SetAttribute};
use crossterm::terminal::{
    self, Clear, ClearType, DisableLineWrap, EnableLineWrap, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::{ExecutableCommand, cursor};
use rmatrix::{BaseColor, Charset, Config, Depth, Rain, Renderer, Theme};
use std::io::{Write, stdout};
use std::process::ExitCode;
use std::str::FromStr;
use std::time::{Duration, Instant};

#[derive(Parser, Debug, Clone)]
#[command(
    name = "rmatrix",
    version,
    about = "Digital rain for modern terminals",
    after_help = "KEYS:\n  q, Esc, Ctrl-C  quit\n  space           pause\n  1-9             speed\n  r               toggle rainbow\n  c               cycle charset\n  b               toggle bold"
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

    /// Frame rate cap
    #[arg(long, default_value_t = 60)]
    fps: u16,

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

fn setup(bold: bool) -> Result<()> {
    terminal::enable_raw_mode().context("entering raw mode")?;
    let mut out = stdout();
    out.execute(EnterAlternateScreen)?;
    out.execute(DisableLineWrap)?;
    out.execute(cursor::Hide)?;
    out.execute(Clear(ClearType::All))?;
    if bold {
        out.execute(SetAttribute(Attribute::Bold))?;
    }
    Ok(())
}

/// Best-effort teardown. Used by the panic hook too, so it must not panic and
/// must be safe to call more than once.
fn restore() {
    let mut out = stdout();
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
    let mut bold = args.bold;
    let mut rain = Rain::new(w, h, s.config);
    let mut renderer = Renderer::new(w, h);

    let mut out = std::io::BufWriter::with_capacity(1 << 18, stdout());
    let mut last = Instant::now();
    let mut paused = false;

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
                        _ => {}
                    }
                }
                Event::Resize(nw, nh) => {
                    w = nw.max(1);
                    h = nh.max(1);
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
        renderer.draw(&mut out, &rain, &theme, s.depth)?;
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
}
