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
//! M0 limitation: slices are only contiguous up to the wrap point, so a block
//! may see a short slice near the end of the buffer. A double-mapped
//! (memfd + mmap) buffer removes that later — see docs/ARCHITECTURE.md.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering, fence};
use std::sync::{Arc, OnceLock};
use std::thread::Thread;

use crate::tag::{TAG_QUEUE, Tag, TagQueue};

/// Default buffer capacity in items.
pub const DEFAULT_CAPACITY: usize = 1 << 15;

struct ReaderState {
    /// Total items consumed by this reader (monotonic).
    read: AtomicUsize,
    /// The reader block has terminated and will never read again.
    done: AtomicBool,
    thread: OnceLock<Thread>,
    /// Tags on their way to this reader.
    tags: TagQueue,
}

struct Ring {
    // u64 backing guarantees 8-byte alignment, enough for every StreamType.
    storage: Box<[UnsafeCell<u64>]>,
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
    writer_thread: OnceLock<Thread>,
    readers: Box<[ReaderState]>,
}

// SAFETY: access to `storage` is coordinated via the atomic cursors; the writer
// only touches the free region and readers only touch committed regions.
unsafe impl Sync for Ring {}
unsafe impl Send for Ring {}

impl Ring {
    fn base(&self) -> *mut u8 {
        self.storage.as_ptr() as *mut u8
    }

    fn min_read(&self) -> usize {
        self.readers
            .iter()
            .filter(|r| !r.done.load(Ordering::Acquire))
            .map(|r| r.read.load(Ordering::Acquire))
            .min()
            .unwrap_or_else(|| self.written.load(Ordering::Relaxed))
    }
}

