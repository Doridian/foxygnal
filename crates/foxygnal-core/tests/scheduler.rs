//! Scheduler behaviour: the worker pool, dedicated threads and the
//! thread-per-block fallback must all honour the same contract.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use foxygnal_core::{Block, Error, Flowgraph, Instance, StreamType, WorkIo, WorkStatus};

/// Counts up from 0 forever (or up to `limit`).
struct Count {
    next: u32,
    limit: u32,
}

impl Block for Count {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        if self.next == self.limit {
            return Ok(WorkStatus::Done);
        }
        let out = io.outputs[0].slice_mut::<f32>();
        let n = out.len().min((self.limit - self.next) as usize);
        for (i, v) in out[..n].iter_mut().enumerate() {
            *v = (self.next + i as u32) as f32;
        }
        self.next += n as u32;
        io.produce(0, n);
        Ok(if n == 0 {
            WorkStatus::Blocked
        } else {
            WorkStatus::Ok
        })
    }
}

/// Passes items through; optionally panics after `panic_at` items, or adds
/// a live `offset`, or waits `wait` between small batches.
#[derive(Default)]
struct Pass {
    seen: u64,
    panic_at: Option<u64>,
    offset: f32,
    wait: Option<Duration>,
    due: Option<Instant>,
}

impl Block for Pass {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        if let Some(d) = self.wait {
            let now = Instant::now();
            if self.due.is_some_and(|due| now < due) {
                return Ok(WorkStatus::Wait(self.due.unwrap() - now));
            }
            self.due = Some(now + d);
        }
        let (inp, out) = io.in_out::<f32, f32>(0, 0);
        let mut n = inp.len().min(out.len());
        if self.wait.is_some() {
            n = n.min(64);
        }
        for (o, i) in out[..n].iter_mut().zip(inp) {
            *o = i + self.offset;
        }
        self.seen += n as u64;
        if self.panic_at.is_some_and(|p| self.seen >= p) {
            panic!("boom");
        }
        io.consume(0, n);
        io.produce(0, n);
        Ok(if n == 0 {
            WorkStatus::Blocked
        } else {
            WorkStatus::Ok
        })
    }

    fn set_param(&mut self, name: &str, value: &serde_json::Value) -> Result<(), Error> {
        match name {
            "offset" => {
                self.offset = value.as_f64().unwrap_or_default() as f32;
                Ok(())
            }
            _ => Err(Error::Other(format!("no parameter {name}"))),
        }
    }
}

/// Collects (or just counts) what arrives.
struct Sink {
    out: Arc<Mutex<Vec<f32>>>,
    keep: bool,
    count: Arc<AtomicU64>,
}

impl Block for Sink {
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let inp = io.inputs[0].slice::<f32>();
        let n = inp.len();
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        if self.keep {
            self.out.lock().unwrap().extend_from_slice(inp);
        }
        self.count.fetch_add(n as u64, Ordering::Relaxed);
        io.consume(0, n);
        Ok(WorkStatus::Ok)
    }
}

/// Never produces anything and never finishes on its own.
struct Silent;

impl Block for Silent {
    fn work(&mut self, _io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        Ok(WorkStatus::Blocked)
    }
}

/// Records which thread each call ran on.
struct Affine {
    dedicated: bool,
    threads: Arc<Mutex<Vec<ThreadId>>>,
}

impl Affine {
    fn note(&self) {
        self.threads
            .lock()
            .unwrap()
            .push(std::thread::current().id());
    }
}

impl Block for Affine {
    fn dedicated_thread(&self) -> bool {
        self.dedicated
    }
    fn start(&mut self) -> Result<(), Error> {
        self.note();
        Ok(())
    }
    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        self.note();
        let n = io.inputs[0].slice::<f32>().len();
        io.consume(0, n);
        Ok(if n == 0 {
            WorkStatus::Blocked
        } else {
            WorkStatus::Ok
        })
    }
    fn stop(&mut self) -> Result<(), Error> {
        self.note();
        Ok(())
    }
}

fn src(limit: u32) -> Instance {
    Instance {
        block: Box::new(Count { next: 0, limit }),
        inputs: vec![],
        outputs: vec![("out".into(), StreamType::F32)],
    }
}

fn pass(p: Pass) -> Instance {
    Instance {
        block: Box::new(p),
        inputs: vec![("in".into(), StreamType::F32)],
        outputs: vec![("out".into(), StreamType::F32)],
    }
}

fn sink(keep: bool) -> (Instance, Arc<Mutex<Vec<f32>>>, Arc<AtomicU64>) {
    let out = Arc::new(Mutex::new(Vec::new()));
    let count = Arc::new(AtomicU64::new(0));
    let inst = Instance {
        block: Box::new(Sink {
            out: out.clone(),
            keep,
            count: count.clone(),
        }),
        inputs: vec![("in".into(), StreamType::F32)],
        outputs: vec![],
    };
    (inst, out, count)
}

