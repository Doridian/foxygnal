//! `rtltcp`: a client for `rtl_tcp` servers (osmocom rtl-sdr's `rtl_tcp.c`).
//!
//! Device string: `rtltcp:host=10.0.0.5,port=1234` (defaults `127.0.0.1`,
//! `1234`). Options, each `1` or `0`: `agc` (the RTL2832's digital AGC),
//! `offset_tuning`, `bias_tee`; and `direct_sampling=0|1|2` (off, I branch,
//! Q branch).
//!
//! Protocol: on connect the server sends a 12-byte header, `"RTL0"`, the tuner
//! type (u32 BE) and the number of tuner gains (u32 BE), then streams raw
//! interleaved u8 I/Q until the client disconnects. Commands are 5 bytes: a
//! command byte and a u32 BE parameter. `rtl_tcp` serves one client at a
//! time; a second client's connection is accepted by the kernel but gets no
//! header until the first leaves.

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::thread::Thread;
use std::time::Duration;

use crate::net::{self, NetRx};
use crate::ring::Producer;
use crate::{
    Capabilities, Complex32, Device, DeviceArgs, DeviceInfo, Driver, Gain, Range, Result, RxConfig,
    RxStream, SdrError,
};

pub const DEFAULT_PORT: u16 = 1234;
const WHAT: &str = "rtl_tcp";

/// Command bytes (rtl_tcp.c `command_worker`).
pub mod cmd {
    pub const FREQUENCY: u8 = 0x01;
    pub const SAMPLE_RATE: u8 = 0x02;
    /// 0 = automatic tuner gain, 1 = manual.
    pub const GAIN_MODE: u8 = 0x03;
    /// Tuner gain in tenths of a dB.
    pub const GAIN: u8 = 0x04;
    /// Signed ppm.
    pub const FREQ_CORRECTION: u8 = 0x05;
    /// `stage << 16 | (gain as i16 as u16)`.
    pub const IF_GAIN: u8 = 0x06;
    pub const AGC_MODE: u8 = 0x08;
    pub const DIRECT_SAMPLING: u8 = 0x09;
    pub const OFFSET_TUNING: u8 = 0x0a;
    pub const GAIN_BY_INDEX: u8 = 0x0d;
    pub const BIAS_TEE: u8 = 0x0e;
}

/// librtlsdr's sample-rate limits (`rtlsdr_set_sample_rate`).
const SAMPLE_RATES: [Range; 2] = [
    Range::new(225_001.0, 300_000.0),
    Range::new(900_001.0, 3_200_000.0),
];

/// The RTL2832's crystal: the direct-sampling tuning range.
const RTL_XTAL: f64 = 28_800_000.0;

/// What librtlsdr knows about a tuner type (`enum rtlsdr_tuner`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Tuner {
    pub name: &'static str,
    /// Tunable ranges, from gr-osmosdr's `rtl_source_c::get_freq_range`.
    pub freq: &'static [(f64, f64)],
    /// Gains in tenths of a dB, from librtlsdr's `rtlsdr_get_tuner_gains`.
    pub gains: &'static [i32],
}

const E4K_GAINS: &[i32] = &[
    -10, 15, 40, 65, 90, 115, 140, 165, 190, 215, 240, 290, 340, 420,
];
const FC0012_GAINS: &[i32] = &[-99, -40, 71, 179, 192];
const FC0013_GAINS: &[i32] = &[
    -99, -73, -65, -63, -60, -58, -54, 58, 61, 63, 65, 67, 68, 70, 71, 179, 181, 182, 184, 186,
    188, 191, 197,
];
const R82XX_GAINS: &[i32] = &[
    0, 9, 14, 27, 37, 77, 87, 125, 144, 157, 166, 197, 207, 229, 254, 280, 297, 328, 338, 364, 372,
    386, 402, 421, 434, 439, 445, 480, 496,
];

