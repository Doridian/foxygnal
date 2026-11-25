//! Any SoapySDR-supported device through the SoapySDR C API (loaded at run
//! time; the 0.8 ABI).
//!
//! Device strings: `soapy:<kwargs>`; every pair is passed to
//! `SoapySDRDevice_make` as is, e.g. `soapy:driver=plutosdr,uri=ip:192.168.2.1`
//! or `soapy:driver=lime,serial=…`. `soapy:` alone opens the first device
//! SoapySDR finds.
//!
//! Discovery runs `SoapySDRDevice_enumerate` on a helper thread and gives up
//! after [`SCAN_TIMEOUT`]: some modules scan the network and can hang. Network
//! devices that don't answer a scan (a PlutoSDR at a fixed address without
//! Avahi, say) can be listed with hints: [`SoapyDriver::with_hints`] or the
//! `FOXYGNAL_SOAPY_DEVICES` environment variable, `;`-separated kwargs
//! strings. A hint the scan doesn't confirm is still listed, marked as
//! unverified (making the device just to check would take seconds).
//!
//! Streams use `CF32`. Receiving runs `readStream` (100 ms timeout) on an I/O
//! thread that pushes into the ring. Control calls (retune, gain) are
//! serialised by a mutex the I/O loop never takes; SoapySDR allows control
//! calls while another thread streams.

use std::ffi::{c_char, c_int, c_long, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{JoinHandle, Thread};
use std::time::Duration;

use super::{RawPtr, RxRing, cached, cstr, dl_api, open_library};
use crate::ring::{self, Producer};
use crate::{
    Capabilities, Complex32, Device, DeviceArgs, DeviceInfo, Driver, Gain, Range, Result, RxConfig,
    RxStream, SdrError, TxConfig, TxStream,
};

const SONAMES: &[&str] = &[
    "libSoapySDR.so.0.8",
    "libSoapySDR.so.0.8-3",
    "libSoapySDR.so",
    "libSoapySDR.0.8.dylib",
    "libSoapySDR.dylib",
    "SoapySDR.dll",
    "libSoapySDR.dll",
];
const MISSING: &str =
    "libSoapySDR not found (install SoapySDR 0.8 and the modules for your devices)";

/// How long discovery waits for `SoapySDRDevice_enumerate`.
pub const SCAN_TIMEOUT: Duration = Duration::from_secs(2);

const SOAPY_SDR_TX: c_int = 0;
const SOAPY_SDR_RX: c_int = 1;
const SOAPY_SDR_TIMEOUT: c_int = -1;
const SOAPY_SDR_OVERFLOW: c_int = -4;
const SOAPY_SDR_NOT_SUPPORTED: c_int = -5;
const SOAPY_SDR_UNDERFLOW: c_int = -7;
const CF32: &std::ffi::CStr = c"CF32";

/// `SoapySDRKwargs` from SoapySDR/Types.h.
#[repr(C)]
pub(crate) struct Kwargs {
    pub size: usize,
    pub keys: *mut *mut c_char,
    pub vals: *mut *mut c_char,
}

/// `SoapySDRRange` from SoapySDR/Types.h.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct SoapyRange {
    pub minimum: f64,
    pub maximum: f64,
    pub step: f64,
}

/// Opaque `SoapySDRDevice` / `SoapySDRStream`.
#[repr(C)]
pub(crate) struct Dev {
    _p: [u8; 0],
}
#[repr(C)]
pub(crate) struct Stream {
    _p: [u8; 0],
}

