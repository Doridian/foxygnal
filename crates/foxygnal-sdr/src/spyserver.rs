//! `spyserver`: a client for Airspy SpyServer (protocol 2.0.1700).
//!
//! Device string: `spyserver:host=10.0.0.5,port=5555` (defaults `127.0.0.1`,
//! `5555`). Option `format=int16|uint8|int24|float` picks the I/Q sample
//! format on the wire (default `int16`; `uint8` halves the bandwidth). A
//! server that forces a format overrides it.
//!
//! Protocol (all integers little-endian): the client sends commands, a header
//! `{u32 command, u32 body size}` and a body. HELLO's body is the protocol
//! version and the client's name; SET_SETTING's is `{u32 setting, u32
//! value}`. The server sends messages with a 20-byte header `{u32 protocol id,
//! u32 type, u32 stream type, u32 sequence, u32 body size}`; the low 16 bits
//! of the type say what it is and the high 16 carry the digital gain (dB)
//! applied to I/Q samples. After HELLO the server sends DEVICE_INFO and
//! CLIENT_SYNC; it sends a new CLIENT_SYNC whenever control or the tuning
//! changes.
//!
//! Sample rates are the device's maximum rate divided by 2^n for n from the
//! minimum I/Q decimation up to the number of decimation stages.
//!
//! Gain is the device's gain *index* (0 to its maximum index), not dB.
//! [`Gain::Auto`] leaves the server's gain alone. A SpyServer shared by
//! several clients gives control to one; the others (`can_control` 0) may not
//! change the gain and may only tune within the band the server reports.

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::thread::Thread;
use std::time::{Duration, Instant};

use crate::net::{self, NetRx};
use crate::ring::Producer;
use crate::{
    Capabilities, Complex32, Device, DeviceArgs, DeviceInfo, Driver, Gain, Range, Result, RxConfig,
    RxStream, SdrError,
};

pub const DEFAULT_PORT: u16 = 5555;
const WHAT: &str = "SpyServer";

pub const PROTOCOL_VERSION: u32 = (2 << 24) | 1700;
const MAX_BODY: usize = 1 << 20;
const CLIENT_NAME: &[u8] = b"foxygnal";

pub mod proto {
    pub const CMD_HELLO: u32 = 0;
    pub const CMD_SET_SETTING: u32 = 2;

    pub const SETTING_STREAMING_MODE: u32 = 0;
    pub const SETTING_STREAMING_ENABLED: u32 = 1;
    pub const SETTING_GAIN: u32 = 2;
    pub const SETTING_IQ_FORMAT: u32 = 100;
    pub const SETTING_IQ_FREQUENCY: u32 = 101;
    pub const SETTING_IQ_DECIMATION: u32 = 102;
    pub const SETTING_IQ_DIGITAL_GAIN: u32 = 103;

    pub const STREAM_MODE_IQ_ONLY: u32 = 1;

    pub const FORMAT_UINT8: u32 = 1;
    pub const FORMAT_INT16: u32 = 2;
    pub const FORMAT_INT24: u32 = 3;
    pub const FORMAT_FLOAT: u32 = 4;

    pub const MSG_DEVICE_INFO: u32 = 0;
    pub const MSG_CLIENT_SYNC: u32 = 1;
    pub const MSG_UINT8_IQ: u32 = 100;
    pub const MSG_INT16_IQ: u32 = 101;
    pub const MSG_INT24_IQ: u32 = 102;
    pub const MSG_FLOAT_IQ: u32 = 103;

    pub const DEVICE_AIRSPY_ONE: u32 = 1;
    pub const DEVICE_AIRSPY_HF: u32 = 2;
    pub const DEVICE_RTLSDR: u32 = 3;
}
use proto::*;

fn u32s<const N: usize>(b: &[u8]) -> [u32; N] {
    std::array::from_fn(|i| {
        u32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]])
    })
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DevInfo {
    pub device_type: u32,
    pub serial: u32,
    pub max_sample_rate: u32,
    pub max_bandwidth: u32,
    pub decimation_stages: u32,
    pub gain_stages: u32,
    pub max_gain_index: u32,
    pub min_frequency: u32,
    pub max_frequency: u32,
    pub resolution: u32,
    pub min_iq_decimation: u32,
    pub forced_iq_format: u32,
}

