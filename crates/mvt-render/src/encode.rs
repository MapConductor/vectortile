//! PNG encoding for raw RGBA buffers produced outside the renderer.
//!
//! The implementation lives in the `tile-png` crate so the SDK cores can encode
//! tiles without depending on a rasteriser. This module is the name the
//! renderer and its bindings already use for it.

pub use tile_png::{encode_png_premultiplied, encode_png_rgba, EncodeError};
