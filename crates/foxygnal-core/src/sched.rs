//! The worker pool that runs blocks.
//!
//! Each pooled block is a [`Task`]. Neighbours schedule it through a
//! [`std::task::Waker`] (the ring buffers call it when data or space appears),
//! and a [`Next::Until`] timer schedules it when it asked to wait. A fixed set
//! of worker threads takes scheduled tasks from per-worker queues (stealing
//! from each other and from a global queue) and runs them.
//!
//! A task's state machine guarantees a block never runs on two workers at
//! once and that no wake-up is lost:
//!
//! ```text
//!   IDLE ──wake──▶ SCHEDULED ──worker──▶ RUNNING ──Idle/Until──▶ IDLE
//!                      ▲                  │   ▲
//!                      │                 wake │
//!                      │                  ▼   │
//!                      └──── Idle ──── NOTIFIED (run again)
//! ```
//!
//! A wake that arrives while the task runs moves it to NOTIFIED, so the
//! worker queues it again instead of letting it go idle. Waking an IDLE task
//! is the only transition that queues it, so it is queued at most once.
//! The wake path is a single compare-and-swap (plus a lock-free queue push
//! when the task was idle); it never blocks.

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering, fence};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::task::{Wake, Waker};
use std::thread::{JoinHandle, Thread};
use std::time::Instant;

use crossbeam_deque::{Injector, Steal, Stealer, Worker};

/// What a task wants after one turn on a worker.
pub(crate) enum Next {
    /// Made progress and may have more to do: queue it again.
    Again,
    /// Nothing to do until a neighbour wakes it.
    Idle,
    /// Nothing to do until this time (or an earlier wake).
    Until(Instant),
    /// Finished for good.
    Done,
}

/// The work a task does each time it is scheduled.
pub(crate) trait Job: Send {
    fn run(&mut self) -> Next;
}

/// How many times an idle worker looks for work before it sleeps.
const SPIN: usize = 16;
/// Every this many tasks, a worker checks the global queue before its own.
const GLOBAL_EVERY: u32 = 31;

const IDLE: u8 = 0;
const SCHEDULED: u8 = 1;
const RUNNING: u8 = 2;
const NOTIFIED: u8 = 3;
const DONE: u8 = 4;

pub(crate) struct Task {
    state: AtomicU8,
    /// Bumped every time the task runs: timers armed before that are stale.
    timer_gen: AtomicU64,
    /// Only the worker that moved the task to RUNNING touches the job, so the
    /// lock is never contended; it is here to make that exclusivity checked
    /// rather than assumed.
    job: Mutex<Option<Box<dyn Job>>>,
    pool: Arc<Shared>,
}

impl Task {
    fn schedule(self: &Arc<Self>) {
        let mut s = self.state.load(Ordering::Acquire);
        loop {
            let next = match s {
                IDLE => SCHEDULED,
                RUNNING => NOTIFIED,
                // Already queued, already going to run again, or finished.
                _ => return,
            };
            match self
                .state
                .compare_exchange_weak(s, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    if next == SCHEDULED {
                        self.pool.push(self.clone());
                    }
                    return;
                }
                Err(actual) => s = actual,
            }
        }
    }

    /// After a turn that ended idle: go idle, unless woken meanwhile.
    fn idle_or_requeue(self: &Arc<Self>) {
        if self
            .state
            .compare_exchange(RUNNING, IDLE, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // NOTIFIED: a wake arrived while running. Run again.
            self.state.store(SCHEDULED, Ordering::Release);
            self.pool.push(self.clone());
        }
    }

    pub(crate) fn waker(self: &Arc<Self>) -> Waker {
        Waker::from(self.clone())
    }

    /// Give the task its job. Call before [`Pool::start`].
    pub(crate) fn set_job(&self, job: Box<dyn Job>) {
        *self.job.lock().unwrap_or_else(PoisonError::into_inner) = Some(job);
    }
}

impl Wake for Task {
    fn wake(self: Arc<Self>) {
        self.schedule();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.schedule();
    }
}

