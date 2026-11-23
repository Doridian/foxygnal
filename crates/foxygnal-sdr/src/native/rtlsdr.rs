//! RTL-SDR dongles through librtlsdr (loaded at run time).
//!
//! Device strings: `rtlsdr:` (first dongle), `rtlsdr:serial=00000001`,
//! `rtlsdr:index=1`. Options:
//!
//! - `direct_sampling=1|2`: direct sampling on the I (1) or Q (2) branch, for
//!   HF on dongles wired for it (tuning range becomes 0–28.8 MHz)
//! - `offset_tuning=1`: E4000 offset tuning (avoids the DC spike)
//! - `bias_tee=1`: power an active antenna (librtlsdr ≥ 0.6.0-git / 2.0)
//! - `rtl_agc=1`: the RTL2832's digital AGC (separate from tuner AGC, which
//!   is [`Gain::Auto`](crate::Gain::Auto))
//!
//! Streaming runs `rtlsdr_read_async` on its own thread; the callback converts
//! `u8` IQ to complex floats (`(x − 127.5) / 127.5`) straight into the ring.

use std::ffi::{c_char, c_int, c_uchar, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

use super::{
    RawPtr, RxPump, RxRing, cached, cstr, cu8_to_c32, dl_api, flag, nearest, open_library,
};
use crate::{
    Capabilities, Complex32, Device, DeviceArgs, DeviceInfo, Driver, Gain, Range, Result, RxConfig,
    RxStream, SdrError,
};

const SONAMES: &[&str] = &[
    "librtlsdr.so.0",
    "librtlsdr.so.2",
    "librtlsdr.so",
    "librtlsdr.0.dylib",
    "librtlsdr.dylib",
    "rtlsdr.dll",
    "librtlsdr.dll",
];
const MISSING: &str = "librtlsdr not found (install rtl-sdr)";

/// Opaque `rtlsdr_dev_t`.
#[repr(C)]
pub(crate) struct Dev {
    _p: [u8; 0],
}

type ReadAsyncCb = unsafe extern "C" fn(buf: *mut c_uchar, len: u32, ctx: *mut c_void);

dl_api! {
    /// The parts of `rtl-sdr.h` we use.
    pub(crate) struct Api {
        fn rtlsdr_get_device_count() -> u32;
        fn rtlsdr_get_device_name(u32) -> *const c_char;
        fn rtlsdr_get_device_usb_strings(u32, *mut c_char, *mut c_char, *mut c_char) -> c_int;
        fn rtlsdr_get_index_by_serial(*const c_char) -> c_int;
        fn rtlsdr_open(*mut *mut Dev, u32) -> c_int;
        fn rtlsdr_close(*mut Dev) -> c_int;
        fn rtlsdr_set_center_freq(*mut Dev, u32) -> c_int;
        fn rtlsdr_set_freq_correction(*mut Dev, c_int) -> c_int;
        fn rtlsdr_get_tuner_type(*mut Dev) -> c_int;
        fn rtlsdr_get_tuner_gains(*mut Dev, *mut c_int) -> c_int;
        fn rtlsdr_set_tuner_gain(*mut Dev, c_int) -> c_int;
        fn rtlsdr_set_tuner_bandwidth(*mut Dev, u32) -> c_int;
        fn rtlsdr_set_tuner_gain_mode(*mut Dev, c_int) -> c_int;
        fn rtlsdr_set_sample_rate(*mut Dev, u32) -> c_int;
        fn rtlsdr_get_sample_rate(*mut Dev) -> u32;
        fn rtlsdr_set_agc_mode(*mut Dev, c_int) -> c_int;
        fn rtlsdr_set_direct_sampling(*mut Dev, c_int) -> c_int;
        fn rtlsdr_set_offset_tuning(*mut Dev, c_int) -> c_int;
        fn rtlsdr_reset_buffer(*mut Dev) -> c_int;
        fn rtlsdr_read_async(*mut Dev, ReadAsyncCb, *mut c_void, u32, u32) -> c_int;
        fn rtlsdr_cancel_async(*mut Dev) -> c_int;
    }
    optional {
        // Not in librtlsdr 0.6.0, which some distributions still ship.
        fn rtlsdr_set_bias_tee(*mut Dev, c_int) -> c_int;
    }
}

static DEFAULT_API: OnceLock<&'static Api> = OnceLock::new();

/// USB string buffers for `rtlsdr_get_device_usb_strings` (the header says
/// "may be NULL, otherwise 256 bytes").
const USB_STR: usize = 256;

/// Tuner chips as `enum rtlsdr_tuner`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Tuner {
    Unknown,
    E4000,
    Fc0012,
    Fc0013,
    Fc2580,
    R820t,
    R828d,
}

