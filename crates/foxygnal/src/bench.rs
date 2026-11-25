//! `foxygnal bench`: run a graph headless for a while and report what it
//! cost: process CPU, CPU per block and per thread, items/s per wire, memory.
//! See docs/PERFORMANCE.md.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use foxygnal_core::{EdgeMeta, GraphCounters, GraphSpec, Running};
use serde::Serialize;
use serde_json::json;

use crate::procstat::{self, CpuTime, ThreadTime};

pub struct Options {
    /// How long to measure (after the warm-up).
    pub seconds: f64,
    /// Run this long before measuring, so start-up (filter design, buffers
    /// filling) doesn't count.
    pub warmup: f64,
    /// Remove whatever paces the graph, see [`unpace`].
    pub max: bool,
}

/// Blocks that set the pace of a graph: hardware and clocks.
const PACING: &[&str] = &[
    "throttle",
    "sdr_source",
    "sdr_source_dual",
    "sdr_sink",
    "audio_sink",
    "audio_source",
    "browser_audio",
];

/// Whether something in `graph` makes it run in real time.
pub fn is_paced(graph: &GraphSpec) -> bool {
    graph.blocks.iter().any(|b| {
        PACING.contains(&b.kind.as_str())
            && !(b.kind == "browser_audio" && b.params.get("pace") == Some(&json!(false)))
    })
}

/// Rewrite a resolved graph to run as fast as it can: throttles are bypassed,
/// `sdr_source` becomes an unpaced c32 tone at the same rate (both outputs of
/// `sdr_source_dual` read the same one) and
/// `browser_audio` stops pacing. Returns what was changed, one line each.
/// Blocks that can't be unpaced (sound cards, transmitters) are an error.
pub fn unpace(graph: &mut GraphSpec) -> anyhow::Result<Vec<String>> {
    let mut notes = Vec::new();
    let throttles: Vec<String> = graph
        .blocks
        .iter()
        .filter(|b| b.kind == "throttle")
        .map(|b| b.id.clone())
        .collect();
    for id in throttles {
        let block_of = |end: &str| end.rsplit_once('.').map_or(end, |(b, _)| b).to_owned();
        let feed = graph
            .connections
            .iter()
            .position(|c| block_of(&c.to) == id)
            .with_context(|| format!("throttle `{id}` has no input"))?;
        let upstream = graph.connections.remove(feed).from;
        for c in &mut graph.connections {
            if block_of(&c.from) == id {
                c.from = upstream.clone();
            }
        }
        graph.blocks.retain(|b| b.id != id);
        notes.push(format!("throttle `{id}` bypassed"));
    }
    let mut duals = Vec::new();
    for b in &mut graph.blocks {
        match b.kind.as_str() {
            kind @ ("sdr_source" | "sdr_source_dual") => {
                let kind = kind.to_owned();
                if kind == "sdr_source_dual" {
                    duals.push(b.id.clone());
                }
                let rate = b.params.get("sample_rate").cloned().unwrap_or(json!(2.4e6));
                b.kind = "signal_source".into();
                b.params = serde_json::from_value(json!({
                    "type": "c32",
                    "waveform": "sine",
                    "frequency": 100e3,
                    "amplitude": 0.5,
                    "sample_rate": rate,
                }))?;
                notes.push(format!(
                    "{kind} `{}` replaced by an unpaced signal_source (c32 tone)",
                    b.id
                ));
            }
            "browser_audio" => {
                b.params.insert("pace".into(), json!(false));
                notes.push(format!("browser_audio `{}` not pacing", b.id));
            }
            kind if PACING.contains(&kind) => {
                bail!(
                    "--max can't take the pacing out of {kind} `{}`; bench the graph \
                     without --max, or replace that block with a null_sink",
                    b.id
                )
            }
            _ => {}
        }
    }
    for c in &mut graph.connections {
        if let Some((block, "rx1" | "rx2")) = c.from.rsplit_once('.')
            && duals.iter().any(|d| d == block)
        {
            c.from = format!("{block}.out");
        }
    }
    Ok(notes)
}

struct Snapshot {
    at: Instant,
    cpu: Option<CpuTime>,
    threads: Vec<ThreadTime>,
    counters: GraphCounters,
    memory: procstat::Memory,
}

