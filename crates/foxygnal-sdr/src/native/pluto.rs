//! ADALM-Pluto and other AD936x boards (PlutoSky, Pluto+, LibreSDR, …)
//! through libiio 0.x (loaded at run time).
//!
//! Device strings: `pluto:` (the first Pluto on USB), `pluto:uri=ip:10.0.0.5`,
//! `pluto:uri=usb:1.4.5`. Any libiio URI works. Only USB boards are found by
//! discovery; network ones are opened by their URI.
//!
//! **Two receive channels.** An AD9361 (or an AD9363 Pluto Rev C/D) has two
//! receivers sharing one local oscillator and one sample clock. The board
//! only exposes the second when its firmware runs in 2r2t mode
//! (`fw_setenv mode 2r2t`, then reboot). [`Device::rx_channels`] says how
//! many there are; `RxConfig::channel` 1 streams RX2 alone, and
//! [`Device::start_rx_dual`] streams both, sample-aligned. Only the enabled
//! channels cross the link, so one channel costs the same in either mode.
//! The board has one receive DMA, so one stream at a time.
//!
//! **Sample rates.** The AD936x itself runs at 2.083–61.44 MS/s. Below that
//! the FPGA's ×8 decimator (in the stock Pluto HDL) takes over: the chip runs
//! at 8× the rate and the FPGA filters and decimates, down to 260.4 kS/s.
//!
//! **Gain** is the chip's `hardwaregain` per channel (−3 to 71 dB on an
//! AD9361 below 1.3 GHz; the board reports its range). [`Gain::Auto`] is the
//! chip's `slow_attack` AGC. The analog filter follows the sample rate
//! unless a bandwidth is given. Frequency correction moves `xo_correction`,
//! the reference clock the chip assumes, from the value it booted with.
//!
//! Samples are 12 bits in 16 (`le:S12/16`), scaled by 1/2048. Receive only:
//! transmit through `soapy:driver=plutosdr` for now.

