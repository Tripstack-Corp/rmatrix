//! Proof that coalesced frames cannot corrupt the display.
//!
//! Under back-pressure the loop stops *drawing* rather than discarding drawn
//! bytes, so several ticks' worth of change arrive as one frame. That is only
//! safe if [`Renderer`]'s `prev` buffer keeps describing the terminal's real
//! state, and `prev` is private, so these tests check the thing that actually
//! matters instead: they replay the emitted byte stream through a model
//! terminal and compare its screen against the simulation.
//!
//! There is a negative control too. It applies the *other* back-pressure policy
//! — throwing away a frame the renderer already emitted — and shows the screen
//! going permanently wrong, which is both the hazard being avoided and proof
//! that these assertions have teeth.

use rmatrix::{Config, Depth, Rain, Renderer, Rgb, Theme};

/// The subset of a terminal the renderer actually drives: absolute and relative
/// cursor moves, a foreground pen, printable glyphs, backspace and line feed.
///
/// Line wrap is off and raw mode clears `OPOST`, so a glyph in the last column
/// leaves the cursor put and `\n` is a bare feed with no carriage return.
struct Term {
    w: usize,
    h: usize,
    cells: Vec<(char, Option<Rgb>)>,
    x: usize,
    y: usize,
    pen: Option<Rgb>,
}

impl Term {
    fn new(w: usize, h: usize) -> Term {
        Term {
            w,
            h,
            cells: vec![(' ', None); w * h],
            x: 0,
            y: 0,
            pen: None,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        let s = std::str::from_utf8(bytes).expect("the renderer emits valid UTF-8");
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            match c {
                '\x1b' => {
                    assert_eq!(it.next(), Some('['), "only CSI sequences are emitted");
                    let mut params = String::new();
                    let final_byte = loop {
                        let p = it.next().expect("truncated CSI sequence");
                        if p.is_ascii_alphabetic() {
                            break p;
                        }
                        params.push(p);
                    };
                    self.csi(&params, final_byte);
                }
                '\x08' => self.x = self.x.saturating_sub(1),
                '\n' => {
                    self.y += 1;
                    assert!(self.y < self.h, "the renderer scrolled the screen");
                }
                '\r' => self.x = 0,
                ch => {
                    let idx = self.y * self.w + self.x;
                    self.cells[idx] = (ch, self.pen);
                    // Wrap is disabled: the cursor stalls in the last column.
                    self.x = (self.x + 1).min(self.w - 1);
                }
            }
        }
    }

    fn csi(&mut self, params: &str, final_byte: char) {
        let n: Vec<usize> = params
            .split(';')
            .map(|p| p.parse().unwrap_or(1))
            .collect::<Vec<_>>();
        match final_byte {
            'H' => {
                self.y = n.first().copied().unwrap_or(1).saturating_sub(1);
                self.x = n.get(1).copied().unwrap_or(1).saturating_sub(1);
                assert!(
                    self.x < self.w && self.y < self.h,
                    "cursor moved off screen"
                );
            }
            'C' => self.x = (self.x + n.first().copied().unwrap_or(1)).min(self.w - 1),
            'm' => {
                // Only truecolor is exercised here, and only ever as a
                // foreground set; anything else would be a renderer change this
                // model has not been taught about.
                assert_eq!(
                    (n.first().copied(), n.get(1).copied()),
                    (Some(38), Some(2)),
                    "unexpected SGR {params:?}"
                );
                let c = |i: usize| u8::try_from(n.get(i).copied().unwrap_or(0)).unwrap_or(255);
                self.pen = Some((c(2), c(3), c(4)));
            }
            other => panic!("unexpected CSI final byte {other:?} (params {params:?})"),
        }
    }

