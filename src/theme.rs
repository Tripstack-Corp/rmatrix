//! Colour ramps.
//!
//! The visual difference from cmatrix lives here: cmatrix has three brightness
//! levels (white head / bold / normal), so its tails step. We map a continuous
//! `glow` in [0,1] onto a 24-bit ramp, which is what makes the trail read as a
//! fade rather than as bands.

use crossterm::style::Color;
use std::str::FromStr;
use thiserror::Error;

pub type Rgb = (u8, u8, u8);

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ColorParseError {
    #[error("expected a colour name or #RRGGBB, got {0:?}")]
    Unrecognised(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    True,
    Ansi256,
    Ansi16,
}

impl Depth {
    /// Terminals advertise 24-bit support via `COLORTERM`; everything modern
    /// (iTerm2, kitty, WezTerm, Ghostty, Alacritty, VTE) sets it.
    #[must_use]
    pub fn detect() -> Depth {
        Depth::from_env(
            std::env::var("COLORTERM").ok().as_deref(),
            std::env::var("TERM").ok().as_deref(),
        )
    }

    /// Split out from [`Depth::detect`] so the decision table is testable
    /// without mutating process-global environment.
    #[must_use]
    pub fn from_env(colorterm: Option<&str>, term: Option<&str>) -> Depth {
        match colorterm {
            Some("truecolor" | "24bit") => Depth::True,
            _ => match term {
                None | Some("" | "dumb") => Depth::Ansi16,
                Some(t) if t.contains("256") => Depth::Ansi256,
                // Assume 256 rather than 16: every terminal emulator in use
                // this decade has it, and guessing low looks noticeably worse.
                Some(_) => Depth::Ansi256,
            },
        }
    }

    #[must_use]
    pub fn to_color(self, (r, g, b): Rgb) -> Color {
        match self {
            Depth::True => Color::Rgb { r, g, b },
            Depth::Ansi256 => Color::AnsiValue(rgb_to_256(r, g, b)),
            Depth::Ansi16 => Color::AnsiValue(rgb_to_16(r, g, b)),
        }
    }
}

/// Brightness steps in the ramp. This is a *performance* control as much as a
/// visual one: a cell only becomes damaged when it crosses a step, so halving
/// the count roughly halves the escape sequences we hand the terminal. 24 is
/// already eight times cmatrix's three levels and reads as continuous.
pub const DEFAULT_LEVELS: u16 = 24;

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    /// Leading glyph — near-white, tinted toward the body hue.
    pub head: Rgb,
    /// Full-intensity body colour just behind the head.
    pub bright: Rgb,
    /// The colour a trail decays toward before it is erased.
    pub dim: Rgb,
    pub rainbow: bool,
    /// Quantisation steps; 0 disables quantisation entirely.
    pub levels: u16,
}

impl Theme {
    #[must_use]
    pub fn from_base(base: Rgb, rainbow: bool) -> Theme {
        Theme {
            head: head_tint(base),
            bright: base,
            dim: scale(base, 0.18),
            rainbow,
            levels: DEFAULT_LEVELS,
        }
    }

    /// `glow` 1.0 is freshly written, 0.0 is about to be erased. `hue_shift`
    /// rotates the hue in rainbow mode and is ignored otherwise.
    #[must_use]
    pub fn color(&self, glow: f32, is_head: bool, hue_shift: f32) -> Rgb {
        let (bright, dim, head) = if self.rainbow {
            let b = hsv_to_rgb(hue_shift.rem_euclid(1.0), 0.85, 1.0);
            (b, scale(b, 0.18), head_tint(b))
        } else {
            (self.bright, self.dim, self.head)
        };

        if is_head {
            return head;
        }
        // Gamma > 1 keeps the bright zone tight to the head and lets the rest of
        // the tail linger dim — closer to the film than a linear ramp.
        let t = if glow.is_finite() {
            glow.clamp(0.0, 1.0)
        } else {
            0.0
        };
        // Quantise *after* the gamma so the steps are evenly spaced in the
        // output colour rather than bunched at the dim end.
        lerp(dim, bright, self.quantise(t.powf(1.7)))
    }