impl DevInfo {
    fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < 48 {
            return None;
        }
        let v = u32s::<12>(b);
        Some(DevInfo {
            device_type: v[0],
            serial: v[1],
            max_sample_rate: v[2],
            max_bandwidth: v[3],
            decimation_stages: v[4],
            gain_stages: v[5],
            max_gain_index: v[6],
            min_frequency: v[7],
            max_frequency: v[8],
            resolution: v[9],
            min_iq_decimation: v[10],
            forced_iq_format: v[11],
        })
    }

    fn name(&self) -> &'static str {
        match self.device_type {
            DEVICE_AIRSPY_ONE => "Airspy",
            DEVICE_AIRSPY_HF => "Airspy HF+",
            DEVICE_RTLSDR => "RTL-SDR",
            _ => "unknown device",
        }
    }

    /// (decimation, rate) for every rate the server offers, fastest first.
    fn rates(&self) -> Vec<(u32, f64)> {
        (self.min_iq_decimation..=self.decimation_stages.min(31))
            .map(|d| (d, self.max_sample_rate as f64 / (1u64 << d) as f64))
            .collect()
    }

    /// The digital gain SDR++ asks for: it keeps decimated low-bit-depth
    /// samples from losing resolution. The server reports what it applied
    /// in each message, so the samples are scaled back either way.
    fn digital_gain(&self, gain_index: u32, decimation: u32) -> u32 {
        let dec_db = (decimation as f32 * 3.01) as u32;
        match self.device_type {
            DEVICE_AIRSPY_ONE => self.max_gain_index.saturating_sub(gain_index) + dec_db,
            DEVICE_AIRSPY_HF | DEVICE_RTLSDR => dec_db,
            _ => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ClientSync {
    pub can_control: bool,
    pub gain: u32,
    pub device_center_frequency: u32,
    pub iq_center_frequency: u32,
    pub min_iq_center_frequency: u32,
    pub max_iq_center_frequency: u32,
}

impl ClientSync {
    fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < 36 {
            return None;
        }
        let v = u32s::<9>(b);
        Some(ClientSync {
            can_control: v[0] != 0,
            gain: v[1],
            device_center_frequency: v[2],
            iq_center_frequency: v[3],
            // v[4] is the FFT centre; v[7], v[8] the FFT limits.
            min_iq_center_frequency: v[5],
            max_iq_center_frequency: v[6],
        })
    }
}

fn command(cmd: u32, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + body.len());
    v.extend(cmd.to_le_bytes());
    v.extend((body.len() as u32).to_le_bytes());
    v.extend(body);
    v
}

fn setting(s: u32, value: u32) -> Vec<u8> {
    let mut body = [0u8; 8];
    body[..4].copy_from_slice(&s.to_le_bytes());
    body[4..].copy_from_slice(&value.to_le_bytes());
    command(CMD_SET_SETTING, &body)
}

struct Header {
    msg_type: u32,
    flags: u32,
    body_size: usize,
}

