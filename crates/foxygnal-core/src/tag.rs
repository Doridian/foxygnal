//! Stream tags: key/value annotations attached to one item of a stream
//! (`rx_freq` after a retune, `burst_start`, `rx_time`).
//!
//! Tags travel next to the ring buffer, not inside it. Each read cursor has
//! its own small lock-free queue; the writer pushes a tag into every queue
//! *before* it publishes the items the tag points at, so a reader that can
//! see an item can also see its tags. Values are cheap to clone (strings are
//! `Arc<str>`), so passing a tag on never allocates.
//!
//! Tags are meant to be rare (a handful per buffer). When a reader's queue is
//! full, new tags for it are dropped and counted rather than blocking.

use std::cell::UnsafeCell;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Tags each reader can hold before new ones are dropped.
pub const TAG_QUEUE: usize = 256;

/// A tag value.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(untagged)]
pub enum TagValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Arc<str>),
}

impl fmt::Display for TagValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TagValue::Bool(v) => write!(f, "{v}"),
            TagValue::Int(v) => write!(f, "{v}"),
            TagValue::Float(v) => write!(f, "{v}"),
            TagValue::Str(v) => write!(f, "{v:?}"),
        }
    }
}

impl From<bool> for TagValue {
    fn from(v: bool) -> Self {
        TagValue::Bool(v)
    }
}
impl From<i64> for TagValue {
    fn from(v: i64) -> Self {
        TagValue::Int(v)
    }
}
impl From<f64> for TagValue {
    fn from(v: f64) -> Self {
        TagValue::Float(v)
    }
}
impl From<&str> for TagValue {
    fn from(v: &str) -> Self {
        TagValue::Str(v.into())
    }
}
impl From<Arc<str>> for TagValue {
    fn from(v: Arc<str>) -> Self {
        TagValue::Str(v)
    }
}

/// A tag on one item of a stream.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Tag {
    /// Absolute index of the item in its stream (0 = the first item ever written).
    pub offset: u64,
    pub key: Arc<str>,
    pub value: TagValue,
}

impl Tag {
    pub fn new(offset: u64, key: impl Into<Arc<str>>, value: impl Into<TagValue>) -> Self {
        Tag {
            offset,
            key: key.into(),
            value: value.into(),
        }
    }
}

/// How the runtime passes a block's input tags on to its outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagPropagation {
    /// Every tag on a consumed input item goes to every output, its offset
    /// scaled by the items produced per item consumed in that `work` call.
    /// Right for 1:1 blocks, decimators and interpolators.
    AllToAll,
    /// The block forwards (or drops) tags itself with `OutputPort::add_tag`.
    Manual,
}

/// Single-producer / single-consumer bounded queue of tags.
pub(crate) struct TagQueue {
    slots: Box<[UnsafeCell<Option<Tag>>]>,
    /// Next slot to pop (consumer-owned).
    head: AtomicUsize,
    /// Next slot to push (producer-owned).
    tail: AtomicUsize,
    dropped: AtomicUsize,
}

// SAFETY: the producer only writes slots in [tail, head + len) and the
// consumer only takes slots in [head, tail); the cursors hand slots over with
// Release/Acquire. The ring guarantees a single producer and consumer.
unsafe impl Sync for TagQueue {}
unsafe impl Send for TagQueue {}

impl TagQueue {
    pub(crate) fn new(len: usize) -> Self {
        TagQueue {
            slots: (0..len).map(|_| UnsafeCell::new(None)).collect(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
        }
    }

    /// Producer side. Returns false (and counts a drop) when full.
    pub(crate) fn push(&self, tag: Tag) -> bool {
        let t = self.tail.load(Ordering::Relaxed);
        let h = self.head.load(Ordering::Acquire);
        if t - h == self.slots.len() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        // SAFETY: slot t is outside [head, tail), so the consumer won't touch it.
        unsafe { *self.slots[t % self.slots.len()].get() = Some(tag) };
        self.tail.store(t + 1, Ordering::Release);
        true
    }

    /// Consumer side.
    pub(crate) fn pop(&self) -> Option<Tag> {
        let h = self.head.load(Ordering::Relaxed);
        let t = self.tail.load(Ordering::Acquire);
        if h == t {
            return None;
        }
        // SAFETY: slot h is in [head, tail): written by the producer, ours to take.
        let tag = unsafe { (*self.slots[h % self.slots.len()].get()).take() };
        self.head.store(h + 1, Ordering::Release);
        tag
    }

    pub(crate) fn dropped(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_is_fifo_and_drops_when_full() {
        let q = TagQueue::new(2);
        assert!(q.push(Tag::new(1, "a", 1i64)));
        assert!(q.push(Tag::new(2, "b", true)));
        assert!(!q.push(Tag::new(3, "c", 1.5)));
        assert_eq!(q.dropped(), 1);
        assert_eq!(q.pop().unwrap().offset, 1);
        assert!(q.push(Tag::new(4, "d", "x")));
        assert_eq!(q.pop().unwrap().offset, 2);
        assert_eq!(q.pop().unwrap().value, TagValue::from("x"));
        assert!(q.pop().is_none());
    }
}
