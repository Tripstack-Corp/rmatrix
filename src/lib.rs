//! Digital rain for modern terminals.
//!
//! The crate is split so the simulation can be driven directly by tests without
//! a terminal: [`rain`] owns the model, [`theme`] owns the colour ramp,
//! [`charset`] owns the glyph repertoires, and [`render`] is the only module
//! that writes bytes. The binary in `main.rs` is a thin CLI wrapper.
//!
//! Randomness is injected via a caller-supplied seed rather than drawn from a
//! thread-local generator, so a given seed replays a bit-identical animation.

pub mod charset;
pub mod rain;
pub mod render;
pub mod theme;

pub use charset::Charset;
pub use rain::{Config, Rain};
pub use render::Renderer;
pub use theme::{BaseColor, Depth, Rgb, Theme};