dl_api! {
    /// The parts of the SoapySDR 0.8 C API we use.
    pub(crate) struct Api {
        fn SoapySDR_free(*mut c_void);
        fn SoapySDR_errToStr(c_int) -> *const c_char;
        fn SoapySDRStrings_clear(*mut *mut *mut c_char, usize);
        fn SoapySDRKwargs_set(*mut Kwargs, *const c_char, *const c_char) -> c_int;
        fn SoapySDRKwargs_clear(*mut Kwargs);
        fn SoapySDRKwargsList_clear(*mut Kwargs, usize);
        fn SoapySDRDevice_lastError() -> *const c_char;
        fn SoapySDRDevice_enumerate(*const Kwargs, *mut usize) -> *mut Kwargs;
        fn SoapySDRDevice_make(*const Kwargs) -> *mut Dev;
        fn SoapySDRDevice_unmake(*mut Dev) -> c_int;
        fn SoapySDRDevice_getHardwareKey(*const Dev) -> *mut c_char;
        fn SoapySDRDevice_getNumChannels(*const Dev, c_int) -> usize;
        fn SoapySDRDevice_setupStream(*mut Dev, c_int, *const c_char, *const usize, usize, *const Kwargs) -> *mut Stream;
        fn SoapySDRDevice_closeStream(*mut Dev, *mut Stream) -> c_int;
        fn SoapySDRDevice_getStreamMTU(*const Dev, *mut Stream) -> usize;
        fn SoapySDRDevice_activateStream(*mut Dev, *mut Stream, c_int, i64, usize) -> c_int;
        fn SoapySDRDevice_deactivateStream(*mut Dev, *mut Stream, c_int, i64) -> c_int;
        fn SoapySDRDevice_readStream(*mut Dev, *mut Stream, *const *mut c_void, usize, *mut c_int, *mut i64, c_long) -> c_int;
        fn SoapySDRDevice_writeStream(*mut Dev, *mut Stream, *const *const c_void, usize, *mut c_int, i64, c_long) -> c_int;
        fn SoapySDRDevice_readStreamStatus(*mut Dev, *mut Stream, *mut usize, *mut c_int, *mut i64, c_long) -> c_int;
        fn SoapySDRDevice_listAntennas(*const Dev, c_int, usize, *mut usize) -> *mut *mut c_char;
        fn SoapySDRDevice_setAntenna(*mut Dev, c_int, usize, *const c_char) -> c_int;
        fn SoapySDRDevice_hasGainMode(*const Dev, c_int, usize) -> bool;
        fn SoapySDRDevice_setGainMode(*mut Dev, c_int, usize, bool) -> c_int;
        fn SoapySDRDevice_setGain(*mut Dev, c_int, usize, f64) -> c_int;
        fn SoapySDRDevice_getGainRange(*const Dev, c_int, usize) -> SoapyRange;
        fn SoapySDRDevice_setFrequency(*mut Dev, c_int, usize, f64, *const Kwargs) -> c_int;
        fn SoapySDRDevice_getFrequencyRange(*const Dev, c_int, usize, *mut usize) -> *mut SoapyRange;
        fn SoapySDRDevice_hasFrequencyCorrection(*const Dev, c_int, usize) -> bool;
        fn SoapySDRDevice_setFrequencyCorrection(*mut Dev, c_int, usize, f64) -> c_int;
        fn SoapySDRDevice_setSampleRate(*mut Dev, c_int, usize, f64) -> c_int;
        fn SoapySDRDevice_listSampleRates(*const Dev, c_int, usize, *mut usize) -> *mut f64;
        fn SoapySDRDevice_getSampleRateRange(*const Dev, c_int, usize, *mut usize) -> *mut SoapyRange;
        fn SoapySDRDevice_setBandwidth(*mut Dev, c_int, usize, f64) -> c_int;
        fn SoapySDRDevice_listBandwidths(*const Dev, c_int, usize, *mut usize) -> *mut f64;
        fn SoapySDRDevice_getBandwidthRange(*const Dev, c_int, usize, *mut usize) -> *mut SoapyRange;
    }
}

static DEFAULT_API: OnceLock<&'static Api> = OnceLock::new();

/// Scans running right now (each leaves a thread behind until it returns).
static SCANS: AtomicUsize = AtomicUsize::new(0);

pub struct SoapyDriver {
    sonames: Option<Vec<String>>,
    own: OnceLock<&'static Api>,
    hints: Vec<String>,
}

