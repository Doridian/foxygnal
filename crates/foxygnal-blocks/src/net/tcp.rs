//! `tcp_source` and `tcp_sink`.

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

use foxygnal_core::{
    Block, BlockDescriptor, Configure, Error, Instance, ParamKind, ParamReader, ParamSpec,
    PortSpec, Registry, WorkIo, WorkStatus,
};
use foxygnal_sdr::ring::{self, Consumer, Producer};
use serde_json::json;

use super::feed::{Feed, Feeder, Shared};
use super::{
    CATEGORY, POLL, host_param, host_port, nap, param_err, port, port_param, resolve, transient,
};
use crate::params;

/// Ring between the receive thread and the block. TCP has flow control, so
/// this only has to smooth out scheduling.
const RX_RING: usize = 1 << 20;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// A client that keeps missing data is logged at most this often.
const SKIP_LOG_INTERVAL: Duration = Duration::from_secs(5);

const HOST_HELP: &str = "Server mode: the local address to listen on. 127.0.0.1 (the default) \
                         is this machine only; 0.0.0.0 (or a LAN address) exposes the stream \
                         to the network, without any authentication. Client mode: the host to \
                         connect to.";

fn mode_param(help: &'static str) -> ParamSpec {
    ParamSpec {
        name: "mode",
        label: "Mode",
        kind: ParamKind::Enum {
            options: vec!["server", "client"],
        },
        default: json!("server"),
        help,
        live: false,
    }
}

fn reconnect_param(help: &'static str) -> ParamSpec {
    ParamSpec {
        name: "reconnect",
        label: "Reconnect",
        kind: ParamKind::Bool,
        default: json!(true),
        help,
        live: false,
    }
}

fn retry_param() -> ParamSpec {
    ParamSpec {
        name: "retry",
        label: "Retry interval",
        kind: ParamKind::Float {
            min: Some(0.01),
            max: None,
            unit: Some("s"),
        },
        default: json!(1.0),
        help: "Client mode: time between connection attempts.",
        live: false,
    }
}

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "tcp_source",
            label: "TCP Source",
            category: CATEGORY,
            help: "Receives raw little-endian samples over TCP (from GNU Radio's TCP Sink, \
                   `nc`, `socat`, …). One connection at a time; the first item of each \
                   connection is tagged `connected` (int, counting from 1).",
            inputs: vec![],
            outputs: vec![PortSpec {
                name: "out",
                ty: None,
            }],
            params: vec![
                params::any_type(),
                mode_param(
                    "server: listen and accept one sender at a time; client: connect to a \
                     sender.",
                ),
                host_param(HOST_HELP),
                port_param(),
                reconnect_param(
                    "When the sender disconnects, wait for the next one (server) or connect \
                     again (client) instead of ending the stream. Off: the end of the first \
                     connection ends the stream, and a client that can't connect fails.",
                ),
                retry_param(),
                params::optional_rate(
                    "The stream's sample rate, for everything downstream (a network stream \
                     has no rate of its own). 0 = unknown.",
                ),
            ],
        },
        build_source,
    );
    r.register(
        BlockDescriptor {
            kind: "tcp_sink",
            label: "TCP Sink",
            category: CATEGORY,
            help: "Sends raw little-endian samples over TCP (to GNU Radio's TCP Source, `nc`, \
                   `socat`, …). In server mode every connected client gets the stream from \
                   the moment it connects, each through its own bounded queue: a client that \
                   can't keep up misses data (logged) instead of slowing the graph, unless \
                   Backpressure is on.",
            inputs: vec![PortSpec {
                name: "in",
                ty: None,
            }],
            outputs: vec![],
            params: vec![
                params::any_type(),
                mode_param("server: listen for receivers; client: connect to a receiver."),
                host_param(HOST_HELP),
                port_param(),
                reconnect_param(
                    "Client mode: connect again after a failed attempt or a lost \
                     connection. Off: either ends the stream.",
                ),
                retry_param(),
                ParamSpec {
                    name: "backpressure",
                    label: "Backpressure",
                    kind: ParamKind::Bool,
                    default: json!(false),
                    help: "Make the graph wait for the slowest client (and, with none \
                           connected, for the first one) instead of dropping data. Then a \
                           stalled client stalls the whole graph.",
                    live: false,
                },
                ParamSpec {
                    name: "queue",
                    label: "Queue per client",
                    kind: ParamKind::Int {
                        min: Some(4),
                        max: Some(1 << 20),
                    },
                    default: json!(1024),
                    help: "KiB buffered for each client before it starts missing data.",
                    live: false,
                },
                ParamSpec {
                    name: "max_clients",
                    label: "Max clients",
                    kind: ParamKind::Int {
                        min: Some(1),
                        max: Some(1024),
                    },
                    default: json!(8),
                    help: "Server mode: further connections are refused.",
                    live: false,
                },
            ],
        },
        build_sink,
    );
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Server,
    Client,
}