impl Tuner {
    fn from_c(v: c_int) -> Tuner {
        match v {
            1 => Tuner::E4000,
            2 => Tuner::Fc0012,
            3 => Tuner::Fc0013,
            4 => Tuner::Fc2580,
            5 => Tuner::R820t,
            6 => Tuner::R828d,
            _ => Tuner::Unknown,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Tuner::Unknown => "unknown tuner",
            Tuner::E4000 => "E4000",
            Tuner::Fc0012 => "FC0012",
            Tuner::Fc0013 => "FC0013",
            Tuner::Fc2580 => "FC2580",
            Tuner::R820t => "R820T",
            Tuner::R828d => "R828D",
        }
    }

    /// Tunable ranges (approximate, as commonly documented for each chip).
    fn frequency(self) -> Vec<Range> {
        match self {
            Tuner::E4000 => vec![Range::new(52e6, 1100e6), Range::new(1250e6, 2200e6)],
            Tuner::Fc0012 => vec![Range::new(22e6, 948.6e6)],
            Tuner::Fc0013 => vec![Range::new(22e6, 1100e6)],
            Tuner::Fc2580 => vec![Range::new(146e6, 308e6), Range::new(438e6, 924e6)],
            Tuner::R820t | Tuner::R828d | Tuner::Unknown => vec![Range::new(24e6, 1766e6)],
        }
    }
}

/// Sample rates the RTL2832U accepts (librtlsdr rejects the gap between).
/// Above 2.4 MS/s many dongles drop samples.
fn sample_rates() -> Vec<Range> {
    vec![
        Range::new(225_001.0, 300_000.0),
        Range::new(900_001.0, 3_200_000.0),
    ]
}

/// The gain range from librtlsdr's list (tenths of a dB). `step` is set only
/// when the list is evenly spaced.
fn gain_range(tenths: &[c_int]) -> Option<Range> {
    let (&first, &last) = (tenths.first()?, tenths.last()?);
    let step = tenths.windows(2).map(|w| w[1] - w[0]).collect::<Vec<_>>();
    let even = !step.is_empty() && step.iter().all(|s| *s == step[0]);
    Some(Range {
        min: first as f64 / 10.0,
        max: last as f64 / 10.0,
        step: if even { step[0] as f64 / 10.0 } else { 0.0 },
    })
}

/// The librtlsdr driver.
pub struct RtlSdrDriver {
    sonames: Option<Vec<String>>,
    own: OnceLock<&'static Api>,
}

impl Default for RtlSdrDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl RtlSdrDriver {
    pub fn new() -> Self {
        RtlSdrDriver {
            sonames: None,
            own: OnceLock::new(),
        }
    }

    /// Loads the library from these names instead of the usual ones (tests).
    #[cfg(test)]
    pub(crate) fn with_sonames(names: &[&str]) -> Self {
        RtlSdrDriver {
            sonames: Some(names.iter().map(|s| s.to_string()).collect()),
            own: OnceLock::new(),
        }
    }

    fn api(&self) -> Result<&'static Api> {
        let load = |names: &[String]| {
            open_library(names, MISSING).and_then(|l| Api::resolve(l, "librtlsdr"))
        };
        match &self.sonames {
            None => cached(&DEFAULT_API, || {
                load(&SONAMES.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            }),
            Some(n) => cached(&self.own, || load(n)),
        }
    }
}