use std::ffi::{c_char, c_int, c_longlong, c_uint, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{JoinHandle, Thread};

use super::{RawPtr, RxRing, c_string, cached, cstr, dl_api, open_library};
use crate::ring::Producer;
use crate::{
    Capabilities, Complex32, Device, DeviceArgs, DeviceInfo, Driver, DualRxStream, Gain, Range,
    Result, RxConfig, RxStream, SdrError,
};

const SONAMES: &[&str] = &[
    "libiio.so.0",
    "libiio.so",
    "libiio.0.dylib",
    "libiio.dylib",
    "/Library/Frameworks/iio.framework/iio",
    "libiio.dll",
];
const MISSING: &str = "libiio not found (install libiio 0.x)";

/// Opaque libiio types.
#[repr(C)]
pub(crate) struct Ctx {
    _p: [u8; 0],
}
#[repr(C)]
pub(crate) struct Dev {
    _p: [u8; 0],
}
#[repr(C)]
pub(crate) struct Chn {
    _p: [u8; 0],
}
#[repr(C)]
pub(crate) struct Buf {
    _p: [u8; 0],
}
#[repr(C)]
pub(crate) struct ScanCtx {
    _p: [u8; 0],
}
#[repr(C)]
pub(crate) struct Info {
    _p: [u8; 0],
}

dl_api! {
    /// The parts of libiio 0.x's `iio.h` we use (1.x renamed most of them).
    pub(crate) struct Api {
        fn iio_create_context_from_uri(*const c_char) -> *mut Ctx;
        fn iio_context_destroy(*mut Ctx);
        fn iio_context_find_device(*const Ctx, *const c_char) -> *mut Dev;
        fn iio_context_get_attr_value(*const Ctx, *const c_char) -> *const c_char;
        fn iio_device_find_channel(*const Dev, *const c_char, bool) -> *mut Chn;
        fn iio_device_attr_read_longlong(*const Dev, *const c_char, *mut c_longlong) -> c_int;
        fn iio_device_attr_write_longlong(*const Dev, *const c_char, c_longlong) -> c_int;
        fn iio_channel_attr_read(*const Chn, *const c_char, *mut c_char, usize) -> isize;
        fn iio_channel_attr_read_longlong(*const Chn, *const c_char, *mut c_longlong) -> c_int;
        fn iio_channel_attr_write(*const Chn, *const c_char, *const c_char) -> isize;
        fn iio_channel_attr_write_longlong(*const Chn, *const c_char, c_longlong) -> c_int;
        fn iio_channel_enable(*mut Chn);
        fn iio_channel_disable(*mut Chn);
        fn iio_device_create_buffer(*const Dev, usize, bool) -> *mut Buf;
        fn iio_buffer_destroy(*mut Buf);
        fn iio_buffer_refill(*mut Buf) -> isize;
        fn iio_buffer_start(*const Buf) -> *mut c_void;
        fn iio_buffer_end(*const Buf) -> *mut c_void;
        fn iio_buffer_cancel(*mut Buf);
        fn iio_strerror(c_int, *mut c_char, usize);
        fn iio_create_scan_context(*const c_char, c_uint) -> *mut ScanCtx;
        fn iio_scan_context_destroy(*mut ScanCtx);
        fn iio_scan_context_get_info_list(*mut ScanCtx, *mut *mut *mut Info) -> isize;
        fn iio_context_info_list_free(*mut *mut Info);
        fn iio_context_info_get_description(*const Info) -> *const c_char;
        fn iio_context_info_get_uri(*const Info) -> *const c_char;
    }
}

static DEFAULT_API: OnceLock<&'static Api> = OnceLock::new();

/// Device names of the AD936x driver and the receive DMA in the ADI HDL.
const PHY: &str = "ad9361-phy";
const RX_DEV: &str = "cf-ad9361-lpc";
/// USB vendor:product of the ADALM-Pluto (clones keep it).
const PLUTO_USB_ID: &str = "0456:b673";
/// Full scale of the 12-bit samples.
const SCALE: f32 = 1.0 / 2048.0;
/// `EBUSY`: the receive DMA is in use.
const EBUSY: c_int = 16;

pub struct PlutoDriver {
    sonames: Option<Vec<String>>,
    own: OnceLock<&'static Api>,
}

impl Default for PlutoDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl PlutoDriver {
    pub fn new() -> Self {
        PlutoDriver {
            sonames: None,
            own: OnceLock::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_sonames(names: &[&str]) -> Self {
        PlutoDriver {
            sonames: Some(names.iter().map(|s| s.to_string()).collect()),
            own: OnceLock::new(),
        }
    }

    fn api(&self) -> Result<&'static Api> {
        let load = |names: &[String]| {
            open_library(names, MISSING).and_then(|l| Api::resolve(l, "libiio (0.x needed)"))
        };
        match &self.sonames {
            None => cached(&DEFAULT_API, || {
                load(&SONAMES.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            }),
            Some(n) => cached(&self.own, || load(n)),
        }
    }
}

/// libiio's message for a (positive) errno.
fn strerror(api: &Api, err: c_int) -> String {
    let mut buf = [0 as c_char; 256];
    // SAFETY: writes a NUL-terminated string of at most `len` bytes.
    unsafe {
        (api.iio_strerror)(err, buf.as_mut_ptr(), buf.len());
        cstr(buf.as_ptr())
    }
}

/// The errno a failed `iio_create_*` call left behind.
fn last_errno() -> c_int {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// A USB context found by scanning.
struct Found {
    uri: String,
    description: String,
}

/// Lists Plutos on USB. An empty list if libiio was built without USB.
fn scan_usb(api: &Api) -> Vec<Found> {
    let mut found = Vec::new();
    // SAFETY: the scan context and list are freed below; strings are copied
    // before the list is.
    unsafe {
        let scan = (api.iio_create_scan_context)(c"usb".as_ptr(), 0);
        if scan.is_null() {
            return found;
        }
        let mut list: *mut *mut Info = std::ptr::null_mut();
        let n = (api.iio_scan_context_get_info_list)(scan, &mut list);
        if n > 0 && !list.is_null() {
            for i in 0..n as usize {
                let info = *list.add(i);
                let description = cstr((api.iio_context_info_get_description)(info));
                let uri = cstr((api.iio_context_info_get_uri)(info));
                if description.contains(PLUTO_USB_ID)
                    || description.to_ascii_lowercase().contains("pluto")
                {
                    found.push(Found { uri, description });
                }
            }
        }
        if !list.is_null() {
            (api.iio_context_info_list_free)(list);
        }
        (api.iio_scan_context_destroy)(scan);
    }
    found
}

/// "0456:b673 (Analog Devices Inc. PlutoSDR (ADALM-PLUTO)), serial=1044…"
/// → ("PlutoSDR (ADALM-PLUTO)", Some("1044…")).
fn describe(description: &str) -> (String, Option<String>) {
    let serial = description
        .split_once("serial=")
        .map(|(_, s)| s.trim().trim_end_matches(')').to_string())
        .filter(|s| !s.is_empty());
    let name = description
        .split_once('(')
        .map(|(_, rest)| {
            let rest = rest.split("),").next().unwrap_or(rest);
            rest.trim_start_matches("Analog Devices Inc.").trim()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or("PlutoSDR");
    (name.to_string(), serial)
}

fn info_for(f: &Found) -> DeviceInfo {
    let (name, serial) = describe(&f.description);
    DeviceInfo {
        driver: "pluto".into(),
        args: DeviceArgs::new("pluto").with("uri", &f.uri).to_string(),
        label: name,
        serial,
        rx: true,
        tx: false,
    }
}

impl Driver for PlutoDriver {
    fn name(&self) -> &'static str {
        "pluto"
    }

    fn label(&self) -> &'static str {
        "PlutoSDR / AD936x (libiio)"
    }

    fn discover(&self) -> Result<Vec<DeviceInfo>> {
        let api = self.api()?;
        Ok(scan_usb(api).iter().map(info_for).collect())
    }

    fn open(&self, args: &DeviceArgs) -> Result<Box<dyn Device>> {
        let api = self.api()?;
        let uri = match args.get("uri").filter(|u| !u.is_empty()) {
            Some(u) => u.to_string(),
            None => scan_usb(api)
                .into_iter()
                .next()
                .map(|f| f.uri)
                .ok_or_else(|| {
                    SdrError::NotFound(
                        ": no Pluto on USB (for one on the network use `pluto:uri=ip:ADDRESS`)"
                            .into(),
                    )
                })?,
        };
        Handle::open(api, &uri).map(|(handle, info, caps, rx_channels)| {
            Box::new(PlutoDevice {
                info,
                caps,
                rx_channels,
                handle: Arc::new(handle),
            }) as Box<dyn Device>
        })
    }
}

// ---- Device handle ------------------------------------------------------------

/// An open libiio context. Control calls take `ctl`; the receive thread
/// only refills its buffer, which libiio allows alongside them (the network
/// backend opens a socket per buffer, the USB one a pipe).
struct Handle {
    api: &'static Api,
    ctx: RawPtr<Ctx>,
    phy: RawPtr<Dev>,
    rx: RawPtr<Dev>,
    uri: String,
    ctl: Mutex<()>,
    /// A receive stream is running (the board has one receive DMA).
    streaming: AtomicBool,
    /// `xo_correction` as booted, in Hz; 0 if the board has none.
    xo: f64,
    /// Lowest rate the AD936x runs at by itself.
    chip_min_rate: f64,
    /// FPGA decimation factor (8 on a stock Pluto), or 1 without one.
    decimation: u32,
}

// SAFETY: libiio handles are used from the owning stream's threads only as
// described on `Handle`; `ctl` serialises control calls.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: every stream holds an Arc to us, so none is running.
        unsafe { (self.api.iio_context_destroy)(self.ctx.0) };
    }
}

/// "[2083333 1 61440000]" → Range; "A B" style lists aren't ranges.
fn parse_range(s: &str) -> Option<Range> {
    let inner = s.trim().strip_prefix('[')?.strip_suffix(']')?;
    let v: Vec<f64> = inner
        .split_whitespace()
        .map(|x| x.parse().ok())
        .collect::<Option<_>>()?;
    match v[..] {
        [min, step, max] => Some(Range { min, max, step }),
        [min, max] => Some(Range::new(min, max)),
        _ => None,
    }
}

/// The AD936x rate and whether the FPGA decimates, for a wanted `rate`.
fn plan_rate(rate: f64, chip_min: f64, decimation: u32) -> (f64, bool) {
    if rate < chip_min && decimation > 1 {
        (rate * decimation as f64, true)
    } else {
        (rate, false)
    }
}

/// Samples per buffer refill: about 10 ms, a power of two from 16 Ki to 256 Ki.
fn buffer_samples(rate: f64) -> usize {
    ((rate / 100.0) as usize)
        .next_power_of_two()
        .clamp(1 << 14, 1 << 18)
}

impl Handle {
    /// Opens `uri` and reads what the board can do.
    fn open(api: &'static Api, uri: &str) -> Result<(Self, DeviceInfo, Capabilities, usize)> {
        let c_uri = c_string(uri)?;
        // SAFETY: valid C string; NULL on failure with errno set.
        let ctx = unsafe { (api.iio_create_context_from_uri)(c_uri.as_ptr()) };
        if ctx.is_null() {
            return Err(SdrError::Device(format!(
                "could not reach `{uri}`: {}",
                strerror(api, last_errno())
            )));
        }
        let find = |name: &std::ffi::CStr| {
            // SAFETY: open context, C string.
            unsafe { (api.iio_context_find_device)(ctx, name.as_ptr()) }
        };
        let (phy, rx) = (find(c"ad9361-phy"), find(c"cf-ad9361-lpc"));
        if phy.is_null() || rx.is_null() {
            // SAFETY: nothing else uses the context.
            unsafe { (api.iio_context_destroy)(ctx) };
            return Err(SdrError::Unsupported(format!(
                "`{uri}` is not an AD936x radio (no `{PHY}` / `{RX_DEV}`); \
                 receiving from it"
            )));
        }
        let ctx_attr = |name: &std::ffi::CStr| {
            // SAFETY: open context; the value lives as long as it.
            unsafe { cstr((api.iio_context_get_attr_value)(ctx, name.as_ptr())) }
        };
        let model = ctx_attr(c"hw_model");
        let serial = ctx_attr(c"hw_serial");
        let booted_xo: f64 = ctx_attr(c"ad9361-phy,xo_correction").parse().unwrap_or(0.0);
        let mut h = Handle {
            api,
            ctx: RawPtr(ctx),
            phy: RawPtr(phy),
            rx: RawPtr(rx),
            uri: uri.to_string(),
            ctl: Mutex::new(()),
            streaming: AtomicBool::new(false),
            xo: booted_xo,
            chip_min_rate: 0.0,
            decimation: 1,
        };
        if h.xo == 0.0 {
            let mut v: c_longlong = 0;
            // SAFETY: open device, valid out pointer.
            if unsafe {
                (api.iio_device_attr_read_longlong)(phy, c"xo_correction".as_ptr(), &mut v)
            } == 0
            {
                h.xo = v as f64;
            }
        }
        let rx_channels =
            if h.chan(phy, "voltage1", false).is_ok() && h.chan(rx, "voltage2", false).is_ok() {
                2
            } else {
                1
            };
        let (caps, chip_min_rate, decimation) = h.capabilities()?;
        h.chip_min_rate = chip_min_rate;
        h.decimation = decimation;
        let label = if model.is_empty() {
            "PlutoSDR".to_string()
        } else {
            model
        };
        let info = DeviceInfo {
            driver: "pluto".into(),
            args: DeviceArgs::new("pluto").with("uri", uri).to_string(),
            label,
            serial: (!serial.is_empty()).then_some(serial),
            rx: true,
            tx: false,
        };
        Ok((h, info, caps, rx_channels))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.ctl.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn chan(&self, dev: *mut Dev, name: &str, output: bool) -> Result<*mut Chn> {
        let c = c_string(name)?;
        // SAFETY: open device, C string.
        let ch = unsafe { (self.api.iio_device_find_channel)(dev, c.as_ptr(), output) };
        if ch.is_null() {
            return Err(SdrError::Device(format!(
                "{}: no channel `{name}`",
                self.uri
            )));
        }
        Ok(ch)
    }

    fn rx_chan(&self, ch: usize) -> Result<*mut Chn> {
        self.chan(self.phy.0, &format!("voltage{ch}"), false)
    }

    fn lo(&self) -> Result<*mut Chn> {
        self.chan(self.phy.0, "altvoltage0", true)
    }

    fn read(&self, ch: *mut Chn, attr: &str) -> Result<String> {
        let a = c_string(attr)?;
        let mut buf = [0 as c_char; 1024];
        // SAFETY: open channel; libiio NUL-terminates within `len`.
        let r = unsafe {
            (self.api.iio_channel_attr_read)(ch, a.as_ptr(), buf.as_mut_ptr(), buf.len())
        };
        if r < 0 {
            return Err(self.err(r as c_int, &format!("reading `{attr}`")));
        }
        // SAFETY: as above.
        Ok(unsafe { cstr(buf.as_ptr()) })
    }

    fn read_ll(&self, ch: *mut Chn, attr: &str) -> Result<i64> {
        let a = c_string(attr)?;
        let mut v: c_longlong = 0;
        // SAFETY: open channel, valid out pointer.
        let r = unsafe { (self.api.iio_channel_attr_read_longlong)(ch, a.as_ptr(), &mut v) };
        if r < 0 {
            return Err(self.err(r, &format!("reading `{attr}`")));
        }
        Ok(v)
    }

    fn write(&self, ch: *mut Chn, attr: &str, value: &str) -> Result<()> {
        let (a, v) = (c_string(attr)?, c_string(value)?);
        // SAFETY: open channel, C strings.
        let r = unsafe { (self.api.iio_channel_attr_write)(ch, a.as_ptr(), v.as_ptr()) };
        if r < 0 {
            return Err(self.err(r as c_int, &format!("setting `{attr}` to `{value}`")));
        }
        Ok(())
    }

    fn write_ll(&self, ch: *mut Chn, attr: &str, value: i64) -> Result<()> {
        let a = c_string(attr)?;
        // SAFETY: open channel, C string.
        let r = unsafe { (self.api.iio_channel_attr_write_longlong)(ch, a.as_ptr(), value) };
        if r < 0 {
            return Err(self.err(r, &format!("setting `{attr}` to {value}")));
        }
        Ok(())
    }

    /// An error from a negative libiio return code.
    fn err(&self, r: c_int, what: &str) -> SdrError {
        SdrError::Device(format!("{what}: {}", strerror(self.api, -r)))
    }

    /// What the board can do, the AD936x's lowest rate, and the FPGA's
    /// decimation factor (1 for none).
    fn capabilities(&self) -> Result<(Capabilities, f64, u32)> {
        let v0 = self.rx_chan(0)?;
        let range = |ch, attr: &str| self.read(ch, attr).ok().as_deref().and_then(parse_range);
        let chip_rate =
            range(v0, "sampling_frequency_available").unwrap_or(Range::new(2_083_333.0, 61.44e6));
        // The FPGA decimator lists the current rate and the decimated one,
        // e.g. "30720000 3840000 ".
        let dma = self.chan(self.rx.0, "voltage0", false)?;
        let listed: Vec<f64> = self
            .read(dma, "sampling_frequency_available")
            .unwrap_or_default()
            .split_whitespace()
            .filter_map(|x| x.parse().ok())
            .collect();
        let decimation = match listed[..] {
            [a, b, ..] if a > 0.0 && b > 0.0 => (a.max(b) / a.min(b)).round().max(1.0) as u32,
            _ => 1,
        };
        let lo = self.lo()?;
        let caps = Capabilities {
            frequency: range(lo, "frequency_available")
                .map(|r| vec![Range::new(r.min, r.max)])
                .unwrap_or_default(),
            sample_rate: vec![Range::new(chip_rate.min / decimation as f64, chip_rate.max)],
            gain: range(v0, "hardwaregain_available"),
            agc: true,
            antennas: self
                .read(v0, "rf_port_select_available")
                .unwrap_or_default()
                .split_whitespace()
                .map(String::from)
                .collect(),
            bandwidth: range(v0, "rf_bandwidth_available")
                .map(|r| vec![Range::new(r.min, r.max)])
                .unwrap_or_default(),
        };
        Ok((caps, chip_rate.min, decimation))
    }

    /// Applies `cfg` to the receive channels `chs`. Rate, bandwidth and LO
    /// are shared by both channels; antenna and gain are per channel.
    fn configure(&self, cfg: &RxConfig, caps: &Capabilities, chs: &[usize]) -> Result<()> {
        let _g = self.lock();
        let v0 = self.rx_chan(0)?;
        let (chip_rate, decimate) = plan_rate(cfg.sample_rate, self.chip_min_rate, self.decimation);
        self.write_ll(v0, "sampling_frequency", chip_rate.round() as i64)?;
        if self.decimation > 1 {
            let actual = self.read_ll(v0, "sampling_frequency")? as f64;
            let dma = self.chan(self.rx.0, "voltage0", false)?;
            let fpga = if decimate {
                actual / self.decimation as f64
            } else {
                actual
            };
            self.write_ll(dma, "sampling_frequency", fpga.round() as i64)?;
        }
        let bw = cfg.bandwidth.unwrap_or(cfg.sample_rate);
        let bw = match caps.bandwidth.first() {
            Some(r) => bw.clamp(r.min, r.max),
            None => bw,
        };
        self.write_ll(v0, "rf_bandwidth", bw.round() as i64)?;
        self.set_xo(cfg.ppm)?;
        self.write_ll(self.lo()?, "frequency", cfg.frequency.round() as i64)?;
        for &ch in chs {
            let c = self.rx_chan(ch)?;
            if let Some(a) = &cfg.antenna {
                if !caps.antennas.is_empty() && !caps.antennas.iter().any(|x| x == a) {
                    return Err(SdrError::Range(format!(
                        "antenna `{a}` doesn't exist; this board has {}",
                        caps.antennas.join(", ")
                    )));
                }
                self.write(c, "rf_port_select", a)?;
            }
        }
        self.gain_locked(chs, cfg.gain)
    }

    fn set_xo(&self, ppm: f64) -> Result<()> {
        if self.xo <= 0.0 {
            return if ppm == 0.0 {
                Ok(())
            } else {
                Err(SdrError::Unsupported("frequency correction (ppm)".into()))
            };
        }
        let a = c"xo_correction";
        let v = (self.xo * (1.0 + ppm * 1e-6)).round() as c_longlong;
        // SAFETY: open device, C string.
        let r = unsafe { (self.api.iio_device_attr_write_longlong)(self.phy.0, a.as_ptr(), v) };
        if r < 0 {
            return Err(self.err(r, "setting `xo_correction`"));
        }
        Ok(())
    }

    fn gain_locked(&self, chs: &[usize], gain: Gain) -> Result<()> {
        for &ch in chs {
            let c = self.rx_chan(ch)?;
            match gain {
                Gain::Auto => self.write(c, "gain_control_mode", "slow_attack")?,
                Gain::Db(db) => {
                    self.write(c, "gain_control_mode", "manual")?;
                    self.write_ll(c, "hardwaregain", db.round() as i64)?;
                }
            }
        }
        Ok(())
    }

    fn set_gain(&self, chs: &[usize], gain: Gain) -> Result<()> {
        let _g = self.lock();
        self.gain_locked(chs, gain)
    }

    fn set_frequency(&self, freq: f64) -> Result<()> {
        let _g = self.lock();
        self.write_ll(self.lo()?, "frequency", freq.round() as i64)
    }

    /// Moves the reference, then retunes so the LO is recomputed from it.
    fn set_ppm(&self, ppm: f64, freq: f64) -> Result<()> {
        let _g = self.lock();
        self.set_xo(ppm)?;
        self.write_ll(self.lo()?, "frequency", freq.round() as i64)
    }

    /// Enables the I/Q DMA channels of `chs` (and disables the rest), then
    /// creates the receive buffer.
    fn create_buffer(&self, chs: &[usize], samples: usize) -> Result<RawPtr<Buf>> {
        let _g = self.lock();
        for i in 0..4 {
            if let Ok(c) = self.chan(self.rx.0, &format!("voltage{i}"), false) {
                let on = chs.contains(&(i / 2));
                // SAFETY: open channel.
                unsafe {
                    if on {
                        (self.api.iio_channel_enable)(c);
                    } else {
                        (self.api.iio_channel_disable)(c);
                    }
                }
            }
        }
        // SAFETY: open device with channels enabled; NULL + errno on failure.
        let buf = unsafe { (self.api.iio_device_create_buffer)(self.rx.0, samples, false) };
        if buf.is_null() {
            let e = last_errno();
            return Err(SdrError::Device(if e == EBUSY {
                "the board is already receiving (another block or program has its \
                 receive DMA); to get RX1 and RX2 together use one two-channel source"
                    .into()
            } else {
                format!("creating the receive buffer: {}", strerror(self.api, e))
            }));
        }
        Ok(RawPtr(buf))
    }

    /// Claims the receive DMA for one stream.
    fn claim(&self) -> Result<()> {
        if self.streaming.swap(true, Ordering::AcqRel) {
            return Err(SdrError::Device(
                "this device is already receiving; one stream at a time (use a \
                 two-channel source for RX1 and RX2 together)"
                    .into(),
            ));
        }
        Ok(())
    }
}

// ---- Device ---------------------------------------------------------------------

pub struct PlutoDevice {
    info: DeviceInfo,
    caps: Capabilities,
    rx_channels: usize,
    handle: Arc<Handle>,
}

impl PlutoDevice {
    fn check_channel(&self, ch: usize) -> Result<()> {
        if ch < self.rx_channels {
            return Ok(());
        }
        Err(if ch == 1 {
            SdrError::Unsupported(
                "RX2 (channel 1): the board runs with one receive channel. On a Pluto \
                 Rev C/D or an AD9361 board, run `fw_setenv mode 2r2t` on it and reboot"
                    .into(),
            )
        } else {
            SdrError::Range(format!(
                "receive channel {ch} doesn't exist (channels 0–{})",
                self.rx_channels - 1
            ))
        })
    }

    /// Configures, claims the DMA and starts the receive thread for `chs`.
    fn start<T: Copy + Default + Send + 'static>(
        &self,
        cfg: &RxConfig,
        chs: Vec<usize>,
        convert: fn(&[i16]) -> T,
    ) -> Result<PlutoRx<T>> {
        self.caps.check_rate(cfg.sample_rate)?;
        self.caps.check_frequency(cfg.frequency)?;
        let h = &self.handle;
        h.claim()?;
        let started = (|| {
            h.configure(cfg, &self.caps, &chs)?;
            h.create_buffer(&chs, buffer_samples(cfg.sample_rate))
        })();
        let buf = match started {
            Ok(b) => b,
            Err(e) => {
                h.streaming.store(false, Ordering::Release);
                return Err(e);
            }
        };
        let (producer, ring, error) = RxRing::<T>::new(cfg.sample_rate);
        let stop = Arc::new(AtomicBool::new(false));
        let io = RxIo {
            api: h.api,
            buf,
            stop: stop.clone(),
            error,
            words: 2 * chs.len(),
            convert,
        };
        let thread = std::thread::Builder::new()
            .name("fox:pluto-rx".into())
            .spawn(move || io.run(producer));
        let thread = match thread {
            Ok(t) => t,
            Err(e) => {
                // SAFETY: no thread uses the buffer.
                unsafe { (h.api.iio_buffer_destroy)(buf.0) };
                h.streaming.store(false, Ordering::Release);
                return Err(e.into());
            }
        };
        Ok(PlutoRx {
            handle: h.clone(),
            chs,
            ring,
            buf,
            stop,
            freq: cfg.frequency,
            thread: Some(thread),
        })
    }
}

/// I/Q of one channel: two 12-bit words.
fn iq(w: &[i16]) -> Complex32 {
    Complex32::new(w[0] as f32 * SCALE, w[1] as f32 * SCALE)
}

/// Both channels of one sample instant: I0 Q0 I1 Q1.
fn iq_pair(w: &[i16]) -> [Complex32; 2] {
    [iq(&w[..2]), iq(&w[2..4])]
}

impl Device for PlutoDevice {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn capabilities(&self) -> Capabilities {
        self.caps.clone()
    }

    fn rx_channels(&self) -> usize {
        self.rx_channels
    }

    fn start_rx(&mut self, cfg: &RxConfig) -> Result<Box<dyn RxStream>> {
        self.check_channel(cfg.channel)?;
        Ok(Box::new(self.start(cfg, vec![cfg.channel], iq)?))
    }

    fn start_rx_dual(&mut self, cfg: &RxConfig) -> Result<Box<dyn DualRxStream>> {
        self.check_channel(1)?;
        Ok(Box::new(self.start(cfg, vec![0, 1], iq_pair)?))
    }
}

// ---- Receive ----------------------------------------------------------------------

/// The receive thread's side: refills the buffer and converts into the ring.
struct RxIo<T> {
    api: &'static Api,
    buf: RawPtr<Buf>,
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    /// 16-bit words per sample instant (2 per channel).
    words: usize,
    convert: fn(&[i16]) -> T,
}

impl<T: Copy + Default> RxIo<T> {
    fn run(self, mut producer: Producer<T>) {
        let mut scratch = vec![T::default(); 16384].into_boxed_slice();
        while !self.stop.load(Ordering::Relaxed) && !producer.abandoned() {
            // SAFETY: the buffer lives until the stream joins this thread.
            let r = unsafe { (self.api.iio_buffer_refill)(self.buf.0) };
            if self.stop.load(Ordering::Relaxed) {
                break;
            }
            if r < 0 {
                *self.error.lock().unwrap_or_else(|p| p.into_inner()) = Some(format!(
                    "the board stopped streaming: {}",
                    strerror(self.api, -(r as c_int))
                ));
                break;
            }
            // SAFETY: after a successful refill, [start, end) holds the
            // samples, 16-bit aligned (the buffer is 8-byte aligned).
            let words = unsafe {
                let start = (self.api.iio_buffer_start)(self.buf.0) as *const i16;
                let end = (self.api.iio_buffer_end)(self.buf.0) as *const i16;
                let n = (end as usize).saturating_sub(start as usize) / 2;
                std::slice::from_raw_parts(start, n)
            };
            for chunk in words.chunks(scratch.len() * self.words) {
                let n = chunk.len() / self.words;
                for (d, s) in scratch[..n].iter_mut().zip(chunk.chunks_exact(self.words)) {
                    *d = (self.convert)(s);
                }
                producer.push(&scratch[..n]);
            }
        }
    }
}

struct PlutoRx<T> {
    handle: Arc<Handle>,
    chs: Vec<usize>,
    ring: RxRing<T>,
    buf: RawPtr<Buf>,
    stop: Arc<AtomicBool>,
    freq: f64,
    thread: Option<JoinHandle<()>>,
}

impl<T> PlutoRx<T> {
    fn set_frequency(&mut self, freq: f64) -> Result<()> {
        self.handle.set_frequency(freq)?;
        self.freq = freq;
        Ok(())
    }
}

impl<T> Drop for PlutoRx<T> {
    fn drop(&mut self) {
        let h = &self.handle;
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            // A refill takes one buffer's worth of time (≤ 63 ms). Cancelling
            // one in flight makes libiio's network backend print errors, so
            // only cancel a thread that seems stuck (board gone).
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
            while !t.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            if !t.is_finished() {
                // SAFETY: cancel may be called from another thread to end a refill.
                unsafe { (h.api.iio_buffer_cancel)(self.buf.0) };
            }
            let _ = t.join();
        }
        // SAFETY: the thread is gone; nothing else uses the buffer.
        unsafe { (h.api.iio_buffer_destroy)(self.buf.0) };
        h.streaming.store(false, Ordering::Release);
    }
}

impl RxStream for PlutoRx<Complex32> {
    fn read(&mut self, out: &mut [Complex32]) -> Result<usize> {
        self.ring.read(out)
    }

    fn set_waker(&mut self, thread: Thread) {
        self.ring.set_waker(thread);
    }

    fn set_frequency(&mut self, freq: f64) -> Result<()> {
        PlutoRx::set_frequency(self, freq)
    }

    fn set_gain(&mut self, gain: Gain) -> Result<()> {
        self.handle.set_gain(&self.chs, gain)
    }

    fn set_ppm(&mut self, ppm: f64) -> Result<()> {
        self.handle.set_ppm(ppm, self.freq)
    }

    fn overflows(&self) -> u64 {
        self.ring.dropped()
    }

    fn finished(&self) -> bool {
        self.ring.finished()
    }
}

impl DualRxStream for PlutoRx<[Complex32; 2]> {
    fn read(&mut self, a: &mut [Complex32], b: &mut [Complex32]) -> Result<usize> {
        self.ring.read_pair(a, b)
    }

    fn set_waker(&mut self, thread: Thread) {
        self.ring.set_waker(thread);
    }

    fn set_frequency(&mut self, freq: f64) -> Result<()> {
        PlutoRx::set_frequency(self, freq)
    }

    fn set_gain(&mut self, gain: Gain) -> Result<()> {
        self.handle.set_gain(&self.chs, gain)
    }

    fn set_ppm(&mut self, ppm: f64) -> Result<()> {
        self.handle.set_ppm(ppm, self.freq)
    }

    fn overflows(&self) -> u64 {
        self.ring.dropped()
    }

    fn finished(&self) -> bool {
        self.ring.finished()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(
            parse_range("[2083333 1 61440000]"),
            Some(Range {
                min: 2_083_333.0,
                max: 61.44e6,
                step: 1.0
            })
        );
        assert_eq!(parse_range("[-3 1 71]").map(|r| r.min), Some(-3.0));
        assert_eq!(parse_range("A_BALANCED B_BALANCED"), None);
        assert_eq!(parse_range("[1 x 2]"), None);
    }

    #[test]
    fn low_rates_use_the_fpga_decimator() {
        assert_eq!(plan_rate(12e6, 2_083_333.0, 8), (12e6, false));
        assert_eq!(plan_rate(1e6, 2_083_333.0, 8), (8e6, true));
        assert_eq!(plan_rate(1e6, 2_083_333.0, 1), (1e6, false));
        assert_eq!(buffer_samples(260e3), 1 << 14);
        assert_eq!(buffer_samples(12e6), 1 << 17);
        assert_eq!(buffer_samples(61.44e6), 1 << 18);
    }

    #[test]
    fn converts_samples() {
        assert_eq!(iq(&[2047, -2048]), Complex32::new(2047.0 / 2048.0, -1.0));
        let [a, b] = iq_pair(&[1024, 0, 0, -1024]);
        assert_eq!(
            (a, b),
            (Complex32::new(0.5, 0.0), Complex32::new(0.0, -0.5))
        );
    }

    #[test]
    fn usb_descriptions() {
        let (name, serial) = describe(
            "0456:b673 (Analog Devices Inc. PlutoSDR (ADALM-PLUTO)), serial=104473dc5993000915001c00bb64c1a3d8",
        );
        assert_eq!(name, "PlutoSDR (ADALM-PLUTO)");
        assert_eq!(
            serial.as_deref(),
            Some("104473dc5993000915001c00bb64c1a3d8")
        );
        assert_eq!(describe("something").0, "PlutoSDR");
    }

    #[test]
    fn loads_library_and_discovers() {
        match PlutoDriver::new().discover() {
            Ok(list) => assert!(list.iter().all(|i| i.driver == "pluto" && i.rx)),
            Err(e) => assert_eq!(e.to_string(), MISSING),
        }
    }

    #[test]
    fn missing_library_is_reported() {
        let d = PlutoDriver::with_sonames(&["/nonexistent/libiio.so.0"]);
        assert_eq!(d.discover().unwrap_err().to_string(), MISSING);
    }

    /// Against a real board: `FOXYGNAL_PLUTO_URI=ip:10.0.0.5 cargo test -p
    /// foxygnal-sdr pluto -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a Pluto (FOXYGNAL_PLUTO_URI)"]
    fn real_board() {
        let uri = std::env::var("FOXYGNAL_PLUTO_URI").expect("FOXYGNAL_PLUTO_URI");
        let args = DeviceArgs::new("pluto").with("uri", &uri);
        let mut dev = PlutoDriver::new().open(&args).unwrap();
        let caps = dev.capabilities();
        eprintln!(
            "{:?} {} rx channels\n{caps:?}",
            dev.info(),
            dev.rx_channels()
        );
        let cfg = |rate: f64, channel| RxConfig {
            sample_rate: rate,
            frequency: 100e6,
            gain: Gain::Db(30.0),
            ppm: 0.0,
            bandwidth: None,
            antenna: None,
            channel,
        };
        let wait_for = |want: usize, read: &mut dyn FnMut() -> usize| {
            let t0 = std::time::Instant::now();
            let mut got = 0;
            while got < want && t0.elapsed() < std::time::Duration::from_secs(5) {
                got += read();
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            got
        };
        let mut buf = vec![Complex32::default(); 1 << 16];
        for (rate, ch) in [(2.4e6, 0), (1e6, 0), (12e6, dev.rx_channels() - 1)] {
            let mut rx = dev.start_rx(&cfg(rate, ch)).unwrap();
            rx.set_frequency(433.92e6).unwrap();
            rx.set_gain(Gain::Auto).unwrap();
            let got = wait_for(rate as usize / 2, &mut || rx.read(&mut buf).unwrap());
            let p = buf.iter().map(|c| c.norm_sqr()).sum::<f32>() / buf.len() as f32;
            eprintln!("{rate} S/s ch{ch}: {got} samples, power {p}");
            assert!(got >= rate as usize / 2);
            // One stream at a time.
            assert!(dev.start_rx(&cfg(rate, 0)).is_err());
        }
        if dev.rx_channels() == 2 {
            let mut rx = dev.start_rx_dual(&cfg(4e6, 0)).unwrap();
            let mut b = vec![Complex32::default(); 1 << 16];
            let got = wait_for(2_000_000, &mut || rx.read(&mut buf, &mut b).unwrap());
            eprintln!("dual: {got} sample pairs, {} dropped", rx.overflows());
            assert!(got >= 2_000_000);
        }
    }
}