/// A waker that unparks a thread: for blocks on a dedicated thread.
pub(crate) fn thread_waker(thread: Thread) -> Waker {
    struct Unpark(Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }
    Waker::from(Arc::new(Unpark(thread)))
}

struct Sleeper {
    asleep: AtomicBool,
    thread: OnceLock<Thread>,
}

struct Timer {
    at: Instant,
    seq: u64,
    generation: u64,
    task: Arc<Task>,
}

impl PartialEq for Timer {
    fn eq(&self, other: &Self) -> bool {
        (self.at, self.seq) == (other.at, other.seq)
    }
}
impl Eq for Timer {}
impl PartialOrd for Timer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Timer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.at, self.seq).cmp(&(other.at, other.seq))
    }
}

#[derive(Default)]
struct Timers {
    heap: BinaryHeap<Reverse<Timer>>,
    seq: u64,
}

struct Shared {
    injector: Injector<Arc<Task>>,
    stealers: OnceLock<Box<[Stealer<Arc<Task>>]>>,
    sleepers: Box<[Sleeper]>,
    /// Workers asleep or about to sleep (see `Shared::sleep`).
    sleeping: AtomicUsize,
    /// Awake workers looking for work.
    searching: AtomicUsize,
    /// Tasks not yet DONE; the pool shuts down when it reaches 0.
    live: AtomicUsize,
    shutdown: AtomicBool,
    timers: Mutex<Timers>,
    timer_thread: OnceLock<Thread>,
}

/// A worker's pool (only compared, never dereferenced) and its own queue.
type Local = (*const Shared, Worker<Arc<Task>>);

thread_local! {
    /// The worker running on this thread, if any.
    static LOCAL: RefCell<Option<Local>> = const { RefCell::new(None) };
}

impl Shared {
    /// Queue a scheduled task: on the current worker's own queue when called
    /// from one of our workers (the data it just produced is in this core's
    /// cache), on the global queue otherwise.
    fn push(&self, task: Arc<Task>) {
        let mut task = Some(task);
        // `try_with`: a thread being torn down has no worker queue anyway.
        let _ = LOCAL.try_with(|l| {
            if let Some((pool, queue)) = &*l.borrow()
                && std::ptr::eq(*pool, self)
            {
                queue.push(task.take().expect("not taken yet"));
            }
        });
        if let Some(task) = task {
            self.injector.push(task);
        }
        self.notify_one();
    }

    /// Wake one sleeping worker, if any, to pick up new work.
    fn notify_one(&self) {
        // Pairs with the fence in `sleep`: either we see the sleeper, or it
        // sees the task we just queued.
        fence(Ordering::SeqCst);
        // A worker that is still searching will find the task: it checks the
        // queues again after it stops searching and before it sleeps.
        if self.searching.load(Ordering::SeqCst) > 0 || self.sleeping.load(Ordering::SeqCst) == 0 {
            return;
        }
        for s in self.sleepers.iter() {
            if s.asleep.load(Ordering::Relaxed)
                && s.asleep
                    .compare_exchange(true, false, Ordering::SeqCst, Ordering::Relaxed)
                    .is_ok()
            {
                self.sleeping.fetch_sub(1, Ordering::SeqCst);
                if let Some(t) = s.thread.get() {
                    t.unpark();
                }
                return;
            }
        }
    }

    fn has_work(&self, own: &Worker<Arc<Task>>) -> bool {
        !own.is_empty()
            || !self.injector.is_empty()
            || self
                .stealers
                .get()
                .is_some_and(|ss| ss.iter().any(|s| !s.is_empty()))
    }

    fn find_task(&self, own: &Worker<Arc<Task>>, me: usize, tick: u32) -> Option<Arc<Task>> {
        // Now and then look at the global queue first: a worker whose own
        // queue never runs dry (blocks waking each other) must not starve
        // tasks woken from outside (timers, drivers, parameter changes).
        if tick.is_multiple_of(GLOBAL_EVERY)
            && let Steal::Success(t) = self.injector.steal()
        {
            return Some(t);
        }
        if let Some(t) = own.pop() {
            return Some(t);
        }
        let stealers = self.stealers.get()?;
        loop {
            let found = self.injector.steal_batch_and_pop(own).or_else(|| {
                // Start after ourselves so workers don't all raid the same one.
                (1..stealers.len())
                    .map(|i| stealers[(me + i) % stealers.len()].steal())
                    .collect()
            });
            match found {
                Steal::Success(t) => return Some(t),
                Steal::Empty => return None,
                Steal::Retry => {}
            }
        }
    }