/// One attached dongle as librtlsdr lists it.
struct Listed {
    index: u32,
    name: String,
    manufacturer: String,
    product: String,
    serial: String,
}

fn list(api: &Api) -> Vec<Listed> {
    // SAFETY: plain calls with valid buffers of the documented size.
    unsafe {
        let n = (api.rtlsdr_get_device_count)();
        (0..n)
            .map(|index| {
                let mut m = [0 as c_char; USB_STR];
                let mut p = [0 as c_char; USB_STR];
                let mut s = [0 as c_char; USB_STR];
                // Fails (and leaves the buffers empty) if the dongle is busy.
                let _ = (api.rtlsdr_get_device_usb_strings)(
                    index,
                    m.as_mut_ptr(),
                    p.as_mut_ptr(),
                    s.as_mut_ptr(),
                );
                Listed {
                    index,
                    name: cstr((api.rtlsdr_get_device_name)(index)),
                    manufacturer: cstr(m.as_ptr()),
                    product: cstr(p.as_ptr()),
                    serial: cstr(s.as_ptr()),
                }
            })
            .collect()
    }
}

fn info_for(l: &Listed, all: &[Listed]) -> DeviceInfo {
    // Many dongles share the serial 00000001; fall back to the index then.
    let unique = !l.serial.is_empty() && all.iter().filter(|o| o.serial == l.serial).count() == 1;
    let args = if unique {
        DeviceArgs::new("rtlsdr").with("serial", &l.serial)
    } else {
        DeviceArgs::new("rtlsdr").with("index", l.index.to_string())
    };
    let what = if l.product.is_empty() {
        l.name.clone()
    } else {
        format!("{} {}", l.manufacturer, l.product)
            .trim()
            .to_string()
    };
    let label = if l.serial.is_empty() {
        what
    } else {
        format!("{what} ({})", l.serial)
    };
    DeviceInfo {
        driver: "rtlsdr".into(),
        args: args.to_string(),
        label,
        serial: (!l.serial.is_empty()).then(|| l.serial.clone()),
        rx: true,
        tx: false,
    }
}

