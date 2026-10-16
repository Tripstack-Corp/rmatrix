//! The real binary, in a real terminal emulator.
//!
//! Everything else in tests/ drives the library and replays the bytes through
//! a model terminal. Some behaviour only exists between the program and an
//! actual terminal: the state the terminal is left in after we exit, whether
//! the mouse reaches us, what colours the terminal says it has. These tests
//! run the built `rmatrix` under tui-test, a headless terminal driven from the
//! command line, using its Ghostty backend.
//!
//! They are opt-in, because tui-test is a separate install:
//!
//! ```sh
//! brew tap microsoft/tui-test && brew install tui-test
//! cargo test --test tui -- --ignored
//! ```
//!
//! Each test runs in a session of its own, named after the test and this
//! process, and closes only that session. Never `tui-test close --all`: it
//! closes every session on the machine, including other people's runs.
//!
//! Two things tui-test cannot show, so nothing here tries:
//!
//! - `mouse move` sends a mouse report whether or not the program asked for
//!   one, so stray reports reaching the shell after exit are invisible here.
//!   The emulator's own mode state is honest, so teardown is checked through
//!   that instead.
//! - It answers OSC 4 and DA1 at once. A slow or silent terminal needs a pty
//!   that answers on a schedule; see `src/bin/rmatrix/palette.rs`.

// The crate denies these; clippy's `allow-expect-in-tests` only reaches
// `#[cfg(test)]` modules, and an integration test is its own crate. The rule
// they enforce is about production code paths, and none of this is one.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use rmatrix::{DEFAULT_TERMINAL_PALETTE, Rgb, Theme};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::thread::sleep;
use std::time::Duration;

/// Long enough to be past the screensaver's 250 ms startup grace.
const PAST_GRACE: Duration = Duration::from_millis(500);

/// One tui-test session running one program, closed when dropped.
struct Session {
    name: String,
    config: Option<PathBuf>,
}

impl Session {
    /// Start `rmatrix` with `args` in a 60x20 Ghostty terminal.
    fn rmatrix(test: &str, args: &[&str]) -> Session {
        Session::start(test, None, env!("CARGO_BIN_EXE_rmatrix"), args)
    }

    /// Like [`Session::rmatrix`], under a profile from a tui-test config file.
    fn rmatrix_with_profile(test: &str, toml: &str, args: &[&str]) -> Session {
        let config =
            std::env::temp_dir().join(format!("rmatrix-tui-{test}-{}.toml", std::process::id()));
        std::fs::write(&config, toml).expect("writing the tui-test config");
        Session::start(test, Some(config), env!("CARGO_BIN_EXE_rmatrix"), args)
    }

    fn start(test: &str, config: Option<PathBuf>, program: &str, args: &[&str]) -> Session {
        let s = Session {
            name: format!("rmatrix-{test}-{}", std::process::id()),
            config,
        };
        let mut run = vec![
            "run",
            "--backend",
            "ghostty",
            "--cols",
            "60",
            "--rows",
            "20",
        ];
        if let Some(config) = &s.config {
            run.extend(["--config", config.to_str().expect("utf-8 temp dir")]);
            run.extend(["--profile", "rmatrix"]);
        }
        run.push(program);
        run.extend(args);
        s.ok(&run);
        s
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new("tui-test")
            .arg("--session")
            .arg(&self.name)
            .args(args)
            .output()
            .expect("running tui-test; is it installed?")
    }

