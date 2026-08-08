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

/// Characters that occupy two terminal columns.
///
/// [`crate::render`] tracks the cursor arithmetically — after printing at `x` it
/// records the cursor at `x + 1` and uses that to emit cheap relative moves — so
/// a glyph that advances two columns desynchronises the damage tracker for the
/// rest of the frame. Every glyph we can emit has to be single-column, and this
/// is the gate.
///
/// The ranges are the East Asian Wide and Fullwidth blocks from `wcwidth`'s
/// table, plus the pictographs and the supplementary ideographic planes. This is
/// not a complete width table — that needs Unicode data we deliberately do not
/// depend on — and it errs toward rejecting: a handful of codepoints in the
/// pictograph block are single-column in some terminals and get turned away
/// anyway. Refusing a flag value costs the user one error message; getting the
/// column count wrong silently smears the screen.
#[must_use]
pub fn is_wide(c: char) -> bool {
    matches!(c as u32,
        // Hangul Jamo initial consonants.
        0x1100..=0x115F
        // CJK radicals through Yi: Han, kana, Hangul compatibility jamo, and the
        // CJK punctuation and symbol blocks.
        | 0x2E80..=0xA4CF
        // Hangul syllables.
        | 0xAC00..=0xD7A3
        // CJK compatibility ideographs.
        | 0xF900..=0xFAFF
        // Vertical forms and CJK compatibility forms.
        | 0xFE30..=0xFE6F
        // Fullwidth ASCII and fullwidth punctuation. This stops at U+FF60 on
        // purpose: U+FF61 begins the *halfwidth* forms block, which is where the
        // katakana this program defaults to lives.
        | 0xFF00..=0xFF60
        // Fullwidth currency signs.
        | 0xFFE0..=0xFFE6
        // Emoji and miscellaneous pictographs.
        | 0x1F300..=0x1FAFF
        // Supplementary and tertiary ideographic planes.
        | 0x20000..=0x3FFFD)
}

/// Characters that advance the cursor by nothing at all.
///
/// The mirror of [`is_wide`]: a combining mark attaches to the glyph before it
/// and a zero-width control is invisible, so either one leaves the renderer
/// believing the cursor moved a column further than it did. Same reasoning, same
/// caveat — these are the blocks that matter in practice rather than a complete
/// table.
#[must_use]
pub fn is_zero_width(c: char) -> bool {
    matches!(c as u32,
        // Soft hyphen: invisible unless the line happens to break there.
        0x00AD
        // Combining diacritical marks, and the three later blocks of them.
        | 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20F0
        // Zero-width space, non-joiner, joiner, and the bidi marks.
        | 0x200B..=0x200F
        // Word joiner, invisible operators, and the deprecated format controls.
        | 0x2060..=0x206F
        // Variation selectors, including the emoji presentation selectors.
        | 0xFE00..=0xFE0F
        // Combining half marks.
        | 0xFE20..=0xFE2F
        // Zero-width no-break space, better known as the BOM.
        | 0xFEFF
        // Tag characters and the supplementary variation selectors.
        | 0xE0000..=0xE01EF)
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

    #[test]
    fn every_built_in_charset_is_single_column() {
        // Generalises the katakana check above to every set we ship. A
        // double-width or combining glyph anywhere in a repertoire would shift
        // every cell the renderer drew after it for the rest of the frame.
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
            for c in cs.glyphs("") {
                assert!(!is_wide(c), "{cs:?} contains the double-width glyph {c:?}");
                assert!(
                    !is_zero_width(c),
                    "{cs:?} contains the zero-width glyph {c:?}"
                );
            }
        }
    }

    #[test]
    fn the_width_check_splits_halfwidth_forms_from_fullwidth_ones() {
        // This boundary is the one that would hurt to get wrong: one codepoint
        // out and the check rejects the program's own default charset.
        assert!(is_wide('\u{FF60}'), "U+FF60 is the last fullwidth form");
        assert!(!is_wide('\u{FF61}'), "U+FF61 opens the halfwidth block");
        for c in Charset::Katakana.glyphs("") {
            assert!(!is_wide(c));
        }
        for c in ['日', '한', '　', 'Ａ', '🌧'] {
            assert!(is_wide(c), "{c:?} should be wide");
        }
        for c in ['a', '9', 'ｱ', 'α', '£'] {
            assert!(!is_wide(c), "{c:?} should be single-column");
        }
    }

    #[test]
    fn the_zero_width_check_catches_combining_marks_and_joiners() {
        for c in ['\u{0301}', '\u{200D}', '\u{FE0F}', '\u{FEFF}', '\u{00AD}'] {
            assert!(is_zero_width(c), "U+{:04X} should be zero-width", c as u32);
        }
        for c in ['a', 'ｱ', '!', 'Ω'] {
            assert!(!is_zero_width(c), "{c:?} is a spacing character");
        }
    }
}
