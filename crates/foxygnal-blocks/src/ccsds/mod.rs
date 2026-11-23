//! The CCSDS space link layers that weather satellites share (GOES HRIT/LRIT,
//! Meteor-M LRPT, Metop AHRPT, …): CCSDS 131.0-B (TM synchronization and
//! channel coding), 732.0-B (AOS space data link) and 133.0-B (space
//! packets).
//!
//! The pieces know nothing about blocks, so a demodulator can embed them:
//!
//! - [`sync`]: phase symmetries, the sync pattern (plain or convolutionally
//!   encoded ASM), and the search for it in soft symbols
//! - [`randomizer`]: the CCSDS pseudo-randomizer
//! - [`conv`]: the r = 1/2, K = 7 code with G2 inverted, on DVB's encoder
//!   and Viterbi decoder
//! - [`rs`]: RS(255, 223) with the dual basis, interleaving and virtual fill,
//!   on DVB's generic Reed-Solomon code
//! - [`deframer`]: soft symbols → frames ([`Deframer`]), with statistics
//! - [`encoder`]: frames → symbols ([`Encoder`]), for loopback and tests
//! - [`vcdu`]: VCDU and space packet headers, packet reassembly
//!   ([`PacketAssembler`]) and the transmit side ([`FrameBuilder`])
//! - [`wire`]: how frames (CADUs) and packets (records) travel on `u8` wires
//!
//! The blocks (`blocks.rs`): `ccsds_deframer` (c32 symbols → CADUs + status
//! text), `ccsds_packets` (CADUs → packets as text lines or records), and for
//! the transmit side `ccsds_framer` (bytes → CADUs) and `ccsds_modulator`
//! (CADUs → symbols).

mod blocks;
pub mod conv;
pub mod deframer;
pub mod encoder;
pub mod randomizer;
pub mod rs;
pub mod sync;
pub mod vcdu;
pub mod wire;

#[cfg(test)]
mod vectors;

use foxygnal_core::Registry;

pub use blocks::{DeframerBlock, FramerBlock, ModulatorBlock, PacketsBlock};
pub use deframer::{Config, Deframer, Frame, Stats};
pub use encoder::Encoder;
pub use sync::{Modulation, Symmetry};
pub use vcdu::{FrameBuilder, Layout, Packet, PacketAssembler};

/// The attached sync marker.
pub const ASM: u32 = 0x1ACF_FC1D;

pub fn register(r: &mut Registry) {
    blocks::register(r);
}
