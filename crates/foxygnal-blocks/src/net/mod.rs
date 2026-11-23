//! Raw samples over the network: `udp_source`, `udp_sink`, `tcp_source`,
//! `tcp_sink` (category *Network*).
//!
//! The wire format is the same as a raw file: interleaved little-endian items
//! (c32 = float I, float Q, …), with no framing, so the blocks talk to GNU
//! Radio's UDP / TCP blocks, `nc` and `socat`. UDP can prefix each datagram
//! with a sequence number (GNU Radio's "64-bit sequence number" header, or a
//! 32-bit one) so the receiver can tag lost datagrams.
//!
//! Threading follows the SDR drivers: sockets are only touched by I/O threads,
//! which talk to the block through the lock-free SPSC rings of
//! `foxygnal_sdr::ring` and unpark its dedicated thread, so `work()` never
//! waits on the network. (`udp_sink` is the exception: it sends from `work()`
//! on a non-blocking socket, which never waits either.)
//!
//! Security: every address defaults to 127.0.0.1. Listening on another
//! address (0.0.0.0 = all interfaces) must be typed into the block and
//! exposes the stream, unauthenticated, to the network; the engine prints a
//! note when it happens. This is independent of `foxygnal serve
//! --allow-remote`, which is about the web UI only.

mod feed;
mod tcp;
mod udp;

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use foxygnal_core::{Error, ParamKind, ParamReader, ParamSpec, Registry};
use serde_json::json;

pub fn register(r: &mut Registry) {
    udp::register(r);
    tcp::register(r);
}

const CATEGORY: &str = "Network";

/// How often I/O threads look at their stop flag while waiting on a socket.
const POLL: Duration = Duration::from_millis(50);

fn host_param(help: &'static str) -> ParamSpec {
    ParamSpec {
        name: "host",
        label: "Address",
        kind: ParamKind::String,
        default: json!("127.0.0.1"),
        help,
        live: false,
    }
}

fn port_param() -> ParamSpec {
    ParamSpec {
        name: "port",
        label: "Port",
        kind: ParamKind::Int {
            min: Some(1),
            max: Some(65535),
        },
        default: json!(2000),
        help: "UDP / TCP port number.",
        live: false,
    }
}

const LISTEN_HELP: &str = "Local address to listen on. 127.0.0.1 (the default) is this \
                           machine only; 0.0.0.0 (or a LAN address) exposes the stream to the \
                           network, without any authentication.";

fn port(p: &ParamReader<'_>, kind: &str) -> Result<u16, Error> {
    match p.u64("port")? {
        v @ 1..=65535 => Ok(v as u16),
        _ => Err(param_err(kind, "port", "must be 1–65535")),
    }
}

fn param_err(kind: &str, param: &str, msg: &str) -> Error {
    Error::Param {
        block: kind.into(),
        param: param.into(),
        msg: msg.into(),
    }
}

/// `host:port` for messages, with brackets around IPv6 literals.
fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Resolve `host` (an IP literal or a name; IPv6 may be bracketed).
fn resolve(kind: &str, host: &str, port: u16) -> Result<SocketAddr, String> {
    let bare = host.trim().trim_start_matches('[').trim_end_matches(']');
    (bare, port)
        .to_socket_addrs()
        .map_err(|e| format!("{kind}: cannot resolve {}: {e}", host_port(host, port)))?
        .next()
        .ok_or_else(|| format!("{kind}: {} resolved to no addresses", host_port(host, port)))
}

/// Say so when a listener is reachable from other machines.
fn exposure_note(kind: &str, addr: SocketAddr) {
    if !addr.ip().is_loopback() {
        eprintln!(
            "{kind}: listening on {addr}, which other machines can reach (the stream has no \
             authentication)"
        );
    }
}

/// Sleep for `d`, waking early (and returning false) when `stop` is set.
fn nap(stop: &AtomicBool, d: Duration) -> bool {
    let end = Instant::now() + d;
    loop {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        let now = Instant::now();
        if now >= end {
            return true;
        }
        std::thread::sleep((end - now).min(Duration::from_millis(20)));
    }
}

/// Errors from a socket read/write/accept that only mean "try again".
fn transient(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(e.kind(), WouldBlock | TimedOut | Interrupted)
}
