//! `udp_source` and `udp_sink`.

use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use foxygnal_core::{
    Block, BlockDescriptor, Configure, Error, Instance, ParamKind, ParamReader, ParamSpec,
    PortSpec, Registry, WorkIo, WorkStatus,
};
use serde_json::json;

use super::feed::{Feed, Feeder, Shared};
use super::{CATEGORY, LISTEN_HELP, POLL, host_param, port, port_param, resolve, transient};
use crate::params;

/// Ring between the receive thread and the block: 4 MiB, 50 ms of a
/// 10 MS/s c32 stream.
const RX_RING: usize = 1 << 22;

/// Kernel receive buffer udp_source asks for: 8 MiB, 0.4 s of a 2.4 MS/s c32
/// stream. The default (208 KiB on Linux) is 11 ms of it: a receive thread
/// that is late by that much loses datagrams. The kernel caps the request
/// (Linux: `net.core.rmem_max`).
const RX_SOCKET_BUFFER: usize = 8 << 20;

/// Largest UDP payload over Ethernet without fragmenting (1500 − 20 − 8).
const MTU_PAYLOAD: u64 = 1472;

/// A partial packet waits this long for more items before going out alone.
const LINGER: Duration = Duration::from_millis(10);

pub fn register(r: &mut Registry) {
    r.register(
        BlockDescriptor {
            kind: "udp_source",
            label: "UDP Source",
            category: CATEGORY,
            help: "Receives raw little-endian samples in UDP datagrams (GNU Radio's UDP Sink, \
                   `socat`, `nc -u`, …). With a sequence header, lost datagrams are tagged \
                   `dropped` on the next item (int: about how many items were lost); so is \
                   data dropped because the graph fell behind. A multicast address joins \
                   the group.",
            inputs: vec![],
            outputs: vec![PortSpec {
                name: "out",
                ty: None,
            }],
            params: vec![
                params::any_type(),
                host_param(LISTEN_HELP),
                port_param(),
                header_param(),
                params::optional_rate(
                    "The stream's sample rate, for everything downstream (a network stream \
                     has no rate of its own). 0 = unknown.",
                ),
                ParamSpec {
                    name: "interface",
                    label: "Multicast interface",
                    kind: ParamKind::String,
                    default: json!(""),
                    help: "For a multicast address: the IPv4 address of the interface to \
                           join on. Empty = the system's choice.",
                    live: false,
                },
            ],
        },
        build_source,
    );
    r.register(
        BlockDescriptor {
            kind: "udp_sink",
            label: "UDP Sink",
            category: CATEGORY,
            help: "Sends raw little-endian samples in UDP datagrams (to GNU Radio's UDP \
                   Source, `socat`, `nc -ul`, another foxygnal…). Fire and forget: nothing \
                   waits for a receiver.",
            inputs: vec![PortSpec {
                name: "in",
                ty: None,
            }],
            outputs: vec![],
            params: vec![
                params::any_type(),
                host_param(
                    "Destination host. May be a multicast or broadcast address; anything \
                     other than 127.0.0.1 sends the stream over the network.",
                ),
                port_param(),
                header_param(),
                ParamSpec {
                    name: "payload",
                    label: "Datagram size",
                    kind: ParamKind::Int {
                        min: Some(16),
                        max: Some(65507),
                    },
                    default: json!(MTU_PAYLOAD),
                    help: "Bytes per datagram, header included; the samples are a whole \
                           number of items. 1472 fits one Ethernet frame.",
                    live: false,
                },
            ],
        },
        build_sink,
    );
}

