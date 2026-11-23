//! The standard block library.

pub mod audio;
pub mod broadcast;
pub mod browser_audio;
pub mod convert;
pub mod demod;
mod design;
pub mod digital;
pub mod dvb;
pub mod file;
pub mod filters;
pub mod flow;
pub mod iir;
pub mod image;
pub mod level;
pub mod math;
pub mod measure;
pub mod net;
mod params;
pub mod resample;
pub mod ring;
pub mod sdr;
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
    sources::register_const(r);
    math::register(r);
    measure::register(r);
    flow::register(r);
    file::register(r);
    filters::register(r);
    iir::register(r);
    resample::register(r);
    demod::register(r);
    level::register(r);
    wav::register(r);
    sigmf::register(r);
    convert::register(r);
    audio::register(r);
    browser_audio::register(r);
    sdr::register(r);
    digital::register(r);
    net::register(r);
    broadcast::register(r);
    image::register(r);
    dvb::register(r);
}
