//! A lock-free single-producer / single-consumer ring of samples between a
//! driver's thread and a block's `work()`.
//!
//! The producer (driver thread) writes with [`Producer::push`] and the
//! consumer is woken by unparking its registered thread. When the consumer
//! falls behind, `push` drops the newest samples and counts them: an SDR
//! can't be paused, so dropping is the only option, and dropping whole
//! chunks keeps what does get through contiguous.
//!
//! Neither side allocates or locks after [`ring`].

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::Thread;

struct Shared<T> {
    slots: Box<[UnsafeCell<T>]>,
    mask: usize,
    /// Total items written (wraps; only differences matter).
    head: AtomicUsize,
    /// Total items read.
    tail: AtomicUsize,
    dropped: AtomicU64,
    closed: AtomicBool,
    consumer: OnceLock<Thread>,
    producer: OnceLock<Thread>,
}

// SAFETY: the producer only writes slots in [head, tail + cap) and the consumer
// only reads slots in [tail, head); the cursors hand slots over with
// Release/Acquire. `Producer`/`Consumer` aren't Clone, so there is exactly one
// of each.
unsafe impl<T: Send> Sync for Shared<T> {}
unsafe impl<T: Send> Send for Shared<T> {}

/// A ring holding at least `min_capacity` items (rounded up to a power of two).
pub fn ring<T: Copy + Default + Send>(min_capacity: usize) -> (Producer<T>, Consumer<T>) {
    let cap = min_capacity.max(2).next_power_of_two();
    let shared = Arc::new(Shared {
        slots: (0..cap).map(|_| UnsafeCell::new(T::default())).collect(),
        mask: cap - 1,
        head: AtomicUsize::new(0),
        tail: AtomicUsize::new(0),
        dropped: AtomicU64::new(0),
        closed: AtomicBool::new(false),
        consumer: OnceLock::new(),
        producer: OnceLock::new(),
    });
    (
        Producer {
            shared: shared.clone(),
        },
        Consumer { shared },
    )
}

pub struct Producer<T> {
    shared: Arc<Shared<T>>,
}

impl<T: Copy> Producer<T> {
    pub fn capacity(&self) -> usize {
        self.shared.slots.len()
    }

    /// Free space right now.
    pub fn free(&self) -> usize {
        let s = &*self.shared;
        s.slots.len()
            - s.head
                .load(Ordering::Relaxed)
                .wrapping_sub(s.tail.load(Ordering::Acquire))
    }

    /// Write all of `items` if they fit, else none (counted as dropped).
    /// Wakes the consumer. Returns whether they were written.
    pub fn push(&mut self, items: &[T]) -> bool {
        if items.len() > self.free() {
            self.shared
                .dropped
                .fetch_add(items.len() as u64, Ordering::Relaxed);
            self.wake();
            return false;
        }
        self.write(items);
        true
    }

    /// Write as many of `items` as fit; returns how many. Wakes the consumer.
    pub fn push_partial(&mut self, items: &[T]) -> usize {
        let n = items.len().min(self.free());
        self.write(&items[..n]);
        n
    }

    fn write(&mut self, items: &[T]) {
        let s = &*self.shared;
        let head = s.head.load(Ordering::Relaxed);
        for (i, v) in items.iter().enumerate() {
            // SAFETY: the slot is free (checked by the caller); only we write it.
            unsafe { *s.slots[(head + i) & s.mask].get() = *v };
        }
        s.head
            .store(head.wrapping_add(items.len()), Ordering::Release);
        self.wake();
    }

    fn wake(&self) {
        if let Some(t) = self.shared.consumer.get() {
            t.unpark();
        }
    }

    /// The thread to unpark when the consumer frees space (for TX).
    pub fn set_waker(&self, t: Thread) {
        let _ = self.shared.producer.set(t);
    }

    /// Mark the stream ended; the consumer drains what's left.
    pub fn close(&self) {
        self.shared.closed.store(true, Ordering::Release);
        self.wake();
    }

    /// The consumer is gone.
    pub fn abandoned(&self) -> bool {
        Arc::strong_count(&self.shared) == 1
    }
}

impl<T> Drop for Producer<T> {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Release);
        if let Some(t) = self.shared.consumer.get() {
            t.unpark();
        }
    }
}

pub struct Consumer<T> {
    shared: Arc<Shared<T>>,
}

impl<T: Copy> Consumer<T> {
    /// Items waiting.
    pub fn len(&self) -> usize {
        let s = &*self.shared;
        s.head
            .load(Ordering::Acquire)
            .wrapping_sub(s.tail.load(Ordering::Relaxed))
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copy up to `out.len()` items; returns how many. Wakes the producer.
    pub fn pop(&mut self, out: &mut [T]) -> usize {
        let s = &*self.shared;
        let tail = s.tail.load(Ordering::Relaxed);
        let n = out.len().min(self.len());
        for (i, o) in out[..n].iter_mut().enumerate() {
            // SAFETY: the slot is filled (below head); only we read it.
            *o = unsafe { *s.slots[(tail + i) & s.mask].get() };
        }
        s.tail.store(tail.wrapping_add(n), Ordering::Release);
        if n > 0
            && let Some(t) = s.producer.get()
        {
            t.unpark();
        }
        n
    }

    /// The thread to unpark when items arrive (the block's thread).
    pub fn set_waker(&self, t: Thread) {
        let _ = self.shared.consumer.set(t);
    }

    /// Items the producer dropped because the ring was full.
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// The producer closed (or was dropped) and everything was read.
    pub fn finished(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire) && self.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_drops_and_finishes() {
        let (mut p, mut c) = ring::<u32>(4);
        assert!(p.push(&[1, 2, 3]));
        assert!(!p.push(&[4, 5]));
        assert_eq!(c.dropped(), 2);
        let mut out = [0; 2];
        assert_eq!(c.pop(&mut out), 2);
        assert_eq!(out, [1, 2]);
        assert!(p.push(&[4, 5, 6]));
        let mut out = [0; 8];
        assert_eq!(c.pop(&mut out), 4);
        assert_eq!(&out[..4], &[3, 4, 5, 6]);
        assert!(!c.finished());
        drop(p);
        assert!(c.finished());
    }

    #[test]
    fn threads_see_every_item_in_order() {
        let (mut p, mut c) = ring::<u64>(64);
        let t = std::thread::spawn(move || {
            let mut next = 0u64;
            while next < 100_000 {
                let chunk: Vec<u64> = (next..next + 7).collect();
                next += p.push_partial(&chunk) as u64;
            }
        });
        let mut expect = 0;
        let mut buf = [0u64; 16];
        while expect < 100_000 {
            let n = c.pop(&mut buf);
            for v in &buf[..n] {
                assert_eq!(*v, expect);
                expect += 1;
            }
        }
        t.join().unwrap();
    }
}