/// Create a ring with `readers` read cursors.
pub fn channel(item_size: usize, capacity: usize, readers: usize) -> (Writer, Vec<Reader>) {
    assert!(item_size > 0 && capacity > 0);
    let words = (item_size * capacity).div_ceil(8);
    let ring = Arc::new(Ring {
        storage: (0..words).map(|_| UnsafeCell::new(0)).collect(),
        item_size,
        capacity,
        written: AtomicUsize::new(0),
        reserved: AtomicUsize::new(0),
        writer_done: AtomicBool::new(false),
        writer_thread: OnceLock::new(),
        readers: (0..readers)
            .map(|_| ReaderState {
                read: AtomicUsize::new(0),
                done: AtomicBool::new(false),
                thread: OnceLock::new(),
                tags: TagQueue::new(TAG_QUEUE),
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

    /// Register the calling thread to be woken when readers free space.
    pub fn register_thread(&self) {
        let _ = self.ring.writer_thread.set(std::thread::current());
    }

    /// Contiguous writable region, in bytes. Empty once [`Self::abandoned`].
    pub fn slice_mut(&mut self) -> &mut [u8] {
        if self.abandoned() {
            return &mut [];
        }
        let r = &*self.ring;
        let written = r.written.load(Ordering::Relaxed) + self.staged;
        let free = r.capacity - (written - r.min_read());
        let start = written % r.capacity;
        let n = free.min(r.capacity - start);
        // Announce the region before writing into it, for taps (seqlock-style).
        r.reserved.store(written + n, Ordering::Relaxed);
        fence(Ordering::Release);
        // SAFETY: region [start, start+n) is not readable by any reader until
        // `commit`, and `&mut self` excludes other writers.
        unsafe {
            std::slice::from_raw_parts_mut(r.base().add(start * r.item_size), n * r.item_size)
        }
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
        for rd in self.ring.readers.iter() {
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
        for rd in r.readers.iter() {
            if let Some(t) = rd.thread.get() {
                t.unpark();
            }
        }
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
        for rd in self.ring.readers.iter() {
            if let Some(t) = rd.thread.get() {
                t.unpark();
            }
        }
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

    /// Register the calling thread to be woken when new data arrives.
    pub fn register_thread(&self) {
        let _ = self.state().thread.set(std::thread::current());
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
        let start = read % r.capacity;
        let n = avail.min(r.capacity - start);
        // SAFETY: committed region; the writer won't touch it until we consume.
        unsafe { std::slice::from_raw_parts(r.base().add(start * r.item_size), n * r.item_size) }
    }

    pub fn consume(&mut self, items: usize) {
        if items == 0 {
            return;
        }
        self.state().read.fetch_add(items, Ordering::Release);
        if let Some(t) = self.ring.writer_thread.get() {
            t.unpark();
        }
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
        if let Some(t) = self.ring.writer_thread.get() {
            t.unpark();
        }
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

    /// Oldest stream index that is still intact.
    fn oldest(&self, order: Ordering) -> usize {
        self.ring
            .reserved
            .load(order)
            .saturating_sub(self.ring.capacity)
    }

    /// Copy committed items starting at stream index `from` into `out`, as
    /// many as fit. Items that were already overwritten are skipped, so check
    /// [`TapRead::start`]. Never blocks and never holds the writer back.
    pub fn read(&self, from: usize, out: &mut [u8]) -> TapRead {
        let r = &*self.ring;
        let size = r.item_size;
        let written = r.written.load(Ordering::Acquire);
        let mut start = from.max(self.oldest(Ordering::Acquire)).min(written);
        let mut items = (written - start).min(out.len() / size);
        let first = items.min(r.capacity - start % r.capacity);
        // SAFETY: both ranges are inside the storage. The writer may be
        // overwriting the oldest part concurrently (a seqlock-style race);
        // the check below finds and discards anything it may have touched.
        unsafe {
            let base = r.base();
            std::ptr::copy_nonoverlapping(
                base.add(start % r.capacity * size),
                out.as_mut_ptr(),
                first * size,
            );
            std::ptr::copy_nonoverlapping(
                base,
                out.as_mut_ptr().add(first * size),
                (items - first) * size,
            );
        }
        fence(Ordering::Acquire);
        let oldest = self.oldest(Ordering::Relaxed);
        if oldest > start {
            let lost = (oldest - start).min(items);
            out.copy_within(lost * size..items * size, 0);
            start += lost;
            items -= lost;
        }
        TapRead { start, items }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_and_fans_out() {
        let (mut w, mut rs) = channel(4, 8, 2);
        for round in 0..10u32 {
            let buf = w.slice_mut();
            let n = (buf.len() / 4).min(3);
            let vals: Vec<u32> = (0..n as u32).map(|i| round * 10 + i).collect();
            buf[..n * 4].copy_from_slice(bytemuck::cast_slice(&vals));
            w.commit(n);
            for r in rs.iter_mut() {
                let mut got = Vec::new();
                while got.len() < n {
                    let s: &[u32] = bytemuck::cast_slice(r.slice());
                    let k = s.len();
                    got.extend_from_slice(s);
                    r.consume(k);
                }
                assert_eq!(got, vals);
            }
        }
    }

    #[test]
    fn slowest_reader_limits_writer() {
        let (mut w, mut rs) = channel(1, 4, 2);
        w.slice_mut()[..4].copy_from_slice(&[1, 2, 3, 4]);
        w.commit(4);
        rs[0].consume(4);
        assert_eq!(w.slice_mut().len(), 0, "reader 1 still holds the data");
        rs[1].consume(2);
        assert_eq!(w.slice_mut().len(), 2);
    }

    #[test]
    fn tap_reads_without_holding_the_writer_back() {
        let (mut w, mut rs) = channel(1, 8, 1);
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

    /// A writer racing a fast reader and a tap: everything the tap returns
    /// must be exactly what was written at that index, never torn data.
    #[test]
    fn tap_never_returns_overwritten_data() {
        const TOTAL: u32 = 2_000_000;
        let (mut w, mut rs) = channel(4, 64, 1);
        let tap = w.tap();
        let mut r = rs.pop().unwrap();
        let writer = std::thread::spawn(move || {
            let mut next = 0u32;
            while next < TOTAL {
                let buf: &mut [u32] = bytemuck::cast_slice_mut(w.slice_mut());
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
        let mut checked = 0usize;
        while !tap.finished() || checked == 0 {
            let from = tap.written().saturating_sub(40);
            let got = tap.read(from, bytemuck::cast_slice_mut(&mut out));
            for (i, v) in out[..got.items].iter().enumerate() {
                assert_eq!(*v as usize, got.start + i, "torn read");
            }
            checked += got.items;
        }
        writer.join().unwrap();
        reader.join().unwrap();
        assert!(checked > 0);
    }
}