/// Read one message header and check it.
fn read_header(s: &mut TcpStream, hp: &str) -> std::result::Result<Header, std::io::Error> {
    let mut h = [0u8; 20];
    s.read_exact(&mut h)?;
    let v = u32s::<5>(&h);
    let (id, ty, size) = (v[0], v[1], v[4] as usize);
    if id >> 16 != PROTOCOL_VERSION >> 16 {
        return Err(std::io::Error::other(format!(
            "SpyServer at {hp} speaks protocol {}.{}.{}; foxygnal supports 2.0",
            id >> 24,
            (id >> 16) & 0xff,
            id & 0xffff
        )));
    }
    if size > MAX_BODY {
        return Err(std::io::Error::other(format!(
            "SpyServer at {hp} sent a {size}-byte message (limit {MAX_BODY}); protocol error"
        )));
    }
    Ok(Header {
        msg_type: ty & 0xffff,
        flags: ty >> 16,
        body_size: size,
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Format {
    U8,
    I16,
    I24,
    F32,
}

impl Format {
    fn wire(self) -> u32 {
        match self {
            Format::U8 => FORMAT_UINT8,
            Format::I16 => FORMAT_INT16,
            Format::I24 => FORMAT_INT24,
            Format::F32 => FORMAT_FLOAT,
        }
    }

    fn from_wire(v: u32) -> Option<Self> {
        match v {
            FORMAT_UINT8 => Some(Format::U8),
            FORMAT_INT16 => Some(Format::I16),
            FORMAT_INT24 => Some(Format::I24),
            FORMAT_FLOAT => Some(Format::F32),
            _ => None,
        }
    }
}

/// The `spyserver` driver.
pub struct SpyServerDriver;

impl Driver for SpyServerDriver {
    fn name(&self) -> &'static str {
        "spyserver"
    }

    fn label(&self) -> &'static str {
        "Airspy SpyServer (network)"
    }

    /// Nothing: SpyServers are found by address, not discovered.
    fn discover(&self) -> Result<Vec<DeviceInfo>> {
        Ok(vec![])
    }

    fn open(&self, args: &DeviceArgs) -> Result<Box<dyn Device>> {
        Ok(Box::new(SpyServerDevice::open(args)?))
    }
}

/// A connected SpyServer.
pub struct SpyServerDevice {
    info: DeviceInfo,
    host: String,
    port: u16,
    format: Format,
    dev: DevInfo,
    sync: Arc<Mutex<ClientSync>>,
    /// The connection from `open`, until a stream takes it.
    conn: Option<TcpStream>,
}

/// Connect, say HELLO and wait for DEVICE_INFO and CLIENT_SYNC.
fn handshake(host: &str, port: u16) -> Result<(TcpStream, DevInfo, ClientSync)> {
    let hp = net::host_port(host, port);
    let mut s = net::connect(WHAT, host, port)?;
    s.set_read_timeout(Some(net::CONNECT_TIMEOUT))?;
    s.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut hello = PROTOCOL_VERSION.to_le_bytes().to_vec();
    hello.extend(CLIENT_NAME);
    s.write_all(&command(CMD_HELLO, &hello))
        .map_err(|e| SdrError::Device(format!("SpyServer at {hp}: sending HELLO: {e}")))?;
    let deadline = Instant::now() + net::CONNECT_TIMEOUT;
    let (mut dev, mut sync) = (None, None);
    let mut body = vec![0u8; 4096];
    let fail = |e: std::io::Error| {
        SdrError::Device(match e.kind() {
            ErrorKind::WouldBlock | ErrorKind::TimedOut => format!(
                "SpyServer at {hp} didn't send its device info within {} s \
                 (is it a SpyServer?)",
                net::CONNECT_TIMEOUT.as_secs()
            ),
            ErrorKind::UnexpectedEof => format!(
                "SpyServer at {hp} closed the connection during the handshake \
                 (server full, or it refused this client)"
            ),
            ErrorKind::Other => e.to_string(),
            _ => format!("SpyServer at {hp}: {e}"),
        })
    };
    while dev.is_none() || sync.is_none() {
        if Instant::now() > deadline {
            return Err(fail(ErrorKind::TimedOut.into()));
        }
        let h = read_header(&mut s, &hp).map_err(fail)?;
        if body.len() < h.body_size {
            body.resize(h.body_size, 0);
        }
        s.read_exact(&mut body[..h.body_size]).map_err(fail)?;
        let b = &body[..h.body_size];
        match h.msg_type {
            MSG_DEVICE_INFO => dev = DevInfo::parse(b),
            MSG_CLIENT_SYNC => sync = ClientSync::parse(b),
            _ => {}
        }
    }
    let dev = dev.unwrap_or_default();
    if dev.device_type == 0 || dev.max_sample_rate == 0 {
        return Err(SdrError::Device(format!(
            "SpyServer at {hp} has no device attached"
        )));
    }
    Ok((s, dev, sync.unwrap_or_default()))
}

impl SpyServerDevice {
    pub fn open(args: &DeviceArgs) -> Result<Self> {
        net::check_keys(args, &["host", "port", "format"])?;
        let host = args.get("host").unwrap_or("127.0.0.1").to_string();
        let port = args.get_parsed::<u16>("port")?.unwrap_or(DEFAULT_PORT);
        let format = match args.get("format").unwrap_or("int16") {
            "uint8" | "u8" => Format::U8,
            "int16" | "i16" => Format::I16,
            "int24" | "i24" => Format::I24,
            "float" | "f32" => Format::F32,
            other => {
                return Err(SdrError::BadArgs(
                    args.to_string(),
                    format!("`format={other}`: expected int16, uint8, int24 or float"),
                ));
            }
        };
        let (conn, dev, sync) = handshake(&host, port)?;
        let hp = net::host_port(&host, port);
        let mut canonical = DeviceArgs::new("spyserver")
            .with("host", host.clone())
            .with("port", port.to_string());
        if let Some(f) = args.get("format") {
            canonical = canonical.with("format", f);
        }
        Ok(SpyServerDevice {
            info: DeviceInfo {
                driver: "spyserver".into(),
                args: canonical.to_string(),
                label: format!("{} ({:08X}) via SpyServer at {hp}", dev.name(), dev.serial),
                serial: Some(format!("{:08X}", dev.serial)),
                rx: true,
                tx: false,
            },
            host,
            port,
            format,
            dev,
            sync: Arc::new(Mutex::new(sync)),
            conn: Some(conn),
        })
    }

    /// Whether the server lets this client control the device (from the
    /// latest CLIENT_SYNC).
    pub fn can_control(&self) -> bool {
        lock(&self.sync).can_control
    }

    pub fn device_info(&self) -> DevInfo {
        self.dev
    }

    /// The latest CLIENT_SYNC (updated by a running stream).
    pub fn client_sync(&self) -> ClientSync {
        *lock(&self.sync)
    }
}

fn lock(m: &Mutex<ClientSync>) -> std::sync::MutexGuard<'_, ClientSync> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn capabilities(dev: &DevInfo) -> Capabilities {
    Capabilities {
        frequency: vec![Range::new(
            dev.min_frequency as f64,
            dev.max_frequency as f64,
        )],
        sample_rate: dev.rates().iter().map(|&(_, r)| Range::new(r, r)).collect(),
        gain: Some(Range {
            min: 0.0,
            max: dev.max_gain_index as f64,
            step: 1.0,
        }),
        agc: false,
        antennas: vec![],
        bandwidth: vec![],
    }
}

