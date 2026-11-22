//! Lock-free single-writer / multi-reader ring buffer connecting blocks.
//!
//! One output port owns one [`Ring`]; every downstream connection gets its own
//! read cursor, so fan-out costs nothing extra. The writer may only overwrite
//! items that *all* readers have consumed.
//!
//! M0 limitation: slices are only contiguous up to the wrap point, so a block
//! may see a short slice near the end of the buffer. A double-mapped
//! (memfd + mmap) buffer removes that later — see docs/ARCHITECTURE.md.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::Thread;

/// Default buffer capacity in items.
pub const DEFAULT_CAPACITY: usize = 1 << 15;

struct ReaderState {
    /// Total items consumed by this reader (monotonic).
    read: AtomicUsize,
    /// The reader block has terminated and will never read again.
    done: AtomicBool,
    thread: OnceLock<Thread>,
}

struct Ring {
    // u64 backing guarantees 8-byte alignment, enough for every StreamType.
    storage: Box<[UnsafeCell<u64>]>,
    item_size: usize,
    capacity: usize,
    /// Total items committed by the writer (monotonic).
    written: AtomicUsize,
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
        writer_done: AtomicBool::new(false),
        writer_thread: OnceLock::new(),
        readers: (0..readers)
            .map(|_| ReaderState {
                read: AtomicUsize::new(0),
                done: AtomicBool::new(false),
                thread: OnceLock::new(),
            })
            .collect(),
    });
    let rs = (0..readers)
        .map(|idx| Reader {
            ring: ring.clone(),
            idx,
        })
        .collect();
    (Writer { ring }, rs)
}

/// The unique write end of a ring. Not `Clone`: `&mut self` guarantees exclusivity.
pub struct Writer {
    ring: Arc<Ring>,
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
        let written = r.written.load(Ordering::Relaxed);
        let free = r.capacity - (written - r.min_read());
        let start = written % r.capacity;
        let n = free.min(r.capacity - start);
        // SAFETY: region [start, start+n) is not readable by any reader until
        // `commit`, and `&mut self` excludes other writers.
        unsafe {
            std::slice::from_raw_parts_mut(r.base().add(start * r.item_size), n * r.item_size)
        }
    }

    /// Publish `items` items previously written into [`Self::slice_mut`].
    pub fn commit(&mut self, items: usize) {
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

    /// Mark end of stream and wake readers.
    pub fn finish(&self) {
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
}