fn snapshot(running: &Running) -> Snapshot {
    Snapshot {
        counters: running.monitor().stats(),
        threads: procstat::thread_times(),
        cpu: procstat::cpu_time(),
        memory: procstat::memory(),
        at: Instant::now(),
    }
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub graph: String,
    /// "realtime" (paced as the graph says) or "max" (as fast as it goes).
    pub mode: &'static str,
    pub host: Host,
    /// Changes made for `--max`, and other remarks.
    pub notes: Vec<String>,
    /// "time" (ran for --seconds), "finished" (the graph ended on its own)
    /// or "interrupted" (Ctrl-C).
    pub ended: &'static str,
    pub warmup_s: f64,
    /// Length of the measured window.
    pub wall_s: f64,
    pub cpu: Option<Cpu>,
    pub blocks: Vec<BlockReport>,
    /// Threads that aren't block threads (drivers, probes, runtime), by name.
    pub threads: Vec<ThreadReport>,
    pub edges: Vec<EdgeReport>,
    pub memory: procstat::Memory,
}

#[derive(Debug, Serialize)]
pub struct Host {
    pub cpu: String,
    pub arch: &'static str,
    pub os: &'static str,
}

/// Percent of one core over the window (100 = one core fully busy).
#[derive(Debug, Serialize)]
pub struct Cpu {
    pub process_pct: f64,
    pub user_pct: f64,
    pub system_pct: f64,
}

