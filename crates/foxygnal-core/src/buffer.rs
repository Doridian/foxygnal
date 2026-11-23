//! Lock-free single-writer / multi-reader ring buffer connecting blocks.
//!
//! One output port owns one [`Ring`]; every downstream connection gets its own
//! read cursor, so fan-out costs nothing extra. The writer may only overwrite
//! items that *all* readers have consumed.
//!
//! A [`Tap`] is a *lossy* view of a ring for probes and stats. It is not a
//! reader: the writer never waits for it, so watching a stream can't slow the
//! graph down. If a tap falls behind, the data it missed is simply gone.
//!
//! For a lossless view of a running stream (recordings), [`Tap::attach`]
//! claims one of a few spare read cursors every ring has ([`SPARE_READERS`]).
//! An attached [`TapReader`] holds the writer back like any reader and
//! receives the stream's tags.
//!
//! The storage is normally *double-mapped* ([`crate::vmem`]): the same pages
//! appear twice in a row in memory, so [`Reader::slice`] and
//! [`Writer::slice_mut`] always hand out *every* available item as one
//! contiguous slice, even across the wrap point (a writer gets at most half
//! the ring at once, so taps can see the rest). Its capacity is rounded up to
//! whole memory pages. Where the OS can't do that, the ring falls back to a
//! plain allocation ([`BufferKind::Plain`]) whose slices stop at the wrap
//! point, so a block may then see fewer items than are available.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering, fence};
use std::sync::{Arc, OnceLock};
use std::task::Waker;

use crate::tag::{TAG_QUEUE, Tag, TagQueue};
use crate::vmem::{Storage, page_size};

/// Default buffer capacity in items.
pub const DEFAULT_CAPACITY: usize = 1 << 15;

/// Read cursors per ring that can be attached while running ([`Tap::attach`]).
pub const SPARE_READERS: usize = 2;

/// How a ring's storage is laid out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BufferKind {
    /// Double-mapped where the platform allows it (contiguous slices, the
    /// capacity rounded up to whole pages), plain otherwise.
    #[default]
    Auto,
    /// One plain allocation of exactly the requested capacity; slices stop
    /// at the wrap point. Mostly useful to test the fallback.
    Plain,
}

struct ReaderState {
    /// Total items consumed by this reader (monotonic).
    read: AtomicUsize,
    /// The reader block has terminated and will never read again.
    done: AtomicBool,
    /// Called when new data (or the end of the stream) arrives.
    waker: OnceLock<Waker>,
    /// Tags on their way to this reader.
    tags: TagQueue,
}

impl ReaderState {
    fn new(done: bool) -> Self {
        ReaderState {
            read: AtomicUsize::new(0),
            done: AtomicBool::new(done),
            waker: OnceLock::new(),
            tags: TagQueue::new(TAG_QUEUE),
        }
    }
}

/// A read cursor a [`TapReader`] can claim while the graph runs. Free (and
/// `done`, so the writer ignores it) until then. Its owner isn't a block and
/// polls instead of registering a waker (wakers can't be replaced, and the
/// owner changes from one attach to the next).
struct Spare {
    claimed: AtomicBool,
    state: ReaderState,
}

struct Ring {
    /// Aligned to at least 8 bytes, enough for every StreamType.
    storage: Storage,
    item_size: usize,
    capacity: usize,
    /// Total items committed by the writer (monotonic).
    written: AtomicUsize,
    /// End of the region the writer may be writing into: `written` plus the
    /// length of the last slice handed out (monotonic). Items before
    /// `reserved - capacity` may be overwritten at any moment; taps use it to
    /// detect data that changed under them.
    reserved: AtomicUsize,
    writer_done: AtomicBool,
    /// Called when a reader frees space or leaves.
    writer_waker: OnceLock<Waker>,
    readers: Box<[ReaderState]>,
    spare: Box<[Spare]>,
}

// SAFETY: access to `storage` is coordinated via the atomic cursors; the writer
// only touches the free region and readers only touch committed regions.
unsafe impl Sync for Ring {}
unsafe impl Send for Ring {}

impl Ring {
    fn base(&self) -> *mut u8 {
        self.storage.base()
    }

    /// How many of the `n` items starting at stream index `start` can be
    /// addressed as one slice from [`Self::at`]: all of them on a mirrored
    /// ring (`n` never exceeds the capacity), up to the wrap point otherwise.
    fn contiguous(&self, start: usize, n: usize) -> usize {
        if self.storage.is_mirrored() {
            n
        } else {
            n.min(self.capacity - start % self.capacity)
        }
    }

    /// Most items one [`Writer::slice_mut`] hands out.
    fn max_write(&self) -> usize {
        if self.storage.is_mirrored() {
            self.capacity / 2
        } else {
            self.capacity
        }
    }