pub(crate) fn tuner(kind: u32) -> Tuner {
    match kind {
        1 => Tuner {
            name: "E4000",
            // There is a temperature-dependent gap around 1100–1250 MHz.
            freq: &[(52e6, 2.2e9)],
            gains: E4K_GAINS,
        },
        2 => Tuner {
            name: "FC0012",
            freq: &[(22e6, 948e6)],
            gains: FC0012_GAINS,
        },
        3 => Tuner {
            name: "FC0013",
            freq: &[(22e6, 1.1e9)],
            gains: FC0013_GAINS,
        },
        4 => Tuner {
            name: "FC2580",
            freq: &[(146e6, 308e6), (438e6, 924e6)],
            gains: &[],
        },
        5 => Tuner {
            name: "R820T",
            freq: &[(24e6, 1766e6)],
            gains: R82XX_GAINS,
        },
        6 => Tuner {
            // Over rtl_tcp we can't tell an RTL-SDR Blog V4 (which has an
            // upconverter and tunes from ~0 Hz) from other R828D dongles, so
            // allow the V4's range.
            name: "R828D",
            freq: &[(0.0, 1766e6)],
            gains: R82XX_GAINS,
        },
        _ => Tuner {
            name: "unknown tuner",
            freq: &[],
            gains: &[],
        },
    }
}

/// The nearest supported gain in tenths of a dB (librtlsdr picks the nearest
/// step too, but sending a listed value makes the result predictable).
pub(crate) fn nearest_gain(gains: &[i32], db: f64) -> i32 {
    let want = (db * 10.0).round() as i32;
    gains
        .iter()
        .copied()
        .min_by_key(|g| (g - want).abs())
        .unwrap_or(want)
}

fn command(c: u8, param: u32) -> [u8; 5] {
    let p = param.to_be_bytes();
    [c, p[0], p[1], p[2], p[3]]
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Options {
    agc: bool,
    offset_tuning: bool,
    bias_tee: bool,
    direct_sampling: u32,
}

/// The `rtltcp` driver.
pub struct RtlTcpDriver;

impl Driver for RtlTcpDriver {
    fn name(&self) -> &'static str {
        "rtltcp"
    }

    fn label(&self) -> &'static str {
        "rtl_tcp (network)"
    }

    /// Nothing: probing would claim a single-client `rtl_tcp` (and power up
    /// its dongle) on every discovery. Open with a device string instead.
    fn discover(&self) -> Result<Vec<DeviceInfo>> {
        Ok(vec![])
    }

    fn open(&self, args: &DeviceArgs) -> Result<Box<dyn Device>> {
        Ok(Box::new(RtlTcpDevice::open(args)?))
    }
}

/// An `rtl_tcp` server, connected.
pub struct RtlTcpDevice {
    info: DeviceInfo,
    host: String,
    port: u16,
    opts: Options,
    tuner: Tuner,
    /// The connection from `open`, until a stream takes it.
    conn: Option<TcpStream>,
}

impl RtlTcpDevice {
    pub fn open(args: &DeviceArgs) -> Result<Self> {
        net::check_keys(
            args,
            &[
                "host",
                "port",
                "agc",
                "offset_tuning",
                "bias_tee",
                "direct_sampling",
            ],
        )?;
        let host = args.get("host").unwrap_or("127.0.0.1").to_string();
        let port = args.get_parsed::<u16>("port")?.unwrap_or(DEFAULT_PORT);
        let direct_sampling = args.get_parsed::<u32>("direct_sampling")?.unwrap_or(0);
        if direct_sampling > 2 {
            return Err(SdrError::BadArgs(
                args.to_string(),
                "`direct_sampling` is 0 (off), 1 (I branch) or 2 (Q branch)".into(),
            ));
        }
        let opts = Options {
            agc: net::get_bool(args, "agc")?.unwrap_or(false),
            offset_tuning: net::get_bool(args, "offset_tuning")?.unwrap_or(false),
            bias_tee: net::get_bool(args, "bias_tee")?.unwrap_or(false),
            direct_sampling,
        };
        let (conn, kind, _gain_count) = handshake(&host, port)?;
        let tuner = tuner(kind);
        let hp = net::host_port(&host, port);
        let mut canonical = DeviceArgs::new("rtltcp")
            .with("host", host.clone())
            .with("port", port.to_string());
        for (k, v) in args.pairs() {
            if k != "host" && k != "port" {
                canonical = canonical.with(k, v.clone());
            }
        }
        Ok(RtlTcpDevice {
            info: DeviceInfo {
                driver: "rtltcp".into(),
                args: canonical.to_string(),
                label: format!("rtl_tcp {} at {hp}", tuner.name),
                serial: None,
                rx: true,
                tx: false,
            },
            host,
            port,
            opts,
            tuner,
            conn: Some(conn),
        })
    }

