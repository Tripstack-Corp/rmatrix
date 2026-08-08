//! Command line: the flags, and turning them into something the loop can trust.
//!
//! [`validate`] is the only place that rejects input, and it is pure — no
//! terminal, no globals — so every rule in it is testable directly.

use anyhow::{Context, Result, bail};
use clap::Parser;
use rmatrix::charset::{is_wide, is_zero_width};
use rmatrix::{BaseColor, Charset, Config, Depth, Levels};
use std::str::FromStr;
use std::time::Duration;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "rmatrix",
    version,
    about = "Digital rain for modern terminals",
    after_help = "KEYS:\n  q, Esc, Ctrl-C  quit\n  space           pause\n  1-9             speed\n  r               toggle rainbow\n  c               cycle charset\n  b               toggle bold\n  f               toggle stats overlay"
)]
pub(crate) struct Args {
    /// Colour name, #RRGGBB, or "rainbow"
    #[arg(short = 'C', long, default_value = "green")]
    pub(crate) color: String,

    /// Glyph set
    #[arg(short = 'c', long, value_enum, default_value_t = Charset::Classic)]
    pub(crate) charset: Charset,

    /// Glyphs to use with `--charset custom`
    ///
    /// Every glyph must occupy exactly one terminal column: no control
    /// characters, no double-width or fullwidth characters, and no combining
    /// or zero-width characters. The renderer positions the cursor by counting
    /// columns, so anything else smears the frame.
    #[arg(long, default_value = "")]
    pub(crate) custom: String,

    /// Overall speed multiplier
    #[arg(short = 'S', long, default_value_t = 1.0)]
    pub(crate) speed: f32,

    /// Frame rate cap. Output volume scales linearly with this, and the
    /// terminal — not rmatrix — is what pays for it.
    #[arg(long, default_value_t = 30)]
    pub(crate) fps: u16,

    /// Brightness steps in the trail, or "auto" to size them from the terminal.
    /// Lower means fewer escape sequences for the terminal to parse; 0 disables
    /// quantisation. "auto" re-picks on resize.
    #[arg(long, default_value = "auto")]
    pub(crate) levels: Levels,

    /// Start with the stats overlay visible (toggle with `f`)
    #[arg(long)]
    pub(crate) stats: bool,

    /// Fraction of columns raining at any moment (0.0-1.0)
    #[arg(short = 'd', long, default_value_t = 0.55)]
    pub(crate) density: f32,

    /// Shortest trail, in rows
    #[arg(long, default_value_t = 6.0)]
    pub(crate) tail_min: f32,

    /// Longest trail, in rows
    #[arg(long, default_value_t = 26.0)]
    pub(crate) tail_max: f32,

    /// Glyph churn rate (screens per second); 0 disables
    #[arg(short = 'm', long, default_value_t = 0.35)]
    pub(crate) mutate: f32,

    /// Bold glyphs
    #[arg(short = 'b', long)]
    pub(crate) bold: bool,

    /// Exit on any keypress
    #[arg(short = 's', long)]
    pub(crate) screensaver: bool,

    /// Replay a specific animation
    #[arg(long)]
    pub(crate) seed: Option<u64>,

    /// Force colour depth instead of detecting it
    #[arg(long, value_parser = ["auto", "truecolor", "256", "16"], default_value = "auto")]
    pub(crate) color_depth: String,

    /// Write frames from the simulation thread, blocking on the terminal.
    ///
    /// The pre-writer-thread behaviour, kept so the two I/O paths can be A/B'd
    /// under an identical run. Nobody should want this.
    #[arg(long, hide = true)]
    pub(crate) sync_io: bool,

    /// Run this many loop ticks with timing instrumentation, then exit,
    /// reporting frame-time percentiles on stderr.
    #[arg(long, hide = true, value_name = "TICKS")]
    pub(crate) bench: Option<u32>,

    /// Seconds of rain to simulate before `--bench` starts measuring.
    ///
    /// The slowest drops fall at 6 rows/s, so a 175-row window needs ~29s to
    /// reach steady state; measuring a half-empty screen understates the output
    /// volume by roughly 2x.
    #[arg(long, hide = true, default_value_t = 32.0)]
    pub(crate) bench_warmup: f32,

    /// Override the largest simulation step, in seconds. 0 derives it from
    /// `--fps`; see `max_step`. Exposed only so the clamp can be ablated.
    #[arg(long, hide = true, default_value_t = 0.0)]
    pub(crate) max_step: f32,
}

/// Charsets reachable with the `c` key, in order.
pub(crate) const CYCLE: [Charset; 6] = [
    Charset::Classic,
    Charset::Katakana,
    Charset::Ascii,
    Charset::Alnum,
    Charset::Binary,
    Charset::Greek,
];

/// Everything the loop needs, once the arguments are known-good.
#[derive(Debug)]
pub(crate) struct Settings {
    pub(crate) base: (u8, u8, u8),
    pub(crate) rainbow: bool,
    pub(crate) depth: Depth,
    pub(crate) config: Config,
    pub(crate) frame: Duration,
    /// Largest `dt` handed to the simulation in one tick, in seconds.
    pub(crate) max_step: f32,
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

/// Pure argument validation, split out so it is testable without a terminal.
pub(crate) fn validate(args: &Args) -> Result<Settings> {
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
}