fn header_param() -> ParamSpec {
    ParamSpec {
        name: "header",
        label: "Header",
        kind: ParamKind::Enum {
            options: vec!["none", "seq64", "seq32"],
        },
        default: json!("none"),
        help: "Prefix of each datagram: none (raw samples only), seq64 (64-bit little-endian \
               sequence number, as GNU Radio's \"64-bit Sequence Number\" header) or seq32 (32-bit). \
               Both ends must agree.",
        live: false,
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Header {
    None,
    Seq32,
    Seq64,
}

impl Header {
    fn parse(p: &ParamReader<'_>, kind: &str) -> Result<Self, Error> {
        match p.str("header")? {
            "none" => Ok(Header::None),
            "seq32" => Ok(Header::Seq32),
            "seq64" => Ok(Header::Seq64),
            _ => Err(super::param_err(kind, "header", "none, seq64 or seq32")),
        }
    }

    fn len(self) -> usize {
        match self {
            Header::None => 0,
            Header::Seq32 => 4,
            Header::Seq64 => 8,
        }
    }

    /// Split a datagram into (sequence number, payload); None for a runt.
    fn split(self, pkt: &[u8]) -> Option<(u64, &[u8])> {
        let (h, payload) = pkt.split_at_checked(self.len())?;
        let seq = match self {
            Header::None => 0,
            Header::Seq32 => u32::from_le_bytes(h.try_into().ok()?) as u64,
            Header::Seq64 => u64::from_le_bytes(h.try_into().ok()?),
        };
        Some((seq, payload))
    }

    /// `seq − expected`, wrapping at the header's width.
    fn gap(self, seq: u64, expected: u64) -> i64 {
        match self {
            Header::Seq32 => (seq as u32).wrapping_sub(expected as u32) as i32 as i64,
            _ => seq.wrapping_sub(expected) as i64,
        }
    }

    fn write(self, seq: u64, out: &mut Vec<u8>) {
        match self {
            Header::None => {}
            Header::Seq32 => out.extend_from_slice(&(seq as u32).to_le_bytes()),
            Header::Seq64 => out.extend_from_slice(&seq.to_le_bytes()),
        }
    }
}

/// Datagrams this far behind the expected sequence number are reordered or
/// duplicated and are dropped; further back, the sender restarted.
const REORDER_WINDOW: i64 = 64;

// ---- Source ----------------------------------------------------------------

pub struct UdpSource {
    host: String,
    port: u16,
    interface: String,
    header: Header,
    item: usize,
    sample_rate: f64,
    feed: Option<Feed>,
}

fn build_source(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let ty = p.stream_type("type")?;
    Ok(Instance {
        block: Box::new(UdpSource {
            host: p.str("host")?.to_owned(),
            port: port(p, "udp_source")?,
            interface: p.str("interface")?.trim().to_owned(),
            header: Header::parse(p, "udp_source")?,
            item: ty.item_size(),
            sample_rate: p.f64("sample_rate")?,
            feed: None,
        }),
        inputs: vec![],
        outputs: vec![("out".into(), ty)],
    })
}

impl UdpSource {
    fn bind(&self) -> Result<(UdpSocket, SocketAddr), String> {
        let kind = "udp_source";
        let addr = resolve(kind, &self.host, self.port)?;
        let err = |e: std::io::Error| format!("{kind}: cannot listen on {addr}: {e}");
        let ip = addr.ip();
        if !ip.is_multicast() {
            let sock = UdpSocket::bind(addr).map_err(err)?;
            super::exposure_note(kind, addr);
            return Ok((sock, addr));
        }
        // Multicast: bind the group's port (on Unix, the group address itself,
        // which keeps other traffic to that port out) and join.
        let local = if cfg!(unix) {
            ip
        } else if ip.is_ipv4() {
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        } else {
            IpAddr::V6(Ipv6Addr::UNSPECIFIED)
        };
        let sock = UdpSocket::bind((local, self.port)).map_err(err)?;
        let join = match ip {
            IpAddr::V4(g) => {
                let iface = if self.interface.is_empty() {
                    Ipv4Addr::UNSPECIFIED
                } else {
                    self.interface.parse().map_err(|_| {
                        format!(
                            "{kind}: multicast interface `{}` is not an IPv4 address",
                            self.interface
                        )
                    })?
                };
                sock.join_multicast_v4(&g, &iface)
            }
            IpAddr::V6(g) => sock.join_multicast_v6(&g, 0),
        };
        join.map_err(|e| format!("{kind}: cannot join multicast group {ip}: {e}"))?;
        eprintln!("{kind}: joined multicast group {addr} (receives from the network)");
        Ok((sock, addr))
    }
}

impl Block for UdpSource {
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
        let (sock, addr) = self.bind().map_err(Error::Other)?;
        sock.set_read_timeout(Some(POLL))?;
        // Best effort: a smaller buffer still works, it just tolerates less.
        let _ = socket2::SockRef::from(&sock).set_recv_buffer_size(RX_SOCKET_BUFFER);
        let header = self.header;
        self.feed = Some(Feed::spawn(
            "fox:udp_source",
            self.item,
            RX_RING,
            move |f, sh| receive(&sock, addr, header, f, sh),
        )?);
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        let feed = self
            .feed
            .as_mut()
            .ok_or_else(|| Error::Other("udp_source: not started".into()))?;
        feed.work(&mut io.outputs[0])
    }

    fn stop(&mut self) -> Result<(), Error> {
        self.feed = None;
        Ok(())
    }
}

/// The receive thread.
fn receive(
    sock: &UdpSocket,
    addr: SocketAddr,
    header: Header,
    f: &mut Feeder,
    sh: &Shared,
) -> Result<(), String> {
    let mut buf = vec![0u8; 65536];
    let mut expected: Option<u64> = None;
    while !sh.stopping() {
        let n = match sock.recv(&mut buf) {
            Ok(n) => n,
            // ICMP errors from earlier traffic surface here on some systems.
            Err(e)
                if transient(&e)
                    || matches!(
                        e.kind(),
                        ErrorKind::ConnectionRefused | ErrorKind::ConnectionReset
                    ) =>
            {
                continue;
            }
            Err(e) => return Err(format!("udp_source on {addr}: {e}")),
        };
        let Some((seq, payload)) = header.split(&buf[..n]) else {
            continue;
        };
        if header != Header::None {
            if let Some(exp) = expected {
                let gap = header.gap(seq, exp);
                if (-REORDER_WINDOW..0).contains(&gap) {
                    continue;
                }
                if gap > 0 {
                    f.lost((gap as u64).saturating_mul(payload.len() as u64));
                }
            }
            expected = Some(seq.wrapping_add(1));
        }
        f.push_datagram(payload);
    }
    Ok(())
}

// ---- Sink ------------------------------------------------------------------

pub struct UdpSink {
    host: String,
    port: u16,
    header: Header,
    item: usize,
    /// Items per datagram.
    per_packet: usize,
    sock: Option<UdpSocket>,
    /// Header + samples, reused for every datagram that has a header.
    packet: Vec<u8>,
    seq: u64,
    /// When a partial datagram's worth of items first sat waiting.
    partial_since: Option<Instant>,
}

fn build_sink(p: &ParamReader<'_>) -> Result<Instance, Error> {
    let kind = "udp_sink";
    let ty = p.stream_type("type")?;
    let header = Header::parse(p, kind)?;
    let payload = p.u64("payload")? as usize;
    let item = ty.item_size();
    let per_packet = payload.saturating_sub(header.len()) / item;
    if per_packet == 0 || payload > 65507 {
        return Err(super::param_err(
            kind,
            "payload",
            "must hold the header and at least one item, and be at most 65507 bytes",
        ));
    }
    Ok(Instance {
        block: Box::new(UdpSink {
            host: p.str("host")?.to_owned(),
            port: port(p, kind)?,
            header,
            item,
            per_packet,
            sock: None,
            packet: Vec::with_capacity(header.len() + per_packet * item),
            seq: 0,
            partial_since: None,
        }),
        inputs: vec![("in".into(), ty)],
        outputs: vec![],
    })
}

impl UdpSink {
    /// Send one datagram. Ok(false): the socket buffer is full, try later.
    fn send(&mut self, samples: &[u8]) -> Result<bool, Error> {
        let sock = self.sock.as_ref().expect("started");
        let r = if self.header == Header::None {
            sock.send(samples)
        } else {
            self.packet.clear();
            self.header.write(self.seq, &mut self.packet);
            self.packet.extend_from_slice(samples);
            sock.send(&self.packet)
        };
        match r {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(false),
            // Nobody listening (an ICMP error for an earlier datagram): UDP
            // doesn't care, the datagram counts as sent.
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::ConnectionRefused | ErrorKind::ConnectionReset
                ) => {}
            Err(e) => {
                return Err(Error::Other(format!(
                    "udp_sink to {}: {e}",
                    super::host_port(&self.host, self.port)
                )));
            }
        }
        self.seq = self.seq.wrapping_add(1);
        Ok(true)
    }
}