    /// Address of the item at stream index `index`.
    fn at(&self, index: usize) -> *mut u8 {
        // SAFETY: `index % capacity` is a slot inside the storage.
        unsafe { self.base().add(index % self.capacity * self.item_size) }
    }

    /// Every read cursor: the connections, then the spare ones.
    fn cursors(&self) -> impl Iterator<Item = &ReaderState> {
        self.readers
            .iter()
            .chain(self.spare.iter().map(|s| &s.state))
    }

    fn wake_writer(&self) {
        if let Some(w) = self.writer_waker.get() {
            w.wake_by_ref();
        }
    }

    fn min_read(&self) -> usize {
        self.cursors()
            .filter(|r| !r.done.load(Ordering::Acquire))
            .map(|r| r.read.load(Ordering::Acquire))
            .min()
            .unwrap_or_else(|| self.written.load(Ordering::Relaxed))
    }
}

/// Create a ring of at least `capacity` items with `readers` read cursors.
pub fn channel(item_size: usize, capacity: usize, readers: usize) -> (Writer, Vec<Reader>) {
    channel_with(item_size, capacity, readers, BufferKind::Auto)
}

/// [`channel`] with a choice of storage layout.
pub fn channel_with(
    item_size: usize,
    capacity: usize,
    readers: usize,
    kind: BufferKind,
) -> (Writer, Vec<Reader>) {
    assert!(item_size > 0 && capacity > 0);
    let mirrored = match kind {
        BufferKind::Plain => None,
        BufferKind::Auto => {
            // Whole pages, and a whole number of items in them (item sizes
            // 1, 2, 4 and 8 always divide a page).
            let bytes = (item_size * capacity).next_multiple_of(page_size());
            if bytes % item_size == 0 {
                Storage::mirrored(bytes).map(|s| (s, bytes / item_size))
            } else {
                None
            }
        }
    };
    let (storage, capacity) =
        mirrored.unwrap_or_else(|| (Storage::plain(item_size * capacity), capacity));
    let ring = Arc::new(Ring {
        storage,
        item_size,
        capacity,
        written: AtomicUsize::new(0),
        reserved: AtomicUsize::new(0),
        writer_done: AtomicBool::new(false),
        writer_waker: OnceLock::new(),
        readers: (0..readers).map(|_| ReaderState::new(false)).collect(),
        spare: (0..SPARE_READERS)
            .map(|_| Spare {
                claimed: AtomicBool::new(false),
                state: ReaderState::new(true),
            })
            .collect(),
    });
    let rs = (0..readers)
        .map(|idx| Reader {
            ring: ring.clone(),
            idx,
        })
        .collect();
    (Writer { ring, staged: 0 }, rs)
}

/// The unique write end of a ring. Not `Clone`: `&mut self` guarantees exclusivity.
pub struct Writer {
    ring: Arc<Ring>,
    /// Items written by [`Self::stage`] but not yet published.
    staged: usize,
}

impl Writer {
    pub fn item_size(&self) -> usize {
        self.ring.item_size
    }

    /// Register what to wake when readers free space (or leave). The first
    /// registration wins.
    pub fn register_waker(&self, waker: Waker) {
        let _ = self.ring.writer_waker.set(waker);
    }

    fn wake_readers(&self) {
        for rd in self.ring.readers.iter() {
            if let Some(w) = rd.waker.get() {
                w.wake_by_ref();
            }
        }
    }

    /// Contiguous writable region, in bytes. Empty once [`Self::abandoned`].
    ///
    /// On a double-mapped ring this is all free space up to half the
    /// capacity (the other half stays visible to taps); on a plain ring, the
    /// free space up to the wrap point.
    pub fn slice_mut(&mut self) -> &mut [u8] {
        if self.abandoned() {
            return &mut [];
        }
        let r = &*self.ring;
        let written = r.written.load(Ordering::Relaxed) + self.staged;
        // Saturating: a cursor attached while the writer was mid-slice may
        // briefly sit behind data that was overwritten (see `Tap::attach`).
        let free = r.capacity.saturating_sub(written - r.min_read());
        // Taps can't trust anything the writer may be writing into, so a
        // mirrored ring (where all free space is contiguous) hands out at most
        // half of itself: probes can always see the newest half.
        let n = r.contiguous(written, free.min(r.max_write()));
        // Announce the region before writing into it, for taps (seqlock-style).
        r.reserved.store(written + n, Ordering::Relaxed);
        fence(Ordering::Release);
        // SAFETY: the `n` free slots from `written` are addressable as one
        // slice (`contiguous`), no reader can see them until they are
        // published, and `&mut self` excludes other writers.
        unsafe { std::slice::from_raw_parts_mut(r.at(written), n * r.item_size) }
    }

    /// Publish `items` items previously written into [`Self::slice_mut`]
    /// (and everything staged before them).
    pub fn commit(&mut self, items: usize) {
        self.stage(items);
        self.flush();
    }