/// Checks a frequency against the device and, without control, against the
/// band the server allows this client.
fn check_frequency(caps: &Capabilities, sync: &ClientSync, hp: &str, freq: f64) -> Result<()> {
    caps.check_frequency(freq)?;
    if !sync.can_control
        && (freq < sync.min_iq_center_frequency as f64
            || freq > sync.max_iq_center_frequency as f64)
    {
        return Err(SdrError::Range(format!(
            "SpyServer at {hp} doesn't give this client control of the device (another \
             client has it), so the centre frequency must stay within {}–{} Hz; {freq} Hz isn't",
            sync.min_iq_center_frequency, sync.max_iq_center_frequency
        )));
    }
    Ok(())
}

fn gain_denied(hp: &str) -> SdrError {
    SdrError::Unsupported(format!(
        "setting the gain on SpyServer at {hp} (it denies this client control: can_control \
         is 0; use automatic gain to keep the server's setting)"
    ))
}

impl Device for SpyServerDevice {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn capabilities(&self) -> Capabilities {
        capabilities(&self.dev)
    }

    fn start_rx(&mut self, cfg: &RxConfig) -> Result<Box<dyn RxStream>> {
        if cfg.channel != 0 {
            return Err(SdrError::Range(format!(
                "SpyServer has one channel; channel {} doesn't exist",
                cfg.channel
            )));
        }
        let hp = net::host_port(&self.host, self.port);
        let caps = self.capabilities();
        caps.check_rate(cfg.sample_rate)?;
        let decimation = self
            .dev
            .rates()
            .iter()
            .find(|&&(_, r)| (r - cfg.sample_rate).abs() < 0.5)
            .map(|&(d, _)| d)
            .unwrap_or(self.dev.min_iq_decimation);
        let sock = match self.conn.take() {
            Some(s) => s,
            None => {
                let (s, dev, sync) = handshake(&self.host, self.port)?;
                self.dev = dev;
                *lock(&self.sync) = sync;
                s
            }
        };
        let sync = *lock(&self.sync);
        let checked = check_frequency(&caps, &sync, &hp, cfg.frequency).and_then(|()| {
            Ok(match cfg.gain {
                Gain::Auto => None,
                Gain::Db(_) if !sync.can_control => return Err(gain_denied(&hp)),
                Gain::Db(g) => Some(g.round().clamp(0.0, self.dev.max_gain_index as f64) as u32),
            })
        });
        let gain_index = match checked {
            Ok(g) => g,
            Err(e) => {
                // Keep the connection for the next attempt.
                self.conn = Some(sock);
                return Err(e);
            }
        };
        let format = Format::from_wire(self.dev.forced_iq_format).unwrap_or(self.format);
        let mut setup = vec![
            setting(SETTING_STREAMING_MODE, STREAM_MODE_IQ_ONLY),
            setting(SETTING_IQ_FORMAT, format.wire()),
            setting(SETTING_IQ_DECIMATION, decimation),
            setting(SETTING_IQ_FREQUENCY, cfg.frequency.round() as u32),
        ];
        if let Some(g) = gain_index {
            setup.push(setting(SETTING_GAIN, g));
        }
        let gain_now = gain_index.unwrap_or(sync.gain);
        setup.push(setting(
            SETTING_IQ_DIGITAL_GAIN,
            self.dev.digital_gain(gain_now, decimation),
        ));
        setup.push(setting(SETTING_STREAMING_ENABLED, 1));
        (&sock)
            .write_all(&setup.concat())
            .map_err(|e| SdrError::Device(format!("SpyServer at {hp}: sending settings: {e}")))?;
        let io_hp = hp.clone();
        let io_sync = self.sync.clone();
        let rx = NetRx::spawn("spyserver-rx", sock, cfg.sample_rate, move |s, p| {
            io_loop(s, p, &io_hp, &io_sync)
        })?;
        Ok(Box::new(SpyServerRx {
            rx,
            hp,
            caps,
            dev: self.dev,
            sync: self.sync.clone(),
            decimation,
        }))
    }
}

