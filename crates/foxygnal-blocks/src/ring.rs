//! A fixed-size, lock-free single-producer / single-consumer ring of `f32`.
//!
//! Used between a block's `work()` and a realtime callback (the audio device),
//! where neither side may lock or allocate. Samples are stored as `AtomicU32`
//! bit patterns, so there is no `unsafe`: the producer writes slots with
//! relaxed stores and publishes them with a release store of `head`; the
//! consumer acquires `head`, reads, and releases the slots by storing `tail`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

struct Shared {
    slots: Box<[AtomicU32]>,
    mask: usize,
    /// Total samples ever written (wraps; only differences matter).
    head: AtomicUsize,
    /// Total samples ever read.
    tail: AtomicUsize,
}

impl Shared {
    fn len(&self) -> usize {
        self.head
            .load(Ordering::Acquire)
            .wrapping_sub(self.tail.load(Ordering::Acquire))
    }
}

/// Create a ring holding at least `min_capacity` samples (rounded up to a
/// power of two). Allocates once, here.
pub fn ring(min_capacity: usize) -> (Producer, Consumer) {
    let cap = min_capacity.max(2).next_power_of_two();
    let shared = Arc::new(Shared {
        slots: (0..cap).map(|_| AtomicU32::new(0)).collect(),
        mask: cap - 1,
        head: AtomicUsize::new(0),
        tail: AtomicUsize::new(0),
    });
    (
        Producer {
            shared: shared.clone(),
        },
        Consumer { shared },
    )
}

/// The writing half.
pub struct Producer {
    shared: Arc<Shared>,
}

impl Producer {
    pub fn capacity(&self) -> usize {
        self.shared.slots.len()
    }

    /// Samples waiting to be read.
    pub fn len(&self) -> usize {
        self.shared.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Free space in samples.
    pub fn free(&self) -> usize {
        self.capacity() - self.len()
    }

    /// Write up to `n` samples produced by `f(i)`; returns how many fit.
    pub fn push_with(&mut self, n: usize, mut f: impl FnMut(usize) -> f32) -> usize {
        let s = &*self.shared;
        let head = s.head.load(Ordering::Relaxed);
        let tail = s.tail.load(Ordering::Acquire);
        let n = n.min(s.slots.len() - head.wrapping_sub(tail));
        for i in 0..n {
            s.slots[head.wrapping_add(i) & s.mask].store(f(i).to_bits(), Ordering::Relaxed);
        }
        s.head.store(head.wrapping_add(n), Ordering::Release);
        n
    }

    /// Write as much of `data` as fits; returns how many samples were written.
    pub fn push(&mut self, data: &[f32]) -> usize {
        self.push_with(data.len(), |i| data[i])
    }
}

/// The reading half.
pub struct Consumer {
    shared: Arc<Shared>,
}

impl Consumer {
    pub fn capacity(&self) -> usize {
        self.shared.slots.len()
    }

    /// Samples waiting to be read.
    pub fn len(&self) -> usize {
        self.shared.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Read up to `n` samples, handing each to `f(i, sample)`; returns how
    /// many were read.
    pub fn pop_with(&mut self, n: usize, mut f: impl FnMut(usize, f32)) -> usize {
        let s = &*self.shared;
        let tail = s.tail.load(Ordering::Relaxed);
        let head = s.head.load(Ordering::Acquire);
        let n = n.min(head.wrapping_sub(tail));
        for i in 0..n {
            f(
                i,
                f32::from_bits(s.slots[tail.wrapping_add(i) & s.mask].load(Ordering::Relaxed)),
            );
        }
        s.tail.store(tail.wrapping_add(n), Ordering::Release);
        n
    }

    /// Fill the front of `out`; returns how many samples were read.
    pub fn pop(&mut self, out: &mut [f32]) -> usize {
        let n = out.len();
        self.pop_with(n, |i, v| out[i] = v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_and_keeps_order() {
        let (mut p, mut c) = ring(5);
        assert_eq!(p.capacity(), 8);
        let mut next = 0.0f32;
        let mut expect = 0.0f32;
        let mut out = [0.0f32; 3];
        for _ in 0..1000 {
            let chunk: Vec<f32> = (0..5).map(|i| next + i as f32).collect();
            let n = p.push(&chunk);
            next += n as f32;
            let got = c.pop(&mut out);
            for &v in &out[..got] {
                assert_eq!(v, expect);
                expect += 1.0;
            }
        }
        assert!(expect > 2000.0);
    }

    #[test]
    fn full_and_empty() {
        let (mut p, mut c) = ring(4);
        assert_eq!(p.push(&[1.0; 10]), 4);
        assert_eq!(p.free(), 0);
        assert_eq!(p.push(&[1.0]), 0);
        let mut out = [0.0; 10];
        assert_eq!(c.pop(&mut out), 4);
        assert!(c.is_empty());
        assert_eq!(c.pop(&mut out), 0);
    }

    #[test]
    fn across_threads() {
        let (mut p, mut c) = ring(64);
        const N: usize = 200_000;
        let t = std::thread::spawn(move || {
            let mut i = 0;
            while i < N {
                i += p.push_with(N - i, |k| (i + k) as f32);
                std::hint::spin_loop();
            }
        });
        let mut want = 0usize;
        while want < N {
            c.pop_with(usize::MAX, |_, v| {
                assert_eq!(v, want as f32);
                want += 1;
            });
        }
        t.join().unwrap();
    }
}