    /// Count `items` as written without publishing them yet, so tags for
    /// them can be pushed first. [`Self::flush`] publishes.
    pub fn stage(&mut self, items: usize) {
        self.staged += items;
    }

    /// Stream index of the next item [`Self::slice_mut`] hands out.
    pub fn position(&self) -> u64 {
        (self.ring.written.load(Ordering::Relaxed) + self.staged) as u64
    }

    /// Send a tag to every reader. Push tags before publishing the items
    /// they point at; offsets should not go backwards.
    pub fn push_tag(&mut self, tag: &Tag) {
        for rd in self.ring.cursors() {
            if !rd.done.load(Ordering::Relaxed) {
                rd.tags.push(tag.clone());
            }
        }
    }

    /// Publish staged items and wake readers.
    pub fn flush(&mut self) {
        let items = std::mem::take(&mut self.staged);
        if items == 0 {
            return;
        }
        let r = &*self.ring;
        r.written.fetch_add(items, Ordering::Release);
        self.wake_readers();
    }

    /// True when no reader will ever consume again (all readers finished).
    /// An output with zero readers is never "abandoned": its data is discarded.
    pub fn abandoned(&self) -> bool {
        let rs = &self.ring.readers;
        !rs.is_empty() && rs.iter().all(|r| r.done.load(Ordering::Acquire))
    }

    /// A lossy view of this ring for probes and stats.
    pub fn tap(&self) -> Tap {
        Tap {
            ring: self.ring.clone(),
        }
    }

    /// Mark end of stream and wake readers. Staged items are published first.
    pub fn finish(&mut self) {
        self.flush();
        self.ring.writer_done.store(true, Ordering::Release);
        self.wake_readers();
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.finish();
    }
}

/// One read cursor of a ring.
pub struct Reader {
    ring: Arc<Ring>,
    idx: usize,
}

impl Reader {
    pub fn item_size(&self) -> usize {
        self.ring.item_size
    }

    fn state(&self) -> &ReaderState {
        &self.ring.readers[self.idx]
    }

    /// Register what to wake when new data arrives (or the stream ends).
    /// The first registration wins.
    pub fn register_waker(&self, waker: Waker) {
        let _ = self.state().waker.set(waker);
    }

    fn wake_writer(&self) {
        self.ring.wake_writer();
    }

    /// Stream index of the next item to read.
    pub fn position(&self) -> u64 {
        self.state().read.load(Ordering::Relaxed) as u64
    }

    /// Items published so far (Acquire: tags pushed before them are visible).
    pub fn written(&self) -> u64 {
        self.ring.written.load(Ordering::Acquire) as u64
    }

    /// The next tag sent to this reader, if any.
    pub fn pop_tag(&mut self) -> Option<Tag> {
        self.state().tags.pop()
    }

    /// Tags dropped because this reader's queue was full.
    pub fn dropped_tags(&self) -> usize {
        self.state().tags.dropped()
    }

    /// Contiguous readable region, in bytes.
    pub fn slice(&self) -> &[u8] {
        let r = &*self.ring;
        let read = self.state().read.load(Ordering::Relaxed);
        let avail = r.written.load(Ordering::Acquire) - read;
        let n = r.contiguous(read, avail);
        // SAFETY: published items, addressable as one slice (`contiguous`);
        // the writer won't touch them until we consume them.
        unsafe { std::slice::from_raw_parts(r.at(read), n * r.item_size) }
    }

    pub fn consume(&mut self, items: usize) {
        if items == 0 {
            return;
        }
        self.state().read.fetch_add(items, Ordering::Release);
        self.wake_writer();
    }

    /// Writer has finished *and* everything has been read.
    pub fn exhausted(&self) -> bool {
        let r = &*self.ring;
        r.writer_done.load(Ordering::Acquire)
            && r.written.load(Ordering::Acquire) == self.state().read.load(Ordering::Relaxed)
    }

