//! The standard block library.

pub mod file;
pub mod flow;
pub mod math;
mod params;
pub mod sources;
pub mod testing;

use foxygnal_core::Registry;

/// A registry pre-filled with every standard block.
pub fn registry() -> Registry {
    let mut r = Registry::new();
    register_all(&mut r);
    r
}

pub fn register_all(r: &mut Registry) {
    sources::register(r);
    math::register(r);
    flow::register(r);
    file::register(r);
}