    /// Manhattan RGB distance between two adjacent steps of the ramp.
    ///
    /// The unit the quality governor works in: a drift budget of "one step" is
    /// the same visual concession whether the ramp is fine or coarse, bright or
    /// dim, where a raw RGB budget would not be.
    #[must_use]
    pub fn ramp_step(&self) -> f32 {
        let span = u16::from(self.bright.0.abs_diff(self.dim.0))
            + u16::from(self.bright.1.abs_diff(self.dim.1))
            + u16::from(self.bright.2.abs_diff(self.dim.2));
        let n = if self.levels == 0 {
            DEFAULT_LEVELS
        } else {
            self.levels
        };
        f32::from(span) / f32::from(n)
    }

    /// Snap to the nearest of `levels` steps. Holding a cell's colour steady
    /// across frames is what stops it generating damage every single frame.
    fn quantise(&self, t: f32) -> f32 {
        if self.levels == 0 {
            return t;
        }
        let n = f32::from(self.levels);
        (t * n).round() / n
    }
}

fn head_tint(base: Rgb) -> Rgb {
    lerp(base, (255, 255, 255), 0.80)
}

fn lerp((r1, g1, b1): Rgb, (r2, g2, b2): Rgb, t: f32) -> Rgb {
    let f = |a: u8, b: u8| {
        (f32::from(a) + (f32::from(b) - f32::from(a)) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    (f(r1, r2), f(g1, g2), f(b1, b2))
}

fn scale((r, g, b): Rgb, t: f32) -> Rgb {
    let f = |c: u8| (f32::from(c) * t).round().clamp(0.0, 255.0) as u8;
    (f(r), f(g), f(b))
}

#[must_use]
pub fn hsv_to_rgb(h: f32, s: f32, v: f32) -> Rgb {
    let i = (h * 6.0).floor();
    let f = h * 6.0 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    let (r, g, b) = match (i as i32).rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    ((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}

/// A parsed base colour plus whether the user asked for rainbow mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BaseColor(pub Rgb, pub bool);

impl FromStr for BaseColor {
    type Err = ColorParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let key = s.trim().to_ascii_lowercase();
        let rgb = match key.as_str() {
            "green" => (0, 255, 65), // the canonical Matrix green
            "red" => (255, 40, 40),
            "blue" => (60, 120, 255),
            "cyan" => (0, 230, 230),
            "magenta" => (230, 60, 230),
            "yellow" => (240, 220, 60),
            "orange" => (255, 140, 30),
            "purple" => (170, 90, 255),
            "white" => (220, 220, 220),
            "rainbow" => return Ok(BaseColor((0, 255, 65), true)),
            hex => {
                let hex = hex.strip_prefix('#').unwrap_or(hex);
                if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err(ColorParseError::Unrecognised(s.to_string()));
                }
                // Safe: the guard above proved all six chars are hex digits.
                let p = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0);
                (p(0), p(2), p(4))
            }
        };
        Ok(BaseColor(rgb, false))
    }
}

fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    // Greys have their own ramp and quantise much better there.
    if r.abs_diff(g) < 8 && g.abs_diff(b) < 8 {
        let level = (u16::from(r) + u16::from(g) + u16::from(b)) / 3;
        if level < 8 {
            return 16;
        }
        if level > 248 {
            return 231;
        }
        // The grey ramp is 232..=255 — 24 slots, so the index must top out at
        // 23. Scaling by 24 here reaches 24 at level 248 and overflows u8.
        return 232 + ((level - 8) * 23 / 240) as u8;
    }
    let q = |c: u8| u16::from(c) * 5 / 255;
    (16 + 36 * q(r) + 6 * q(g) + q(b)) as u8
}

