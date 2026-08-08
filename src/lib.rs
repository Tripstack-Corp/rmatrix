//! Digital rain for modern terminals.
//!
//! The crate is split so the simulation can be driven directly by tests without
//! a terminal: [`rain`] owns the model, [`theme`] owns the colour ramp,
//! [`charset`] owns the glyph repertoires, [`render`] is the only module that
//! produces bytes, and [`writer`] is the only module that hands them to a file
//! descriptor. The binary in `main.rs` is a thin CLI wrapper.
//!
//! Randomness is injected via a caller-supplied seed rather than drawn from a
//! thread-local generator, so a given seed replays a bit-identical animation.

pub mod charset;
pub mod rain;
pub mod render;
pub mod theme;
pub mod writer;

pub use charset::Charset;
pub use rain::{Config, Rain};
pub use render::{DEFAULT_COLOR_TOLERANCE, DrawStats, Renderer};
pub use theme::{
    BaseColor, DEFAULT_LEVELS, Depth, Levels, MIN_AUTO_LEVELS, Rgb, Theme, auto_levels,
};
pub use writer::FrameWriter;
