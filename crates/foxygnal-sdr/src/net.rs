//! Plumbing shared by the network drivers (`rtltcp`, `spyserver`): connecting
//! with a timeout and readable errors, and a receive stream fed by an I/O
//! thread that owns the socket's read side.

use std::io;
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{JoinHandle, Thread};
use std::time::Duration;

use crate::ring::{self, Consumer, Producer};
use crate::{Complex32, DeviceArgs, Result, SdrError};

/// How long to wait for a TCP connection and for the server's greeting.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// A streaming server that sends nothing for this long is considered gone.
pub(crate) const STALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Samples buffered between the I/O thread and the block (≈0.4 s at 2.4 MS/s).
pub(crate) const RING_SAMPLES: usize = 1 << 20;

/// `host:port`, with brackets around IPv6 literals.
pub(crate) fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Connect to `host:port`, trying every resolved address. Errors name the
/// server (`what`, e.g. "rtl_tcp") and the address.
pub(crate) fn connect(what: &str, host: &str, port: u16) -> Result<TcpStream> {
    let hp = host_port(host, port);
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| SdrError::Device(format!("could not resolve {what} host {hp}: {e}")))?
        .collect();
    let mut last = None;
    for addr in &addrs {
        match TcpStream::connect_timeout(addr, CONNECT_TIMEOUT) {
            Ok(s) => {
                s.set_nodelay(true)?;
                return Ok(s);
            }
            Err(e) => last = Some(e),
        }
    }
    let why = match last {
        Some(e) if e.kind() == io::ErrorKind::TimedOut => {
            format!("no answer within {} s", CONNECT_TIMEOUT.as_secs())
        }
        Some(e) => e.to_string(),
        None => "the host name resolved to no addresses".into(),
    };
    Err(SdrError::Device(format!(
        "could not connect to {what} at {hp}: {why}"
    )))
}

/// Parses `1/0/true/false/on/off/yes/no` with an error naming the key.
pub(crate) fn get_bool(args: &DeviceArgs, key: &str) -> Result<Option<bool>> {
    match args.get(key) {
        None => Ok(None),
        Some(v) => match v.to_ascii_lowercase().as_str() {
            "1" | "true" | "on" | "yes" | "" => Ok(Some(true)),
            "0" | "false" | "off" | "no" => Ok(Some(false)),
            _ => Err(SdrError::BadArgs(
                args.to_string(),
                format!("`{key}={v}` should be 1 or 0"),
            )),
        },
    }
}

/// Rejects keys the driver doesn't know, listing the ones it does.
pub(crate) fn check_keys(args: &DeviceArgs, known: &[&str]) -> Result<()> {
    for (k, _) in args.pairs() {
        if !known.contains(&k.as_str()) {
            return Err(SdrError::BadArgs(
                args.to_string(),
                format!("unknown option `{k}` (known: {})", known.join(", ")),
            ));
        }
    }
    Ok(())
}

/// Maps a socket read error on the I/O thread to the message that ends the stream.
pub(crate) fn read_error(what: &str, hp: &str, e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::UnexpectedEof => format!("{what} at {hp} closed the connection"),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => format!(
            "{what} at {hp} stopped sending samples (nothing for {} s)",
            STALL_TIMEOUT.as_secs()
        ),
        _ => format!("{what} at {hp}: connection lost: {e}"),
    }
}

struct Shared {
    stop: AtomicBool,
    error: Mutex<Option<String>>,
}

/// A receive stream fed by an I/O thread. Dropping it shuts the socket down
/// (which wakes the thread out of its blocking read) and joins the thread.
pub(crate) struct NetRx {
    consumer: Consumer<Complex32>,
    shared: Arc<Shared>,
    /// The write side, for control commands. Also used to shut the socket down.
    pub(crate) writer: TcpStream,
    thread: Option<JoinHandle<()>>,
}

impl NetRx {
    /// Spawn `io` on its own thread with the read side of `sock`. When `io`
    /// returns an error the stream ends with it (unless we are stopping).
    pub(crate) fn spawn<F>(name: &str, sock: TcpStream, io: F) -> Result<Self>
    where
        F: FnOnce(TcpStream, &mut Producer<Complex32>) -> std::result::Result<(), String>
            + Send
            + 'static,
    {
        let writer = sock.try_clone()?;
        sock.set_read_timeout(Some(STALL_TIMEOUT))?;
        let (mut producer, consumer) = ring::ring(RING_SAMPLES);
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            error: Mutex::new(None),
        });
        let sh = shared.clone();
        let thread = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let r = io(sock, &mut producer);
                if let Err(msg) = r
                    && !sh.stop.load(Ordering::Acquire)
                {
                    *sh.error.lock().unwrap_or_else(|p| p.into_inner()) = Some(msg);
                }
                // Dropping the producer closes the ring after the error is set.
                drop(producer);
            })?;
        Ok(NetRx {
            consumer,
            shared,
            writer,
            thread: Some(thread),
        })
    }

    pub(crate) fn read(&mut self, out: &mut [Complex32]) -> Result<usize> {
        let n = self.consumer.pop(out);
        if n == 0 && self.consumer.finished() {
            let msg = self
                .shared
                .error
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
                .unwrap_or_else(|| "the stream stopped".into());
            return Err(SdrError::Device(msg));
        }
        Ok(n)
    }

    pub(crate) fn set_waker(&mut self, thread: Thread) {
        self.consumer.set_waker(thread);
    }

    pub(crate) fn overflows(&self) -> u64 {
        self.consumer.dropped()
    }

    pub(crate) fn finished(&self) -> bool {
        self.consumer.finished()
    }
}

impl Drop for NetRx {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        let _ = self.writer.shutdown(Shutdown::Both);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