    fn add_timer(&self, task: &Arc<Task>, at: Instant) {
        let mut timers = self.timers.lock().unwrap_or_else(PoisonError::into_inner);
        let earliest = timers.heap.peek().map(|t| t.0.at);
        timers.seq += 1;
        let seq = timers.seq;
        timers.heap.push(Reverse(Timer {
            at,
            seq,
            generation: task.timer_gen.load(Ordering::Relaxed),
            task: task.clone(),
        }));
        drop(timers);
        if earliest.is_none_or(|e| at < e)
            && let Some(t) = self.timer_thread.get()
        {
            t.unpark();
        }
    }

    fn run_task(&self, task: Arc<Task>) {
        task.state.store(RUNNING, Ordering::Release);
        task.timer_gen.fetch_add(1, Ordering::Relaxed);
        let next = {
            let mut job = task
                .job
                .try_lock()
                .expect("a task never runs on two workers at once");
            let next = job.as_mut().map_or(Next::Done, |j| j.run());
            if let Next::Done = next {
                *job = None;
            }
            next
        };
        match next {
            Next::Again => {
                task.state.store(SCHEDULED, Ordering::Release);
                self.push(task);
            }
            Next::Idle => task.idle_or_requeue(),
            Next::Until(at) => {
                self.add_timer(&task, at);
                task.idle_or_requeue();
            }
            Next::Done => {
                task.state.store(DONE, Ordering::Release);
                if self.live.fetch_sub(1, Ordering::AcqRel) == 1 {
                    self.shut_down();
                }
            }
        }
    }

    fn shut_down(&self) {
        self.shutdown.store(true, Ordering::Release);
        for s in self.sleepers.iter() {
            if let Some(t) = s.thread.get() {
                t.unpark();
            }
        }
        if let Some(t) = self.timer_thread.get() {
            t.unpark();
        }
    }