    fn hp(&self) -> String {
        net::host_port(&self.host, self.port)
    }
}

/// Connect and read the 12-byte header. Returns (socket, tuner type, gain count).
fn handshake(host: &str, port: u16) -> Result<(TcpStream, u32, u32)> {
    let hp = net::host_port(host, port);
    let mut s = net::connect(WHAT, host, port)?;
    s.set_read_timeout(Some(net::CONNECT_TIMEOUT))?;
    let mut hdr = [0u8; 12];
    if let Err(e) = s.read_exact(&mut hdr) {
        return Err(SdrError::Device(match e.kind() {
            ErrorKind::WouldBlock | ErrorKind::TimedOut => format!(
                "rtl_tcp at {hp} accepted the connection but sent no header within {} s \
                 (is another client connected? rtl_tcp serves one client at a time)",
                net::CONNECT_TIMEOUT.as_secs()
            ),
            ErrorKind::UnexpectedEof => format!("rtl_tcp at {hp} closed the connection"),
            _ => format!("rtl_tcp at {hp}: {e}"),
        }));
    }
    if &hdr[..4] != b"RTL0" {
        return Err(SdrError::Device(format!(
            "{hp} doesn't look like an rtl_tcp server (expected header \"RTL0\", got {:02x?})",
            &hdr[..4]
        )));
    }
    let kind = u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
    let gains = u32::from_be_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);
    s.set_write_timeout(Some(Duration::from_secs(2)))?;
    Ok((s, kind, gains))
}

impl Device for RtlTcpDevice {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn capabilities(&self) -> Capabilities {
        let frequency = if self.opts.direct_sampling != 0 {
            vec![Range::new(0.0, RTL_XTAL)]
        } else {
            self.tuner
                .freq
                .iter()
                .map(|&(a, b)| Range::new(a, b))
                .collect()
        };
        let g = self.tuner.gains;
        Capabilities {
            frequency,
            sample_rate: SAMPLE_RATES.to_vec(),
            gain: match (g.first(), g.last()) {
                (Some(&lo), Some(&hi)) => Some(Range::new(lo as f64 / 10.0, hi as f64 / 10.0)),
                _ => None,
            },
            agc: true,
            antennas: vec![],
            bandwidth: vec![],
        }
    }

    fn start_rx(&mut self, cfg: &RxConfig) -> Result<Box<dyn RxStream>> {
        if cfg.channel != 0 {
            return Err(SdrError::Range(format!(
                "rtl_tcp has one channel; channel {} doesn't exist",
                cfg.channel
            )));
        }
        let caps = self.capabilities();
        caps.check_rate(cfg.sample_rate)?;
        caps.check_frequency(cfg.frequency)?;
        let sock = match self.conn.take() {
            Some(s) => s,
            None => handshake(&self.host, self.port)?.0,
        };
        let hp = self.hp();
        let o = self.opts;
        let mut setup = vec![
            command(cmd::DIRECT_SAMPLING, o.direct_sampling),
            command(cmd::OFFSET_TUNING, o.offset_tuning as u32),
            command(cmd::BIAS_TEE, o.bias_tee as u32),
            command(cmd::SAMPLE_RATE, cfg.sample_rate.round() as u32),
            command(cmd::FREQ_CORRECTION, cfg.ppm.round() as i32 as u32),
            command(cmd::FREQUENCY, cfg.frequency.round() as u32),
            command(cmd::AGC_MODE, o.agc as u32),
        ];
        setup.extend(gain_commands(self.tuner, cfg.gain));
        let bytes: Vec<u8> = setup.concat();
        (&sock)
            .write_all(&bytes)
            .map_err(|e| SdrError::Device(format!("rtl_tcp at {hp}: sending settings: {e}")))?;
        let io_hp = hp.clone();
        let rx = NetRx::spawn("rtltcp-rx", sock, move |s, p| io_loop(s, p, &io_hp))?;
        Ok(Box::new(RtlTcpRx {
            rx,
            hp,
            caps,
            tuner: self.tuner,
        }))
    }
}

