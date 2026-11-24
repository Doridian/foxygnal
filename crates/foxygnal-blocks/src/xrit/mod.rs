//! LRIT / HRIT ("xRIT"): the file layer on top of CCSDS packets that GOES,
//! GK-2A and Elektro-L use to send pictures, bulletins and other files.
//!
//! The layers, bottom up (the CCSDS layer below is [`crate::ccsds`]):
//!
//! - [`transport`]: files from space packets (per virtual channel and APID,
//!   CRC-checked), Rice-coded pictures decompressed line by line ([`rice`]).
//! - [`header`]: the header records at the start of every file.
//! - [`products`]: what a file is (satellite, region, channel, time) and
//!   names for it.
//! - [`picture`]: segments into pictures, sent one at a time as row records.
//! - [`emwin`]: NWS bulletins (EMWIN): names, headings, messages, zip.
//! - [`decoder`]: what to do with each file.
//! - [`synth`]: a made-up HRIT broadcast for loopback.
//!
//! Blocks: `xrit_decoder` and `xrit_source`.

mod blocks;
pub mod decoder;
pub mod emwin;
pub mod header;
pub mod picture;
pub mod products;
pub mod rice;
pub mod synth;
pub mod transport;
#[cfg(test)]
mod vectors;

use foxygnal_core::Registry;

pub fn register(r: &mut Registry) {
    blocks::register(r);
}