#[derive(Debug, Serialize)]
pub struct BlockReport {
    pub block: String,
    /// Time inside `work` as a percentage of the window (wall clock, so it
    /// includes time the thread was preempted while in `work`).
    pub work_pct: f64,
    /// CPU time of the block's own thread as a percentage of one core
    /// (Linux, thread-per-block scheduler only), including the scheduler's
    /// overhead around `work`.
    pub thread_cpu_pct: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct ThreadReport {
    pub name: String,
    pub count: usize,
    pub cpu_pct: f64,
}

#[derive(Debug, Serialize)]
pub struct EdgeReport {
    pub from: String,
    pub to: String,
    pub items_per_s: f64,
    /// The stream's sample rate from metadata, if known.
    pub sample_rate: Option<f64>,
    /// `items_per_s / sample_rate`: 1.0 is real time.
    pub realtime_factor: Option<f64>,
    /// Whether this wire leaves a source block (nothing feeds it).
    pub source: bool,
}

/// Measure `running` and stop it. `graph` is the resolved graph it was built
/// from, `meta` its configured edge metadata.
pub fn measure(
    name: &str,
    graph: &GraphSpec,
    running: Running,
    meta: &[EdgeMeta],
    opts: &Options,
    mut notes: Vec<String>,
    interrupted: &Arc<AtomicBool>,
) -> anyhow::Result<Report> {
    let paced = is_paced(graph);
    if !opts.max && !paced {
        notes.push(
            "nothing paces this graph (no SDR, throttle or audio block): \
             it runs as fast as it can"
                .into(),
        );
    }
    let poll = Duration::from_millis(50);
    // /proc reads cost a little CPU themselves: snapshot once a second.
    let every = Duration::from_secs(1);
    let started = Instant::now();
    let stopped = || running.is_finished() || interrupted.load(Ordering::Relaxed);

    let mut first = snapshot(&running);
    let warmup = Duration::from_secs_f64(opts.warmup.max(0.0));
    while started.elapsed() < warmup && !stopped() {
        std::thread::sleep(poll);
    }
    let mut ended = "time";
    if running.is_finished() {
        notes.push("the graph finished during the warm-up; measured from its start".into());
    } else {
        first = snapshot(&running);
    }
    let mut last = snapshot(&running);
    let window = Duration::from_secs_f64(opts.seconds.max(0.1));
    loop {
        if running.is_finished() {
            ended = "finished";
            break;
        }
        if interrupted.load(Ordering::Relaxed) {
            ended = "interrupted";
            last = snapshot(&running);
            break;
        }
        let elapsed = first.at.elapsed();
        if elapsed >= window {
            last = snapshot(&running);
            break;
        }
        std::thread::sleep(poll.min(window - elapsed));
        if last.at.elapsed() >= every {
            last = snapshot(&running);
        }
    }
    // RSS and threads while it ran; the peak from the very end.
    let memory = procstat::Memory {
        peak_rss: procstat::memory().peak_rss,
        ..last.memory
    };
    running.stop();
    running.wait().context("the graph failed")?;

    let wall = last.at.duration_since(first.at);
    let secs = wall.as_secs_f64().max(1e-9);
    let pct = |d: Duration| d.as_secs_f64() / secs * 100.0;

    let cpu = match (first.cpu, last.cpu) {
        (Some(a), Some(b)) => Some(Cpu {
            process_pct: pct(b.total().saturating_sub(a.total())),
            user_pct: pct(b.user.saturating_sub(a.user)),
            system_pct: pct(b.system.saturating_sub(a.system)),
        }),
        _ => None,
    };

    // Thread CPU over the window, by tid.
    let before: HashMap<u64, Duration> = first.threads.iter().map(|t| (t.tid, t.cpu)).collect();
    let used = |t: &ThreadTime| {
        t.cpu
            .saturating_sub(before.get(&t.tid).copied().unwrap_or_default())
    };
    // Block threads are named `fox:<id>`, cut to 15 bytes by the kernel.
    let comm = |id: &str| {
        let mut s = format!("fox:{id}");
        let mut n = s.len().min(15);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        s.truncate(n);
        s
    };
    let mut by_comm: HashMap<String, usize> = HashMap::new();
    for b in &last.counters.blocks {
        *by_comm.entry(comm(&b.block)).or_default() += 1;
    }
    let busy0: HashMap<&str, u64> = first
        .counters
        .blocks
        .iter()
        .map(|b| (b.block.as_str(), b.busy_ns))
        .collect();
    let mut block_threads = HashSet::new();
    let blocks = last
        .counters
        .blocks
        .iter()
        .map(|b| {
            let c = comm(&b.block);
            let thread_cpu = (by_comm[&c] == 1)
                .then(|| last.threads.iter().find(|t| t.name == c))
                .flatten()
                .map(|t| {
                    block_threads.insert(t.tid);
                    pct(used(t))
                });
            let busy = b.busy_ns - busy0.get(b.block.as_str()).copied().unwrap_or(0);
            BlockReport {
                block: b.block.clone(),
                work_pct: pct(Duration::from_nanos(busy)),
                thread_cpu_pct: thread_cpu,
            }
        })
        .collect();
    let mut groups: HashMap<&str, (usize, Duration)> = HashMap::new();
    for t in last
        .threads
        .iter()
        .filter(|t| !block_threads.contains(&t.tid))
    {
        let g = groups.entry(t.name.as_str()).or_default();
        g.0 += 1;
        g.1 += used(t);
    }
    let mut threads: Vec<ThreadReport> = groups
        .into_iter()
        .map(|(name, (count, cpu))| ThreadReport {
            name: name.to_owned(),
            count,
            cpu_pct: pct(cpu),
        })
        .collect();
    threads.sort_by(|a, b| b.cpu_pct.total_cmp(&a.cpu_pct).then(a.name.cmp(&b.name)));

    let fed: HashSet<&str> = graph
        .connections
        .iter()
        .filter_map(|c| Some(c.endpoints().ok()?.1.block))
        .collect();
    let written0: HashMap<(&str, &str), u64> = first
        .counters
        .edges
        .iter()
        .map(|e| ((e.from.as_str(), e.to.as_str()), e.written))
        .collect();
    let edges = last
        .counters
        .edges
        .iter()
        .map(|e| {
            let n = e.written
                - written0
                    .get(&(e.from.as_str(), e.to.as_str()))
                    .copied()
                    .unwrap_or(0);
            let rate = n as f64 / secs;
            let sample_rate = meta
                .iter()
                .find(|m| m.from == e.from && m.to == e.to)
                .and_then(|m| m.meta.sample_rate)
                .filter(|r| *r > 0.0);
            let from_block = e.from.rsplit_once('.').map_or(e.from.as_str(), |(b, _)| b);
            EdgeReport {
                from: e.from.clone(),
                to: e.to.clone(),
                items_per_s: rate,
                sample_rate,
                realtime_factor: sample_rate.map(|r| rate / r),
                source: !fed.contains(from_block),
            }
        })
        .collect();

    Ok(Report {
        graph: name.to_owned(),
        mode: if opts.max || !paced {
            "max"
        } else {
            "realtime"
        },
        host: Host {
            cpu: procstat::cpu_model(),
            arch: std::env::consts::ARCH,
            os: std::env::consts::OS,
        },
        notes,
        ended,
        warmup_s: opts.warmup,
        wall_s: secs,
        cpu,
        blocks,
        threads,
        edges,
        memory,
    })
}

/// SI-prefixed number: 2400000 → "2.40 M".
fn si(v: f64) -> String {
    let (d, p) = match v.abs() {
        x if x >= 1e9 => (1e9, "G"),
        x if x >= 1e6 => (1e6, "M"),
        x if x >= 1e3 => (1e3, "k"),
        _ => (1.0, ""),
    };
    format!("{:.2} {p}", v / d)
}

fn mib(b: Option<u64>) -> String {
    b.map_or("n/a".into(), |b| format!("{:.1} MiB", b as f64 / 1048576.0))
}

fn opt_pct(v: Option<f64>) -> String {
    v.map_or("-".into(), |v| format!("{v:.1}%"))
}

/// The report as a readable table.
pub fn render(r: &Report) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let w = &mut s;
    let _ = writeln!(w, "graph    {}  ({})", r.graph, r.mode);
    let _ = writeln!(w, "host     {} [{}/{}]", r.host.cpu, r.host.os, r.host.arch);
    let _ = writeln!(
        w,
        "window   {:.2} s after {:.1} s warm-up (ended: {})",
        r.wall_s, r.warmup_s, r.ended
    );
    for n in &r.notes {
        let _ = writeln!(w, "note     {n}");
    }
    let _ = writeln!(w);
    match &r.cpu {
        Some(c) => {
            let _ = writeln!(
                w,
                "process CPU  {:.1}% of one core  (user {:.1}%, system {:.1}%)",
                c.process_pct, c.user_pct, c.system_pct
            );
        }
        None => {
            let _ = writeln!(w, "process CPU  n/a on this platform");
        }
    }
    let _ = writeln!(
        w,
        "memory       peak RSS {}, RSS {}, {} threads",
        mib(r.memory.peak_rss),
        mib(r.memory.rss),
        r.memory.threads.map_or("n/a".into(), |t| t.to_string())
    );
    let _ = writeln!(w);
    let bw = r
        .blocks
        .iter()
        .map(|b| b.block.len())
        .max()
        .unwrap_or(0)
        .max(5);
    let _ = writeln!(w, "{:<bw$}  {:>8}  {:>10}", "block", "work", "thread CPU");
    for b in &r.blocks {
        let _ = writeln!(
            w,
            "{:<bw$}  {:>8}  {:>10}",
            b.block,
            format!("{:.1}%", b.work_pct),
            opt_pct(b.thread_cpu_pct)
        );
    }
    let busy: Vec<&ThreadReport> = r.threads.iter().filter(|t| t.cpu_pct >= 0.05).collect();
    if !busy.is_empty() {
        let tw = busy
            .iter()
            .map(|t| t.name.len() + 5)
            .max()
            .unwrap_or(0)
            .max(12);
        let _ = writeln!(w);
        let _ = writeln!(w, "{:<tw$}  {:>10}", "other thread", "CPU");
        for t in busy {
            let name = if t.count > 1 {
                format!("{} ×{}", t.name, t.count)
            } else {
                t.name.clone()
            };
            let _ = writeln!(w, "{name:<tw$}  {:>10}", format!("{:.1}%", t.cpu_pct));
        }
    }
    let _ = writeln!(w);
    let ew = r
        .edges
        .iter()
        .map(|e| e.from.len() + e.to.len() + 3)
        .max()
        .unwrap_or(0)
        .max(4);
    let _ = writeln!(
        w,
        "{:<ew$}  {:>12}  {:>12}  {:>9}",
        "wire", "items/s", "stream rate", "×realtime"
    );
    for e in &r.edges {
        let _ = writeln!(
            w,
            "{:<ew$}  {:>12}  {:>12}  {:>9}",
            format!("{} → {}", e.from, e.to),
            si(e.items_per_s),
            e.sample_rate.map_or("-".into(), si),
            e.realtime_factor.map_or("-".into(), |f| format!("{f:.2}")),
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRAPH: &str = r#"
[[block]]
id = "src"
kind = "sdr_source"
params = { device = "sim:", frequency = 100e6, sample_rate = 1e6, gain = 0.0 }
[[block]]
id = "thr"
kind = "throttle"
params = { type = "c32", sample_rate = 0.0 }
[[block]]
id = "sink"
kind = "null_sink"
params = { type = "c32" }
[[connect]]
from = "src.out"
to = "thr.in"
[[connect]]
from = "thr.out"
to = "sink.in"
"#;

    #[test]
    fn unpace_bypasses_throttles_and_replaces_sdrs() {
        let mut g = GraphSpec::from_toml(GRAPH).unwrap();
        assert!(is_paced(&g));
        let notes = unpace(&mut g).unwrap();
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(!is_paced(&g));
        assert_eq!(g.blocks.len(), 2);
        assert_eq!(g.blocks[0].kind, "signal_source");
        assert_eq!(g.blocks[0].params["sample_rate"], json!(1e6));
        assert_eq!(g.connections.len(), 1);
        assert_eq!(g.connections[0].from, "src.out");
        assert_eq!(g.connections[0].to, "sink.in");
        // What's left builds.
        let reg = foxygnal_blocks::registry();
        foxygnal_core::Flowgraph::from_resolved(&g, &reg).unwrap();
    }

    #[test]
    fn unpace_rewires_both_outputs_of_a_dual_source() {
        let mut g = GraphSpec::from_toml(
            r#"
[[block]]
id = "rx"
kind = "sdr_source_dual"
params = { device = "pluto:", sample_rate = 4e6 }
[[block]]
id = "a"
kind = "null_sink"
params = { type = "c32" }
[[block]]
id = "b"
kind = "null_sink"
params = { type = "c32" }
[[connect]]
from = "rx.rx1"
to = "a.in"
[[connect]]
from = "rx.rx2"
to = "b.in"
"#,
        )
        .unwrap();
        assert!(is_paced(&g));
        let notes = unpace(&mut g).unwrap();
        assert!(
            notes[0].starts_with("sdr_source_dual `rx` replaced"),
            "{notes:?}"
        );
        assert!(!is_paced(&g));
        assert!(g.connections.iter().all(|c| c.from == "rx.out"));
        let reg = foxygnal_blocks::registry();
        foxygnal_core::Flowgraph::from_resolved(&g, &reg).unwrap();
    }

    #[test]
    fn bundled_bench_graphs_build() {
        let reg = foxygnal_blocks::registry();
        for text in [
            include_str!("../../../bench/fm-receiver-sim.fox.toml"),
            include_str!("../../../bench/fm-receiver-max.fox.toml"),
        ] {
            let g = GraphSpec::from_toml(text).unwrap();
            let mut g = foxygnal_core::resolve(&g, &reg, &foxygnal_core::NoLoader)
                .unwrap()
                .graph;
            let mut fg = foxygnal_core::Flowgraph::from_resolved(&g, &reg).unwrap();
            let meta = fg.configure().unwrap();
            assert_eq!(meta[0].meta.sample_rate, Some(2.4e6));
            assert_eq!(meta[3].meta.sample_rate, Some(48e3));
            unpace(&mut g).unwrap();
            assert!(!is_paced(&g));
            foxygnal_core::Flowgraph::from_resolved(&g, &reg)
                .unwrap()
                .configure()
                .unwrap();
        }
    }

    #[test]
    fn unpace_refuses_sound_cards() {
        let mut g = GraphSpec::from_toml(GRAPH).unwrap();
        g.blocks[2].kind = "audio_sink".into();
        let e = unpace(&mut g).unwrap_err().to_string();
        assert!(e.contains("audio_sink `sink`"), "{e}");
    }

    #[test]
    fn measures_a_short_run() {
        let text = r#"
[[block]]
id = "src"
kind = "signal_source"
params = { type = "c32", sample_rate = 1e6 }
[[block]]
id = "sink"
kind = "null_sink"
params = { type = "c32" }
[[connect]]
from = "src.out"
to = "sink.in"
"#;
        let g = GraphSpec::from_toml(text).unwrap();
        let reg = foxygnal_blocks::registry();
        let mut fg = foxygnal_core::Flowgraph::from_resolved(&g, &reg).unwrap();
        let meta = fg.configure().unwrap();
        let running = fg.start().unwrap();
        let opts = Options {
            seconds: 0.3,
            warmup: 0.1,
            max: false,
        };
        let r = measure(
            "t",
            &g,
            running,
            &meta,
            &opts,
            Vec::new(),
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert_eq!(r.mode, "max");
        assert_eq!(r.ended, "time");
        assert_eq!(r.blocks.len(), 2);
        assert_eq!(r.edges.len(), 1);
        assert!(r.edges[0].source);
        assert!(r.edges[0].items_per_s > 0.0);
        assert_eq!(r.edges[0].sample_rate, Some(1e6));
        let table = render(&r);
        assert!(table.contains("src.out → sink.in"), "{table}");
        serde_json::to_string(&r).unwrap();
    }
}