impl Driver for RtlSdrDriver {
    fn name(&self) -> &'static str {
        "rtlsdr"
    }

    fn label(&self) -> &'static str {
        "RTL-SDR (librtlsdr)"
    }

    fn discover(&self) -> Result<Vec<DeviceInfo>> {
        let api = self.api()?;
        let all = list(api);
        Ok(all.iter().map(|l| info_for(l, &all)).collect())
    }

    fn open(&self, args: &DeviceArgs) -> Result<Box<dyn Device>> {
        let api = self.api()?;
        let all = list(api);
        let index = if let Some(i) = args.get_parsed::<u32>("index")? {
            i
        } else if let Some(serial) = args.get("serial") {
            let c = super::c_string(serial)?;
            // SAFETY: valid C string.
            let i = unsafe { (api.rtlsdr_get_index_by_serial)(c.as_ptr()) };
            if i < 0 {
                return Err(SdrError::NotFound(format!(
                    ": no RTL-SDR with serial `{serial}` (found {})",
                    found_serials(&all)
                )));
            }
            i as u32
        } else {
            0
        };
        let listed = all.iter().find(|l| l.index == index).ok_or_else(|| {
            SdrError::NotFound(if all.is_empty() {
                ": no RTL-SDR attached".into()
            } else {
                format!(": no RTL-SDR at index {index} ({} attached)", all.len())
            })
        })?;
        let info = info_for(listed, &all);

        let direct = match args.get("direct_sampling") {
            None | Some("0") => 0,
            Some("1" | "i" | "I") => 1,
            Some("2" | "q" | "Q") => 2,
            Some(v) => {
                return Err(SdrError::BadArgs(
                    args.to_string(),
                    format!("`direct_sampling={v}` should be 0, 1 (I) or 2 (Q)"),
                ));
            }
        };
        let offset = flag(args, "offset_tuning")?;
        let bias = flag(args, "bias_tee")?;
        let rtl_agc = flag(args, "rtl_agc")?;

        let mut dev: *mut Dev = std::ptr::null_mut();
        // SAFETY: `dev` is a valid out pointer.
        let r = unsafe { (api.rtlsdr_open)(&mut dev, index) };
        if r < 0 || dev.is_null() {
            return Err(SdrError::Device(format!(
                "rtlsdr_open failed ({r}): is the dongle in use, or is the DVB kernel \
                 driver (dvb_usb_rtl28xxu) loaded? Check USB permissions too"
            )));
        }
        let handle = Arc::new(Handle {
            api,
            dev: RawPtr(dev),
            ctl: Mutex::new(()),
            streaming: AtomicBool::new(false),
        });

        // SAFETY: the handle is open; we're the only user so far.
        let (tuner, gains) = unsafe {
            let tuner = Tuner::from_c((api.rtlsdr_get_tuner_type)(dev));
            let n = (api.rtlsdr_get_tuner_gains)(dev, std::ptr::null_mut());
            let mut gains = vec![0 as c_int; n.max(0) as usize];
            if n > 0 {
                (api.rtlsdr_get_tuner_gains)(dev, gains.as_mut_ptr());
            }
            (tuner, gains)
        };
        handle.check(
            // SAFETY: open handle.
            unsafe { (api.rtlsdr_set_agc_mode)(dev, rtl_agc as c_int) },
            "rtlsdr_set_agc_mode",
        )?;
        if direct != 0 {
            // SAFETY: open handle.
            let r = unsafe { (api.rtlsdr_set_direct_sampling)(dev, direct) };
            handle.check(r, "rtlsdr_set_direct_sampling")?;
        }
        if offset {
            // SAFETY: open handle.
            let r = unsafe { (api.rtlsdr_set_offset_tuning)(dev, 1) };
            handle.check(r, "rtlsdr_set_offset_tuning (E4000 only)")?;
        }
        if bias {
            let f = api.rtlsdr_set_bias_tee.ok_or_else(|| {
                SdrError::Unsupported("bias tee (librtlsdr is too old for it)".into())
            })?;
            // SAFETY: open handle.
            handle.check(unsafe { f(dev, 1) }, "rtlsdr_set_bias_tee")?;
        }

        let mut info = info;
        info.label = format!("{} [{}]", info.label, tuner.name());
        Ok(Box::new(RtlSdrDevice {
            handle,
            info,
            tuner,
            gains,
            direct: direct != 0,
        }))
    }
}

