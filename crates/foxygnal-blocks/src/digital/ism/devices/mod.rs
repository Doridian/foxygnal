//! The device table: each entry names a line code, its timing, and a
//! function that finds and checks a message in the sliced rows and writes
//! its fields.
//!
//! Field names follow rtl_433's where they exist (`model`, `id`, `channel`,
//! `battery_ok`, `temperature_C`, `humidity`, `pressure_kPa`, `wind_avg_km_h`,
//! `rain_mm`, `mic`, …), so tools built for its output read ours too.

use super::bits::BitBuffer;
use super::out::Out;
use super::slicer::{Modulation, Timing};

mod acurite;
mod ambient;
mod ert;
mod fineoffset;
mod lacrosse;
mod lacrosse_tx;
mod nexus;
mod oregon;
mod remote;
mod security;
mod tpms;

/// What a decoder function gets: the sliced rows, a scratch buffer for
/// derived bits (Manchester-decoded rows, say), and the output.
pub type DecodeFn = fn(&BitBuffer, &mut BitBuffer, &mut Out);

pub struct Device {
    /// Short name for the `devices` parameter.
    pub key: &'static str,
    /// What it decodes, for the block's help.
    pub label: &'static str,
    pub modulation: Modulation,
    pub timing: Timing,
    pub decode: DecodeFn,
}

/// Timing in microseconds: short, long, sync, gap limit, reset limit,
/// tolerance (0 = unused).
pub const fn timing(
    short: f32,
    long: f32,
    sync: f32,
    gap_limit: f32,
    reset: f32,
    tolerance: f32,
) -> Timing {
    Timing {
        short,
        long,
        sync,
        gap_limit,
        reset,
        tolerance,
    }
}

/// Every device, in the order they are tried.
pub fn all() -> &'static [Device] {
    DEVICES
}

static DEVICES: &[Device] = &[
    acurite::TXR,
    acurite::T606,
    acurite::T609,
    acurite::T986,
    ambient::F007TH,
    ambient::WH31E,
    lacrosse::TX141,
    lacrosse::TX29,
    lacrosse::TX35,
    lacrosse_tx::LACROSSE_TX,
    fineoffset::WH24,
    fineoffset::WH1080,
    oregon::OREGON,
    nexus::NEXUS,
    nexus::PROLOGUE,
    nexus::RUBICSON,
    tpms::SCHRADER,
    tpms::TOYOTA,
    tpms::FORD,
    tpms::CITROEN,
    security::HONEYWELL,
    security::INTERLOGIX,
    remote::EV1527,
    remote::PT2262,
    ert::SCM,
    ert::SCMPLUS,
    ert::IDM,
];

/// Temperature from tenths of a degree Fahrenheit offset by `offset`.
pub fn f_to_c(f: f64) -> f64 {
    (f - 32.0) * 5.0 / 9.0
}