    fn worker(&self, me: usize, own: Worker<Arc<Task>>) {
        // The pointer is only compared (in `push`), never dereferenced.
        LOCAL.with(|l| *l.borrow_mut() = Some((self as *const Shared, own)));
        let _ = self.sleepers[me].thread.set(std::thread::current());
        let mut tick = 0u32;
        let mut find = || {
            tick = tick.wrapping_add(1);
            LOCAL.with(|l| {
                let l = l.borrow();
                let (_, own) = l.as_ref().expect("set above");
                self.find_task(own, me, tick)
            })
        };
        'outer: loop {
            if let Some(task) = find() {
                self.run_task(task);
                continue;
            }
            // Out of work: look around briefly before sleeping. While we do,
            // `notify_one` needn't wake anybody (see there).
            self.searching.fetch_add(1, Ordering::SeqCst);
            for _ in 0..SPIN {
                if let Some(task) = find() {
                    // The last searcher found something: there may be more,
                    // so make sure someone else is looking.
                    if self.searching.fetch_sub(1, Ordering::SeqCst) == 1 {
                        self.notify_one();
                    }
                    self.run_task(task);
                    continue 'outer;
                }
                std::hint::spin_loop();
            }
            self.searching.fetch_sub(1, Ordering::SeqCst);
            if self.shutdown.load(Ordering::Acquire) {
                break;
            }
            self.sleep(me);
        }
        LOCAL.with(|l| *l.borrow_mut() = None);
    }

    fn sleep(&self, me: usize) {
        let s = &self.sleepers[me];
        s.asleep.store(true, Ordering::SeqCst);
        self.sleeping.fetch_add(1, Ordering::SeqCst);
        // Pairs with the fence in `notify_one`.
        fence(Ordering::SeqCst);
        let work = LOCAL.with(|l| {
            let l = l.borrow();
            self.has_work(&l.as_ref().expect("set in worker").1)
        });
        if !work {
            while s.asleep.load(Ordering::SeqCst) && !self.shutdown.load(Ordering::Acquire) {
                std::thread::park();
            }
        }
        // Still marked asleep (found work, or shutting down): undo it. If a
        // notifier got here first it already did.
        if s.asleep.swap(false, Ordering::SeqCst) {
            self.sleeping.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn timer_loop(&self) {
        loop {
            let mut timers = self.timers.lock().unwrap_or_else(PoisonError::into_inner);
            if self.shutdown.load(Ordering::Acquire) {
                // Timers hold tasks, which hold the pool: break the cycle.
                timers.heap.clear();
                return;
            }
            let now = Instant::now();
            while timers.heap.peek().is_some_and(|t| t.0.at <= now) {
                let Reverse(t) = timers.heap.pop().expect("peeked");
                // Skip timers armed before the task last ran.
                if t.task.timer_gen.load(Ordering::Relaxed) == t.generation {
                    t.task.schedule();
                }
            }
            let next = timers.heap.peek().map(|t| t.0.at);
            drop(timers);
            match next {
                Some(at) => std::thread::park_timeout(at.saturating_duration_since(now)),
                None => std::thread::park(),
            }
        }
    }
}

/// A set of worker threads running [`Task`]s.
pub(crate) struct Pool {
    shared: Arc<Shared>,
    workers: usize,
    tasks: Vec<Arc<Task>>,
    threads: Vec<JoinHandle<()>>,
}

impl Pool {
    pub(crate) fn new(workers: usize) -> Pool {
        let workers = workers.max(1);
        Pool {
            shared: Arc::new(Shared {
                injector: Injector::new(),
                stealers: OnceLock::new(),
                sleepers: (0..workers)
                    .map(|_| Sleeper {
                        asleep: AtomicBool::new(false),
                        thread: OnceLock::new(),
                    })
                    .collect(),
                sleeping: AtomicUsize::new(0),
                searching: AtomicUsize::new(0),
                live: AtomicUsize::new(0),
                shutdown: AtomicBool::new(false),
                timers: Mutex::new(Timers::default()),
                timer_thread: OnceLock::new(),
            }),
            workers,
            tasks: Vec::new(),
            threads: Vec::new(),
        }
    }

    /// A new task without a job yet ([`Task::set_job`]).
    pub(crate) fn task(&mut self) -> Arc<Task> {
        let t = Arc::new(Task {
            state: AtomicU8::new(IDLE),
            timer_gen: AtomicU64::new(0),
            job: Mutex::new(None),
            pool: self.shared.clone(),
        });
        self.shared.live.fetch_add(1, Ordering::Relaxed);
        self.tasks.push(t.clone());
        t
    }

    /// Spawn the workers and schedule every task once.
    pub(crate) fn start(&mut self) -> std::io::Result<()> {
        let mut queues = Vec::new();
        let mut stealers = Vec::new();
        for _ in 0..self.workers {
            let q = Worker::new_fifo();
            stealers.push(q.stealer());
            queues.push(q);
        }
        let _ = self.shared.stealers.set(stealers.into());
        let shared = self.shared.clone();
        let timer = std::thread::Builder::new()
            .name("fox:timer".into())
            .spawn(move || shared.timer_loop())?;
        let _ = self.shared.timer_thread.set(timer.thread().clone());
        self.threads.push(timer);
        for (i, q) in queues.into_iter().enumerate() {
            let shared = self.shared.clone();
            match std::thread::Builder::new()
                .name(format!("fox:worker-{i}"))
                .spawn(move || shared.worker(i, q))
            {
                Ok(h) => self.threads.push(h),
                Err(e) => {
                    // Without workers nothing would ever finish: give up.
                    self.shared.shut_down();
                    return Err(e);
                }
            }
        }
        for t in std::mem::take(&mut self.tasks) {
            t.schedule();
        }
        Ok(())
    }

    /// Wait for the workers to exit (after every task is done).
    pub(crate) fn join(self) {
        for h in self.threads {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    /// Counts runs; finishes after `limit`.
    struct Counter {
        runs: Arc<AtomicUsize>,
        limit: usize,
        busy: Arc<AtomicBool>,
        then: fn(usize) -> Next,
    }

    impl Job for Counter {
        fn run(&mut self) -> Next {
            assert!(!self.busy.swap(true, Ordering::SeqCst), "ran twice at once");
            let n = self.runs.fetch_add(1, Ordering::SeqCst) + 1;
            std::hint::spin_loop();
            self.busy.store(false, Ordering::SeqCst);
            if n >= self.limit {
                Next::Done
            } else {
                (self.then)(n)
            }
        }
    }

    fn counter(
        pool: &mut Pool,
        limit: usize,
        then: fn(usize) -> Next,
    ) -> (Arc<Task>, Arc<AtomicUsize>) {
        let runs = Arc::new(AtomicUsize::new(0));
        let t = pool.task();
        t.set_job(Box::new(Counter {
            runs: runs.clone(),
            limit,
            busy: Arc::new(AtomicBool::new(false)),
            then,
        }));
        (t, runs)
    }

    #[test]
    fn again_runs_until_done_and_the_pool_shuts_down() {
        let mut pool = Pool::new(4);
        let (_, a) = counter(&mut pool, 1000, |_| Next::Again);
        let (_, b) = counter(&mut pool, 10, |_| Next::Again);
        pool.start().unwrap();
        pool.join();
        assert_eq!(a.load(Ordering::SeqCst), 1000);
        assert_eq!(b.load(Ordering::SeqCst), 10);
    }

    #[test]
    fn timers_wake_waiting_tasks() {
        let mut pool = Pool::new(2);
        let (_, runs) = counter(&mut pool, 5, |_| {
            Next::Until(Instant::now() + Duration::from_millis(10))
        });
        let t0 = Instant::now();
        pool.start().unwrap();
        pool.join();
        assert_eq!(runs.load(Ordering::SeqCst), 5);
        assert!(t0.elapsed() >= Duration::from_millis(40));
    }

    /// Idles until woken; finishes once `finish` is set.
    struct Sleepy {
        runs: Arc<AtomicUsize>,
        busy: AtomicBool,
        finish: Arc<AtomicBool>,
    }

    impl Job for Sleepy {
        fn run(&mut self) -> Next {
            assert!(!self.busy.swap(true, Ordering::SeqCst), "ran twice at once");
            self.runs.fetch_add(1, Ordering::SeqCst);
            self.busy.store(false, Ordering::SeqCst);
            if self.finish.load(Ordering::SeqCst) {
                Next::Done
            } else {
                Next::Idle
            }
        }
    }

    /// Hammer one idle task with wakes from many threads: it must never run
    /// twice at once, and a wake after its last run must cause another.
    #[test]
    fn wakes_are_never_lost_and_runs_never_overlap() {
        let mut pool = Pool::new(8);
        let runs = Arc::new(AtomicUsize::new(0));
        let finish = Arc::new(AtomicBool::new(false));
        let task = pool.task();
        task.set_job(Box::new(Sleepy {
            runs: runs.clone(),
            busy: AtomicBool::new(false),
            finish: finish.clone(),
        }));
        let waker = task.waker();
        pool.start().unwrap();
        for _ in 0..50 {
            let wakers: Vec<_> = (0..4)
                .map(|_| {
                    let w = waker.clone();
                    std::thread::spawn(move || {
                        for _ in 0..2_000 {
                            w.wake_by_ref();
                        }
                    })
                })
                .collect();
            for w in wakers {
                w.join().unwrap();
            }
            // After the last wake the task must run at least once more.
            let before = runs.load(Ordering::SeqCst);
            waker.wake_by_ref();
            let t0 = Instant::now();
            while runs.load(Ordering::SeqCst) == before {
                assert!(t0.elapsed() < Duration::from_secs(5), "lost wake-up");
                std::thread::yield_now();
            }
        }
        finish.store(true, Ordering::SeqCst);
        waker.wake_by_ref();
        pool.join();
    }
}