fn gain_commands(tuner: Tuner, gain: Gain) -> Vec<[u8; 5]> {
    match gain {
        Gain::Auto => vec![command(cmd::GAIN_MODE, 0)],
        Gain::Db(db) => vec![
            command(cmd::GAIN_MODE, 1),
            command(cmd::GAIN, nearest_gain(tuner.gains, db) as u32),
        ],
    }
}

/// The I/O thread: read u8 I/Q in large chunks, convert, push.
fn io_loop(
    mut sock: TcpStream,
    producer: &mut Producer<Complex32>,
    hp: &str,
) -> std::result::Result<(), String> {
    const CHUNK: usize = 64 * 1024;
    let lut: [f32; 256] = std::array::from_fn(|i| (i as f32 - 127.5) / 127.5);
    // One extra byte for a half sample left over from the previous read.
    let mut buf = vec![0u8; CHUNK + 1];
    let mut out = vec![Complex32::default(); CHUNK / 2];
    let mut have = 0;
    loop {
        let n = match sock.read(&mut buf[have..]) {
            Ok(0) => return Err(format!("rtl_tcp at {hp} closed the connection")),
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(net::read_error(WHAT, hp, &e)),
        };
        have += n;
        let pairs = have / 2;
        for (o, iq) in out.iter_mut().zip(buf[..pairs * 2].chunks_exact(2)) {
            *o = Complex32::new(lut[iq[0] as usize], lut[iq[1] as usize]);
        }
        producer.push(&out[..pairs]);
        if have % 2 == 1 {
            buf[0] = buf[have - 1];
            have = 1;
        } else {
            have = 0;
        }
    }
}

struct RtlTcpRx {
    rx: NetRx,
    hp: String,
    caps: Capabilities,
    tuner: Tuner,
}

impl RtlTcpRx {
    fn send(&mut self, cmds: &[[u8; 5]]) -> Result<()> {
        let bytes: Vec<u8> = cmds.concat();
        self.rx.writer.write_all(&bytes).map_err(|e| {
            SdrError::Device(format!("rtl_tcp at {}: sending a command: {e}", self.hp))
        })
    }
}

impl RxStream for RtlTcpRx {
    fn read(&mut self, out: &mut [Complex32]) -> Result<usize> {
        self.rx.read(out)
    }

    fn set_waker(&mut self, thread: Thread) {
        self.rx.set_waker(thread);
    }

    fn set_frequency(&mut self, freq: f64) -> Result<()> {
        self.caps.check_frequency(freq)?;
        self.send(&[command(cmd::FREQUENCY, freq.round() as u32)])
    }

    fn set_gain(&mut self, gain: Gain) -> Result<()> {
        let cmds = gain_commands(self.tuner, gain);
        self.send(&cmds)
    }

    fn set_ppm(&mut self, ppm: f64) -> Result<()> {
        self.send(&[command(cmd::FREQ_CORRECTION, ppm.round() as i32 as u32)])
    }

    fn overflows(&self) -> u64 {
        self.rx.overflows()
    }

    fn finished(&self) -> bool {
        self.rx.finished()
    }
}

#[cfg(test)]
mod tests {
    //! Everything here runs against an in-process fake server; no `rtl_tcp`
    //! binary or dongle is needed.

    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Instant;