    /// Mark this reader as gone so the writer no longer waits for it.
    pub fn finish(&self) {
        self.state().done.store(true, Ordering::Release);
        self.wake_writer();
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Result of [`Tap::read`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TapRead {
    /// Stream index of the first item copied. Later than the requested start
    /// if older items were already overwritten.
    pub start: usize,
    /// Number of items copied.
    pub items: usize,
}

/// A lossy, non-blocking view of a ring (see the module docs). Cheap to clone.
#[derive(Clone)]
pub struct Tap {
    ring: Arc<Ring>,
}

impl Tap {
    pub fn item_size(&self) -> usize {
        self.ring.item_size
    }

    pub fn capacity(&self) -> usize {
        self.ring.capacity
    }

    /// The ring is double-mapped: every slice is contiguous.
    pub fn is_mirrored(&self) -> bool {
        self.ring.storage.is_mirrored()
    }

    /// Total items the writer has committed so far.
    pub fn written(&self) -> usize {
        self.ring.written.load(Ordering::Acquire)
    }

    /// The writer has finished.
    pub fn finished(&self) -> bool {
        self.ring.writer_done.load(Ordering::Acquire)
    }

    /// Items written but not yet consumed by reader `reader` (0 once it left).
    pub fn backlog(&self, reader: usize) -> usize {
        let Some(rd) = self.ring.readers.get(reader) else {
            return 0;
        };
        if rd.done.load(Ordering::Acquire) {
            return 0;
        }
        self.written()
            .saturating_sub(rd.read.load(Ordering::Acquire))
    }

    /// Copy committed items starting at stream index `from` into `out`, as
    /// many as fit. Items that were already overwritten are skipped, so check
    /// [`TapRead::start`]. Never blocks and never holds the writer back.
    pub fn read(&self, from: usize, out: &mut [u8]) -> TapRead {
        self.ring.copy(from, out)
    }

    /// Attach a lossless read cursor at the current end of the stream, or
    /// `None` if all [`SPARE_READERS`] are taken. From then on the writer
    /// waits for it like for any downstream block (and is woken when it
    /// reads), and it gets every tag. Its owner polls ([`TapReader::read`]).
    ///
    /// The writer may be in the middle of a `work` call that began before
    /// it could see the new cursor, so the first items read can still be
    /// overwritten; [`TapReader::read`] detects that like [`Tap::read`]
    /// does, by reporting a later start.
    pub fn attach(&self) -> Option<TapReader> {
        let r = &*self.ring;
        let idx = r.spare.iter().position(|s| {
            s.claimed
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        })?;
        let st = &r.spare[idx].state;
        // Tags left over from an earlier owner.
        while st.tags.pop().is_some() {}
        let pos = r.written.load(Ordering::Acquire);
        st.read.store(pos, Ordering::Release);
        st.done.store(false, Ordering::SeqCst);
        Some(TapReader {
            ring: self.ring.clone(),
            idx,
            pos,
        })
    }
}

impl Ring {
    /// Oldest stream index that is still intact.
    fn oldest(&self, order: Ordering) -> usize {
        self.reserved.load(order).saturating_sub(self.capacity)
    }

    /// See [`Tap::read`].
    fn copy(&self, from: usize, out: &mut [u8]) -> TapRead {
        let r = self;
        let size = r.item_size;
        let written = r.written.load(Ordering::Acquire);
        let mut start = from.max(r.oldest(Ordering::Acquire)).min(written);
        let mut items = (written - start).min(out.len() / size);
        // One copy on a mirrored ring; two (up to and from the wrap) otherwise.
        let first = r.contiguous(start, items);
        // SAFETY: both ranges are inside the storage. The writer may be
        // overwriting the oldest part concurrently (a seqlock-style race);
        // the check below finds and discards anything it may have touched.
        // Raw copies, never references, so the race doesn't create aliasing
        // `&`/`&mut` slices.
        unsafe {
            std::ptr::copy_nonoverlapping(r.at(start), out.as_mut_ptr(), first * size);
            std::ptr::copy_nonoverlapping(
                r.at(start + first),
                out.as_mut_ptr().add(first * size),
                (items - first) * size,
            );
        }
        fence(Ordering::Acquire);
        let oldest = r.oldest(Ordering::Relaxed);
        if oldest > start {
            let lost = (oldest - start).min(items);
            out.copy_within(lost * size..items * size, 0);
            start += lost;
            items -= lost;
        }
        TapRead { start, items }
    }
}

/// A lossless read cursor attached to a running stream ([`Tap::attach`]).
/// Dropping it detaches it.
pub struct TapReader {
    ring: Arc<Ring>,
    idx: usize,
    /// Stream index of the next item to read.
    pos: usize,
}

impl TapReader {
    fn state(&self) -> &ReaderState {
        &self.ring.spare[self.idx].state
    }

    pub fn item_size(&self) -> usize {
        self.ring.item_size
    }

    /// Stream index of the next item to read.
    pub fn position(&self) -> u64 {
        self.pos as u64
    }

    /// Copy the next items into `out`, as many as fit, let the writer reuse
    /// their space and wake it. A [`TapRead::start`] past
    /// [`Self::position`] means items were lost, which can only happen
    /// right after attaching. Tags for every item returned are in the queue
    /// ([`Self::pop_tag`]) when this returns.
    pub fn read(&mut self, out: &mut [u8]) -> TapRead {
        let got = self.ring.copy(self.pos, out);
        let old = std::mem::replace(&mut self.pos, got.start + got.items);
        if self.pos != old {
            self.state().read.store(self.pos, Ordering::Release);
            self.ring.wake_writer();
        }
        got
    }

    /// The next tag sent to this cursor, if any. Tags may point at items
    /// before the attach point or not read yet; check their offsets.
    pub fn pop_tag(&mut self) -> Option<Tag> {
        self.state().tags.pop()
    }

    /// Tags dropped because the queue was full.
    pub fn dropped_tags(&self) -> usize {
        self.state().tags.dropped()
    }

    /// The writer has finished and everything has been read.
    pub fn finished(&self) -> bool {
        let r = &*self.ring;
        r.writer_done.load(Ordering::Acquire) && r.written.load(Ordering::Acquire) == self.pos
    }
}

impl Drop for TapReader {
    fn drop(&mut self) {
        let spare = &self.ring.spare[self.idx];
        spare.state.done.store(true, Ordering::Release);
        spare.claimed.store(false, Ordering::Release);
        self.ring.wake_writer();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tag::TagValue;

    const BOTH: [BufferKind; 2] = [BufferKind::Auto, BufferKind::Plain];

    /// Write `vals` (u32) into `w`, as much as fits; returns how many.
    fn put(w: &mut Writer, vals: &[u32]) -> usize {
        let buf: &mut [u32] = bytemuck::cast_slice_mut(w.slice_mut());
        let n = buf.len().min(vals.len());
        buf[..n].copy_from_slice(&vals[..n]);
        w.commit(n);
        n
    }

    #[cfg(unix)]
    #[test]
    fn unix_rings_are_mirrored_and_page_sized() {
        let (w, _rs) = channel(4, 10, 1);
        let tap = w.tap();
        assert!(tap.is_mirrored());
        assert_eq!(tap.capacity() * 4 % page_size(), 0);
        assert!(tap.capacity() >= 10);
        let (w, _rs) = channel_with(4, 10, 1, BufferKind::Plain);
        assert!(!w.tap().is_mirrored());
        assert_eq!(w.tap().capacity(), 10);
    }

    #[test]
    fn wraps_and_fans_out() {
        for kind in BOTH {
            let (mut w, mut rs) = channel_with(4, 8, 2, kind);
            let cap = w.tap().capacity() as u32;
            // Odd-sized rounds so the wrap point moves around.
            for round in 0..(3 * cap / 7 + 10) {
                let vals: Vec<u32> = (0..7).map(|i| round * 10 + i).collect();
                let mut sent = 0;
                while sent < vals.len() {
                    let n = put(&mut w, &vals[sent..]);
                    sent += n;
                    for r in rs.iter_mut() {
                        let mut got = Vec::new();
                        while got.len() < n {
                            let s: &[u32] = bytemuck::cast_slice(r.slice());
                            let k = s.len();
                            got.extend_from_slice(s);
                            r.consume(k);
                        }
                        assert_eq!(got, vals[sent - n..sent], "{kind:?}");
                    }
                }
            }
        }
    }

    /// The point of the double mapping: all available items in one slice,
    /// for the writer and every reader, across the wrap point.
    #[cfg(unix)]
    #[test]
    fn slices_are_contiguous_across_the_wrap() {
        let (mut w, mut rs) = channel(4, 1, 2);
        let cap = w.tap().capacity();
        assert!(w.tap().is_mirrored());
        // Move every cursor to 3 items before the end of the storage.
        let pre: Vec<u32> = (0..cap as u32 - 3).collect();
        let mut sent = 0;
        while sent < pre.len() {
            sent += put(&mut w, &pre[sent..]);
        }
        for r in rs.iter_mut() {
            r.consume(pre.len());
        }
        // The writer gets half the ring (the most it hands out at once),
        // wrapping after 3 items, then the other half.
        assert_eq!(w.slice_mut().len(), cap / 2 * 4);
        let vals: Vec<u32> = (1000..1000 + cap as u32).collect();
        assert_eq!(put(&mut w, &vals), cap / 2);
        assert_eq!(put(&mut w, &vals[cap / 2..]), cap / 2);
        // Every reader sees all of it at once.
        for r in rs.iter_mut() {
            let s: &[u32] = bytemuck::cast_slice(r.slice());
            assert_eq!(s, &vals[..]);
        }
        // Partial consumption keeps the rest contiguous.
        rs[0].consume(5);
        let s: &[u32] = bytemuck::cast_slice(rs[0].slice());
        assert_eq!(s, &vals[5..]);
        // The slowest reader still holds everything.
        assert_eq!(w.slice_mut().len(), 0);
        rs[1].consume(cap);
        assert_eq!(w.slice_mut().len(), 5 * 4);
    }

    #[cfg(unix)]
    #[test]
    fn mirrored_writer_slices_leave_half_for_taps() {
        let (mut w, _rs) = channel(1, 1, 1);
        let cap = w.tap().capacity();
        assert_eq!(w.slice_mut().len(), cap / 2);
        w.commit(cap / 2);
        assert_eq!(w.slice_mut().len(), cap / 2);
        // Most recent half is intact for a tap even while the writer holds
        // the other half.
        let mut out = vec![0u8; cap];
        assert_eq!(
            w.tap().read(0, &mut out),
            TapRead {
                start: 0,
                items: cap / 2
            }
        );
    }

    #[test]
    fn plain_rings_stop_at_the_wrap() {
        let (mut w, mut rs) = channel_with(1, 8, 1, BufferKind::Plain);
        assert_eq!(w.slice_mut().len(), 8);
        w.slice_mut()[..6].copy_from_slice(&[1, 2, 3, 4, 5, 6]);
        w.commit(6);
        rs[0].consume(6);
        assert_eq!(w.slice_mut().len(), 2, "contiguous up to the wrap point");
        w.slice_mut()[..2].copy_from_slice(&[7, 8]);
        w.commit(2);
        w.slice_mut()[..3].copy_from_slice(&[9, 10, 11]);
        w.commit(3);
        assert_eq!(rs[0].slice(), &[7, 8]);
        rs[0].consume(2);
        assert_eq!(rs[0].slice(), &[9, 10, 11]);
    }

    #[test]
    fn slowest_reader_limits_writer() {
        for kind in BOTH {
            let (mut w, mut rs) = channel_with(1, 4, 2, kind);
            let cap = w.tap().capacity();
            while !w.slice_mut().is_empty() {
                let n = w.slice_mut().len();
                w.commit(n);
            }
            assert_eq!(w.tap().written(), cap);
            rs[0].consume(cap);
            assert_eq!(w.slice_mut().len(), 0, "reader 1 still holds the data");
            rs[1].consume(2);
            assert_eq!(w.slice_mut().len(), 2);
            // A finished reader no longer holds the writer back.
            rs[1].finish();
            assert_eq!(w.slice_mut().len(), w.ring.max_write());
        }
    }

    #[test]
    fn tags_travel_with_items_across_the_wrap() {
        for kind in BOTH {
            let (mut w, mut rs) = channel_with(4, 6, 1, kind);
            let cap = w.tap().capacity() as u64;
            let mut next = 0u32;
            let mut expect_tag = 0u64;
            while (next as u64) < 4 * cap {
                let at = w.position();
                if at % 5 == 0 {
                    w.push_tag(&Tag::new(at, "i", at as i64));
                }
                next += put(&mut w, &[next]) as u32;
                let r = &mut rs[0];
                let written = r.written();
                let k = r.slice().len() / 4;
                assert_eq!(r.position() + k as u64, written, "{kind:?}");
                while let Some(t) = r.pop_tag() {
                    assert_eq!(t.offset, expect_tag);
                    assert_eq!(t.value, TagValue::Int(expect_tag as i64));
                    expect_tag += 5;
                }
                r.consume(k);
            }
            assert!(expect_tag > cap);
        }
    }

    #[test]
    fn tap_reads_without_holding_the_writer_back() {
        let (mut w, mut rs) = channel_with(1, 8, 1, BufferKind::Plain);
        let tap = w.tap();
        let mut out = [0u8; 16];
        assert_eq!(tap.read(0, &mut out).items, 0);

        w.slice_mut()[..6].copy_from_slice(&[1, 2, 3, 4, 5, 6]);
        w.commit(6);
        let got = tap.read(0, &mut out);
        assert_eq!(got, TapRead { start: 0, items: 6 });
        assert_eq!(&out[..6], &[1, 2, 3, 4, 5, 6]);
        assert_eq!(tap.backlog(0), 6);

        // The reader moves on; the writer reuses space the tap never "read".
        rs[0].consume(6);
        let n = w.slice_mut().len();
        assert_eq!(n, 2, "contiguous up to the wrap point");
        w.slice_mut()[..2].copy_from_slice(&[7, 8]);
        w.commit(2);
        let buf = w.slice_mut();
        buf[..4].copy_from_slice(&[9, 10, 11, 12]);
        w.commit(4);

        // Items 0..4 are gone (slots reserved for 8..14); the tap skips them.
        let got = tap.read(0, &mut out);
        assert_eq!(got.start, 6, "slots up to item 14 are reserved");
        assert_eq!(&out[..got.items], &[7, 8, 9, 10, 11, 12]);
        // Reading from a cursor in the middle works across the wrap.
        let got = tap.read(7, &mut out[..3]);
        assert_eq!(got, TapRead { start: 7, items: 3 });
        assert_eq!(&out[..3], &[8, 9, 10]);
    }

    #[cfg(unix)]
    #[test]
    fn tap_on_a_mirrored_ring() {
        let (mut w, mut rs) = channel(1, 1, 1);
        let cap = w.tap().capacity();
        let tap = w.tap();
        let vals: Vec<u8> = (0..cap).map(|i| i as u8).collect();
        w.slice_mut().copy_from_slice(&vals[..cap / 2]);
        w.commit(cap / 2);
        w.slice_mut().copy_from_slice(&vals[cap / 2..]);
        w.commit(cap / 2);
        rs[0].consume(cap - 4);
        // Room for cap - 4 items, wrapping; the writer reserves half the
        // ring, so the newest half stays intact for the tap.
        let buf = w.slice_mut();
        assert_eq!(buf.len(), cap / 2);
        buf[..6].copy_from_slice(&[100, 101, 102, 103, 104, 105]);
        w.commit(6);
        let mut out = vec![0u8; cap];
        let got = tap.read(0, &mut out);
        // Slots of items before cap / 2 were handed to the writer again.
        assert_eq!(
            got,
            TapRead {
                start: cap / 2,
                items: cap / 2 + 6
            }
        );
        let end = vals[cap - 4..].to_vec();
        assert_eq!(&out[got.items - 10..got.items - 6], &end[..]);
        assert_eq!(
            &out[got.items - 6..got.items],
            &[100, 101, 102, 103, 104, 105]
        );
        // One copy across the wrap from a cursor in the middle.
        let got = tap.read(cap - 2, &mut out[..5]);
        assert_eq!(
            got,
            TapRead {
                start: cap - 2,
                items: 5
            }
        );
        assert_eq!(&out[..5], &[end[2], end[3], 100, 101, 102]);
    }

    /// A writer racing a fast reader and a tap: everything the tap returns
    /// must be exactly what was written at that index, never torn data.
    #[test]
    fn tap_never_returns_overwritten_data() {
        for kind in BOTH {
            const TOTAL: u32 = 2_000_000;
            let (mut w, mut rs) = channel_with(4, 64, 1, kind);
            let tap = w.tap();
            let mut r = rs.pop().unwrap();
            let writer = std::thread::spawn(move || {
                let mut next = 0u32;
                while next < TOTAL {
                    let buf: &mut [u32] = bytemuck::cast_slice_mut(w.slice_mut());
                    // Odd chunks so writes straddle the wrap point.
                    let n = buf.len().min((TOTAL - next) as usize).min(97);
                    for (i, v) in buf[..n].iter_mut().enumerate() {
                        *v = next + i as u32;
                    }
                    w.commit(n);
                    next += n as u32;
                }
            });
            let reader = std::thread::spawn(move || {
                while !r.exhausted() {
                    let n = r.slice().len() / 4;
                    r.consume(n);
                }
            });
            let mut out = vec![0u32; 48];
            let mut checked = 0usize;
            while !tap.finished() || checked == 0 {
                let from = tap.written().saturating_sub(40);
                let got = tap.read(from, bytemuck::cast_slice_mut(&mut out));
                for (i, v) in out[..got.items].iter().enumerate() {
                    assert_eq!(*v as usize, got.start + i, "torn read ({kind:?})");
                }
                checked += got.items;
            }
            writer.join().unwrap();
            reader.join().unwrap();
            assert!(checked > 0);
        }
    }

    /// Threads hammering tiny rings: two readers that each insist on `want`
    /// contiguous items at a time (like a decimator), with a `want` that
    /// doesn't divide the capacity. On a mirrored ring that always works; a
    /// plain ring only promises progress one item at a time.
    #[test]
    fn stress_tiny_rings() {
        const TOTAL: u32 = 300_000;
        let cases: &[(BufferKind, usize, usize)] = &[
            (BufferKind::Auto, 1, 3),
            (BufferKind::Auto, 1, 7),
            (BufferKind::Auto, 100, 1000),
            (BufferKind::Plain, 1, 1),
            (BufferKind::Plain, 2, 1),
            (BufferKind::Plain, 3, 1),
            (BufferKind::Plain, 7, 1),
        ];
        for &(kind, cap, want) in cases {
            let (mut w, rs) = channel_with(4, cap, 2, kind);
            let mirrored = w.tap().is_mirrored();
            let writer = std::thread::spawn(move || {
                let mut next = 0u32;
                while next < TOTAL {
                    let buf: &mut [u32] = bytemuck::cast_slice_mut(w.slice_mut());
                    let n = buf.len().min((TOTAL - next) as usize).min(13);
                    for (i, v) in buf[..n].iter_mut().enumerate() {
                        *v = next + i as u32;
                    }
                    w.commit(n);
                    next += n as u32;
                    if n == 0 {
                        std::thread::yield_now();
                    }
                }
            });
            let readers: Vec<_> = rs
                .into_iter()
                .map(|mut r| {
                    std::thread::spawn(move || {
                        let mut expect = 0u32;
                        while expect < TOTAL {
                            let avail = (r.written() - r.position()) as usize;
                            let s: &[u32] = bytemuck::cast_slice(r.slice());
                            if mirrored {
                                assert!(s.len() >= avail, "short slice on a mirrored ring");
                            }
                            // Whole groups of `want`, or the tail of the stream.
                            let group = want.min((TOTAL - expect) as usize);
                            let n = s.len() / group * group;
                            for &v in &s[..n] {
                                assert_eq!(v, expect);
                                expect += 1;
                            }
                            r.consume(n);
                            if n == 0 {
                                std::thread::yield_now();
                            }
                        }
                    })
                })
                .collect();
            writer.join().unwrap();
            for r in readers {
                r.join().unwrap();
            }
        }
    }

    #[test]
    fn attached_reader_holds_the_writer_and_gets_tags() {
        let (mut w, mut rs) = channel_with(1, 8, 1, BufferKind::Plain);
        w.slice_mut()[..3].copy_from_slice(&[1, 2, 3]);
        w.commit(3);
        let tap = w.tap();
        let mut a = tap.attach().expect("a spare cursor");
        assert_eq!(a.position(), 3, "starts at the current end");
        w.push_tag(&Tag::new(3, "rx_freq", 1e6));
        w.slice_mut()[..4].copy_from_slice(&[4, 5, 6, 7]);
        w.commit(4);
        rs[0].consume(7);
        // The attached cursor hasn't read 3..7, so only 4 slots are free.
        assert_eq!(w.slice_mut().len(), 1, "up to the wrap point");
        let mut out = [0u8; 16];
        let got = a.read(&mut out);
        assert_eq!(got, TapRead { start: 3, items: 4 });
        assert_eq!(&out[..4], &[4, 5, 6, 7]);
        let t = a.pop_tag().unwrap();
        assert_eq!((t.offset, &*t.key), (3, "rx_freq"));
        assert!(a.pop_tag().is_none());

        // Every spare can be claimed once; dropping one frees it.
        let b = tap.attach().unwrap();
        assert!(tap.attach().is_none());
        drop(a);
        let c = tap.attach().expect("freed");
        assert_eq!(c.position(), 7);
        drop((b, c));
        // Detached cursors don't hold the writer back.
        assert_eq!(w.slice_mut().len(), 1);
        w.commit(1);
        assert_eq!(w.slice_mut().len(), 7);
        // Nor do spares count as readers for abandonment.
        assert!(!w.abandoned());
        rs[0].finish();
        assert!(w.abandoned());
    }

    /// Reading frees space, so it must wake a writer that waits for it.
    #[test]
    fn attached_reader_wakes_the_writer() {
        use std::sync::atomic::AtomicUsize;
        struct Count(AtomicUsize);
        impl std::task::Wake for Count {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let (mut w, mut rs) = channel_with(1, 4, 1, BufferKind::Plain);
        w.register_waker(Waker::from(count.clone()));
        let mut a = w.tap().attach().unwrap();
        w.slice_mut()[..4].copy_from_slice(&[1, 2, 3, 4]);
        w.commit(4);
        rs[0].consume(4);
        assert_eq!(w.slice_mut().len(), 0, "the attached cursor holds it");
        let before = count.0.load(Ordering::Relaxed);
        assert_eq!(a.read(&mut [0u8; 8]).items, 4);
        assert!(count.0.load(Ordering::Relaxed) > before, "writer woken");
        assert_eq!(w.slice_mut().len(), 4);
    }

    /// A writer racing an attached reader: after the first item, nothing is
    /// ever skipped, and the data is exactly what was written.
    #[test]
    fn attached_reader_is_lossless() {
        const TOTAL: u32 = 1_000_000;
        for kind in BOTH {
            let (mut w, mut rs) = channel_with(4, 64, 1, kind);
            let tap = w.tap();
            let mut r = rs.pop().unwrap();
            let mut a = tap.attach().unwrap();
            let writer = std::thread::spawn(move || {
                let mut next = 0u32;
                while next < TOTAL {
                    let buf: &mut [u32] = bytemuck::cast_slice_mut(w.slice_mut());
                    if buf.is_empty() {
                        std::thread::yield_now();
                        continue;
                    }
                    let n = buf.len().min((TOTAL - next) as usize);
                    for (i, v) in buf[..n].iter_mut().enumerate() {
                        *v = next + i as u32;
                    }
                    w.commit(n);
                    next += n as u32;
                }
            });
            let reader = std::thread::spawn(move || {
                while !r.exhausted() {
                    let n = r.slice().len() / 4;
                    r.consume(n);
                }
            });
            let mut out = vec![0u32; 48];
            let mut next = 0usize;
            while !a.finished() {
                let got = a.read(bytemuck::cast_slice_mut(&mut out));
                assert_eq!(got.start, next, "nothing skipped ({kind:?})");
                for (i, v) in out[..got.items].iter().enumerate() {
                    assert_eq!(*v as usize, got.start + i, "torn read ({kind:?})");
                }
                next += got.items;
            }
            writer.join().unwrap();
            reader.join().unwrap();
            assert_eq!(next, TOTAL as usize);
        }
    }
}