    /// Every cell must match the simulation. Colour is only meaningful where a
    /// glyph is lit — a blank is a blank whatever pen drew it.
    fn assert_matches(&self, rain: &Rain, theme: &Theme, when: &str) {
        for y in 0..self.h {
            for x in 0..self.w {
                let want = rain.color_of(x as u16, y as u16, theme);
                let got = self.cells[y * self.w + x];
                match want {
                    Some((ch, rgb)) => assert_eq!(
                        got,
                        (ch, Some(rgb)),
                        "{when}: cell ({x},{y}) is stale — screen {got:?}, simulation {:?}",
                        (ch, rgb)
                    ),
                    None => assert_eq!(
                        got.0, ' ',
                        "{when}: cell ({x},{y}) kept the glyph {:?} after going dark",
                        got.0
                    ),
                }
            }
        }
    }

    /// Cells that never changed hands still carry whatever pen was current, so
    /// a raw comparison of two runs would flag a blank drawn in one shade of
    /// green against a blank drawn in another. Only what is visible counts.
    fn visible(&self) -> Vec<(char, Option<Rgb>)> {
        self.cells
            .iter()
            .map(|&(ch, col)| if ch == ' ' { (' ', None) } else { (ch, col) })
            .collect()
    }

    /// Cells whose contents disagree with the simulation.
    fn stale(&self, rain: &Rain, theme: &Theme) -> usize {
        (0..self.h)
            .flat_map(|y| (0..self.w).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let got = self.cells[y * self.w + x];
                match rain.color_of(x as u16, y as u16, theme) {
                    Some((ch, rgb)) => got != (ch, Some(rgb)),
                    None => got.0 != ' ',
                }
            })
            .count()
    }

    fn lit(&self) -> usize {
        self.cells.iter().filter(|c| c.0 != ' ').count()
    }
}

const W: u16 = 60;
const H: u16 = 40;

fn scene(seed: u64) -> (Rain, Theme, Renderer) {
    let rain = Rain::new(
        W,
        H,
        Config {
            seed: Some(seed),
            density: 0.9,
            ..Config::default()
        },
    );
    let mut rr = Renderer::new(W, H);
    // The pen-reuse tolerance deliberately lets a cell's colour drift a shade
    // from the ideal, which would mask nothing but would make an exact
    // comparison meaningless. Turn it off so every colour is checked exactly.
    rr.set_color_tolerance(0);
    (rain, Theme::from_base((0, 255, 65), false), rr)
}

/// `draw_on(i)` decides whether tick `i` gets a frame — i.e. whether the writer
/// happened to be free. Returns the model terminal and total bytes emitted.
fn replay(
    seed: u64,
    frames: usize,
    draw_on: &mut dyn FnMut(usize) -> bool,
    check_each: bool,
) -> (Term, usize) {
    let (mut rain, theme, mut rr) = scene(seed);
    let mut term = Term::new(W as usize, H as usize);
    let mut buf = Vec::new();
    let mut bytes = 0;
    for i in 0..frames {
        rain.step(1.0 / 30.0);
        if !draw_on(i) {
            continue;
        }
        buf.clear();
        rr.draw(&mut buf, &rain, &theme, Depth::True)
            .expect("a Vec cannot fail");
        bytes += buf.len();
        term.feed(&buf);
        if check_each {
            term.assert_matches(&rain, &theme, &format!("after the draw on tick {i}"));
        }
    }
    (term, bytes)
}

#[test]
fn the_screen_matches_the_simulation_after_every_coalesced_frame() {
    // A deterministic but irregular schedule: bursts of back-pressure of every
    // length from 1 to 12 ticks, which is the range a hitching terminal
    // produces at 30fps.
    let mut state = 0x5eedu64;
    let mut gap = 0usize;
    let mut schedule = move |_: usize| -> bool {
        if gap > 0 {
            gap -= 1;
            return false;
        }
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        gap = ((state >> 33) % 12) as usize;
        true
    };
    let (term, _) = replay(3, 600, &mut schedule, true);
    assert!(
        term.lit() > 200,
        "the scene was too empty to prove anything"
    );
}