impl Default for SoapyDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl SoapyDriver {
    /// The driver, with hints from `FOXYGNAL_SOAPY_DEVICES` if set.
    pub fn new() -> Self {
        let hints = std::env::var("FOXYGNAL_SOAPY_DEVICES")
            .map(|v| {
                v.split(';')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        Self::with_hints(hints)
    }

    /// Also list these devices (SoapySDR kwargs strings such as
    /// `driver=plutosdr,uri=ip:192.168.2.1`), which a scan may not find.
    pub fn with_hints(hints: Vec<String>) -> Self {
        SoapyDriver {
            sonames: None,
            own: OnceLock::new(),
            hints,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_sonames(names: &[&str]) -> Self {
        SoapyDriver {
            sonames: Some(names.iter().map(|s| s.to_string()).collect()),
            own: OnceLock::new(),
            hints: vec![],
        }
    }

    fn api(&self) -> Result<&'static Api> {
        let load = |names: &[String]| {
            open_library(names, MISSING).and_then(|l| Api::resolve(l, "libSoapySDR"))
        };
        match &self.sonames {
            None => cached(&DEFAULT_API, || {
                load(&SONAMES.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            }),
            Some(n) => cached(&self.own, || load(n)),
        }
    }
}

type Pairs = Vec<(String, String)>;

/// SoapySDR kwargs built from Rust pairs; cleared on drop.
struct OwnedKwargs<'a> {
    api: &'a Api,
    kw: Kwargs,
}

impl<'a> OwnedKwargs<'a> {
    fn new(api: &'a Api, pairs: &[(String, String)]) -> Result<Self> {
        let mut k = OwnedKwargs {
            api,
            kw: Kwargs {
                size: 0,
                keys: std::ptr::null_mut(),
                vals: std::ptr::null_mut(),
            },
        };
        for (key, val) in pairs {
            let (ck, cv) = (super::c_string(key)?, super::c_string(val)?);
            // SAFETY: `kw` is a valid (empty or library-grown) kwargs; the
            // library copies the strings.
            let r = unsafe { (api.SoapySDRKwargs_set)(&mut k.kw, ck.as_ptr(), cv.as_ptr()) };
            if r != 0 {
                return Err(SdrError::Device("SoapySDRKwargs_set: out of memory".into()));
            }
        }
        Ok(k)
    }
}

impl Drop for OwnedKwargs<'_> {
    fn drop(&mut self) {
        // SAFETY: built by SoapySDRKwargs_set (or empty).
        unsafe { (self.api.SoapySDRKwargs_clear)(&mut self.kw) };
    }
}

/// Copies a library kwargs into pairs.
///
/// # Safety
/// `kw` must be a valid kwargs.
unsafe fn read_kwargs(kw: &Kwargs) -> Pairs {
    (0..kw.size)
        // SAFETY: `keys`/`vals` hold `size` C strings.
        .map(|i| unsafe { (cstr(*kw.keys.add(i)), cstr(*kw.vals.add(i))) })
        .collect()
}

/// `SoapySDRDevice_enumerate` with `filter`, right here (may be slow).
fn enumerate_now(api: &Api, filter: &[(String, String)]) -> Result<Vec<Pairs>> {
    let args = OwnedKwargs::new(api, filter)?;
    let mut n = 0usize;
    // SAFETY: valid kwargs and out pointer; the returned list has `n` entries
    // and is freed with KwargsList_clear.
    unsafe {
        let list = (api.SoapySDRDevice_enumerate)(&args.kw, &mut n);
        if list.is_null() {
            return Ok(vec![]);
        }
        let out = (0..n).map(|i| read_kwargs(&*list.add(i))).collect();
        (api.SoapySDRKwargsList_clear)(list, n);
        Ok(out)
    }
}

/// Runs the scans on a helper thread so a hanging module can't block the
/// caller longer than `timeout`. Returns `(scan results, results per hint)`.
fn enumerate_bounded(
    api: &'static Api,
    hints: Vec<Pairs>,
    timeout: Duration,
) -> Result<(Vec<Pairs>, Vec<Vec<Pairs>>)> {
    if SCANS.load(Ordering::Acquire) > 0 {
        return Err(SdrError::Device(
            "a SoapySDR device scan is still running (a module may be stuck scanning the network)"
                .into(),
        ));
    }
    let (tx, rx) = std::sync::mpsc::channel();
    SCANS.fetch_add(1, Ordering::AcqRel);
    std::thread::Builder::new()
        .name("soapy-scan".into())
        .spawn(move || {
            let r = (|| {
                let _usb = super::usb_lock(super::USB_LOCK_WAIT)?;
                let all = enumerate_now(api, &[])?;
                let per_hint = hints
                    .iter()
                    .map(|h| enumerate_now(api, h))
                    .collect::<Result<Vec<_>>>()?;
                Ok((all, per_hint))
            })();
            SCANS.fetch_sub(1, Ordering::AcqRel);
            let _ = tx.send(r);
        })
        .inspect_err(|_| {
            SCANS.fetch_sub(1, Ordering::AcqRel);
        })?;
    rx.recv_timeout(timeout).unwrap_or_else(|_| {
        Err(SdrError::Device(format!(
            "SoapySDR device scan took longer than {} s; open devices by their string instead \
             (e.g. `soapy:driver=plutosdr,uri=ip:192.168.2.1`)",
            timeout.as_secs_f64()
        )))
    })
}

/// The pairs that identify a found device (`label` is for people only).
fn info_for(pairs: &Pairs, verified: bool) -> DeviceInfo {
    let get = |k: &str| {
        pairs
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    };
    let mut args = DeviceArgs::new("soapy");
    for (k, v) in pairs {
        if k != "label" {
            args = args.with(k, v.clone());
        }
    }
    let base = match get("label") {
        Some(l) if !l.is_empty() => l.to_string(),
        _ => {
            let driver = get("driver").unwrap_or("SoapySDR device");
            match get("serial").or(get("uri")).or(get("addr")) {
                Some(id) => format!("{driver} {id}"),
                None => driver.to_string(),
            }
        }
    };
    DeviceInfo {
        driver: "soapy".into(),
        args: args.to_string(),
        label: if verified {
            base
        } else {
            format!("{base} (configured, not verified)")
        },
        serial: get("serial").map(String::from),
        rx: true,
        // Unknown without making the device; `open()` reports the truth.
        tx: false,
    }
}

impl Driver for SoapyDriver {
    fn name(&self) -> &'static str {
        "soapy"
    }

    fn label(&self) -> &'static str {
        "SoapySDR"
    }

    fn discover(&self) -> Result<Vec<DeviceInfo>> {
        let api = self.api()?;
        let hints: Vec<Pairs> = self
            .hints
            .iter()
            .filter_map(|h| DeviceArgs::parse(&format!("soapy:{h}")).ok())
            .map(|a| a.pairs().to_vec())
            .collect();
        let (all, per_hint) = enumerate_bounded(api, hints.clone(), SCAN_TIMEOUT)?;
        let mut out: Vec<DeviceInfo> = all.iter().map(|p| info_for(p, true)).collect();
        for (hint, found) in hints.iter().zip(per_hint) {
            if found.is_empty() {
                out.push(info_for(hint, false));
            }
            for p in found {
                let i = info_for(&p, true);
                if !out.iter().any(|o| o.args == i.args) {
                    out.push(i);
                }
            }
        }
        Ok(out)
    }

