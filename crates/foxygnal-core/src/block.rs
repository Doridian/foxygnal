//! The [`Block`] trait and the I/O view a block sees during `work`.

use crate::Error;
use crate::buffer::{Reader, Writer};
use crate::sample::{Sample, StreamType};

/// What happened during one `work` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkStatus {
    /// Progress was made; call again soon.
    Ok,
    /// Nothing could be done (no input / no output space). The scheduler
    /// sleeps until a neighbour signals, and handles end-of-stream.
    Blocked,
    /// The block is finished (e.g. a file source hit EOF).
    Done,
}

/// A processing block. Implementations hold their own state and parameters.
pub trait Block: Send + 'static {
    /// Process as much as is sensible right now.
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error>;

    /// Called once before the first `work`, on the block's own thread.
    fn start(&mut self) -> Result<(), Error> {
        Ok(())
    }

    /// Called once after the last `work`.
    fn stop(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

/// An input port during `work`.
pub struct InputPort {
    pub(crate) ty: StreamType,
    pub(crate) reader: Option<Reader>,
}

impl InputPort {
    pub fn stream_type(&self) -> StreamType {
        self.ty
    }

    /// Readable items as raw bytes (length is a multiple of the item size).
    pub fn bytes(&self) -> &[u8] {
        self.reader.as_ref().map_or(&[], |r| r.slice())
    }

    /// Readable items. Panics if `T` does not match the port type.
    pub fn slice<T: Sample>(&self) -> &[T] {
        assert_eq!(T::TYPE, self.ty, "input port type mismatch");
        match self.bytes() {
            [] => &[],
            b => bytemuck::cast_slice(b),
        }
    }

    pub fn consume(&mut self, items: usize) {
        if let Some(r) = &mut self.reader {
            r.consume(items);
        }
    }

    /// Upstream finished and all its data was consumed.
    pub fn exhausted(&self) -> bool {
        self.reader.as_ref().is_none_or(|r| r.exhausted())
    }
}

/// An output port during `work`.
pub struct OutputPort {
    pub(crate) ty: StreamType,
    pub(crate) writer: Writer,
}

impl OutputPort {
    pub fn stream_type(&self) -> StreamType {
        self.ty
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        self.writer.slice_mut()
    }

    /// Writable space. Panics if `T` does not match the port type.
    pub fn slice_mut<T: Sample>(&mut self) -> &mut [T] {
        assert_eq!(T::TYPE, self.ty, "output port type mismatch");
        match self.bytes_mut() {
            [] => &mut [],
            b => bytemuck::cast_slice_mut(b),
        }
    }

    pub fn produce(&mut self, items: usize) {
        self.writer.commit(items);
    }
}

/// Everything a block can touch during `work`.
pub struct WorkIo<'a> {
    pub inputs: &'a mut [InputPort],
    pub outputs: &'a mut [OutputPort],
}

impl WorkIo<'_> {
    /// Convenience for the common 1-in/1-out case: input slice + output slice.
    pub fn in_out<I: Sample, O: Sample>(&mut self, i: usize, o: usize) -> (&[I], &mut [O]) {
        (
            self.inputs[i].slice::<I>(),
            self.outputs[o].slice_mut::<O>(),
        )
    }

    pub fn consume(&mut self, port: usize, items: usize) {
        self.inputs[port].consume(items);
    }

    pub fn produce(&mut self, port: usize, items: usize) {
        self.outputs[port].produce(items);
    }
}
