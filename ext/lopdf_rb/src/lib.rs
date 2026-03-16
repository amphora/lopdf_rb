//! Ruby bindings for the lopdf PDF library.
//!
//! This crate exposes one Ruby class under the `LopdfRb` module:
//!
//! - `LopdfRb::Document` — load, inspect, and save PDF documents. Supports
//!   loading from file or binary string, page counting, page dimension queries,
//!   and serialization back to file or binary string.
//!
//! The crate uses `magnus`/`rb-sys` for Ruby-Rust FFI and wraps the `lopdf`
//! crate for all PDF operations.

mod document;

use magnus::{define_module, Error};

/// Magnus init entry point — called when Ruby loads the native extension.
///
/// Defines the top-level `LopdfRb` module and registers the `Document` class
/// with its methods.
#[magnus::init]
fn init() -> Result<(), Error> {
    let module = define_module("LopdfRb")?;
    document::init(module)?;
    Ok(())
}
