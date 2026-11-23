//! The standard block library.

pub mod audio;
pub mod convert;
pub mod demod;
mod design;
pub mod file;
pub mod filters;
pub mod flow;
pub mod level;
pub mod math;
mod params;
pub mod resample;
pub mod ring;
pub mod sigmf;
pub mod sources;
pub mod testing;
pub mod wav;

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
    filters::register(r);
    resample::register(r);
    demod::register(r);
    level::register(r);
    wav::register(r);
    sigmf::register(r);
    convert::register(r);
    audio::register(r);
}
