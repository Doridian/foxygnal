//! The polar-orbiting weather satellites: Meteor-M LRPT and HRPT, Metop
//! AHRPT. Each has a demodulator that owns a PSK front end ([`frontend`])
//! and a deframer, and a decoder that turns the instrument's data into
//! pictures (raster row records, see `foxygnal_core::raster`).

pub mod ahrpt;
pub mod avhrr;
pub mod frontend;
pub mod hrpt;
pub mod jpeg;
pub mod lrpt;
pub mod lrpt_image;
pub mod msumr;
mod strip;

use foxygnal_core::Registry;

pub fn register(r: &mut Registry) {
    lrpt::register(r);
    lrpt_image::register(r);
    ahrpt::register(r);
    avhrr::register(r);
    hrpt::register(r);
    msumr::register(r);
}
