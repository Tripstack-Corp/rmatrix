//! Glyph repertoires for the rain.

use clap::ValueEnum;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Charset {
    /// Halfwidth katakana (U+FF66-FF9D) — the movie's glyph set.
    Katakana,
    /// Katakana mixed with digits, as it actually appears on screen in the film.
    Classic,
    /// Printable ASCII, for terminals without CJK glyph coverage.
    Ascii,
    /// Letters and digits only.
    Alnum,
    Binary,
    Hex,
    Greek,
    Symbols,
    /// Whatever you pass to --custom.
    Custom,
}

impl Charset {
    #[must_use]
    pub fn glyphs(self, custom: &str) -> Vec<char> {
        match self {
            Charset::Katakana => katakana(),
            Charset::Classic => {
                let mut v = katakana();
                // The film's columns are mostly kana with numerals sprinkled in.
                v.extend('0'..='9');
                v
            }
            // 0x21..0x7A matches cmatrix's ASCII range, so the Matrix Code NFI
            // font (Basic Latin only) covers every glyph we can emit.
            Charset::Ascii => (0x21u8..=0x7A).map(|b| b as char).collect(),
            Charset::Alnum => ('0'..='9').chain('A'..='Z').chain('a'..='z').collect(),
            Charset::Binary => vec!['0', '1'],
            Charset::Hex => ('0'..='9').chain('A'..='F').collect(),
            Charset::Greek => ('\u{0391}'..='\u{03C9}')
                .filter(|c| *c != '\u{03A2}')
                .collect(),
            Charset::Symbols => "!@#$%^&*()[]{}<>/\\|=+-_~;:,.?".chars().collect(),
            Charset::Custom => {
                let v: Vec<char> = custom.chars().collect();
                if v.is_empty() { katakana() } else { v }
            }
        }
    }
}

fn katakana() -> Vec<char> {
    ('\u{FF66}'..='\u{FF9D}').collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn katakana_is_the_halfwidth_block() {
        let g = Charset::Katakana.glyphs("");
        assert_eq!(g.len(), 56);
        assert!(g.iter().all(|c| ('\u{FF66}'..='\u{FF9D}').contains(c)));
        // Halfwidth: every glyph must occupy exactly one terminal column, or the
        // damage-tracked renderer's cursor arithmetic goes wrong.
        assert!(g.iter().all(|c| !is_wide(*c)));
    }

    /// East Asian Wide/Fullwidth ranges we could plausibly stray into.
    fn is_wide(c: char) -> bool {
        matches!(c as u32,
            0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF | 0xFE30..=0xFE6F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6)
    }

    #[test]
    fn classic_is_katakana_plus_digits() {
        let g = Charset::Classic.glyphs("");
        assert_eq!(g.len(), 66);
        assert!(('0'..='9').all(|d| g.contains(&d)));
    }

    #[test]
    fn ascii_matches_the_matrix_code_nfi_coverage() {
        // The bundled-font story depends on this: Matrix Code NFI is Basic
        // Latin only, and cmatrix's own ASCII range is 0x21..=0x7A.
        let g = Charset::Ascii.glyphs("");
        assert_eq!(*g.first().expect("non-empty"), '!');
        assert_eq!(*g.last().expect("non-empty"), 'z');
        assert!(g.iter().all(|c| c.is_ascii_graphic()));
    }

    #[test]
    fn greek_skips_the_unassigned_codepoint() {
        let g = Charset::Greek.glyphs("");
        assert!(
            !g.contains(&'\u{03A2}'),
            "U+03A2 is unassigned and renders as tofu"
        );
        assert!(g.contains(&'\u{03A9}') && g.contains(&'\u{03B1}'));
    }

    #[test]
    fn custom_is_used_verbatim() {
        assert_eq!(Charset::Custom.glyphs("abc"), vec!['a', 'b', 'c']);
        assert_eq!(Charset::Custom.glyphs("ｱ0"), vec!['ｱ', '0']);
    }

    #[test]
    fn empty_custom_falls_back_rather_than_producing_a_blank_screen() {
        assert_eq!(Charset::Custom.glyphs(""), Charset::Katakana.glyphs(""));
    }

    #[test]
    fn no_charset_is_empty_or_contains_control_characters() {
        for cs in [
            Charset::Katakana,
            Charset::Classic,
            Charset::Ascii,
            Charset::Alnum,
            Charset::Binary,
            Charset::Hex,
            Charset::Greek,
            Charset::Symbols,
        ] {
            let g = cs.glyphs("");
            assert!(!g.is_empty(), "{cs:?} is empty");
            assert!(
                !g.iter().any(|c| c.is_control()),
                "{cs:?} has a control char"
            );
            assert!(!g.contains(&' '), "{cs:?} has a space");
        }
    }
}