/// The scheduler configurations every test runs under.
type Config = fn(Flowgraph) -> Flowgraph;

fn configs() -> Vec<(&'static str, Config)> {
    vec![
        ("pool", |f| f),
        ("one worker", |f| f.with_workers(1)),
        ("three workers", |f| f.with_workers(3)),
        ("thread per block", |f| f.with_thread_per_block()),
    ]
}

/// Builds src → `stages` × pass → sink, returning the sink's data.
fn chain(cfg: Config, limit: u32, stages: Vec<Pass>) -> Vec<f32> {
    let mut fg = cfg(Flowgraph::new().with_buffer_capacity(1000));
    let mut prev = fg.add("src", src(limit));
    for (i, p) in stages.into_iter().enumerate() {
        let b = fg.add(format!("p{i}"), pass(p));
        fg.connect(prev, 0, b, 0).unwrap();
        prev = b;
    }
    let (s, out, _) = sink(true);
    let s = fg.add("sink", s);
    fg.connect(prev, 0, s, 0).unwrap();
    fg.start().unwrap().wait().unwrap();
    Arc::try_unwrap(out).unwrap().into_inner().unwrap()
}

#[test]
fn long_chains_deliver_everything_in_order() {
    for (name, cfg) in configs() {
        let n = 200_000;
        let out = chain(cfg, n, (0..40).map(|_| Pass::default()).collect());
        assert_eq!(out.len(), n as usize, "{name}");
        assert!(
            out.iter().enumerate().all(|(i, &v)| v == i as f32),
            "{name}"
        );
    }
}

#[test]
fn waiting_blocks_are_woken_by_their_timer() {
    for (name, cfg) in configs() {
        let n = 3_000;
        let t0 = Instant::now();
        let out = chain(
            cfg,
            n,
            vec![Pass {
                wait: Some(Duration::from_millis(1)),
                ..Default::default()
            }],
        );
        assert_eq!(out.len(), n as usize, "{name}");
        // 64 items per millisecond at most.
        assert!(t0.elapsed() >= Duration::from_millis(40), "{name}");
    }
}

#[test]
fn a_panic_stops_the_whole_graph_and_names_the_block() {
    for (name, cfg) in configs() {
        let mut fg = cfg(Flowgraph::new());
        // An endless chain that only stops because of the panic elsewhere.
        let a = fg.add("endless", src(u32::MAX));
        let (s1, _, _) = sink(false);
        let s1 = fg.add("sink1", s1);
        fg.connect(a, 0, s1, 0).unwrap();
        let b = fg.add("src", src(u32::MAX));
        let bad = fg.add(
            "bad",
            pass(Pass {
                panic_at: Some(10_000),
                ..Default::default()
            }),
        );
        let (s2, _, _) = sink(false);
        let s2 = fg.add("sink2", s2);
        fg.connect(b, 0, bad, 0).unwrap();
        fg.connect(bad, 0, s2, 0).unwrap();
        let t0 = Instant::now();
        let err = fg.start().unwrap().wait().unwrap_err();
        assert_eq!(err.block(), Some("bad"), "{name}: {err}");
        assert!(matches!(err, Error::Block { ref source, .. } if matches!(**source, Error::Panic)));
        assert!(t0.elapsed() < Duration::from_secs(5), "{name}");
    }
}

#[test]
fn stop_is_prompt_on_an_idle_graph() {
    for (name, cfg) in configs() {
        let mut fg = cfg(Flowgraph::new());
        let a = fg.add(
            "silent",
            Instance {
                block: Box::new(Silent),
                inputs: vec![],
                outputs: vec![("out".into(), StreamType::F32)],
            },
        );
        let p = fg.add("p", pass(Pass::default()));
        let (s, _, _) = sink(false);
        let s = fg.add("sink", s);
        fg.connect(a, 0, p, 0).unwrap();
        fg.connect(p, 0, s, 0).unwrap();
        let running = fg.start().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert!(!running.is_finished(), "{name}");
        let t0 = Instant::now();
        running.stop();
        running.wait().unwrap();
        assert!(t0.elapsed() < Duration::from_millis(500), "{name}");
    }
}

