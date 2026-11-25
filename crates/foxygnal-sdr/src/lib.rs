//! SDR hardware for foxygnal.
//!
//! A [`Driver`] finds devices ([`Driver::discover`]) and opens them from a
//! device string ([`DeviceArgs`], e.g. `rtlsdr:serial=00000001` or
//! `rtltcp:host=10.0.0.5,port=1234`). An open [`Device`] starts streams.
//!
//! Streams never block the graph: drivers move samples on their own thread
//! into a [`ring`](crate::ring) and wake the reading block's thread. A block's
//! `work()` only calls [`RxStream::read`], which returns what is there now.
//! Control calls (retune, gain) may take a few milliseconds (a USB control
//! transfer or a network round trip); blocks make them from `set_param`.
//!
//! Drivers that need a C library load it at run time (`dlopen`), so building
//! foxygnal never needs one, and a missing library only means that driver
//! finds no devices.

pub mod args;
pub mod arm;
pub mod native;
mod net;
pub mod ring;
pub mod rtltcp;
pub mod sim;
pub mod spyserver;

use std::sync::{Arc, OnceLock};
use std::thread::Thread;

pub use args::DeviceArgs;
pub use arm::{TxArm, arm_tx, tx_armed};
pub use num_complex::Complex32;

#[derive(Debug, thiserror::Error)]
pub enum SdrError {
    /// No driver with this name is registered.
    #[error("unknown SDR driver `{0}` (known: {1})")]
    UnknownDriver(String, String),
    #[error("no SDR device found{0}")]
    NotFound(String),
    #[error("invalid device string `{0}`: {1}")]
    BadArgs(String, String),
    /// The driver's library isn't installed or couldn't be loaded.
    #[error("{0}")]
    Unavailable(String),
    #[error("{0} is not supported by this device")]
    Unsupported(String),
    /// A setting is out of the device's range. The message says what is allowed.
    #[error("{0}")]
    Range(String),
    #[error("device error: {0}")]
    Device(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, SdrError>;

/// A device found by discovery.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DeviceInfo {
    /// Driver name (`rtlsdr`, `soapy`, `rtltcp`, `sim`, …).
    pub driver: String,
    /// The device string that opens exactly this device.
    pub args: String,
    /// Human-readable name, e.g. "Generic RTL2832U OEM (00000001)".
    pub label: String,
    pub serial: Option<String>,
    pub rx: bool,
    pub tx: bool,
}

/// An inclusive range; `step` 0 means continuous.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct Range {
    pub min: f64,
    pub max: f64,
    pub step: f64,
}

impl Range {
    pub const fn new(min: f64, max: f64) -> Self {
        Range {
            min,
            max,
            step: 0.0,
        }
    }

    pub fn contains(&self, v: f64) -> bool {
        v >= self.min && v <= self.max
    }
}

/// What a device can do, for validation and for the UI.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Capabilities {
    /// Tunable centre frequencies.
    pub frequency: Vec<Range>,
    /// Supported sample rates (a list of points is ranges with min == max).
    pub sample_rate: Vec<Range>,
    /// Overall gain in dB.
    pub gain: Option<Range>,
    /// Hardware automatic gain control.
    pub agc: bool,
    pub antennas: Vec<String>,
    /// Analog bandwidth filter settings, if adjustable.
    pub bandwidth: Vec<Range>,
}

impl Capabilities {
    /// Checks `rate` against [`Self::sample_rate`]; the error lists what's allowed.
    pub fn check_rate(&self, rate: f64) -> Result<()> {
        if self.sample_rate.is_empty() || self.sample_rate.iter().any(|r| r.contains(rate)) {
            return Ok(());
        }
        Err(SdrError::Range(format!(
            "sample rate {rate} S/s is not supported; supported: {}",
            fmt_ranges(&self.sample_rate)
        )))
    }

    pub fn check_frequency(&self, freq: f64) -> Result<()> {
        if self.frequency.is_empty() || self.frequency.iter().any(|r| r.contains(freq)) {
            return Ok(());
        }
        Err(SdrError::Range(format!(
            "frequency {freq} Hz is out of range; tunable: {}",
            fmt_ranges(&self.frequency)
        )))
    }
}