    fn open(&self, args: &DeviceArgs) -> Result<Box<dyn Device>> {
        let api = self.api()?;
        let kw = OwnedKwargs::new(api, args.pairs())?;
        let usb = super::usb_lock(super::USB_LOCK_WAIT)?;
        // SAFETY: valid kwargs; may block while the device connects.
        let dev = unsafe { (api.SoapySDRDevice_make)(&kw.kw) };
        drop(usb);
        if dev.is_null() {
            return Err(SdrError::Device(format!(
                "SoapySDR could not open `{args}`: {}",
                last_error(api)
            )));
        }
        let handle = Arc::new(Handle {
            api,
            dev: RawPtr(dev),
            ctl: Mutex::new(()),
            waiting: AtomicUsize::new(0),
        });
        // SAFETY: open device, not shared yet.
        let (hw, nrx, ntx) = unsafe {
            let k = (api.SoapySDRDevice_getHardwareKey)(dev);
            let hw = cstr(k);
            (api.SoapySDR_free)(k.cast());
            (
                hw,
                (api.SoapySDRDevice_getNumChannels)(dev, SOAPY_SDR_RX),
                (api.SoapySDRDevice_getNumChannels)(dev, SOAPY_SDR_TX),
            )
        };
        let caps = if nrx > 0 {
            handle.capabilities(SOAPY_SDR_RX, 0)
        } else {
            Capabilities::default()
        };
        let label = match args.get("driver") {
            Some(d) if !hw.is_empty() => format!("{hw} ({d})"),
            _ if !hw.is_empty() => hw,
            _ => "SoapySDR device".into(),
        };
        Ok(Box::new(SoapyDevice {
            info: DeviceInfo {
                driver: "soapy".into(),
                args: args.to_string(),
                label,
                serial: args.get("serial").map(String::from),
                rx: nrx > 0,
                tx: ntx > 0,
            },
            handle,
            caps,
            rx_channels: nrx,
        }))
    }
}

fn last_error(api: &Api) -> String {
    // SAFETY: returns a thread-local C string.
    let e = unsafe { cstr((api.SoapySDRDevice_lastError)()) };
    if e.is_empty() {
        "unknown error".into()
    } else {
        e
    }
}

fn err_str(api: &Api, code: c_int) -> String {
    // SAFETY: returns a static string for any code.
    unsafe { cstr((api.SoapySDR_errToStr)(code)) }
}

/// An open `SoapySDRDevice`, unmade when the last owner drops it.
struct Handle {
    api: &'static Api,
    dev: RawPtr<Dev>,
    /// Serialises control calls and stream setup/teardown. The stream I/O
    /// loops don't take it.
    ctl: Mutex<()>,
    /// Control calls waiting or running. Some modules guard the device with
    /// an unfair (spin) lock that `readStream` also takes; an I/O loop that
    /// calls `readStream` back to back can starve a retune for many seconds
    /// (seen with SoapyPlutoSDR: 11–19 s). The receive loop pauses briefly
    /// while this is non-zero.
    waiting: AtomicUsize,
}

/// A held control lock; counts in [`Handle::waiting`] while it exists.
struct CtlGuard<'a> {
    _g: std::sync::MutexGuard<'a, ()>,
    waiting: &'a AtomicUsize,
}

