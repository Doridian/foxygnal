//! Digital video broadcasting: the DVB FEC chain and the DVB-S modem.
//!
//! The FEC chain (EN 300 421, shared with DVB-T's EN 300 744 and, outer part,
//! DVB-C) is split into reusable pieces that know nothing about blocks:
//!
//! - [`scramble`]: energy dispersal (PRBS 1 + x¹⁴ + x¹⁵, 8-packet groups)
//! - [`rs`]: Reed-Solomon RS(204,188, t = 8), encoder and decoder
//! - [`interleave`]: the Forney convolutional interleaver (I = 12, M = 17)
//! - [`conv`]: the K = 7 (171₈, 133₈) convolutional code and its puncturing
//! - [`viterbi`]: a soft-decision Viterbi decoder
//! - [`outer`]: the three outer stages chained, plus the packet sync
//!
//! ## The TS wire
//!
//! An MPEG transport stream travels on a `u8` wire as whole 188-byte packets,
//! each starting with 0x47, and nothing else. A packet the RS decoder could
//! not correct is still sent, with its transport_error_indicator bit set, so
//! the stream keeps its timing.

pub mod conv;
mod dvbs;
pub mod dvbt;
pub mod interleave;
pub mod outer;
pub mod rs;
pub mod scramble;
pub mod viterbi;

#[cfg(test)]
mod grdtv_vectors;

use foxygnal_core::Registry;

pub use dvbs::{DvbsDemodulator, DvbsModulator};
pub use dvbt::{DvbtDemodulator, DvbtModulator};

/// MPEG-TS packet length.
pub const TS_LEN: usize = 188;
/// RS-coded packet length.
pub const RS_LEN: usize = 204;
/// The TS sync byte.
pub const SYNC: u8 = 0x47;
/// The inverted sync byte that starts each energy-dispersal group.
pub const SYNC_INV: u8 = 0xB8;

pub fn register(r: &mut Registry) {
    dvbs::register(r);
    dvbt::register(r);
}

/// A preallocated byte FIFO for the TS output.
pub(crate) struct ByteFifo {
    buf: Box<[u8]>,
    head: usize,
    len: usize,
}

impl ByteFifo {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            buf: vec![0; cap].into_boxed_slice(),
            head: 0,
            len: 0,
        }
    }

    pub(crate) fn free(&self) -> usize {
        self.buf.len() - self.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Append (whatever doesn't fit is dropped; callers keep room).
    pub(crate) fn write(&mut self, data: &[u8]) {
        let cap = self.buf.len();
        for &b in data.iter().take(self.free()) {
            self.buf[(self.head + self.len) % cap] = b;
            self.len += 1;
        }
    }

    pub(crate) fn read_into(&mut self, out: &mut [u8]) -> usize {
        let cap = self.buf.len();
        let mut done = 0;
        while done < out.len() && self.len > 0 {
            let run = (cap - self.head).min(self.len).min(out.len() - done);
            out[done..done + run].copy_from_slice(&self.buf[self.head..self.head + run]);
            self.head = (self.head + run) % cap;
            self.len -= run;
            done += run;
        }
        done
    }
}