fn fmt_ranges(rs: &[Range]) -> String {
    rs.iter()
        .map(|r| {
            if r.min == r.max {
                format!("{}", r.min)
            } else {
                format!("{}–{}", r.min, r.max)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Gain setting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Gain {
    /// Hardware AGC (or the device's default if it has none).
    Auto,
    /// Overall gain in dB; drivers pick the nearest supported value.
    Db(f64),
}

/// Settings for a receive stream. For a two-channel stream
/// ([`Device::start_rx_dual`]) every setting applies to both channels and
/// `channel` is ignored.
#[derive(Debug, Clone, PartialEq)]
pub struct RxConfig {
    pub sample_rate: f64,
    pub frequency: f64,
    pub gain: Gain,
    /// Frequency correction in parts per million.
    pub ppm: f64,
    /// Analog bandwidth in Hz; `None` lets the driver choose.
    pub bandwidth: Option<f64>,
    pub antenna: Option<String>,
    /// Receive channel, 0 to [`Device::rx_channels`] − 1.
    pub channel: usize,
}

/// Settings for a transmit stream.
#[derive(Debug, Clone, PartialEq)]
pub struct TxConfig {
    pub sample_rate: f64,
    pub frequency: f64,
    /// Transmit gain / power in dB (driver-specific meaning, documented per driver).
    pub gain: f64,
    pub bandwidth: Option<f64>,
    pub antenna: Option<String>,
    pub channel: usize,
}

/// A device driver: discovery and opening.
pub trait Driver: Send + Sync {
    /// The name used in device strings (`rtlsdr` in `rtlsdr:serial=…`).
    fn name(&self) -> &'static str;

    /// Shown in the UI, e.g. "RTL-SDR (librtlsdr)".
    fn label(&self) -> &'static str;

    /// List attached devices. Network drivers that can't discover return an
    /// empty list. Must not take long (a second at most).
    fn discover(&self) -> Result<Vec<DeviceInfo>>;

    /// Open a device. `args` comes from a device string with this driver's name.
    fn open(&self, args: &DeviceArgs) -> Result<Box<dyn Device>>;
}

/// An open device.
pub trait Device: Send {
    fn info(&self) -> &DeviceInfo;

    fn capabilities(&self) -> Capabilities;

    /// Start receiving. The device stays open while the stream lives.
    fn start_rx(&mut self, cfg: &RxConfig) -> Result<Box<dyn RxStream>>;

    /// How many receive channels [`RxConfig::channel`] can pick from.
    fn rx_channels(&self) -> usize {
        1
    }

    /// Start receiving channels 0 and 1 together, sample-aligned (one
    /// local oscillator and one sample clock, as on an AD9361). Only the
    /// device's own stream can promise that; two `start_rx` calls can't.
    fn start_rx_dual(&mut self, cfg: &RxConfig) -> Result<Box<dyn DualRxStream>> {
        let _ = cfg;
        Err(SdrError::Unsupported(
            "receiving two channels in sync".into(),
        ))
    }

    /// Start transmitting. Only called after the user armed TX.
    fn start_tx(&mut self, cfg: &TxConfig) -> Result<Box<dyn TxStream>> {
        let _ = cfg;
        Err(SdrError::Unsupported("transmitting".into()))
    }
}

/// A running receive stream. Dropping it stops the device.
pub trait RxStream: Send {
    /// Copy available samples into `out`; never blocks. 0 means nothing yet.
    fn read(&mut self, out: &mut [Complex32]) -> Result<usize>;

    /// The thread to unpark when new samples arrive (the block's thread).
    fn set_waker(&mut self, thread: Thread);

    fn set_frequency(&mut self, freq: f64) -> Result<()>;

    fn set_gain(&mut self, gain: Gain) -> Result<()>;

    fn set_ppm(&mut self, ppm: f64) -> Result<()> {
        let _ = ppm;
        Err(SdrError::Unsupported("frequency correction".into()))
    }

    /// Samples dropped because the reader fell behind (monotonic).
    fn overflows(&self) -> u64;

    /// The stream ended for good (device unplugged, connection closed).
    /// `read` returns the error that ended it once the ring is drained.
    fn finished(&self) -> bool {
        false
    }
}

/// A running two-channel receive stream ([`Device::start_rx_dual`]).
/// Dropping it stops the device. Settings apply to both channels.
pub trait DualRxStream: Send {
    /// Copy available samples of channel 0 into `a` and of channel 1 into
    /// `b`; `a[i]` and `b[i]` were sampled at the same instant. Copies the
    /// same count into both (at most the shorter slice) and returns it;
    /// never blocks. Drops always lose both channels together, so they stay
    /// aligned.
    fn read(&mut self, a: &mut [Complex32], b: &mut [Complex32]) -> Result<usize>;

    /// The thread to unpark when new samples arrive (the block's thread).
    fn set_waker(&mut self, thread: Thread);

    fn set_frequency(&mut self, freq: f64) -> Result<()>;

    fn set_gain(&mut self, gain: Gain) -> Result<()>;

    fn set_ppm(&mut self, ppm: f64) -> Result<()> {
        let _ = ppm;
        Err(SdrError::Unsupported("frequency correction".into()))
    }

    /// Sample instants dropped because the reader fell behind (monotonic).
    fn overflows(&self) -> u64;

    /// The stream ended for good; see [`RxStream::finished`].
    fn finished(&self) -> bool {
        false
    }
}

/// A running transmit stream. Dropping it stops transmitting.
pub trait TxStream: Send {
    /// Queue samples; never blocks. Returns how many were taken (0 = full).
    fn write(&mut self, samples: &[Complex32]) -> Result<usize>;

    /// The thread to unpark when space frees up (the block's thread).
    fn set_waker(&mut self, thread: Thread);

    fn set_frequency(&mut self, freq: f64) -> Result<()>;

    fn set_gain(&mut self, gain: f64) -> Result<()>;

    /// Samples the device wanted but didn't get (monotonic).
    fn underflows(&self) -> u64;
}

/// The set of known drivers.
#[derive(Clone, Default)]
pub struct Drivers {
    drivers: Vec<Arc<dyn Driver>>,
}

impl Drivers {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, d: impl Driver + 'static) {
        self.drivers.push(Arc::new(d));
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn Driver>> {
        self.drivers.iter()
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Driver>> {
        self.drivers.iter().find(|d| d.name() == name)
    }

    /// Discover devices on every driver. A driver that fails (library
    /// missing, USB permission error) reports why instead of failing all.
    pub fn discover(&self) -> Vec<Discovery> {
        self.drivers
            .iter()
            .map(|d| match d.discover() {
                Ok(devices) => Discovery {
                    driver: d.name(),
                    label: d.label(),
                    devices,
                    error: None,
                },
                Err(e) => Discovery {
                    driver: d.name(),
                    label: d.label(),
                    devices: vec![],
                    error: Some(e.to_string()),
                },
            })
            .collect()
    }

    /// Open from a device string. An empty string (or just a driver name with
    /// no keys) opens the first device discovery finds.
    pub fn open(&self, device: &str) -> Result<Box<dyn Device>> {
        let args = DeviceArgs::parse(device)?;
        let Some(name) = args.driver() else {
            // First device of any driver, in registration order. Simulated
            // devices only when nothing real is attached.
            let found = self.discover();
            let first = found
                .iter()
                .flat_map(|d| &d.devices)
                .find(|i| i.driver != "sim" && i.rx)
                .or_else(|| found.iter().flat_map(|d| &d.devices).find(|i| i.rx));
            return match first {
                Some(info) => self.open(&info.args),
                None => Err(SdrError::NotFound(String::new())),
            };
        };
        let d = self.get(name).ok_or_else(|| {
            let known: Vec<_> = self.drivers.iter().map(|d| d.name()).collect();
            SdrError::UnknownDriver(name.to_string(), known.join(", "))
        })?;
        d.open(&args)
    }
}

/// One driver's discovery result.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Discovery {
    pub driver: &'static str,
    pub label: &'static str,
    pub devices: Vec<DeviceInfo>,
    /// Why discovery failed (e.g. library not installed).
    pub error: Option<String>,
}

/// Every driver built into foxygnal, in discovery order (hardware first,
/// then network sources, then the simulator).
pub fn default_drivers() -> Drivers {
    #[allow(unused_mut)]
    let mut d = Drivers::new();
    // Hardware.
    d.add(native::RtlSdrDriver::new());
    d.add(native::HackrfDriver::new());
    d.add(native::PlutoDriver::new());
    d.add(native::SoapyDriver::new());
    // Network sources.
    d.add(rtltcp::RtlTcpDriver);
    d.add(spyserver::SpyServerDriver);
    // Always last: real devices are found first.
    d.add(sim::SimDriver);
    d
}

/// The process-wide driver set ([`default_drivers`], built on first use).
/// Blocks open devices through it and the server discovers with it, so
/// drivers that load a library do so once.
pub fn drivers() -> &'static Drivers {
    static DRIVERS: OnceLock<Drivers> = OnceLock::new();
    DRIVERS.get_or_init(default_drivers)
}