impl Drop for CtlGuard<'_> {
    fn drop(&mut self) {
        self.waiting.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Handle {
    fn lock(&self) -> CtlGuard<'_> {
        self.waiting.fetch_add(1, Ordering::AcqRel);
        CtlGuard {
            _g: self.ctl.lock().unwrap_or_else(|e| e.into_inner()),
            waiting: &self.waiting,
        }
    }

    /// Called by an I/O loop before each stream call: lets a pending control
    /// call through, waiting at most `max`.
    fn yield_to_control(&self, max: Duration) {
        let until = std::time::Instant::now() + max;
        while self.waiting.load(Ordering::Acquire) > 0 && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_micros(200));
        }
    }

    fn check(&self, r: c_int, what: &str) -> Result<()> {
        if r != 0 {
            Err(SdrError::Device(format!(
                "{what} failed: {}",
                err_str(self.api, r)
            )))
        } else {
            Ok(())
        }
    }

    /// Ranges from a `(dev, dir, ch, &mut len) -> *mut SoapyRange` getter.
    fn ranges(
        &self,
        f: unsafe extern "C" fn(*const Dev, c_int, usize, *mut usize) -> *mut SoapyRange,
        dir: c_int,
        ch: usize,
    ) -> Vec<Range> {
        let mut n = 0;
        // SAFETY: the array has `n` entries and is freed with SoapySDR_free.
        unsafe {
            let p = f(self.dev.0, dir, ch, &mut n);
            if p.is_null() {
                return vec![];
            }
            let out = std::slice::from_raw_parts(p, n)
                .iter()
                .map(|r| Range {
                    min: r.minimum,
                    max: r.maximum,
                    step: r.step,
                })
                .collect();
            (self.api.SoapySDR_free)(p.cast());
            out
        }
    }

    /// Points from a `(dev, dir, ch, &mut len) -> *mut f64` list, as ranges.
    fn points(
        &self,
        f: unsafe extern "C" fn(*const Dev, c_int, usize, *mut usize) -> *mut f64,
        dir: c_int,
        ch: usize,
    ) -> Vec<Range> {
        let mut n = 0;
        // SAFETY: as in `ranges`.
        unsafe {
            let p = f(self.dev.0, dir, ch, &mut n);
            if p.is_null() {
                return vec![];
            }
            let out = std::slice::from_raw_parts(p, n)
                .iter()
                .map(|&v| Range::new(v, v))
                .collect();
            (self.api.SoapySDR_free)(p.cast());
            out
        }
    }

    fn capabilities(&self, dir: c_int, ch: usize) -> Capabilities {
        let api = self.api;
        let _g = self.lock();
        let mut sample_rate = self.ranges(api.SoapySDRDevice_getSampleRateRange, dir, ch);
        if sample_rate.is_empty() {
            sample_rate = self.points(api.SoapySDRDevice_listSampleRates, dir, ch);
        }
        let mut bandwidth = self.ranges(api.SoapySDRDevice_getBandwidthRange, dir, ch);
        if bandwidth.is_empty() {
            bandwidth = self.points(api.SoapySDRDevice_listBandwidths, dir, ch);
        }
        let dev = self.dev.0;
        // SAFETY: open device, control lock held; the antenna list is freed
        // with SoapySDRStrings_clear.
        let (gain, agc, antennas) = unsafe {
            let g = (api.SoapySDRDevice_getGainRange)(dev, dir, ch);
            let gain = (g.maximum > g.minimum || g.maximum != 0.0).then_some(Range {
                min: g.minimum,
                max: g.maximum,
                step: g.step,
            });
            let agc = (api.SoapySDRDevice_hasGainMode)(dev, dir, ch);
            let mut n = 0;
            let mut list = (api.SoapySDRDevice_listAntennas)(dev, dir, ch, &mut n);
            let antennas = if list.is_null() {
                vec![]
            } else {
                let v = (0..n).map(|i| cstr(*list.add(i))).collect();
                (api.SoapySDRStrings_clear)(&mut list, n);
                v
            };
            (gain, agc, antennas)
        };
        Capabilities {
            frequency: self.ranges(api.SoapySDRDevice_getFrequencyRange, dir, ch),
            sample_rate,
            gain,
            agc,
            antennas,
            bandwidth,
        }
    }

    fn set_frequency(&self, dir: c_int, ch: usize, f: f64) -> Result<()> {
        let _g = self.lock();
        // SAFETY: open device, control lock held; null kwargs is allowed.
        let r = unsafe {
            (self.api.SoapySDRDevice_setFrequency)(self.dev.0, dir, ch, f, std::ptr::null())
        };
        self.check(r, "setFrequency")
    }

    fn set_gain(&self, dir: c_int, ch: usize, gain: Gain) -> Result<()> {
        let api = self.api;
        let dev = self.dev.0;
        let _g = self.lock();
        // SAFETY: open device, control lock held.
        unsafe {
            let has_mode = (api.SoapySDRDevice_hasGainMode)(dev, dir, ch);
            match gain {
                Gain::Auto if has_mode => self.check(
                    (api.SoapySDRDevice_setGainMode)(dev, dir, ch, true),
                    "setGainMode",
                ),
                // No AGC: leave the device's default gain.
                Gain::Auto => Ok(()),
                Gain::Db(db) => {
                    if has_mode {
                        self.check(
                            (api.SoapySDRDevice_setGainMode)(dev, dir, ch, false),
                            "setGainMode",
                        )?;
                    }
                    let g = (api.SoapySDRDevice_getGainRange)(dev, dir, ch);
                    let db = if g.maximum > g.minimum {
                        db.clamp(g.minimum, g.maximum)
                    } else {
                        db
                    };
                    self.check((api.SoapySDRDevice_setGain)(dev, dir, ch, db), "setGain")
                }
            }
        }
    }

    fn set_ppm(&self, dir: c_int, ch: usize, ppm: f64) -> Result<()> {
        let _g = self.lock();
        // SAFETY: open device, control lock held.
        unsafe {
            if !(self.api.SoapySDRDevice_hasFrequencyCorrection)(self.dev.0, dir, ch) {
                return if ppm == 0.0 {
                    Ok(())
                } else {
                    Err(SdrError::Unsupported("frequency correction (ppm)".into()))
                };
            }
            let r = (self.api.SoapySDRDevice_setFrequencyCorrection)(self.dev.0, dir, ch, ppm);
            self.check(r, "setFrequencyCorrection")
        }
    }

    /// Rate, frequency, bandwidth and antenna for one direction.
    fn configure(
        &self,
        dir: c_int,
        ch: usize,
        rate: f64,
        freq: f64,
        bandwidth: Option<f64>,
        antenna: Option<&str>,
    ) -> Result<()> {
        let api = self.api;
        let dev = self.dev.0;
        {
            let _g = self.lock();
            // SAFETY: open device, control lock held.
            unsafe {
                if let Some(a) = antenna {
                    let c = super::c_string(a)?;
                    self.check(
                        (api.SoapySDRDevice_setAntenna)(dev, dir, ch, c.as_ptr()),
                        "setAntenna",
                    )?;
                }
                self.check(
                    (api.SoapySDRDevice_setSampleRate)(dev, dir, ch, rate),
                    "setSampleRate",
                )?;
                if let Some(bw) = bandwidth {
                    self.check(
                        (api.SoapySDRDevice_setBandwidth)(dev, dir, ch, bw),
                        "setBandwidth",
                    )?;
                }
            }
        }
        self.set_frequency(dir, ch, freq)
    }

    /// Sets up and activates a CF32 stream on one channel; returns it and
    /// its MTU.
    fn open_stream(&self, dir: c_int, ch: usize) -> Result<(RawPtr<Stream>, usize)> {
        let api = self.api;
        let dev = self.dev.0;
        let _g = self.lock();
        // SAFETY: open device, control lock held; one channel in the array.
        unsafe {
            let s =
                (api.SoapySDRDevice_setupStream)(dev, dir, CF32.as_ptr(), &ch, 1, std::ptr::null());
            if s.is_null() {
                return Err(SdrError::Device(format!(
                    "setupStream failed: {}",
                    last_error(api)
                )));
            }
            let r = (api.SoapySDRDevice_activateStream)(dev, s, 0, 0, 0);
            if r != 0 {
                (api.SoapySDRDevice_closeStream)(dev, s);
                return Err(SdrError::Device(format!(
                    "activateStream failed: {}",
                    err_str(api, r)
                )));
            }
            let mtu = (api.SoapySDRDevice_getStreamMTU)(dev, s).clamp(256, 1 << 20);
            Ok((RawPtr(s), mtu))
        }
    }

    fn close_stream(&self, s: RawPtr<Stream>) {
        let _g = self.lock();
        // SAFETY: the stream was set up on this device and its I/O loop ended.
        unsafe {
            (self.api.SoapySDRDevice_deactivateStream)(self.dev.0, s.0, 0, 0);
            (self.api.SoapySDRDevice_closeStream)(self.dev.0, s.0);
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // Close regardless if the lock can't be had: leaking is worse.
        let _usb = super::usb_lock(super::USB_LOCK_WAIT).ok();
        // SAFETY: every stream owns an Arc, so all are closed.
        unsafe { (self.api.SoapySDRDevice_unmake)(self.dev.0) };
    }
}

