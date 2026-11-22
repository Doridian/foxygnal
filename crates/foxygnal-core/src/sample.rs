//! Stream item types that can flow along a connection.

use serde::{Deserialize, Serialize};

pub use num_complex::Complex32;

/// The type of items carried by a stream. Every port has exactly one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamType {
    /// 32-bit float (real signal).
    F32,
    /// Complex of two 32-bit floats (I/Q).
    C32,
    /// Signed 16-bit integer (e.g. audio, raw SDR samples).
    I16,
    /// Unsigned byte (bits, packed data, raw RTL-SDR samples).
    U8,
}

impl StreamType {
    /// Size of a single item in bytes.
    pub const fn item_size(self) -> usize {
        match self {
            Self::F32 => 4,
            Self::C32 => 8,
            Self::I16 => 2,
            Self::U8 => 1,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::C32 => "c32",
            Self::I16 => "i16",
            Self::U8 => "u8",
        }
    }
}

impl std::fmt::Display for StreamType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Rust types that correspond to a [`StreamType`].
pub trait Sample: bytemuck::Pod + Send + Sync + 'static {
    const TYPE: StreamType;
}

impl Sample for f32 {
    const TYPE: StreamType = StreamType::F32;
}
impl Sample for Complex32 {
    const TYPE: StreamType = StreamType::C32;
}
impl Sample for i16 {
    const TYPE: StreamType = StreamType::I16;
}
impl Sample for u8 {
    const TYPE: StreamType = StreamType::U8;
}
