//! A long picture written row by row: the pass of a polar orbiter, as raster
//! row records (`foxygnal_core::raster`) on one output. Rows are numbered
//! from 0 within a picture; the height isn't known in advance (0). A
//! picture ends when the decoder says so (the pass ended, a long dropout) by
//! sending its last row again with the last-row flag, and a picture that
//! reaches the record format's 65 535 rows is ended and continued in the
//! next one. A picture may be named (`image::push_name`, see
//! [`Strip::set_name`]); a continued one gets `_2`, `_3`, … after the name.

use foxygnal_core::raster::{LAST, Row};

use crate::digital::MsgQueue;
use crate::image::{push_name, push_row};

pub(crate) struct Strip {
    width: u16,
    channels: u8,
    picture: u32,
    row: u32,
    open: bool,
    last: Vec<u8>,
    name: String,
    /// Pictures opened under the current name.
    parts: u32,
}

impl Strip {
    pub fn new(width: u16, channels: u8) -> Self {
        Self {
            width,
            channels,
            picture: 0,
            row: 0,
            open: false,
            last: vec![0; usize::from(width) * usize::from(channels)],
            name: String::new(),
            parts: 0,
        }
    }

    /// The name of the next picture opened (empty: none). Called before its
    /// first row.
    pub fn set_name(&mut self, name: String) {
        self.name = name;
        self.parts = 0;
    }

    /// Rows written to the picture in progress (0 if none is).
    pub fn rows(&self) -> u32 {
        if self.open { self.row } else { 0 }
    }

    fn header(&self, row: u32, flags: u8) -> Row {
        Row {
            picture: self.picture,
            row: row as u16,
            height: 0,
            width: self.width,
            channels: self.channels,
            flags,
        }
    }

    /// The next row (`width × channels` bytes).
    pub fn row(&mut self, q: &mut MsgQueue, pixels: &[u8]) {
        if !self.open {
            self.picture += 1;
            self.row = 0;
            self.open = true;
            if !self.name.is_empty() {
                self.parts += 1;
                if self.parts == 1 {
                    push_name(q, self.picture, &self.name);
                } else {
                    push_name(q, self.picture, &format!("{}_{}", self.name, self.parts));
                }
            }
        }
        let last = self.row + 1 == u32::from(u16::MAX);
        push_row(
            q,
            self.header(self.row, if last { LAST } else { 0 }),
            pixels,
        );
        self.last.copy_from_slice(pixels);
        self.row += 1;
        if last {
            self.open = false;
        }
    }

    /// End the picture in progress: its last row again, flagged.
    pub fn end(&mut self, q: &mut MsgQueue) {
        if self.open && self.row > 0 {
            let h = self.header(self.row - 1, LAST);
            let last = std::mem::take(&mut self.last);
            push_row(q, h, &last);
            self.last = last;
        }
        self.open = false;
    }
}