fn found_serials(all: &[Listed]) -> String {
    if all.is_empty() {
        return "none attached".into();
    }
    all.iter()
        .map(|l| {
            if l.serial.is_empty() {
                format!("#{} (no serial)", l.index)
            } else {
                format!("#{} `{}`", l.index, l.serial)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// An open `rtlsdr_dev_t`, closed when the last owner drops it.
struct Handle {
    api: &'static Api,
    dev: RawPtr<Dev>,
    /// Serialises control calls (librtlsdr's I²C access isn't reentrant).
    /// The async read loop never takes it.
    ctl: Mutex<()>,
    streaming: AtomicBool,
}

impl Handle {
    fn check(&self, r: c_int, what: &str) -> Result<()> {
        if r < 0 {
            Err(SdrError::Device(format!("{what} failed ({r})")))
        } else {
            Ok(())
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.ctl.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: no stream holds the handle any more (they own an Arc).
        unsafe { (self.api.rtlsdr_close)(self.dev.0) };
    }
}

pub struct RtlSdrDevice {
    handle: Arc<Handle>,
    info: DeviceInfo,
    tuner: Tuner,
    /// Supported tuner gains in tenths of a dB, ascending.
    gains: Vec<c_int>,
    direct: bool,
}

/// Shared by the device and its stream for gain changes.
fn apply_gain(h: &Handle, gains: &[c_int], gain: Gain) -> Result<()> {
    let dev = h.dev.0;
    let _g = h.lock();
    match gain {
        Gain::Auto => {
            // SAFETY: open handle, control lock held.
            h.check(
                unsafe { (h.api.rtlsdr_set_tuner_gain_mode)(dev, 0) },
                "rtlsdr_set_tuner_gain_mode",
            )
        }
        Gain::Db(db) => {
            let g = nearest(gains, db * 10.0).ok_or_else(|| {
                SdrError::Unsupported("manual gain (the tuner reports no gain steps)".into())
            })?;
            // SAFETY: open handle, control lock held.
            unsafe {
                h.check(
                    (h.api.rtlsdr_set_tuner_gain_mode)(dev, 1),
                    "rtlsdr_set_tuner_gain_mode",
                )?;
                h.check(
                    (h.api.rtlsdr_set_tuner_gain)(dev, g),
                    "rtlsdr_set_tuner_gain",
                )
            }
        }
    }
}

fn freq_u32(f: f64) -> Result<u32> {
    if !(0.0..=u32::MAX as f64).contains(&f) {
        return Err(SdrError::Range(format!(
            "frequency {f} Hz is out of range for an RTL-SDR"
        )));
    }
    Ok(f.round() as u32)
}

fn apply_ppm(h: &Handle, ppm: f64) -> Result<()> {
    let _g = h.lock();
    // SAFETY: open handle, control lock held.
    let r = unsafe { (h.api.rtlsdr_set_freq_correction)(h.dev.0, ppm.round() as c_int) };
    // -2 means "already set to this value".
    if r == -2 {
        Ok(())
    } else {
        h.check(r, "rtlsdr_set_freq_correction")
    }
}

impl Device for RtlSdrDevice {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            frequency: if self.direct {
                vec![Range::new(0.0, 28.8e6)]
            } else {
                self.tuner.frequency()
            },
            sample_rate: sample_rates(),
            gain: gain_range(&self.gains),
            agc: true,
            antennas: vec![],
            bandwidth: vec![],
        }
    }

    fn start_rx(&mut self, cfg: &RxConfig) -> Result<Box<dyn RxStream>> {
        let caps = self.capabilities();
        caps.check_rate(cfg.sample_rate)?;
        caps.check_frequency(cfg.frequency)?;
        if cfg.channel != 0 {
            return Err(SdrError::Range(format!(
                "channel {} (an RTL-SDR has only channel 0)",
                cfg.channel
            )));
        }
        if cfg.antenna.is_some() {
            return Err(SdrError::Unsupported("antenna selection".into()));
        }
        if self.handle.streaming.swap(true, Ordering::AcqRel) {
            return Err(SdrError::Device("this RTL-SDR is already streaming".into()));
        }
        let r = self.configure(cfg);
        if r.is_err() {
            self.handle.streaming.store(false, Ordering::Release);
        }
        r?;
        let h = &self.handle;

        let (producer, rx, error) = RxRing::new(cfg.sample_rate);
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = Box::new(Ctx {
            api: h.api,
            dev: h.dev,
            stop: stop.clone(),
            pump: RxPump::new(producer),
        });
        // About 1/60 s per buffer; librtlsdr wants a multiple of 16384 (the URB size).
        let buf_len =
            ((cfg.sample_rate * 2.0 / 60.0 / 16384.0).round() as u32).clamp(1, 32) * 16384;
        let handle = h.clone();
        let thread = std::thread::Builder::new()
            .name("rtlsdr-rx".into())
            .spawn(move || {
                let api = handle.api;
                let ctx = Box::into_raw(ctx);
                // SAFETY: `ctx` stays valid until read_async returns (it's
                // freed below); the callback is the only other user.
                let r = unsafe {
                    (api.rtlsdr_read_async)(handle.dev.0, rx_callback, ctx.cast(), 12, buf_len)
                };
                // SAFETY: read_async returned, so no callback runs any more.
                let ctx = unsafe { Box::from_raw(ctx) };
                if r < 0 && !ctx.stop.load(Ordering::Acquire) {
                    *error.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!(
                        "RTL-SDR stopped streaming (rtlsdr_read_async: {r}); unplugged?"
                    ));
                }
                drop(ctx); // closes the ring
                handle.streaming.store(false, Ordering::Release);
            })
            .inspect_err(|_| h.streaming.store(false, Ordering::Release))?;

        Ok(Box::new(RtlSdrRx {
            handle: self.handle.clone(),
            gains: self.gains.clone(),
            rx,
            stop,
            thread: Some(thread),
        }))
    }
}

impl RtlSdrDevice {
    fn configure(&self, cfg: &RxConfig) -> Result<()> {
        let h = &self.handle;
        let dev = h.dev.0;
        {
            let _g = h.lock();
            // SAFETY: open handle, control lock held, not streaming yet.
            unsafe {
                h.check(
                    (h.api.rtlsdr_set_sample_rate)(dev, cfg.sample_rate.round() as u32),
                    "rtlsdr_set_sample_rate",
                )?;
                h.check(
                    (h.api.rtlsdr_set_center_freq)(dev, freq_u32(cfg.frequency)?),
                    "rtlsdr_set_center_freq",
                )?;
                let bw = cfg.bandwidth.map(|b| b.round() as u32).unwrap_or(0);
                h.check(
                    (h.api.rtlsdr_set_tuner_bandwidth)(dev, bw),
                    "rtlsdr_set_tuner_bandwidth",
                )?;
                let actual = (h.api.rtlsdr_get_sample_rate)(dev);
                if actual == 0 {
                    return Err(SdrError::Device(
                        "the RTL-SDR rejected the sample rate".into(),
                    ));
                }
            }
        }
        apply_ppm(h, cfg.ppm)?;
        apply_gain(h, &self.gains, cfg.gain)?;
        let _g = h.lock();
        // SAFETY: open handle; required before read_async.
        h.check(
            unsafe { (h.api.rtlsdr_reset_buffer)(dev) },
            "rtlsdr_reset_buffer",
        )
    }
}

/// State the read callback owns while streaming.
struct Ctx {
    api: &'static Api,
    dev: RawPtr<Dev>,
    stop: Arc<AtomicBool>,
    pump: RxPump,
}

unsafe extern "C" fn rx_callback(buf: *mut c_uchar, len: u32, ctx: *mut c_void) {
    // SAFETY: `ctx` is the Box<Ctx> handed to read_async, alive until it
    // returns; librtlsdr calls back on the read_async thread only.
    let ctx = unsafe { &mut *ctx.cast::<Ctx>() };
    if ctx.stop.load(Ordering::Acquire) {
        // SAFETY: cancelling from the callback is what rtl_sdr itself does.
        unsafe { (ctx.api.rtlsdr_cancel_async)(ctx.dev.0) };
        return;
    }
    if buf.is_null() {
        return;
    }
    // SAFETY: librtlsdr passes a buffer of `len` valid bytes.
    let bytes = unsafe { std::slice::from_raw_parts(buf, len as usize) };
    ctx.pump.push_with(bytes, cu8_to_c32);
}

pub struct RtlSdrRx {
    handle: Arc<Handle>,
    gains: Vec<c_int>,
    rx: RxRing,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl RxStream for RtlSdrRx {
    fn read(&mut self, out: &mut [Complex32]) -> Result<usize> {
        self.rx.read(out)
    }

    fn set_waker(&mut self, thread: Thread) {
        self.rx.set_waker(thread);
    }

    fn set_frequency(&mut self, freq: f64) -> Result<()> {
        let h = &self.handle;
        let f = freq_u32(freq)?;
        let _g = h.lock();
        // SAFETY: open handle, control lock held; librtlsdr allows retuning
        // while read_async runs.
        h.check(
            unsafe { (h.api.rtlsdr_set_center_freq)(h.dev.0, f) },
            "rtlsdr_set_center_freq",
        )
    }

    fn set_gain(&mut self, gain: Gain) -> Result<()> {
        apply_gain(&self.handle, &self.gains, gain)
    }

    fn set_ppm(&mut self, ppm: f64) -> Result<()> {
        apply_ppm(&self.handle, ppm)
    }

    fn overflows(&self) -> u64 {
        self.rx.dropped()
    }

    fn finished(&self) -> bool {
        self.rx.finished()
    }
}

impl Drop for RtlSdrRx {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let Some(t) = self.thread.take() else { return };
        // cancel_async does nothing if read_async hasn't started yet, so keep
        // asking until the thread ends (the callback also cancels on `stop`).
        let deadline = Instant::now() + Duration::from_secs(3);
        while !t.is_finished() && Instant::now() < deadline {
            // SAFETY: open handle; cancel only sets a flag in librtlsdr.
            unsafe { (self.handle.api.rtlsdr_cancel_async)(self.handle.dev.0) };
            std::thread::sleep(Duration::from_millis(5));
        }
        if t.is_finished() {
            let _ = t.join();
        }
        // Otherwise the thread is stuck in libusb; it owns an Arc of the
        // handle, so the device closes whenever it does finish.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_library_and_discovers_nothing() {
        // librtlsdr is installed here but no dongle is attached.
        let d = RtlSdrDriver::new();
        match d.discover() {
            Ok(list) => assert!(list.iter().all(|i| i.driver == "rtlsdr")),
            Err(e) => assert_eq!(e.to_string(), MISSING),
        }
    }

    #[test]
    fn missing_library_is_reported() {
        let d = RtlSdrDriver::with_sonames(&["/nonexistent/librtlsdr.so.0"]);
        assert_eq!(d.discover().unwrap_err().to_string(), MISSING);
        let e = d
            .open(&DeviceArgs::parse("rtlsdr:").unwrap())
            .err()
            .unwrap();
        assert_eq!(e.to_string(), MISSING);
    }

    #[test]
    fn gain_ranges() {
        // R820T's list from librtlsdr.
        let r820t = [
            0, 9, 14, 27, 37, 77, 87, 125, 144, 157, 166, 197, 207, 229, 254, 280, 297, 328, 338,
            364, 372, 386, 402, 421, 434, 439, 445, 480, 496,
        ];
        let g = gain_range(&r820t).unwrap();
        assert_eq!((g.min, g.max, g.step), (0.0, 49.6, 0.0));
        let g = gain_range(&[0, 20, 40]).unwrap();
        assert_eq!(g.step, 2.0);
        assert_eq!(gain_range(&[]), None);
        assert_eq!(nearest(&r820t, 30.0 * 10.0), Some(297));
    }

    #[test]
    fn device_strings_use_unique_serials() {
        let l = |index, serial: &str| Listed {
            index,
            name: "Generic RTL2832U OEM".into(),
            manufacturer: "Realtek".into(),
            product: "RTL2838UHIDIR".into(),
            serial: serial.into(),
        };
        let all = [l(0, "00000001"), l(1, "00000001"), l(2, "SDR42")];
        assert_eq!(info_for(&all[0], &all).args, "rtlsdr:index=0");
        let i = info_for(&all[2], &all);
        assert_eq!(i.args, "rtlsdr:serial=SDR42");
        assert_eq!(i.label, "Realtek RTL2838UHIDIR (SDR42)");
        assert_eq!(
            DeviceArgs::parse(&i.args).unwrap().get("serial"),
            Some("SDR42")
        );
    }

    #[test]
    fn tuner_ranges() {
        assert_eq!(Tuner::from_c(5), Tuner::R820t);
        assert!(Tuner::E4000.frequency().iter().all(|r| !r.contains(1200e6)));
        let caps = Capabilities {
            sample_rate: sample_rates(),
            ..Default::default()
        };
        assert!(caps.check_rate(2.4e6).is_ok());
        assert!(caps.check_rate(600e3).is_err());
    }
}