#[test]
fn coalescing_changes_the_bytes_but_not_the_pixels() {
    // Same simulation, same final tick drawn; the only difference is how many
    // of the intermediate frames reached the terminal.
    let (dense, dense_bytes) = replay(11, 300, &mut |_| true, false);
    // Draw one tick in five, including the last.
    let (sparse, sparse_bytes) = replay(11, 300, &mut |i| i % 5 == 4, false);

    assert_eq!(
        dense.visible(),
        sparse.visible(),
        "a coalesced run left a different screen"
    );
    assert!(
        sparse_bytes * 2 < dense_bytes,
        "coalescing did not save bytes: {sparse_bytes} vs {dense_bytes}"
    );
}

#[test]
fn one_frame_repairs_a_hundred_ticks_of_back_pressure() {
    // The pathological case: the terminal takes nothing for over three seconds,
    // then one frame has to fix the whole screen.
    let (term, _) = replay(17, 400, &mut |i| !(150..250).contains(&i), false);
    let (mut rain, theme, _) = scene(17);
    for _ in 0..400 {
        rain.step(1.0 / 30.0);
    }
    term.assert_matches(&rain, &theme, "after a 100-tick stall");
}

/// The negative control, and the reason `acquire` gates the *draw* rather than
/// the send.
///
/// Here the renderer draws every frame and the transport throws some away, which
/// is the obvious way to implement back-pressure and is wrong: `prev` still
/// believes those cells were painted, so they are never re-emitted.
///
/// Rain is busy enough that the smear usually washes out on its own within a
/// second or two, which is exactly what makes the bug so easy to ship. Pausing
/// takes that safety net away — nothing changes, so nothing is re-emitted, and
/// the corruption is there for as long as the user looks at it.
#[test]
fn discarding_an_already_drawn_frame_corrupts_the_screen() {
    let (mut rain, theme, mut rr) = scene(23);
    let mut term = Term::new(W as usize, H as usize);
    let mut buf = Vec::new();
    let draw = |rr: &mut Renderer, rain: &Rain, buf: &mut Vec<u8>| {
        buf.clear();
        rr.draw(buf, rain, &theme, Depth::True)
            .expect("a Vec cannot fail");
    };

    for i in 0..160 {
        rain.step(1.0 / 30.0);
        draw(&mut rr, &rain, &mut buf);
        if (100..110).contains(&i) {
            continue; // "the queue was full, drop it"
        }
        term.feed(&buf);
    }
    let right_after = term.stale(&rain, &theme);
    assert!(
        right_after > 0,
        "dropping ten emitted frames left the screen intact, so these tests \
         cannot detect corruption and prove nothing"
    );

    // Now the user hits space. Every subsequent frame is empty, so the damage
    // never repairs.
    for _ in 0..120 {
        draw(&mut rr, &rain, &mut buf);
        assert!(buf.is_empty(), "a paused simulation should emit nothing");
        term.feed(&buf);
    }
    assert_eq!(
        term.stale(&rain, &theme),
        right_after,
        "the smear should still be there — that is the whole point"
    );
}

/// The same stall, handled the way this design handles it: don't draw at all
/// while the writer is busy. The renderer's view of the screen stays true, so
/// the recovery frame is exact and pausing afterwards is safe.
#[test]
fn skipping_the_draw_instead_leaves_nothing_to_repair() {
    let (mut rain, theme, mut rr) = scene(23);
    let mut term = Term::new(W as usize, H as usize);
    let mut buf = Vec::new();
    for i in 0..160 {
        rain.step(1.0 / 30.0);
        if (100..110).contains(&i) {
            continue; // the writer was busy, so no draw happened
        }
        buf.clear();
        rr.draw(&mut buf, &rain, &theme, Depth::True)
            .expect("a Vec cannot fail");
        term.feed(&buf);
    }
    assert_eq!(term.stale(&rain, &theme), 0, "the screen went stale");

    for _ in 0..120 {
        buf.clear();
        rr.draw(&mut buf, &rain, &theme, Depth::True)
            .expect("a Vec cannot fail");
        term.feed(&buf);
    }
    assert_eq!(
        term.stale(&rain, &theme),
        0,
        "the screen went stale on pause"
    );
}