    /// Run a tui-test command that must succeed.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.cli(args);
        assert!(
            out.status.success(),
            "tui-test {args:?} failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Whether a tui-test assertion holds. Exit status 1 is "no"; anything
    /// else that is not success is the harness failing, not the assertion.
    fn holds(&self, args: &[&str]) -> bool {
        let out = self.cli(args);
        match out.status.code() {
            Some(0) => true,
            Some(1) => false,
            _ => panic!(
                "tui-test {args:?} errored: {}",
                String::from_utf8_lossy(&out.stderr)
            ),
        }
    }

    /// Block until rmatrix is up and has taken over the screen.
    fn wait_until_drawing(&self) {
        self.ok(&["expect", "mode", "alternate_screen", "--timeout", "5000"]);
    }

    fn exits_within(&self, ms: u32) -> bool {
        self.holds(&["wait", "exit", "--timeout", &ms.to_string()])
    }

    /// The emulator's mouse reporting level: none, click, drag or motion.
    fn mouse_mode(&self) -> String {
        let state = self.ok(&["--json", "state"]);
        let key = "\"mouse_mode\":\"";
        let at = state.find(key).expect("state reports mouse_mode") + key.len();
        state[at..].split('"').next().unwrap_or_default().to_owned()
    }

    /// The terminal is back the way a shell expects it: main screen, cursor
    /// shown, no mouse reporting, and rmatrix exited cleanly.
    fn assert_handed_back(&self) {
        assert!(self.holds(&["expect", "mode", "alternate_screen", "--off"]));
        assert!(self.holds(&["expect", "cursor", "--visible"]));
        assert_eq!(self.mouse_mode(), "none", "mouse reporting left on");
        assert!(
            self.ok(&["--json", "state"]).contains("\"exited\":0"),
            "rmatrix did not exit cleanly"
        );
    }

    /// Whether some glyph on screen is drawn in `rgb`, waiting up to `ms`.
    fn shows_colour(&self, rgb: Rgb, ms: u32) -> bool {
        let (r, g, b) = rgb;
        self.holds(&[
            "expect",
            "text",
            r"\S",
            "--regex",
            "--match",
            "any",
            "--fg",
            &format!("#{r:02x}{g:02x}{b:02x}"),
            "--timeout",
            &ms.to_string(),
        ])
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cli(&["close"]);
        if let Some(config) = &self.config {
            let _ = std::fs::remove_file(config);
        }
    }
}

#[test]
#[ignore = "needs tui-test: brew tap microsoft/tui-test && brew install tui-test"]
fn quitting_hands_the_terminal_back() {
    let s = Session::rmatrix("quit", &[]);
    s.wait_until_drawing();
    s.ok(&["key", "press", "q"]);
    assert!(s.exits_within(3000), "q did not quit");
    s.assert_handed_back();
}

#[test]
#[ignore = "needs tui-test: brew tap microsoft/tui-test && brew install tui-test"]
fn normal_mode_leaves_the_mouse_to_the_terminal() {
    // Capturing the mouse takes text selection away from the user, which is
    // only worth it when any input is meant to end the program.
    let s = Session::rmatrix("mouse-free", &[]);
    s.wait_until_drawing();
    assert_eq!(s.mouse_mode(), "none");
}

#[test]
#[ignore = "needs tui-test: brew tap microsoft/tui-test && brew install tui-test"]
fn the_screensaver_ends_on_a_keypress() {
    let s = Session::rmatrix("ss-key", &["-s"]);
    s.wait_until_drawing();
    sleep(PAST_GRACE);
    s.ok(&["key", "press", "a"]);
    assert!(
        s.exits_within(3000),
        "a keypress did not end the screensaver"
    );
    s.assert_handed_back();
}

#[test]
#[ignore = "needs tui-test: brew tap microsoft/tui-test && brew install tui-test"]
fn the_screensaver_ends_when_the_mouse_moves_and_gives_the_mouse_back() {
    // Plain motion with no button held, which needs any-motion tracking
    // (?1003). Leaving any mouse mode on after exit would have every twitch
    // type escape codes into the shell until the user ran `reset`.
    let s = Session::rmatrix("ss-mouse", &["-s"]);
    s.wait_until_drawing();
    assert_eq!(s.mouse_mode(), "motion");
    sleep(PAST_GRACE);
    s.ok(&["mouse", "move", "10", "5"]);
    assert!(s.exits_within(3000), "moving the mouse did not end it");
    s.assert_handed_back();
}

#[test]
#[ignore = "needs tui-test: brew tap microsoft/tui-test && brew install tui-test"]
fn a_key_typed_just_before_launch_does_not_end_the_screensaver() {
    // The key sits in the tty's input queue while the shell is still starting
    // rmatrix, and is read on the first poll. Without the startup grace that
    // closed the screensaver before it drew a frame; a build without the grace
    // fails this at `wait_until_drawing`, never having been seen on screen.
    let s = Session::start(
        "ss-grace",
        None,
        "/bin/sh",
        &[
            "-c",
            "sleep 0.5; exec \"$0\" -s",
            env!("CARGO_BIN_EXE_rmatrix"),
        ],
    );
    s.ok(&["key", "press", "a"]);
    s.wait_until_drawing();
    assert!(!s.exits_within(600), "the queued key ended the screensaver");
    s.ok(&["key", "press", "a"]);
    assert!(s.exits_within(3000), "a key after the grace did not end it");
}

#[test]
#[ignore = "needs tui-test: brew tap microsoft/tui-test && brew install tui-test"]
fn terminal_colour_mode_draws_in_the_terminals_own_palette() {
    // A palette nobody ships, so a match cannot be a coincidence. Every head
    // is drawn in its column's colour tinted toward white, and each column
    // takes one of the six, so all six tints turn up within a few seconds.
    let palette: [Rgb; 6] = [
        (0xff, 0x5f, 0x00),
        (0xd7, 0xff, 0x00),
        (0x00, 0xff, 0xaf),
        (0x00, 0xd7, 0xff),
        (0x5f, 0x5f, 0xff),
        (0xff, 0x00, 0xd7),
    ];
    let hex = |(r, g, b): Rgb| format!("\"#{r:02x}{g:02x}{b:02x}\"");
    let toml = format!(
        "[profiles.rmatrix.colors]\nred = {}\nyellow = {}\ngreen = {}\ncyan = {}\nblue = {}\nmagenta = {}\n",
        hex(palette[0]),
        hex(palette[1]),
        hex(palette[2]),
        hex(palette[3]),
        hex(palette[4]),
        hex(palette[5]),
    );
    let head = |base: Rgb| Theme::from_base(base, false).head;

    let s = Session::rmatrix_with_profile(
        "palette",
        &toml,
        &["-C", "terminal", "--color-depth", "truecolor"],
    );
    s.wait_until_drawing();
    for base in palette {
        assert!(
            s.shows_colour(head(base), 5000),
            "no head drawn in the terminal's {base:?}"
        );
    }
    for base in DEFAULT_TERMINAL_PALETTE {
        assert!(
            !s.shows_colour(head(base), 0),
            "fell back to the built-in {base:?} although the terminal answered"
        );
    }
}
