//! The [`Block`] trait and the I/O view a block sees during `work`.

use std::time::Duration;

use crate::Error;
use crate::buffer::{Reader, Writer};
use crate::meta::Configure;
use crate::sample::{Sample, StreamType};
use crate::tag::{TAG_QUEUE, Tag, TagPropagation, TagValue};

/// What happened during one `work` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkStatus {
    /// Progress was made; call again soon.
    Ok,
    /// Nothing could be done (no input / no output space). The scheduler
    /// sleeps until a neighbour signals, and handles end-of-stream.
    Blocked,
    /// Nothing to do until roughly this much time has passed (a throttle
    /// waiting for its next item). The scheduler runs the block again after
    /// this long; a neighbour may wake it earlier. Never sleep inside `work`.
    Wait(Duration),
    /// The block is finished (e.g. a file source hit EOF).
    Done,
}

/// A processing block. Implementations hold their own state and parameters.
pub trait Block: Send + 'static {
    /// Process as much as is sensible right now.
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error>;

    /// Called once after the graph is wired and before `start`, upstream
    /// blocks first, with the metadata of each input. Read what you need
    /// (`cx.input_rate(0)`) and adjust the outputs' metadata if this block
    /// changes it (a decimator divides the rate). The outputs start as a copy
    /// of the inputs, so most blocks don't implement this.
    ///
    /// Building and configuring must have no side effects (no files opened),
    /// because the server does both on every edit to label the wires.
    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        let _ = cx;
        Ok(())
    }

    /// How tags on this block's inputs reach its outputs. The default passes
    /// every tag through, its offset scaled by the block's rate change.
    /// Blocks that reorder, delay or drop items should return
    /// [`TagPropagation::Manual`] and forward tags themselves.
    fn tag_propagation(&self) -> TagPropagation {
        TagPropagation::AllToAll
    }

    /// Run this block on a thread of its own instead of the shared worker
    /// pool. Then `start`, every `work` and `set_param`, and `stop` all run
    /// on that one thread, which parks while the block is idle, so
    /// `std::thread::current()` is a valid handle to wake the block with
    /// (hardware blocks hand it to their driver threads). Ask for this if
    /// the block registers its thread with something else, uses
    /// thread-affine APIs (audio streams), or may block for long in `work`.
    ///
    /// Pooled blocks (the default) are called by whichever worker runs them,
    /// one call at a time: never concurrently, but not always on the same
    /// thread.
    fn dedicated_thread(&self) -> bool {
        false
    }

    /// Called once before the first `work`, on the thread that runs the
    /// block's first `work` (see [`Self::dedicated_thread`]). Open files and
    /// devices here, not in the factory.
    fn start(&mut self) -> Result<(), Error> {
        Ok(())
    }

    /// Called once after the last `work`.
    fn stop(&mut self) -> Result<(), Error> {
        Ok(())
    }

    /// Apply a parameter flagged `live` in the descriptor while running.
    /// Called between `work` calls, never during one. The value has
    /// already been checked against the parameter's spec.
    fn set_param(&mut self, name: &str, _value: &serde_json::Value) -> Result<(), Error> {
        Err(Error::Other(format!(
            "parameter `{name}` cannot change while running"
        )))
    }
}

/// An input port during `work`.
///
/// What a block sees is a snapshot taken just before `work`: items published
/// later show up on the next call. That keeps the readable items and their
/// tags consistent.
pub struct InputPort {
    pub(crate) ty: StreamType,
    pub(crate) reader: Option<Reader>,
    /// Stream index one past the last item visible during this `work` call.
    visible: u64,
    /// Received tags, sorted by offset. Capacity is fixed: no allocation.
    tags: Vec<Tag>,
    dropped_tags: usize,
}

impl InputPort {
    pub(crate) fn new(ty: StreamType, reader: Option<Reader>) -> Self {
        InputPort {
            ty,
            reader,
            visible: 0,
            tags: Vec::with_capacity(TAG_QUEUE),
            dropped_tags: 0,
        }
    }