/// The I/O thread: read messages, convert I/Q, track CLIENT_SYNC.
fn io_loop(
    mut sock: TcpStream,
    producer: &mut Producer<Complex32>,
    hp: &str,
    sync: &Mutex<ClientSync>,
) -> std::result::Result<(), String> {
    const OUT: usize = 64 * 1024;
    let mut body = vec![0u8; MAX_BODY];
    let mut out = vec![Complex32::default(); OUT];
    let err = |e: std::io::Error| match e.kind() {
        ErrorKind::Other => e.to_string(),
        _ => net::read_error(WHAT, hp, &e),
    };
    loop {
        let h = read_header(&mut sock, hp).map_err(err)?;
        let b = &mut body[..h.body_size];
        sock.read_exact(b).map_err(err)?;
        let scale = 10f32.powf(-(h.flags as f32) / 20.0);
        match h.msg_type {
            MSG_CLIENT_SYNC => {
                if let Some(s) = ClientSync::parse(b) {
                    *lock(sync) = s;
                }
            }
            MSG_UINT8_IQ => convert(b, 2, scale, &mut out, producer, comp_u8),
            MSG_INT16_IQ => convert(b, 4, scale, &mut out, producer, comp_i16),
            MSG_INT24_IQ => convert(b, 6, scale, &mut out, producer, comp_i24),
            MSG_FLOAT_IQ => convert(b, 8, scale, &mut out, producer, comp_f32),
            _ => {}
        }
    }
}

fn comp_u8(x: &[u8]) -> f32 {
    (x[0] as f32 - 128.0) / 128.0
}

fn comp_i16(x: &[u8]) -> f32 {
    i16::from_le_bytes([x[0], x[1]]) as f32 / 32_768.0
}

fn comp_i24(x: &[u8]) -> f32 {
    (i32::from_le_bytes([0, x[0], x[1], x[2]]) >> 8) as f32 / 8_388_608.0
}

fn comp_f32(x: &[u8]) -> f32 {
    f32::from_le_bytes([x[0], x[1], x[2], x[3]])
}

/// Convert a message body of `sample_bytes`-sized I/Q pairs and push it in
/// chunks of `out.len()`. `one` converts one component (half a sample) to
/// ±1 full scale; `scale` undoes the server's digital gain.
fn convert(
    body: &[u8],
    sample_bytes: usize,
    scale: f32,
    out: &mut [Complex32],
    producer: &mut Producer<Complex32>,
    one: fn(&[u8]) -> f32,
) {
    let half = sample_bytes / 2;
    for chunk in body.chunks(sample_bytes * out.len()) {
        let mut n = 0;
        for (o, s) in out.iter_mut().zip(chunk.chunks_exact(sample_bytes)) {
            *o = Complex32::new(one(&s[..half]) * scale, one(&s[half..]) * scale);
            n += 1;
        }
        producer.push(&out[..n]);
    }
}