pub struct SoapyDevice {
    handle: Arc<Handle>,
    info: DeviceInfo,
    caps: Capabilities,
    rx_channels: usize,
}

impl Device for SoapyDevice {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn capabilities(&self) -> Capabilities {
        self.caps.clone()
    }

    fn rx_channels(&self) -> usize {
        self.rx_channels.max(1)
    }

    fn start_rx(&mut self, cfg: &RxConfig) -> Result<Box<dyn RxStream>> {
        let h = &self.handle;
        let ch = cfg.channel;
        if !self.info.rx {
            return Err(SdrError::Unsupported("receiving".into()));
        }
        if ch >= self.rx_channels {
            return Err(SdrError::Range(format!(
                "receive channel {ch} doesn't exist; SoapySDR reports {} (0–{})",
                self.rx_channels,
                self.rx_channels - 1
            )));
        }
        let caps = if ch == 0 {
            self.caps.clone()
        } else {
            h.capabilities(SOAPY_SDR_RX, ch)
        };
        caps.check_rate(cfg.sample_rate)?;
        caps.check_frequency(cfg.frequency)?;
        h.configure(
            SOAPY_SDR_RX,
            ch,
            cfg.sample_rate,
            cfg.frequency,
            cfg.bandwidth,
            cfg.antenna.as_deref(),
        )?;
        h.set_ppm(SOAPY_SDR_RX, ch, cfg.ppm)?;
        h.set_gain(SOAPY_SDR_RX, ch, cfg.gain)?;
        let (stream, mtu) = h.open_stream(SOAPY_SDR_RX, ch)?;

        let (producer, rx, error) = RxRing::new(cfg.sample_rate);
        let stop = Arc::new(AtomicBool::new(false));
        let hw_overflows = Arc::new(AtomicU64::new(0));
        let io = RxIo {
            handle: h.clone(),
            stream,
            mtu,
            stop: stop.clone(),
            hw_overflows: hw_overflows.clone(),
            error,
        };
        let thread = std::thread::Builder::new()
            .name("soapy-rx".into())
            .spawn(move || io.run(producer))
            .inspect_err(|_| h.close_stream(stream))?;
        Ok(Box::new(SoapyRx {
            handle: h.clone(),
            ch,
            rx,
            stop,
            hw_overflows,
            mtu,
            thread: Some(thread),
        }))
    }

    /// Never call this against real hardware without authorisation to
    /// transmit; blocks only do after the user armed TX.
    fn start_tx(&mut self, cfg: &TxConfig) -> Result<Box<dyn TxStream>> {
        let h = &self.handle;
        let ch = cfg.channel;
        if !self.info.tx {
            return Err(SdrError::Unsupported("transmitting".into()));
        }
        let caps = h.capabilities(SOAPY_SDR_TX, ch);
        caps.check_rate(cfg.sample_rate)?;
        caps.check_frequency(cfg.frequency)?;
        h.configure(
            SOAPY_SDR_TX,
            ch,
            cfg.sample_rate,
            cfg.frequency,
            cfg.bandwidth,
            cfg.antenna.as_deref(),
        )?;
        h.set_gain(SOAPY_SDR_TX, ch, Gain::Db(cfg.gain))?;
        let (stream, mtu) = h.open_stream(SOAPY_SDR_TX, ch)?;

        let (producer, consumer) = ring::ring(crate::ring::rx_capacity(cfg.sample_rate));
        let stop = Arc::new(AtomicBool::new(false));
        let underflows = Arc::new(AtomicU64::new(0));
        let io = TxIo {
            handle: h.clone(),
            stream,
            mtu,
            stop: stop.clone(),
            underflows: underflows.clone(),
        };
        let thread = std::thread::Builder::new()
            .name("soapy-tx".into())
            .spawn(move || io.run(consumer))
            .inspect_err(|_| h.close_stream(stream))?;
        Ok(Box::new(SoapyTx {
            handle: h.clone(),
            ch,
            ring: producer,
            stop,
            underflows,
            thread: Some(thread),
        }))
    }
}