impl Block for UdpSink {
    fn start(&mut self) -> Result<(), Error> {
        let dest = resolve("udp_sink", &self.host, self.port).map_err(Error::Other)?;
        let local: IpAddr = match dest.ip() {
            IpAddr::V4(ip) if ip.is_loopback() => Ipv4Addr::LOCALHOST.into(),
            IpAddr::V4(_) => Ipv4Addr::UNSPECIFIED.into(),
            IpAddr::V6(ip) if ip.is_loopback() => Ipv6Addr::LOCALHOST.into(),
            IpAddr::V6(_) => Ipv6Addr::UNSPECIFIED.into(),
        };
        let err = |e: std::io::Error| Error::Other(format!("udp_sink to {dest}: {e}"));
        let sock = UdpSocket::bind((local, 0)).map_err(err)?;
        if let IpAddr::V4(ip) = dest.ip()
            && ip.is_broadcast()
        {
            sock.set_broadcast(true).map_err(err)?;
        }
        sock.connect(dest).map_err(err)?;
        sock.set_nonblocking(true).map_err(err)?;
        self.sock = Some(sock);
        Ok(())
    }

    fn work(&mut self, io: &mut WorkIo<'_>) -> Result<WorkStatus, Error> {
        if self.sock.is_none() {
            return Err(Error::Other("udp_sink: not started".into()));
        }
        let inp = io.inputs[0].bytes();
        let avail = inp.len() / self.item;
        if avail == 0 {
            self.partial_since = None;
            return Ok(WorkStatus::Blocked);
        }
        let chunk = self.per_packet * self.item;
        let mut sent = 0;
        let mut full = false;
        while avail - sent >= self.per_packet {
            let off = sent * self.item;
            if !self.send(&inp[off..off + chunk])? {
                full = true;
                break;
            }
            sent += self.per_packet;
        }
        if sent == 0 && !full {
            // Less than one datagram's worth: wait a little for more, then
            // send it short (the end of a stream, or a slow one).
            let now = Instant::now();
            let since = *self.partial_since.get_or_insert(now);
            let waited = now - since;
            if waited < LINGER {
                return Ok(WorkStatus::Wait(LINGER - waited));
            }
            if self.send(&inp[..avail * self.item])? {
                sent = avail;
            } else {
                full = true;
            }
        }
        if sent > 0 {
            self.partial_since = None;
        }
        io.consume(0, sent);
        Ok(if full {
            // The socket's send buffer is full: try again shortly.
            WorkStatus::Wait(Duration::from_millis(1))
        } else {
            WorkStatus::Ok
        })
    }

    fn stop(&mut self) -> Result<(), Error> {
        self.sock = None;
        Ok(())
    }
}