struct SpyServerRx {
    rx: NetRx,
    hp: String,
    caps: Capabilities,
    dev: DevInfo,
    sync: Arc<Mutex<ClientSync>>,
    decimation: u32,
}

impl SpyServerRx {
    fn send(&mut self, cmds: &[Vec<u8>]) -> Result<()> {
        self.rx.writer.write_all(&cmds.concat()).map_err(|e| {
            SdrError::Device(format!("SpyServer at {}: sending a command: {e}", self.hp))
        })
    }
}

impl RxStream for SpyServerRx {
    fn read(&mut self, out: &mut [Complex32]) -> Result<usize> {
        self.rx.read(out)
    }

    fn set_waker(&mut self, thread: Thread) {
        self.rx.set_waker(thread);
    }

    fn set_frequency(&mut self, freq: f64) -> Result<()> {
        let sync = *lock(&self.sync);
        check_frequency(&self.caps, &sync, &self.hp, freq)?;
        self.send(&[setting(SETTING_IQ_FREQUENCY, freq.round() as u32)])
    }

    fn set_gain(&mut self, gain: Gain) -> Result<()> {
        let Gain::Db(g) = gain else {
            return Ok(());
        };
        if !lock(&self.sync).can_control {
            return Err(gain_denied(&self.hp));
        }
        let g = g.round().clamp(0.0, self.dev.max_gain_index as f64) as u32;
        self.send(&[
            setting(SETTING_GAIN, g),
            setting(
                SETTING_IQ_DIGITAL_GAIN,
                self.dev.digital_gain(g, self.decimation),
            ),
        ])
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
    //! Against an in-process fake SpyServer.

    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;

    struct Fake {
        port: u16,
        hello: mpsc::Receiver<(u32, Vec<u8>)>,
        settings: mpsc::Receiver<(u32, u32)>,
        /// A raw server message to send; `None` closes the connection.
        msgs: mpsc::Sender<Option<Vec<u8>>>,
    }

    fn message(ty: u32, flags: u32, seq: u32, body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        for x in [
            PROTOCOL_VERSION,
            ty | (flags << 16),
            1,
            seq,
            body.len() as u32,
        ] {
            v.extend(x.to_le_bytes());
        }
        v.extend(body);
        v
    }

    fn le(v: &[u32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    fn sync_body(can_control: bool) -> Vec<u8> {
        le(&[
            can_control as u32,
            10,
            100_000_000,
            100_000_000,
            100_000_000,
            99_000_000,
            101_000_000,
            99_000_000,
            101_000_000,
        ])
    }

    fn fake(can_control: bool) -> Fake {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (htx, hello) = mpsc::channel();
        let (stx, settings) = mpsc::channel();
        let (msgs, mrx) = mpsc::channel::<Option<Vec<u8>>>();
        std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut r = s.try_clone().unwrap();
            let mut h = [0u8; 8];
            r.read_exact(&mut h).unwrap();
            let [cmd, size] = u32s::<2>(&h);
            let mut body = vec![0u8; size as usize];
            r.read_exact(&mut body).unwrap();
            htx.send((cmd, body)).unwrap();
            // Airspy R2-like: 10 MS/s, decimation 0..=9, gain index 0..=21.
            let info = le(&[
                1,
                0x1234_abcd,
                10_000_000,
                9_000_000,
                9,
                1,
                21,
                24_000_000,
                1_800_000_000,
                12,
                0,
                0,
            ]);
            s.write_all(&message(MSG_DEVICE_INFO, 0, 0, &info)).unwrap();
            s.write_all(&message(MSG_CLIENT_SYNC, 0, 1, &sync_body(can_control)))
                .unwrap();
            std::thread::spawn(move || {
                let mut h = [0u8; 8];
                while r.read_exact(&mut h).is_ok() {
                    let [cmd, size] = u32s::<2>(&h);
                    let mut body = vec![0u8; size as usize];
                    if r.read_exact(&mut body).is_err() {
                        break;
                    }
                    if cmd == CMD_SET_SETTING {
                        let [k, v] = u32s::<2>(&body);
                        if stx.send((k, v)).is_err() {
                            break;
                        }
                    }
                }
            });
            while let Ok(Some(m)) = mrx.recv() {
                if s.write_all(&m).is_err() {
                    break;
                }
            }
            let _ = s.shutdown(std::net::Shutdown::Both);
        });
        Fake {
            port,
            hello,
            settings,
            msgs,
        }
    }

    fn open(f: &Fake, extra: &str) -> SpyServerDevice {
        SpyServerDevice::open(
            &DeviceArgs::parse(&format!("spyserver:host=127.0.0.1,port={}{extra}", f.port))
                .unwrap(),
        )
        .unwrap()
    }

    fn cfg(rate: f64, gain: Gain) -> RxConfig {
        RxConfig {
            sample_rate: rate,
            frequency: 100_500_000.0,
            gain,
            ppm: 0.0,
            bandwidth: None,
            antenna: None,
            channel: 0,
        }
    }

    fn settings(f: &Fake, n: usize) -> Vec<(u32, u32)> {
        (0..n)
            .map(|_| f.settings.recv_timeout(Duration::from_secs(5)).unwrap())
            .collect()
    }

    fn read_n(rx: &mut dyn RxStream, n: usize) -> Vec<Complex32> {
        let mut got = Vec::new();
        let mut buf = vec![Complex32::default(); 1000];
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

    fn read_err(rx: &mut dyn RxStream) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match rx.read(&mut [Complex32::default(); 16]) {
                Ok(0) => {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(n) => panic!("unexpected {n} samples"),
                Err(e) => break e.to_string(),
            }
        }
    }

    #[test]
    fn handshakes_configures_streams_and_retunes() {
        let f = fake(true);
        let mut dev = open(&f, "");
        let (cmd, body) = f.hello.recv().unwrap();
        assert_eq!(cmd, CMD_HELLO);
        assert_eq!(&body[..4], &0x0200_06a4u32.to_le_bytes());
        assert_eq!(&body[4..], b"foxygnal");
        assert!(dev.can_control());
        assert_eq!(
            dev.info().label,
            format!("Airspy (1234ABCD) via SpyServer at 127.0.0.1:{}", f.port)
        );
        let caps = dev.capabilities();
        assert_eq!(caps.sample_rate.len(), 10);
        assert_eq!(caps.sample_rate[0], Range::new(10e6, 10e6));
        assert_eq!(caps.sample_rate[9], Range::new(19_531.25, 19_531.25));
        assert_eq!(caps.gain.unwrap().max, 21.0);
        assert!(dev.start_rx(&cfg(3e6, Gain::Auto)).is_err());

        let mut rx = dev.start_rx(&cfg(625_000.0, Gain::Db(15.0))).unwrap();
        // 625 kS/s = 10 MS/s / 2^4; digital gain = (21 - 15) + int(4 × 3.01).
        assert_eq!(
            settings(&f, 7),
            vec![
                (SETTING_STREAMING_MODE, STREAM_MODE_IQ_ONLY),
                (SETTING_IQ_FORMAT, FORMAT_INT16),
                (SETTING_IQ_DECIMATION, 4),
                (SETTING_IQ_FREQUENCY, 100_500_000),
                (SETTING_GAIN, 15),
                (SETTING_IQ_DIGITAL_GAIN, 18),
                (SETTING_STREAMING_ENABLED, 1),
            ]
        );

        // int16 samples across several messages, the second one with 6 dB of
        // digital gain applied by the server.
        let vals: Vec<i16> = (0..3000).map(|i| (i * 7 - 10_000) as i16).collect();
        let bytes: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()).collect();
        f.msgs
            .send(Some(message(MSG_INT16_IQ, 0, 2, &bytes[..2000])))
            .unwrap();
        f.msgs
            .send(Some(message(MSG_CLIENT_SYNC, 0, 3, &sync_body(true))))
            .unwrap();
        f.msgs
            .send(Some(message(MSG_INT16_IQ, 6, 4, &bytes[2000..])))
            .unwrap();
        let got = read_n(&mut *rx, 1500);
        assert_eq!(got.len(), 1500);
        let g6 = 10f32.powf(-6.0 / 20.0);
        for (k, s) in got.iter().enumerate() {
            let scale = if k < 500 { 1.0 } else { g6 };
            let want = Complex32::new(
                vals[2 * k] as f32 / 32768.0 * scale,
                vals[2 * k + 1] as f32 / 32768.0 * scale,
            );
            assert!((s - want).norm() < 1e-6, "sample {k}: {s} vs {want}");
        }

        rx.set_frequency(433_920_000.0).unwrap();
        assert_eq!(settings(&f, 1), vec![(SETTING_IQ_FREQUENCY, 433_920_000)]);
        rx.set_gain(Gain::Db(30.0)).unwrap();
        assert_eq!(
            settings(&f, 2),
            vec![(SETTING_GAIN, 21), (SETTING_IQ_DIGITAL_GAIN, 12)]
        );
        assert!(rx.set_frequency(5e9).is_err());

        f.msgs.send(None).unwrap();
        assert_eq!(
            read_err(&mut *rx),
            format!(
                "device error: SpyServer at 127.0.0.1:{} closed the connection",
                f.port
            )
        );
        assert!(rx.finished());
    }