fn mode(p: &ParamReader<'_>, kind: &str) -> Result<Mode, Error> {
    match p.str("mode")? {
        "server" => Ok(Mode::Server),
        "client" => Ok(Mode::Client),
        _ => Err(param_err(kind, "mode", "server or client")),
    }
}

fn retry(p: &ParamReader<'_>) -> Result<Duration, Error> {
    Ok(Duration::from_secs_f64(p.f64("retry")?.max(0.01)))
}

fn listen(kind: &str, host: &str, port: u16) -> Result<TcpListener, Error> {
    let addr = resolve(kind, host, port).map_err(Error::Other)?;
    let l = TcpListener::bind(addr)
        .map_err(|e| Error::Other(format!("{kind}: cannot listen on {addr}: {e}")))?;
    // Accept polls, so the thread can notice it should stop.
    l.set_nonblocking(true)?;
    super::exposure_note(kind, addr);
    Ok(l)
}

/// Accept one connection, or None when `stop` is set.
fn accept(
    kind: &str,
    l: &TcpListener,
    stop: impl Fn() -> bool,
) -> Result<Option<(TcpStream, SocketAddr)>, String> {
    loop {
        if stop() {
            return Ok(None);
        }
        match l.accept() {
            Ok((s, peer)) => {
                s.set_nonblocking(false)
                    .map_err(|e| format!("{kind}: {e}"))?;
                let _ = s.set_nodelay(true);
                return Ok(Some((s, peer)));
            }
            Err(e) if transient(&e) => std::thread::sleep(Duration::from_millis(10)),
            // The client gave up before we accepted it, or we're out of file
            // descriptors for a moment.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionAborted => {}
            Err(e) => {
                eprintln!("{kind}: accept failed: {e}");
                std::thread::sleep(POLL);
            }
        }
    }
}

fn connect(kind: &str, host: &str, port: u16) -> Result<TcpStream, String> {
    let addr = resolve(kind, host, port)?;
    let s = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .map_err(|e| format!("{kind}: cannot connect to {}: {e}", host_port(host, port)))?;
    let _ = s.set_nodelay(true);
    Ok(s)
}

// ---- Source ----------------------------------------------------------------

pub struct TcpSource {
    mode: Mode,
    host: String,
    port: u16,
    reconnect: bool,
    retry: Duration,
    item: usize,
    sample_rate: f64,
    feed: Option<Feed>,
}

fn build_source(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let kind = "tcp_source";
    let ty = p.stream_type("type")?;
    Ok(Instance {
        block: Box::new(TcpSource {
            mode: mode(p, kind)?,
            host: p.str("host")?.to_owned(),
            port: port(p, kind)?,
            reconnect: p.bool("reconnect")?,
            retry: retry(p)?,
            item: ty.item_size(),
            sample_rate: p.f64("sample_rate")?,
            feed: None,
        }),
        inputs: vec![],
        outputs: vec![("out".into(), ty)],
    })
}

impl Block for TcpSource {
    // The receive thread wakes the block by unparking the thread that ran `start`.
    fn dedicated_thread(&self) -> bool {
        true
    }

    fn configure(&mut self, cx: &mut Configure<'_>) -> Result<(), Error> {
        for o in cx.outputs.iter_mut() {
            o.sample_rate = (self.sample_rate > 0.0).then_some(self.sample_rate);
        }
        Ok(())
    }