    struct Fake {
        port: u16,
        cmds: mpsc::Receiver<(u8, u32)>,
        /// Send bytes to stream; `None` closes the connection.
        data: mpsc::Sender<Option<Vec<u8>>>,
    }

    fn fake(tuner: u32) -> Fake {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (ctx, cmds) = mpsc::channel();
        let (data, drx) = mpsc::channel::<Option<Vec<u8>>>();
        std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut hdr = b"RTL0".to_vec();
            hdr.extend(tuner.to_be_bytes());
            hdr.extend(29u32.to_be_bytes());
            s.write_all(&hdr).unwrap();
            let mut r = s.try_clone().unwrap();
            std::thread::spawn(move || {
                let mut c = [0u8; 5];
                while r.read_exact(&mut c).is_ok() {
                    let p = u32::from_be_bytes([c[1], c[2], c[3], c[4]]);
                    if ctx.send((c[0], p)).is_err() {
                        break;
                    }
                }
            });
            while let Ok(Some(d)) = drx.recv() {
                if s.write_all(&d).is_err() {
                    break;
                }
            }
            let _ = s.shutdown(std::net::Shutdown::Both);
        });
        Fake { port, cmds, data }
    }

    fn cfg() -> RxConfig {
        RxConfig {
            sample_rate: 2_400_000.0,
            frequency: 100_000_000.0,
            gain: Gain::Db(30.0),
            ppm: -3.0,
            bandwidth: None,
            antenna: None,
            channel: 0,
        }
    }

    fn recv_cmds(f: &Fake, n: usize) -> Vec<(u8, u32)> {
        (0..n)
            .map(|_| f.cmds.recv_timeout(Duration::from_secs(5)).unwrap())
            .collect()
    }

    fn read_n(rx: &mut dyn RxStream, n: usize) -> Vec<Complex32> {
        let mut got = Vec::new();
        let mut buf = vec![Complex32::default(); 4096];
        let deadline = Instant::now() + Duration::from_secs(5);
        while got.len() < n {
            assert!(
                Instant::now() < deadline,
                "only {} of {n} samples",
                got.len()
            );
            let k = rx.read(&mut buf).unwrap();
            got.extend_from_slice(&buf[..k]);
            if k == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        got
    }

    #[test]
    fn configures_streams_and_retunes() {
        let f = fake(5);
        let mut dev = RtlTcpDevice::open(
            &DeviceArgs::parse(&format!("rtltcp:host=127.0.0.1,port={},bias_tee=1", f.port))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            dev.info().label,
            format!("rtl_tcp R820T at 127.0.0.1:{}", f.port)
        );
        let caps = dev.capabilities();
        assert_eq!(caps.gain, Some(Range::new(0.0, 49.6)));
        assert!(caps.check_frequency(1.7e9).is_ok());
        assert!(caps.check_frequency(2.0e9).is_err());
        assert!(caps.check_rate(2_400_000.0).is_ok());
        assert!(caps.check_rate(500_000.0).is_err());

        let mut rx = dev.start_rx(&cfg()).unwrap();
        assert_eq!(
            recv_cmds(&f, 9),
            vec![
                (cmd::DIRECT_SAMPLING, 0),
                (cmd::OFFSET_TUNING, 0),
                (cmd::BIAS_TEE, 1),
                (cmd::SAMPLE_RATE, 2_400_000),
                (cmd::FREQ_CORRECTION, (-3i32) as u32),
                (cmd::FREQUENCY, 100_000_000),
                (cmd::AGC_MODE, 0),
                (cmd::GAIN_MODE, 1),
                // 30 dB → nearest R820T step, 29.7 dB.
                (cmd::GAIN, 297),
            ]
        );

        // Every byte value, in chunks that split I/Q pairs.
        let bytes: Vec<u8> = (0..4096u32).map(|i| (i % 256) as u8).collect();
        for c in bytes.chunks(777) {
            f.data.send(Some(c.to_vec())).unwrap();
        }
        let got = read_n(&mut *rx, 2048);
        assert_eq!(got.len(), 2048);
        for (k, s) in got.iter().enumerate() {
            let i = bytes[2 * k] as f32;
            let q = bytes[2 * k + 1] as f32;
            assert_eq!(*s, Complex32::new((i - 127.5) / 127.5, (q - 127.5) / 127.5));
        }
        assert_eq!(got[0], Complex32::new(-1.0, (1.0 - 127.5) / 127.5));
        assert_eq!(got[127].re, (254.0 - 127.5) / 127.5);

        rx.set_frequency(433_920_000.0).unwrap();
        assert_eq!(recv_cmds(&f, 1), vec![(cmd::FREQUENCY, 433_920_000)]);
        rx.set_gain(Gain::Auto).unwrap();
        assert_eq!(recv_cmds(&f, 1), vec![(cmd::GAIN_MODE, 0)]);
        rx.set_ppm(12.4).unwrap();
        assert_eq!(recv_cmds(&f, 1), vec![(cmd::FREQ_CORRECTION, 12)]);
        assert!(rx.set_frequency(3e9).is_err());

        // The server goes away: the stream drains, then reports why.
        f.data.send(Some(vec![200, 50])).unwrap();
        f.data.send(None).unwrap();
        let tail = read_n(&mut *rx, 1);
        assert_eq!(
            tail,
            vec![Complex32::new(
                (200.0 - 127.5) / 127.5,
                (50.0 - 127.5) / 127.5
            )]
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let err = loop {
            match rx.read(&mut [Complex32::default(); 16]) {
                Ok(0) => {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(n) => panic!("unexpected {n} samples"),
                Err(e) => break e,
            }
        };
        assert!(rx.finished());
        assert_eq!(
            err.to_string(),
            format!(
                "device error: rtl_tcp at 127.0.0.1:{} closed the connection",
                f.port
            )
        );
    }

    #[test]
    fn closed_port_fails_fast_and_names_it() {
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let t = Instant::now();
        let err =
            match RtlTcpDriver.open(&DeviceArgs::parse(&format!("rtltcp:port={port}")).unwrap()) {
                Ok(_) => panic!("connected to a closed port"),
                Err(e) => e.to_string(),
            };
        assert!(
            t.elapsed() < Duration::from_secs(1),
            "took {:?}",
            t.elapsed()
        );
        assert!(
            err.contains(&format!("rtl_tcp at 127.0.0.1:{port}")),
            "{err}"
        );
    }

    #[test]
    fn rejects_non_rtl_tcp_servers_and_bad_options() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            s.write_all(b"HTTP/1.1 400").unwrap();
            std::thread::sleep(Duration::from_millis(200));
        });
        let err = RtlTcpDevice::open(&DeviceArgs::parse(&format!("rtltcp:port={port}")).unwrap())
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("doesn't look like an rtl_tcp server"), "{err}");
        let err = RtlTcpDevice::open(&DeviceArgs::parse("rtltcp:hots=x").unwrap())
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("unknown option `hots`"), "{err}");
    }

    #[test]
    fn busy_server_times_out_with_a_hint() {
        // Like an rtl_tcp that is serving someone else: the kernel accepts the
        // connection into the backlog, but no header ever comes.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let err = RtlTcpDevice::open(&DeviceArgs::parse(&format!("rtltcp:port={port}")).unwrap())
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("sent no header"), "{err}");
        assert!(err.contains("one client at a time"), "{err}");
        drop(l);
    }

    #[test]
    fn tuner_tables() {
        assert_eq!(tuner(1).name, "E4000");
        assert_eq!(nearest_gain(E4K_GAINS, 100.0), 420);
        assert_eq!(nearest_gain(R82XX_GAINS, 0.4), 0);
        assert_eq!(nearest_gain(&[], 12.34), 123);
        assert_eq!(tuner(99).freq, &[] as &[(f64, f64)]);
        assert_eq!(command(cmd::FREQUENCY, 0x0102_0304), [1, 1, 2, 3, 4]);
    }
}