    #[test]
    fn uint8_format_converts() {
        let f = fake(true);
        let mut dev = open(&f, ",format=uint8");
        let mut rx = dev.start_rx(&cfg(10e6, Gain::Auto)).unwrap();
        let s = settings(&f, 5);
        assert_eq!(s[1], (SETTING_IQ_FORMAT, FORMAT_UINT8));
        assert_eq!(s[2], (SETTING_IQ_DECIMATION, 0));
        // No gain setting under Auto.
        assert!(s.iter().all(|&(k, _)| k != SETTING_GAIN));
        f.msgs
            .send(Some(message(MSG_UINT8_IQ, 0, 2, &[0, 128, 255, 64])))
            .unwrap();
        assert_eq!(
            read_n(&mut *rx, 2),
            vec![
                Complex32::new(-1.0, 0.0),
                Complex32::new(127.0 / 128.0, -0.5)
            ]
        );
    }

    #[test]
    fn without_control_gain_is_denied_and_tuning_limited() {
        let f = fake(false);
        let mut dev = open(&f, "");
        assert!(!dev.can_control());
        let err = dev
            .start_rx(&cfg(625_000.0, Gain::Db(10.0)))
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("can_control is 0"), "{err}");
        let mut c = cfg(625_000.0, Gain::Auto);
        c.frequency = 120e6;
        let err = dev.start_rx(&c).err().unwrap().to_string();
        assert!(err.contains("99000000–101000000 Hz"), "{err}");
        let mut rx = dev.start_rx(&cfg(625_000.0, Gain::Auto)).unwrap();
        let err = rx.set_gain(Gain::Db(3.0)).err().unwrap().to_string();
        assert!(err.contains("can_control"), "{err}");
        rx.set_frequency(100_900_000.0).unwrap();
    }

    #[test]
    fn closed_port_fails_fast_and_names_it() {
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let t = Instant::now();
        let err = SpyServerDriver
            .open(&DeviceArgs::parse(&format!("spyserver:port={port}")).unwrap())
            .err()
            .unwrap()
            .to_string();
        assert!(
            t.elapsed() < Duration::from_secs(1),
            "took {:?}",
            t.elapsed()
        );
        assert!(
            err.contains(&format!("SpyServer at 127.0.0.1:{port}")),
            "{err}"
        );
    }

    #[test]
    fn int24_and_float_decode() {
        let mut out = vec![Complex32::default(); 2];
        let (mut p, mut c) = crate::ring::ring::<Complex32>(16);
        // -1 and +0.5 in int24; chunked through a 2-sample buffer.
        let mut b = Vec::new();
        for _ in 0..3 {
            b.extend([0x00, 0x00, 0x80, 0x00, 0x00, 0x40]);
        }
        convert(&b, 6, 1.0, &mut out, &mut p, comp_i24);
        let mut got = [Complex32::default(); 4];
        assert_eq!(c.pop(&mut got), 3);
        assert!(got[..3].iter().all(|s| *s == Complex32::new(-1.0, 0.5)));
        let b: Vec<u8> = [0.25f32, -0.75]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        convert(&b, 8, 0.5, &mut out, &mut p, comp_f32);
        assert_eq!(c.pop(&mut got), 1);
        assert_eq!(got[0], Complex32::new(0.125, -0.375));
    }
}
