# rmatrix

Digital rain for modern terminals. Written in Rust, no ncurses.

```sh
rmatrix
```

## Why another one

[cmatrix](https://github.com/abishekvashok/cmatrix) is great and this owes it the
idea. But it was written against a 1999 terminal: it has three brightness levels
(white head / bold / normal), so its trails step rather than fade, and its
"original Matrix font" modes (`-l`, `-x`) depend on Linux console fonts or an X11
bitmap font and simply cannot work on macOS.

rmatrix targets what terminals can actually do now:

| | cmatrix | rmatrix |
|---|---|---|
| Trail shading | 3 brightness steps | continuous 24-bit ramp (~170 levels of green in a typical frame) |
| Glyphs | ASCII, or katakana via `-c` | halfwidth katakana by default, 8 built-in sets, `--custom` |
| Motion | fixed tick | rows/second, integrated against real time — same speed at any frame rate |
| Redraw | full screen | damage-tracked; an unchanged frame emits zero bytes |
| Terminal layer | ncurses | crossterm — no system library to install |
| Colour | 8 ANSI colours | any `#RRGGBB`, with automatic truecolor/256/16 fallback |
| Reproducibility | — | `--seed` replays an identical animation |

## Install

```sh
cargo install --git https://github.com/Tripstack-Corp/rmatrix
```

> Note: don't `cargo install rmatrix` from crates.io — that name belongs to an
> unrelated project.

Or from a clone:

```sh
cargo build --release && cp target/release/rmatrix ~/.local/bin/
```

## Usage

```
rmatrix [OPTIONS]

  -C, --color <COLOR>      Colour name, #RRGGBB, or "rainbow"  [default: green]
  -c, --charset <SET>      classic | katakana | ascii | alnum | binary | hex |
                           greek | symbols | custom            [default: classic]
      --custom <GLYPHS>    Glyphs to use with `--charset custom`
  -S, --speed <MUL>        Overall speed multiplier            [default: 1]
  -d, --density <0..1>     Fraction of columns raining         [default: 0.55]
  -m, --mutate <RATE>      Glyph churn, screens/sec; 0 disables[default: 0.35]
      --tail-min <ROWS>    Shortest trail                      [default: 6]
      --tail-max <ROWS>    Longest trail                       [default: 26]
      --fps <N>            Frame rate cap                      [default: 30]
      --levels <N>         Brightness steps; 0 = unquantised   [default: 24]
      --stats              Start with the stats overlay shown
  -b, --bold               Bold glyphs
  -s, --screensaver        Exit on any keypress
      --seed <N>           Replay a specific animation
      --color-depth <D>    auto | truecolor | 256 | 16         [default: auto]
```

Keys while running: `q`/`Esc`/`Ctrl-C` quit, `space` pause, `1`–`9` speed,
`r` rainbow, `c` cycle charset, `b` bold, `f` stats overlay.

Some combinations worth knowing:

```sh
rmatrix -C '#00ff41' --tail-max 40 -d 0.8   # dense, long, film-green
rmatrix -c binary -C cyan                   # ones and zeroes
rmatrix -s --fps 30                         # screensaver, easy on the battery
rmatrix --seed 1337                         # same rain every time
```

## Performance

Press `f` for a live readout: frame rate, bytes per frame, output rate, and the
percentage of cells repainted. It appears in two places — a bar across the top
row, **and the window title**.

The title is not redundant. If you pair `-c ascii` with a font that remaps ASCII
to glyphs (see [Fonts](#fonts)), anything drawn into the terminal grid is remapped
too, so the on-screen bar comes out as Matrix glyphs. The title is rendered by
the OS in the UI font, so it stays readable whatever the terminal font is doing.

The thing to understand about a full-screen terminal animation is that **you are
not the expensive process — the terminal is.** Measured on an M4 Pro at 200×50,
rmatrix's own simulation costs ~18 µs/frame; the terminal emulator meanwhile has
to parse and re-render every escape sequence, and that showed up as 87% CPU in
iTerm2 against 8% for rmatrix.

So the tuning knobs are all about emitting fewer bytes:

| Setting | Effect |
|---|---|
| `--levels <N>` | Brightness steps. A cell only repaints when it crosses a step, so this is the biggest lever. `8` is ~4.7× less output than unquantised, and still nearly 3× cmatrix's three levels. |
| `--fps <N>` | Output scales linearly. |
| `-d`, `--tail-max` | Fewer/shorter trails means fewer lit cells. |

Measured at 200×50, 600 frames per row:

| `--levels` | bytes/frame | at 30 fps | cells repainted | vs unquantised |
|---|---|---|---|---|
| none | 33,071 | 0.99 MB/s | 15.7% | 1.00× |
| 32 | 16,741 | 0.50 MB/s | 7.9% | 1.98× |
| **24** (default) | **14,329** | **0.43 MB/s** | **6.8%** | **2.31×** |
| 16 | 11,164 | 0.33 MB/s | 5.4% | 2.96× |
| 8 | 6,989 | 0.21 MB/s | 3.5% | 4.73× |

If your terminal still struggles, `--levels 8 --fps 24` cuts output roughly 8×
against the unquantised 60 fps original and is hard to tell apart in motion.

Reproduce any of this with `cargo run --release --example perf`, which also
breaks the output down by escape-sequence type.

## Fonts

The default `classic` charset emits halfwidth katakana (U+FF66–FF9D). Your
terminal font needs coverage for those, or it will substitute another font —
still fine, just not the film's glyphs. macOS falls back to Hiragino Sans
automatically.

For the actual mirrored glyphs from the movie, install the free
[Matrix Code NFI](https://www.dafont.com/matrix-code-nfi.font) font, set it as
your terminal's font, and run:

```sh
rmatrix -c ascii
```

That font is Basic Latin only — it maps ASCII to Matrix glyphs and has no
katakana — which is why `-c ascii` is the right pairing. `rmatrix`'s ASCII set is
`0x21..=0x7A`, entirely within the font's coverage, so no glyph falls back.

## Bind it to a hotkey (iTerm2)

iTerm2 reads *dynamic profiles* from a folder and picks up changes live — no
restart, and nothing in your existing preferences is touched. Drop this in
`~/Library/Application Support/iTerm2/DynamicProfiles/matrix.json`:

```json
{
  "Profiles": [
    {
      "Name": "Matrix",
      "Guid": "pick-any-stable-unique-string",

      "Custom Command": "Yes",
      "Command": "/absolute/path/to/rmatrix --tail-max 40 -d 0.75",

      "Has Hotkey": true,
      "HotKey Key Code": 46,
      "HotKey Modifier Flags": 1835008,
      "HotKey Window Reopens On Activation": true,
      "HotKey Window AutoHides": true,

      "Background Color": {
        "Color Space": "sRGB",
        "Red Component": 0.0, "Green Component": 0.0, "Blue Component": 0.0
      },
      "Minimum Contrast": 0,
      "Scrollback Lines": 0,
      "Silence Bell": true,
      "Close Sessions On End": true
    }
  ]
}
```

That binds **⌃⌥⌘M** to a drop-down window running rmatrix; press it again to
hide. `q` quits, which closes the session.

To choose a different key: `HotKey Key Code` is the macOS virtual key code
(`M` is 46, `Space` is 49, `J` is 38), and `HotKey Modifier Flags` is the sum of
shift `131072`, control `262144`, option `524288`, command `1048576` — so
⌃⌥⌘ is `1835008`. The key names above are the ones iTerm2 actually reads; note
it is `Has Hotkey`, not "Has Hotkey Window".

### The other kind of shortcut

iTerm2 has a second, unrelated binding — the per-profile `Shortcut`, which is
what fills the *Shortcut* column in the Profiles window:

```json
"Shortcut": "R"
```

That gives **⌃⌘R** to open the profile in a new tab, and **⌃⌥⌘R** to open it in
a new window. Two things worth knowing:

- The base modifier is control-command, *not* option-command.
- A single `Shortcut` claims **both** chords, because option is what switches it
  from tab to window. So `"Shortcut": "M"` alongside a ⌃⌥⌘M hotkey window is a
  silent conflict — pick letters that don't overlap.

The difference in practice: `Has Hotkey` is global and toggles, and works when
iTerm2 isn't focused; `Shortcut` only fires when iTerm2 is frontmost and always
opens something new.

Pair it with the movie font by using an absolute path to the binary, setting
`"Normal Font": "MatrixCodeNFI 14"`, and adding `-c ascii` to the command (see
[Fonts](#fonts) for why).

Delete the JSON file to remove the profile and its hotkey.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

All three must pass; CI runs them on Linux and macOS. The simulation is seeded by
the caller rather than from a thread-local RNG, so tests pin every input and
assert on exact frames — see `same_seed_replays_identically`.

Layout: `rain.rs` is the model (glow-decay grid, no rendering), `theme.rs` the
colour ramp, `charset.rs` the glyph sets, `render.rs` the only module that writes
bytes. `main.rs` is a thin CLI and terminal-state wrapper, so everything else is
testable without a tty.

## License

MIT — see [LICENSE](LICENSE).

Not affiliated with "The Matrix" or Warner Bros. Just fans.