fn rgb_to_16(r: u8, g: u8, b: u8) -> u8 {
    let bright = r.max(g).max(b) > 128;
    let bit = |c: u8| u8::from(c > 96);
    let base = bit(r) | (bit(g) << 1) | (bit(b) << 2);
    if bright { base + 8 } else { base }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luma((r, g, b): Rgb) -> f32 {
        0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)
    }

    #[test]
    fn named_colors_parse() {
        assert_eq!(
            BaseColor::from_str("green"),
            Ok(BaseColor((0, 255, 65), false))
        );
        assert_eq!(
            BaseColor::from_str("  GREEN  "),
            Ok(BaseColor((0, 255, 65), false))
        );
        assert_eq!(
            BaseColor::from_str("rainbow"),
            Ok(BaseColor((0, 255, 65), true))
        );
    }

    #[test]
    fn hex_colors_parse_with_and_without_hash() {
        assert_eq!(
            BaseColor::from_str("#ff8800"),
            Ok(BaseColor((255, 136, 0), false))
        );
        assert_eq!(
            BaseColor::from_str("FF8800"),
            Ok(BaseColor((255, 136, 0), false))
        );
    }

    #[test]
    fn bad_colors_are_rejected_not_panicked() {
        for bad in ["", "#ff88", "#gggggg", "chartreuse", "#ff88000", "##ffffff"] {
            assert!(
                BaseColor::from_str(bad).is_err(),
                "{bad:?} should not parse"
            );
        }
    }

    #[test]
    fn ramp_is_monotonic_in_glow() {
        let t = Theme::from_base((0, 255, 65), false);
        let mut prev = -1.0;
        for i in 0..=20 {
            let l = luma(t.color(i as f32 / 20.0, false, 0.0));
            assert!(l >= prev, "ramp dipped at glow {}", i as f32 / 20.0);
            prev = l;
        }
    }

    #[test]
    fn head_is_the_brightest_thing_on_screen() {
        let t = Theme::from_base((0, 255, 65), false);
        assert!(luma(t.color(1.0, true, 0.0)) > luma(t.color(1.0, false, 0.0)));
        assert!(luma(t.color(1.0, false, 0.0)) > luma(t.color(0.0, false, 0.0)));
    }

    #[test]
    fn out_of_range_glow_is_clamped() {
        let t = Theme::from_base((0, 255, 65), false);
        assert_eq!(t.color(5.0, false, 0.0), t.color(1.0, false, 0.0));
        assert_eq!(t.color(-5.0, false, 0.0), t.color(0.0, false, 0.0));
        assert_eq!(t.color(f32::NAN, false, 0.0), t.color(0.0, false, 0.0));
    }

    #[test]
    fn rainbow_hue_rotates_and_wraps() {
        let t = Theme::from_base((0, 255, 65), true);
        assert_ne!(t.color(1.0, false, 0.0), t.color(1.0, false, 0.33));
        // Negative and >1 shifts must wrap rather than saturate.
        assert_eq!(t.color(1.0, false, 0.25), t.color(1.0, false, 1.25));
        assert_eq!(t.color(1.0, false, 0.25), t.color(1.0, false, -0.75));
    }

    #[test]
    fn depth_detection_table() {
        assert_eq!(
            Depth::from_env(Some("truecolor"), Some("xterm")),
            Depth::True
        );
        assert_eq!(Depth::from_env(Some("24bit"), Some("xterm")), Depth::True);
        assert_eq!(
            Depth::from_env(None, Some("xterm-256color")),
            Depth::Ansi256
        );
        assert_eq!(Depth::from_env(None, Some("xterm")), Depth::Ansi256);
        assert_eq!(Depth::from_env(None, Some("dumb")), Depth::Ansi16);
        assert_eq!(Depth::from_env(None, None), Depth::Ansi16);
        assert_eq!(
            Depth::from_env(Some("something-else"), Some("")),
            Depth::Ansi16
        );
    }

    #[test]
    fn every_depth_maps_every_color_without_panicking() {
        for depth in [Depth::True, Depth::Ansi256, Depth::Ansi16] {
            for c in [
                (0, 0, 0),
                (255, 255, 255),
                (0, 255, 65),
                (7, 7, 7),
                (250, 250, 250),
            ] {
                let _ = depth.to_color(c);
            }
        }
    }

    #[test]
    fn greyscale_quantisation_stays_in_the_256_ramp() {
        for level in 0..=255u8 {
            let v = rgb_to_256(level, level, level);
            assert!(
                v == 16 || v == 231 || (232..=255).contains(&v),
                "grey {level} -> {v}"
            );
        }
    }

    #[test]
    fn hsv_primaries() {
        assert_eq!(hsv_to_rgb(0.0, 1.0, 1.0), (255, 0, 0));
        assert_eq!(hsv_to_rgb(1.0 / 3.0, 1.0, 1.0), (0, 255, 0));
        assert_eq!(hsv_to_rgb(2.0 / 3.0, 1.0, 1.0), (0, 0, 255));
        assert_eq!(hsv_to_rgb(0.0, 0.0, 1.0), (255, 255, 255));
    }
}