/// The receive I/O thread's state.
struct RxIo {
    handle: Arc<Handle>,
    stream: RawPtr<Stream>,
    mtu: usize,
    stop: Arc<AtomicBool>,
    hw_overflows: Arc<AtomicU64>,
    error: Arc<Mutex<Option<String>>>,
}

impl RxIo {
    fn run(self, mut ring: Producer<Complex32>) {
        let api = self.handle.api;
        let dev = self.handle.dev.0;
        let mut buf = vec![Complex32::default(); self.mtu];
        let mut errors = 0;
        while !self.stop.load(Ordering::Acquire) {
            // The ring holds ~1/8 s, so pausing for a retune loses nothing.
            self.handle.yield_to_control(Duration::from_millis(50));
            let ptrs = [buf.as_mut_ptr().cast::<c_void>()];
            let (mut flags, mut time) = (0, 0i64);
            // SAFETY: one channel, `buf` holds `mtu` CF32 samples. Only this
            // thread does stream I/O; control calls may run concurrently.
            let r = unsafe {
                (api.SoapySDRDevice_readStream)(
                    dev,
                    self.stream.0,
                    ptrs.as_ptr(),
                    buf.len(),
                    &mut flags,
                    &mut time,
                    100_000,
                )
            };
            match r {
                n if n > 0 => {
                    errors = 0;
                    ring.push(&buf[..n as usize]);
                }
                0 | SOAPY_SDR_TIMEOUT => {}
                SOAPY_SDR_OVERFLOW => {
                    self.hw_overflows.fetch_add(1, Ordering::Relaxed);
                }
                e => {
                    errors += 1;
                    if errors >= 10 {
                        *self.error.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(format!("SoapySDR readStream failed: {}", err_str(api, e)));
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        self.handle.close_stream(self.stream);
        // Dropping `ring` closes it.
    }
}

pub struct SoapyRx {
    handle: Arc<Handle>,
    ch: usize,
    rx: RxRing,
    stop: Arc<AtomicBool>,
    hw_overflows: Arc<AtomicU64>,
    mtu: usize,
    thread: Option<JoinHandle<()>>,
}

impl SoapyRx {
    /// Overflow events the device reported (each loses an unknown number of
    /// samples).
    pub fn device_overflows(&self) -> u64 {
        self.hw_overflows.load(Ordering::Relaxed)
    }
}

impl RxStream for SoapyRx {
    fn read(&mut self, out: &mut [Complex32]) -> Result<usize> {
        self.rx.read(out)
    }

    fn set_waker(&mut self, thread: Thread) {
        self.rx.set_waker(thread);
    }

    fn set_frequency(&mut self, freq: f64) -> Result<()> {
        self.handle.set_frequency(SOAPY_SDR_RX, self.ch, freq)
    }

    fn set_gain(&mut self, gain: Gain) -> Result<()> {
        self.handle.set_gain(SOAPY_SDR_RX, self.ch, gain)
    }

    fn set_ppm(&mut self, ppm: f64) -> Result<()> {
        self.handle.set_ppm(SOAPY_SDR_RX, self.ch, ppm)
    }

    /// Samples dropped at the ring, plus an estimate of one MTU per overflow
    /// the device reported.
    fn overflows(&self) -> u64 {
        self.rx.dropped() + self.device_overflows() * self.mtu as u64
    }

    fn finished(&self) -> bool {
        self.rx.finished()
    }
}

impl Drop for SoapyRx {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            // At most one readStream timeout (100 ms).
            let _ = t.join();
        }
    }
}

/// The transmit I/O thread's state.
struct TxIo {
    handle: Arc<Handle>,
    stream: RawPtr<Stream>,
    mtu: usize,
    stop: Arc<AtomicBool>,
    underflows: Arc<AtomicU64>,
}

impl TxIo {
    fn run(self, mut ring: ring::Consumer<Complex32>) {
        let api = self.handle.api;
        let dev = self.handle.dev.0;
        ring.set_waker(std::thread::current());
        let mut buf = vec![Complex32::default(); self.mtu];
        let mut status = true;
        while !self.stop.load(Ordering::Acquire) {
            self.handle.yield_to_control(Duration::from_millis(50));
            let n = ring.pop(&mut buf);
            if n == 0 {
                std::thread::park_timeout(Duration::from_millis(20));
            }
            let mut sent = 0;
            while sent < n && !self.stop.load(Ordering::Acquire) {
                let ptrs = [buf[sent..].as_ptr().cast::<c_void>()];
                let mut flags = 0;
                // SAFETY: one channel, `n - sent` valid samples at the pointer.
                let r = unsafe {
                    (api.SoapySDRDevice_writeStream)(
                        dev,
                        self.stream.0,
                        ptrs.as_ptr(),
                        n - sent,
                        &mut flags,
                        0,
                        100_000,
                    )
                };
                if r > 0 {
                    sent += r as usize;
                } else if r != SOAPY_SDR_TIMEOUT {
                    break;
                }
            }
            if status {
                let (mut mask, mut flags, mut time) = (0usize, 0, 0i64);
                // SAFETY: valid out pointers; zero timeout polls.
                let r = unsafe {
                    (api.SoapySDRDevice_readStreamStatus)(
                        dev,
                        self.stream.0,
                        &mut mask,
                        &mut flags,
                        &mut time,
                        0,
                    )
                };
                match r {
                    SOAPY_SDR_UNDERFLOW => {
                        self.underflows.fetch_add(1, Ordering::Relaxed);
                    }
                    SOAPY_SDR_NOT_SUPPORTED => status = false,
                    _ => {}
                }
            }
        }
        self.handle.close_stream(self.stream);
    }
}

pub struct SoapyTx {
    handle: Arc<Handle>,
    ch: usize,
    ring: Producer<Complex32>,
    stop: Arc<AtomicBool>,
    underflows: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl TxStream for SoapyTx {
    fn write(&mut self, samples: &[Complex32]) -> Result<usize> {
        Ok(self.ring.push_partial(samples))
    }

    fn set_waker(&mut self, thread: Thread) {
        self.ring.set_waker(thread);
    }

    fn set_frequency(&mut self, freq: f64) -> Result<()> {
        self.handle.set_frequency(SOAPY_SDR_TX, self.ch, freq)
    }

    fn set_gain(&mut self, gain: f64) -> Result<()> {
        self.handle.set_gain(SOAPY_SDR_TX, self.ch, Gain::Db(gain))
    }

    /// Underflow events the device reported (not samples; SoapySDR doesn't
    /// say how many).
    fn underflows(&self) -> u64 {
        self.underflows.load(Ordering::Relaxed)
    }
}

impl Drop for SoapyTx {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_layouts_match_the_header() {
        if size_of::<usize>() == 8 {
            assert_eq!(size_of::<Kwargs>(), 24);
            assert_eq!(size_of::<SoapyRange>(), 24);
        }
    }

    #[test]
    fn missing_library_is_reported() {
        let d = SoapyDriver::with_sonames(&["/nonexistent/libSoapySDR.so.0.8"]);
        assert_eq!(d.discover().unwrap_err().to_string(), MISSING);
        let e = d
            .open(&DeviceArgs::parse("soapy:driver=x").unwrap())
            .err()
            .unwrap();
        assert_eq!(e.to_string(), MISSING);
    }

    #[test]
    fn kwargs_round_trip_through_the_library() {
        let Ok(api) = SoapyDriver::new().api() else {
            return; // SoapySDR not installed
        };
        let pairs = vec![
            ("driver".to_string(), "plutosdr".to_string()),
            ("uri".to_string(), "ip:10.0.0.1".to_string()),
        ];
        let k = OwnedKwargs::new(api, &pairs).unwrap();
        // SAFETY: built by the library.
        assert_eq!(unsafe { read_kwargs(&k.kw) }, pairs);
    }

    #[test]
    fn discovery_is_bounded_and_lists_hints() {
        let d = SoapyDriver::with_hints(vec!["driver=nosuchdriver,uri=ip:192.0.2.1".into()]);
        if d.api().is_err() {
            return;
        }
        let t = std::time::Instant::now();
        // Tests run in parallel; another scan may be running.
        let r = d.discover();
        assert!(t.elapsed() < SCAN_TIMEOUT + Duration::from_millis(500));
        if let Ok(found) = r {
            let hint = found
                .iter()
                .find(|i| i.args.contains("nosuchdriver"))
                .unwrap();
            assert_eq!(hint.args, "soapy:driver=nosuchdriver,uri=ip:192.0.2.1");
            assert!(hint.label.contains("not verified"), "{}", hint.label);
        }
    }

    #[test]
    fn opening_a_nonexistent_device_fails_cleanly() {
        let d = SoapyDriver::new();
        if d.api().is_err() {
            return;
        }
        let e = d
            .open(&DeviceArgs::parse("soapy:driver=nosuchdriver").unwrap())
            .err()
            .unwrap();
        assert!(e.to_string().contains("SoapySDR could not open"), "{e}");
    }

    #[test]
    fn labels_from_kwargs() {
        let p = |v: &[(&str, &str)]| -> Pairs {
            v.iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect()
        };
        let i = info_for(
            &p(&[
                ("driver", "hackrf"),
                ("label", "HackRF One #0 1234"),
                ("serial", "1234"),
            ]),
            true,
        );
        assert_eq!(i.args, "soapy:driver=hackrf,serial=1234");
        assert_eq!(i.label, "HackRF One #0 1234");
        assert_eq!(i.serial.as_deref(), Some("1234"));
        let i = info_for(&p(&[("driver", "plutosdr"), ("uri", "ip:10.0.0.1")]), false);
        assert_eq!(i.label, "plutosdr ip:10.0.0.1 (configured, not verified)");
    }
}
