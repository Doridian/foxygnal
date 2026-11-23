//! The epoch clock behind call deadlines.
//!
//! wasmtime interrupts a call once the engine's epoch passes the store's
//! deadline. A thread advances the epoch every [`TICK`] while any plugin call
//! is running, and parks (no timeout, so an idle engine uses no CPU) after a
//! second without one.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::thread::Thread;
use std::time::Duration;

use wasmtime::Engine;

pub const TICK: Duration = Duration::from_millis(10);
/// Ticks without a call before the thread parks.
const IDLE_TICKS: u32 = 100;

struct Inner {
    /// Plugin calls in progress.
    busy: AtomicUsize,
    /// The thread is parked (or about to): a caller must unpark it.
    sleeping: AtomicBool,
    /// The host is gone: the thread exits.
    quit: AtomicBool,
}

pub struct Ticker {
    inner: Arc<Inner>,
    thread: Thread,
}

/// Marks a plugin call in progress while it lives.
pub struct Busy<'a>(&'a Ticker);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.inner.busy.fetch_sub(1, SeqCst);
    }
}

impl Ticker {
    pub fn new(engine: Engine) -> Self {
        let inner = Arc::new(Inner {
            busy: AtomicUsize::new(0),
            sleeping: AtomicBool::new(false),
            quit: AtomicBool::new(false),
        });
        let i = inner.clone();
        let handle = std::thread::Builder::new()
            .name("fox:plugin-epoch".into())
            .spawn(move || run(&engine, &i))
            .expect("spawning the plugin epoch thread");
        Ticker {
            inner,
            thread: handle.thread().clone(),
        }
    }

    /// Call before entering a plugin; the deadline counts from here.
    pub fn enter(&self) -> Busy<'_> {
        if self.inner.busy.fetch_add(1, SeqCst) == 0 && self.inner.sleeping.load(SeqCst) {
            self.thread.unpark();
        }
        Busy(self)
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.inner.quit.store(true, SeqCst);
        self.thread.unpark();
    }
}

fn run(engine: &Engine, inner: &Inner) {
    let mut idle = 0;
    while !inner.quit.load(SeqCst) {
        std::thread::sleep(TICK);
        engine.increment_epoch();
        if inner.busy.load(SeqCst) > 0 {
            idle = 0;
            continue;
        }
        idle += 1;
        if idle < IDLE_TICKS {
            continue;
        }
        // Announce the nap, then look again: a caller that entered before
        // seeing `sleeping` is caught by the second check.
        inner.sleeping.store(true, SeqCst);
        while inner.busy.load(SeqCst) == 0 && !inner.quit.load(SeqCst) {
            std::thread::park();
        }
        inner.sleeping.store(false, SeqCst);
        idle = 0;
    }
}