    /// Take the snapshot for the next `work` call: the published items, and
    /// every tag sent for them.
    pub(crate) fn sync(&mut self) {
        let Some(r) = &mut self.reader else { return };
        // Load the count first: tags for these items were pushed before it.
        self.visible = r.written();
        while let Some(tag) = r.pop_tag() {
            if self.tags.len() == self.tags.capacity() {
                self.dropped_tags += 1;
                continue;
            }
            // Insert in order; tags from several upstream inputs can interleave.
            let at = self.tags.partition_point(|t| t.offset <= tag.offset);
            self.tags.insert(at, tag);
        }
    }

    /// Tags on items consumed since stream index `start` (before pruning).
    pub(crate) fn consumed_tags(&self, start: u64) -> &[Tag] {
        let read = self.position();
        let lo = self.tags.partition_point(|t| t.offset < start);
        let hi = self.tags.partition_point(|t| t.offset < read);
        &self.tags[lo..hi]
    }

    /// Forget tags on items already consumed.
    pub(crate) fn prune_tags(&mut self) {
        let read = self.position();
        let gone = self.tags.partition_point(|t| t.offset < read);
        self.tags.drain(..gone);
    }

    pub fn stream_type(&self) -> StreamType {
        self.ty
    }

    /// Stream index of the first readable item.
    pub fn position(&self) -> u64 {
        self.reader.as_ref().map_or(0, Reader::position)
    }

    /// Tags on the readable items, in offset order (offsets are absolute;
    /// subtract [`Self::position`] for an index into [`Self::slice`]).
    pub fn tags(&self) -> &[Tag] {
        let read = self.position();
        let lo = self.tags.partition_point(|t| t.offset < read);
        let hi = self.tags.partition_point(|t| t.offset < self.visible);
        &self.tags[lo..hi.max(lo)]
    }

    /// Tags on the next `n` readable items, as (index into the slice, tag).
    pub fn tags_in(&self, n: usize) -> impl Iterator<Item = (usize, &Tag)> {
        let read = self.position();
        self.tags()
            .iter()
            .take_while(move |t| t.offset < read + n as u64)
            .map(move |t| ((t.offset - read) as usize, t))
    }

    /// Tags lost because too many arrived at once.
    pub fn dropped_tags(&self) -> usize {
        self.dropped_tags + self.reader.as_ref().map_or(0, Reader::dropped_tags)
    }

    /// Readable items as raw bytes (length is a multiple of the item size).
    pub fn bytes(&self) -> &[u8] {
        let Some(r) = &self.reader else { return &[] };
        let b = r.slice();
        let max = self.visible.saturating_sub(r.position()) as usize * r.item_size();
        &b[..b.len().min(max)]
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

    /// Mark `items` written items as produced. They are published when
    /// `work` returns, after their tags.
    pub fn produce(&mut self, items: usize) {
        self.writer.stage(items);
    }

    /// Stream index of the first item of [`Self::slice_mut`].
    pub fn position(&self) -> u64 {
        self.writer.position()
    }

    /// Tag item `index` of [`Self::slice_mut`] (0 = the first writable item).
    pub fn add_tag(&mut self, index: usize, key: &str, value: impl Into<TagValue>) {
        let tag = Tag::new(self.position() + index as u64, key, value);
        self.writer.push_tag(&tag);
    }

    /// Pass an existing tag on, at item `index` of [`Self::slice_mut`].
    /// Cheaper than [`Self::add_tag`]: the key and value are not copied.
    pub fn forward_tag(&mut self, index: usize, tag: &Tag) {
        let tag = Tag {
            offset: self.position() + index as u64,
            key: tag.key.clone(),
            value: tag.value.clone(),
        };
        self.writer.push_tag(&tag);
    }

    pub(crate) fn push_tag(&mut self, tag: &Tag) {
        self.writer.push_tag(tag);
    }

    pub(crate) fn flush(&mut self) {
        self.writer.flush();
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