#[test]
fn live_params_reach_pooled_blocks_and_busy_time_is_counted() {
    for (name, cfg) in configs() {
        let mut fg = cfg(Flowgraph::new());
        let a = fg.add("src", src(u32::MAX));
        let p = fg.add("p", pass(Pass::default()));
        let (s, out, count) = sink(true);
        let s = fg.add("sink", s);
        fg.connect(a, 0, p, 0).unwrap();
        fg.connect(p, 0, s, 0).unwrap();
        let running = fg.start().unwrap();
        while count.load(Ordering::Relaxed) < 1000 {
            std::thread::yield_now();
        }
        running
            .param_sender()
            .send("p", "offset", serde_json::json!(1e9))
            .unwrap();
        let t0 = Instant::now();
        while !out.lock().unwrap().iter().any(|&v| v >= 1e9) {
            assert!(t0.elapsed() < Duration::from_secs(5), "{name}: param lost");
            std::thread::sleep(Duration::from_millis(1));
        }
        let stats = running.monitor().stats();
        assert!(stats.blocks.iter().all(|b| b.busy_ns > 0), "{name}");
        // A rejected parameter stops the graph with an error naming the block.
        running
            .param_sender()
            .send("p", "nope", serde_json::json!(1))
            .unwrap();
        let err = running.wait().unwrap_err();
        assert_eq!(err.block(), Some("p"), "{name}");
    }
}

#[test]
fn dedicated_blocks_keep_one_thread_from_start_to_stop() {
    for (name, cfg) in configs() {
        let threads = Arc::new(Mutex::new(Vec::new()));
        let mut fg = cfg(Flowgraph::new().with_workers(4));
        let a = fg.add("src", src(500_000));
        let b = fg.add(
            "affine",
            Instance {
                block: Box::new(Affine {
                    dedicated: true,
                    threads: threads.clone(),
                }),
                inputs: vec![("in".into(), StreamType::F32)],
                outputs: vec![],
            },
        );
        fg.connect(a, 0, b, 0).unwrap();
        fg.start().unwrap().wait().unwrap();
        let threads = threads.lock().unwrap();
        assert!(threads.len() > 3, "{name}");
        assert!(threads.iter().all(|t| *t == threads[0]), "{name}");
    }
}

#[test]
fn abandoned_sources_finish() {
    // A head-like block that stops after 10 items: the endless source
    // upstream has nobody left to write for and must finish too.
    struct Take(usize);
    impl Block for Take {
        fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
            let n = io.inputs[0].slice::<f32>().len().min(self.0);
            io.consume(0, n);
            self.0 -= n;
            Ok(if self.0 == 0 {
                WorkStatus::Done
            } else {
                WorkStatus::Blocked
            })
        }
    }
    let finished = Arc::new(AtomicUsize::new(0));
    for (name, cfg) in configs() {
        let mut fg = cfg(Flowgraph::new());
        let a = fg.add("src", src(u32::MAX));
        let t = fg.add(
            "take",
            Instance {
                block: Box::new(Take(10)),
                inputs: vec![("in".into(), StreamType::F32)],
                outputs: vec![],
            },
        );
        fg.connect(a, 0, t, 0).unwrap();
        let t0 = Instant::now();
        fg.start().unwrap().wait().unwrap();
        assert!(t0.elapsed() < Duration::from_secs(2), "{name}");
        finished.fetch_add(1, Ordering::Relaxed);
    }
    assert_eq!(finished.load(Ordering::Relaxed), configs().len());
}

/// A dedicated block that fails to start must stop every other block, even
/// idle dedicated ones that nothing else would ever wake.
#[test]
fn a_failed_start_stops_idle_dedicated_blocks() {
    struct FailStart;
    impl Block for FailStart {
        fn dedicated_thread(&self) -> bool {
            true
        }
        fn start(&mut self) -> Result<(), Error> {
            Err(Error::Other("no device".into()))
        }
        fn work(&mut self, _io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
            unreachable!("never started")
        }
    }
    struct DedicatedSilent;
    impl Block for DedicatedSilent {
        fn dedicated_thread(&self) -> bool {
            true
        }
        fn work(&mut self, _io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
            Ok(WorkStatus::Blocked)
        }
    }
    for (name, cfg) in configs() {
        let mut fg = cfg(Flowgraph::new());
        let a = fg.add(
            "quiet",
            Instance {
                block: Box::new(DedicatedSilent),
                inputs: vec![],
                outputs: vec![("out".into(), StreamType::F32)],
            },
        );
        let (s, _, _) = sink(false);
        let s = fg.add("sink", s);
        fg.connect(a, 0, s, 0).unwrap();
        fg.add(
            "broken",
            Instance {
                block: Box::new(FailStart),
                inputs: vec![],
                outputs: vec![],
            },
        );
        let t0 = Instant::now();
        let err = fg.start().unwrap().wait().unwrap_err();
        assert_eq!(err.block(), Some("broken"), "{name}");
        assert!(err.to_string().contains("no device"), "{name}");
        assert!(t0.elapsed() < Duration::from_secs(2), "{name}");
    }
}