    fn start(&mut self) -> Result<(), Error> {
        let (reconnect, retry) = (self.reconnect, self.retry);
        let name = "fox:tcp_source";
        self.feed = Some(match self.mode {
            Mode::Server => {
                // Listen here, so a port in use fails the run right away.
                let l = listen("tcp_source", &self.host, self.port)?;
                Feed::spawn(name, self.item, RX_RING, move |f, sh| {
                    serve(&l, reconnect, f, sh)
                })?
            }
            Mode::Client => {
                let (host, port) = (self.host.clone(), self.port);
                Feed::spawn(name, self.item, RX_RING, move |f, sh| {
                    dial(&host, port, reconnect, retry, f, sh)
                })?
            }
        });
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let feed = self
            .feed
            .as_mut()
            .ok_or_else(|| Error::Other("tcp_source: not started".into()))?;
        feed.work(&mut io.outputs[0])
    }

    fn stop(&mut self) -> Result<(), Error> {
        self.feed = None;
        Ok(())
    }
}

enum End {
    /// The sender closed the connection.
    Eof,
    /// We are stopping.
    Stop,
}

/// Read one connection into the feeder until it ends.
fn read_conn(mut s: TcpStream, f: &mut Feeder, sh: &Shared) -> Result<End, String> {
    s.set_read_timeout(Some(POLL)).map_err(|e| e.to_string())?;
    f.connected();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        if sh.stopping() {
            return Ok(End::Stop);
        }
        match s.read(&mut buf) {
            Ok(0) => return Ok(End::Eof),
            Ok(n) => {
                if !f.push_stream(&buf[..n], sh) {
                    return Ok(End::Stop);
                }
            }
            Err(e) if transient(&e) => {}
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Server mode's receive thread.
fn serve(l: &TcpListener, reconnect: bool, f: &mut Feeder, sh: &Shared) -> Result<(), String> {
    let kind = "tcp_source";
    loop {
        let Some((s, peer)) = accept(kind, l, || sh.stopping())? else {
            return Ok(());
        };
        match read_conn(s, f, sh) {
            Ok(End::Stop) => return Ok(()),
            Ok(End::Eof) if !reconnect => return Ok(()),
            Err(e) if !reconnect => return Err(format!("{kind}: connection from {peer}: {e}")),
            _ => {}
        }
    }
}

/// Client mode's receive thread.
fn dial(
    host: &str,
    port: u16,
    reconnect: bool,
    retry: Duration,
    f: &mut Feeder,
    sh: &Shared,
) -> Result<(), String> {
    let kind = "tcp_source";
    loop {
        if sh.stopping() {
            return Ok(());
        }
        match connect(kind, host, port) {
            Err(e) if !reconnect => return Err(e),
            Err(_) => {}
            Ok(s) => match read_conn(s, f, sh) {
                Ok(End::Stop) => return Ok(()),
                Ok(End::Eof) if !reconnect => return Ok(()),
                Err(e) if !reconnect => {
                    return Err(format!(
                        "{kind}: connection to {}: {e}",
                        host_port(host, port)
                    ));
                }
                _ => {}
            },
        }
        if !nap(sh.stop_flag(), retry) {
            return Ok(());
        }
    }
}

// ---- Sink ------------------------------------------------------------------

/// State shared by the block, its connection thread and the client threads.
struct SinkShared {
    /// The input ended: stop taking clients and let the queues drain.
    closing: AtomicBool,
    /// Stop now.
    stop: AtomicBool,
    /// Client mode without reconnect: the connection is gone for good.
    ended: AtomicBool,
    failed: AtomicBool,
    error: Mutex<Option<String>>,
    /// The block's thread, unparked when a client comes or goes.
    block: Thread,
}

impl SinkShared {
    fn done(&self) -> bool {
        self.closing.load(Ordering::Acquire) || self.stop.load(Ordering::Acquire)
    }

    fn fail(&self, msg: String) {
        *self.error.lock().unwrap_or_else(|p| p.into_inner()) = Some(msg);
        self.failed.store(true, Ordering::Release);
        self.block.unpark();
    }
}

/// A connected client, as the block sees it.
struct Client {
    queue: Producer<u8>,
    peer: SocketAddr,
    /// Items it missed because its queue was full.
    skipped: u64,
    /// When that was last logged.
    logged: Option<Instant>,
}

struct Config {
    mode: Mode,
    host: String,
    port: u16,
    reconnect: bool,
    retry: Duration,
    queue: usize,
    max_clients: usize,
}

pub struct TcpSink {
    cfg: Arc<Config>,
    item: usize,
    backpressure: bool,
    run: Option<SinkRun>,
}

struct SinkRun {
    shared: Arc<SinkShared>,
    new_clients: mpsc::Receiver<Client>,
    clients: Vec<Client>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for SinkRun {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.clients.clear();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn build_sink(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let kind = "tcp_sink";
    let ty = p.stream_type("type")?;
    let cfg = Config {
        mode: mode(p, kind)?,
        host: p.str("host")?.to_owned(),
        port: port(p, kind)?,
        reconnect: p.bool("reconnect")?,
        retry: retry(p)?,
        queue: (p.u64("queue")? as usize).clamp(4, 1 << 20) * 1024,
        max_clients: (p.u64("max_clients")? as usize).max(1),
    };
    Ok(Instance {
        block: Box::new(TcpSink {
            cfg: Arc::new(cfg),
            item: ty.item_size(),
            backpressure: p.bool("backpressure")?,
            run: None,
        }),
        inputs: vec![("in".into(), ty)],
        outputs: vec![],
    })
}

impl Block for TcpSink {
    // Client threads wake the block (room in a queue, a client came or went)
    // by unparking the thread that ran `start`.
    fn dedicated_thread(&self) -> bool {
        true
    }

    fn start(&mut self) -> Result<(), Error> {
        let shared = Arc::new(SinkShared {
            closing: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            ended: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            error: Mutex::new(None),
            block: std::thread::current(),
        });
        let (tx, rx) = mpsc::channel();
        let cfg = self.cfg.clone();
        let sh = shared.clone();
        let thread = match cfg.mode {
            Mode::Server => {
                let l = listen("tcp_sink", &cfg.host, cfg.port)?;
                std::thread::Builder::new()
                    .name("fox:tcp_sink".into())
                    .spawn(move || sink_serve(&l, &cfg, &sh, &tx))?
            }
            Mode::Client => std::thread::Builder::new()
                .name("fox:tcp_sink".into())
                .spawn(move || sink_dial(&cfg, &sh, &tx))?,
        };
        self.run = Some(SinkRun {
            shared,
            new_clients: rx,
            clients: Vec::with_capacity(self.cfg.max_clients + 1),
            thread: Some(thread),
        });
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let run = self
            .run
            .as_mut()
            .ok_or_else(|| Error::Other("tcp_sink: not started".into()))?;
        let sh = &*run.shared;
        if sh.failed.load(Ordering::Acquire) {
            let msg = sh.error.lock().unwrap_or_else(|p| p.into_inner()).take();
            return Err(Error::Other(
                msg.unwrap_or_else(|| "tcp_sink failed".into()),
            ));
        }
        // Clients come through a channel (no lock, no allocation: the vector
        // has room for `max_clients`) and leave by dropping their queue's
        // other end.
        while let Ok(c) = run.new_clients.try_recv() {
            run.clients.push(c);
        }
        run.clients.retain(|c| !c.queue.abandoned());
        if sh.ended.load(Ordering::Acquire) {
            return Ok(WorkStatus::Done);
        }
        let item = self.item;
        let inp = io.inputs[0].bytes();
        let n = inp.len() / item;
        if n == 0 {
            return Ok(WorkStatus::Blocked);
        }
        if self.backpressure {
            let room = run.clients.iter().map(|c| c.queue.free() / item).min();
            let k = n.min(room.unwrap_or(0));
            if k == 0 {
                // A client thread unparks us when it frees room or connects.
                return Ok(WorkStatus::Blocked);
            }
            for c in &mut run.clients {
                c.queue.push_partial(&inp[..k * item]);
            }
            io.consume(0, k);
        } else {
            for c in &mut run.clients {
                let fit = (c.queue.free() / item).min(n);
                c.queue.push_partial(&inp[..fit * item]);
                if fit < n {
                    c.skipped += (n - fit) as u64;
                    // Log the first miss, then at most every few seconds.
                    let now = Instant::now();
                    if c.logged.is_none_or(|t| now - t >= SKIP_LOG_INTERVAL) {
                        eprintln!(
                            "tcp_sink: client {} can't keep up; {} items skipped so far",
                            c.peer, c.skipped
                        );
                        c.logged = Some(now);
                    }
                }
            }
            io.consume(0, n);
        }
        Ok(WorkStatus::Ok)
    }

    fn stop(&mut self) -> Result<(), Error> {
        let Some(mut run) = self.run.take() else {
            return Ok(());
        };
        // Close the queues and give the clients a moment to receive what is
        // still queued, then stop for real (dropping `run`).
        run.shared.closing.store(true, Ordering::Release);
        run.clients.clear();
        while run.new_clients.try_recv().is_ok() {}
        let deadline = Instant::now() + Duration::from_secs(1);
        while run.thread.as_ref().is_some_and(|t| !t.is_finished()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }
}

/// Start a client thread writing a new queue to `s`; hand the queue to the block.
fn add_client(
    s: TcpStream,
    peer: SocketAddr,
    cfg: &Config,
    sh: &Arc<SinkShared>,
    tx: &mpsc::Sender<Client>,
) -> Option<JoinHandle<()>> {
    let (queue, rx) = ring::ring::<u8>(cfg.queue);
    queue.set_waker(sh.block.clone());
    let sh2 = sh.clone();
    let t = std::thread::Builder::new()
        .name("fox:tcp_sink-client".into())
        .spawn(move || {
            send_queue(s, rx, &sh2);
            // Wake the block so it forgets this client (and, with
            // backpressure, stops waiting for it).
            sh2.block.unpark();
        })
        .map_err(|e| eprintln!("tcp_sink: cannot serve {peer}: {e}"))
        .ok()?;
    let _ = tx.send(Client {
        queue,
        peer,
        skipped: 0,
        logged: None,
    });
    sh.block.unpark();
    Some(t)
}

/// A client thread: queue → socket until the queue closes or the client goes.
fn send_queue(mut s: TcpStream, mut queue: Consumer<u8>, sh: &SinkShared) {
    queue.set_waker(std::thread::current());
    let _ = s.set_write_timeout(Some(POLL));
    let mut buf = vec![0u8; 1 << 16];
    let stop = || sh.stop.load(Ordering::Acquire);
    loop {
        if stop() {
            return;
        }
        let n = queue.pop(&mut buf);
        if n == 0 {
            if queue.finished() {
                break;
            }
            std::thread::park_timeout(POLL);
            continue;
        }
        let mut off = 0;
        while off < n {
            if stop() {
                return;
            }
            match s.write(&buf[off..n]) {
                Ok(0) => return,
                Ok(k) => off += k,
                Err(e) if transient(&e) => {}
                Err(_) => return,
            }
        }
    }
    let _ = s.shutdown(Shutdown::Write);
}

/// Server mode's connection thread.
fn sink_serve(l: &TcpListener, cfg: &Config, sh: &Arc<SinkShared>, tx: &mpsc::Sender<Client>) {
    let mut threads: Vec<JoinHandle<()>> = Vec::new();
    while let Ok(Some((s, peer))) = accept("tcp_sink", l, || sh.done()) {
        threads.retain(|t| !t.is_finished());
        if threads.len() >= cfg.max_clients {
            eprintln!(
                "tcp_sink: refusing {peer}: already {} clients",
                cfg.max_clients
            );
            continue;
        }
        threads.extend(add_client(s, peer, cfg, sh, tx));
    }
    for t in threads {
        let _ = t.join();
    }
}

/// Client mode's connection thread.
fn sink_dial(cfg: &Config, sh: &Arc<SinkShared>, tx: &mpsc::Sender<Client>) {
    let kind = "tcp_sink";
    while !sh.done() {
        match connect(kind, &cfg.host, cfg.port) {
            Err(e) if !cfg.reconnect => return sh.fail(e),
            Err(_) => {}
            Ok(s) => {
                let peer = s
                    .peer_addr()
                    .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], cfg.port)));
                if let Some(t) = add_client(s, peer, cfg, sh, tx) {
                    let _ = t.join();
                }
                if !cfg.reconnect {
                    sh.ended.store(true, Ordering::Release);
                    sh.block.unpark();
                    return;
                }
            }
        }
        // Wake early when closing, too.
        let end = Instant::now() + cfg.retry;
        while !sh.done() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
