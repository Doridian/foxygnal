//! HackRF through libhackrf (loaded at run time).
//!
//! Device strings: `hackrf:` (first board), `hackrf:serial=…` (full serial or
//! a unique suffix, as `hackrf_info` prints it), `hackrf:index=N`. Options:
//!
//! - `amp=1`: the ~11 dB RF amplifier near the antenna port (RX and TX)
//! - `bias_tee=1`: antenna port power (3.3 V, 50 mA)
//!
//! **Gain mapping (RX).** One overall gain in dB, 0–102, is split into the IF
//! ("LNA", 0–40 dB in 8 dB steps) and baseband ("VGA", 0–62 dB in 2 dB steps)
//! stages: the IF stage takes as much as it can first (it comes earlier in the
//! chain, so it sets the noise figure), the rest goes to baseband. E.g. 30 dB
//! → LNA 24 + VGA 6; 70 dB → LNA 40 + VGA 30. [`Gain::Auto`] (the HackRF has no
//! AGC) means LNA 16 + VGA 16, the `hackrf_transfer` defaults. The RF amp is
//! not part of the overall gain; it's the `amp` option.
//!
//! **TX gain** is the TX VGA, 0–47 dB in 1 dB steps.
//!
//! The baseband filter follows the sample rate (`hackrf_compute_baseband_filter_bw`
//! of 0.75 × rate, like `hackrf_transfer`) unless a bandwidth is given.
//! The HackRF is half duplex: one stream at a time.

use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::Thread;

use super::{
    RawPtr, RxPump, RxRing, c32_to_cs8, cached, cs8_to_c32, cstr, dl_api, flag, open_library,
};
use crate::ring::{self, Consumer, Producer};
use crate::{
    Capabilities, Complex32, Device, DeviceArgs, DeviceInfo, Driver, Gain, Range, Result, RxConfig,
    RxStream, SdrError, TxConfig, TxStream,
};

const SONAMES: &[&str] = &[
    "libhackrf.so.0",
    "libhackrf.so",
    "libhackrf.0.dylib",
    "libhackrf.dylib",
    "hackrf.dll",
    "libhackrf.dll",
];
const MISSING: &str = "libhackrf not found (install hackrf)";

/// Opaque `hackrf_device`.
#[repr(C)]
pub(crate) struct Dev {
    _p: [u8; 0],
}

/// `hackrf_transfer` from hackrf.h.
#[repr(C)]
pub(crate) struct Transfer {
    pub device: *mut Dev,
    /// Interleaved 8-bit I/Q.
    pub buffer: *mut u8,
    pub buffer_length: c_int,
    pub valid_length: c_int,
    pub rx_ctx: *mut c_void,
    pub tx_ctx: *mut c_void,
}

/// `struct hackrf_device_list` from hackrf.h.
#[repr(C)]
pub(crate) struct DeviceList {
    /// Entries may be NULL.
    pub serial_numbers: *mut *mut c_char,
    /// `enum hackrf_usb_board_id *` (an int-sized enum).
    pub usb_board_ids: *mut c_int,
    pub usb_device_index: *mut c_int,
    pub devicecount: c_int,
    pub usb_devices: *mut *mut c_void,
    pub usb_devicecount: c_int,
}

type SampleBlockCb = unsafe extern "C" fn(*mut Transfer) -> c_int;

dl_api! {
    /// The parts of `libhackrf/hackrf.h` we use.
    pub(crate) struct Api {
        fn hackrf_init() -> c_int;
        fn hackrf_error_name(c_int) -> *const c_char;
        fn hackrf_usb_board_id_name(c_int) -> *const c_char;
        fn hackrf_device_list() -> *mut DeviceList;
        fn hackrf_device_list_open(*mut DeviceList, c_int, *mut *mut Dev) -> c_int;
        fn hackrf_device_list_free(*mut DeviceList);
        fn hackrf_open_by_serial(*const c_char, *mut *mut Dev) -> c_int;
        fn hackrf_close(*mut Dev) -> c_int;
        fn hackrf_start_rx(*mut Dev, SampleBlockCb, *mut c_void) -> c_int;
        fn hackrf_stop_rx(*mut Dev) -> c_int;
        fn hackrf_start_tx(*mut Dev, SampleBlockCb, *mut c_void) -> c_int;
        fn hackrf_stop_tx(*mut Dev) -> c_int;
        fn hackrf_set_freq(*mut Dev, u64) -> c_int;
        fn hackrf_set_sample_rate(*mut Dev, f64) -> c_int;
        fn hackrf_set_baseband_filter_bandwidth(*mut Dev, u32) -> c_int;
        fn hackrf_compute_baseband_filter_bw(u32) -> u32;
        fn hackrf_set_amp_enable(*mut Dev, u8) -> c_int;
        fn hackrf_set_lna_gain(*mut Dev, u32) -> c_int;
        fn hackrf_set_vga_gain(*mut Dev, u32) -> c_int;
        fn hackrf_set_txvga_gain(*mut Dev, u32) -> c_int;
        fn hackrf_set_antenna_enable(*mut Dev, u8) -> c_int;
    }
}

static DEFAULT_API: OnceLock<&'static Api> = OnceLock::new();

/// Overall RX gain range: LNA 0–40 + VGA 0–62.
pub const RX_GAIN: Range = Range {
    min: 0.0,
    max: 102.0,
    step: 2.0,
};
/// TX VGA range.
pub const TX_GAIN: Range = Range {
    min: 0.0,
    max: 47.0,
    step: 1.0,
};

/// Splits an overall RX gain into (LNA, VGA) dB; see the module docs.
pub fn split_gain(db: f64) -> (u32, u32) {
    let db = db.clamp(RX_GAIN.min, RX_GAIN.max);
    let lna = ((db / 8.0).floor() * 8.0).min(40.0);
    let vga = (((db - lna) / 2.0).round() * 2.0).clamp(0.0, 62.0);
    (lna as u32, vga as u32)
}

/// MAX2837 baseband filter settings.
const FILTERS: &[f64] = &[
    1.75e6, 2.5e6, 3.5e6, 5e6, 5.5e6, 6e6, 7e6, 8e6, 9e6, 10e6, 12e6, 14e6, 15e6, 20e6, 24e6, 28e6,
];

pub struct HackrfDriver {
    sonames: Option<Vec<String>>,
    own: OnceLock<&'static Api>,
}

impl Default for HackrfDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl HackrfDriver {
    pub fn new() -> Self {
        HackrfDriver {
            sonames: None,
            own: OnceLock::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_sonames(names: &[&str]) -> Self {
        HackrfDriver {
            sonames: Some(names.iter().map(|s| s.to_string()).collect()),
            own: OnceLock::new(),
        }
    }

    /// Loads libhackrf. Initialization happens in [`usb`] before each use.
    fn api(&self) -> Result<&'static Api> {
        let load = |names: &[String]| {
            open_library(names, MISSING).and_then(|l| Api::resolve(l, "libhackrf"))
        };
        match &self.sonames {
            None => cached(&DEFAULT_API, || {
                load(&SONAMES.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            }),
            Some(n) => cached(&self.own, || load(n)),
        }
    }
}

fn err_name(api: &Api, r: c_int) -> String {
    // SAFETY: returns a static string for any value.
    format!("{} ({r})", unsafe { cstr((api.hackrf_error_name)(r)) })
}

struct Listed {
    index: c_int,
    serial: String,
    board: String,
}

/// Takes the shared USB lock and (re)initializes libhackrf under it.
/// `hackrf_init` is a no-op while its libusb context exists; it recreates the
/// context after SoapyHackRF's `hackrf_exit` freed it. We never call
/// `hackrf_exit` ourselves: the process exit cleans up.
fn usb(api: &Api) -> Result<std::sync::MutexGuard<'static, ()>> {
    let g = super::usb_lock(super::USB_LOCK_WAIT)?;
    // SAFETY: no arguments; the USB lock serializes it with init/exit calls
    // from other users of the library.
    let r = unsafe { (api.hackrf_init)() };
    if r != 0 {
        return Err(SdrError::Device(format!(
            "hackrf_init failed: {}",
            err_name(api, r)
        )));
    }
    Ok(g)
}

/// Calls `f` with the device list, freeing it afterwards. The caller holds
/// [`usb`].
fn with_list<T>(api: &Api, f: impl FnOnce(*mut DeviceList, Vec<Listed>) -> T) -> Result<T> {
    // SAFETY: the list is valid until freed below; arrays have `devicecount`
    // entries.
    unsafe {
        let list = (api.hackrf_device_list)();
        if list.is_null() {
            return Err(SdrError::Device("hackrf_device_list failed".into()));
        }
        let l = &*list;
        let n = l.devicecount.max(0) as usize;
        let listed = (0..n)
            .map(|i| Listed {
                index: i as c_int,
                serial: if l.serial_numbers.is_null() {
                    String::new()
                } else {
                    cstr(*l.serial_numbers.add(i))
                },
                board: if l.usb_board_ids.is_null() {
                    "HackRF".into()
                } else {
                    cstr((api.hackrf_usb_board_id_name)(*l.usb_board_ids.add(i)))
                },
            })
            .collect();
        let out = f(list, listed);
        (api.hackrf_device_list_free)(list);
        Ok(out)
    }
}

fn info_for(l: &Listed) -> DeviceInfo {
    let args = if l.serial.is_empty() {
        DeviceArgs::new("hackrf").with("index", l.index.to_string())
    } else {
        DeviceArgs::new("hackrf").with("serial", &l.serial)
    };
    DeviceInfo {
        driver: "hackrf".into(),
        args: args.to_string(),
        label: if l.serial.is_empty() {
            l.board.clone()
        } else {
            // The full serial is 32 hex digits; the tail identifies the board.
            let tail = &l.serial[l.serial.len().saturating_sub(8)..];
            format!("{} (…{tail})", l.board)
        },
        serial: (!l.serial.is_empty()).then(|| l.serial.clone()),
        rx: true,
        tx: true,
    }
}

impl Driver for HackrfDriver {
    fn name(&self) -> &'static str {
        "hackrf"
    }

    fn label(&self) -> &'static str {
        "HackRF (libhackrf)"
    }

    fn discover(&self) -> Result<Vec<DeviceInfo>> {
        let api = self.api()?;
        let _usb = usb(api)?;
        with_list(api, |_, listed| listed.iter().map(info_for).collect())
    }

    fn open(&self, args: &DeviceArgs) -> Result<Box<dyn Device>> {
        let api = self.api()?;
        let amp = flag(args, "amp")?;
        let bias = flag(args, "bias_tee")?;
        let mut dev: *mut Dev = std::ptr::null_mut();
        let usb = usb(api)?;
        let (r, info) = if let Some(i) = args.get_parsed::<c_int>("index")? {
            with_list(api, |list, listed| {
                let Some(l) = listed.iter().find(|l| l.index == i) else {
                    return Err(SdrError::NotFound(format!(
                        ": no HackRF at index {i} ({} attached)",
                        listed.len()
                    )));
                };
                // SAFETY: valid list and out pointer.
                let r = unsafe { (api.hackrf_device_list_open)(list, i, &mut dev) };
                Ok((r, info_for(l)))
            })??
        } else {
            let serial = args.get("serial").filter(|s| !s.is_empty());
            let c = serial.map(super::c_string).transpose()?;
            let listed = with_list(api, |_, l| l)?;
            let found = listed
                .iter()
                .find(|l| serial.is_none_or(|s| l.serial.ends_with(s)))
                .ok_or_else(|| {
                    SdrError::NotFound(match serial {
                        Some(s) => format!(": no HackRF with serial `{s}`"),
                        None => ": no HackRF attached".into(),
                    })
                })?;
            let info = info_for(found);
            // SAFETY: null (first device) or a valid C string; valid out pointer.
            let r = unsafe {
                (api.hackrf_open_by_serial)(
                    c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                    &mut dev,
                )
            };
            (r, info)
        };
        drop(usb);
        if r != 0 || dev.is_null() {
            return Err(SdrError::Device(format!(
                "could not open the HackRF: {} (in use, or check USB permissions)",
                err_name(api, r)
            )));
        }
        let handle = Arc::new(Handle {
            api,
            dev: RawPtr(dev),
            ctl: Mutex::new(()),
            streaming: AtomicBool::new(false),
        });
        {
            let _g = handle.lock();
            // SAFETY: open handle, control lock held.
            unsafe {
                handle.check(
                    (api.hackrf_set_amp_enable)(dev, amp as u8),
                    "hackrf_set_amp_enable",
                )?;
                handle.check(
                    (api.hackrf_set_antenna_enable)(dev, bias as u8),
                    "hackrf_set_antenna_enable",
                )?;
            }
        }
        Ok(Box::new(HackrfDevice { handle, info }))
    }
}

/// An open `hackrf_device`, closed when the last owner drops it.
struct Handle {
    api: &'static Api,
    dev: RawPtr<Dev>,
    /// Serialises control transfers; the streaming callbacks never take it.
    ctl: Mutex<()>,
    /// Half duplex: set while an RX or TX stream runs.
    streaming: AtomicBool,
}

impl Handle {
    fn check(&self, r: c_int, what: &str) -> Result<()> {
        if r != 0 {
            Err(SdrError::Device(format!(
                "{what} failed: {}",
                err_name(self.api, r)
            )))
        } else {
            Ok(())
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.ctl.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_freq(&self, f: f64) -> Result<()> {
        if !(0.0..=7.25e9).contains(&f) {
            return Err(SdrError::Range(format!(
                "frequency {f} Hz is out of the HackRF's range"
            )));
        }
        let _g = self.lock();
        // SAFETY: open handle, control lock held.
        self.check(
            unsafe { (self.api.hackrf_set_freq)(self.dev.0, f.round() as u64) },
            "hackrf_set_freq",
        )
    }

    fn set_rx_gain(&self, gain: Gain) -> Result<()> {
        let (lna, vga) = match gain {
            Gain::Auto => (16, 16),
            Gain::Db(db) => split_gain(db),
        };
        let _g = self.lock();
        // SAFETY: open handle, control lock held.
        unsafe {
            self.check(
                (self.api.hackrf_set_lna_gain)(self.dev.0, lna),
                "hackrf_set_lna_gain",
            )?;
            self.check(
                (self.api.hackrf_set_vga_gain)(self.dev.0, vga),
                "hackrf_set_vga_gain",
            )
        }
    }

    fn set_tx_gain(&self, db: f64) -> Result<()> {
        if !TX_GAIN.contains(db) {
            return Err(SdrError::Range(format!(
                "HackRF TX gain {db} dB is out of range (0–47 dB)"
            )));
        }
        let _g = self.lock();
        // SAFETY: open handle, control lock held.
        self.check(
            unsafe { (self.api.hackrf_set_txvga_gain)(self.dev.0, db.round() as u32) },
            "hackrf_set_txvga_gain",
        )
    }

    /// Sample rate, baseband filter and frequency, before streaming.
    fn setup(&self, rate: f64, bandwidth: Option<f64>, freq: f64) -> Result<()> {
        {
            let _g = self.lock();
            let api = self.api;
            // SAFETY: open handle, control lock held.
            unsafe {
                self.check(
                    (api.hackrf_set_sample_rate)(self.dev.0, rate),
                    "hackrf_set_sample_rate",
                )?;
                let bw = (api.hackrf_compute_baseband_filter_bw)(
                    bandwidth.unwrap_or(rate * 0.75) as u32
                );
                self.check(
                    (api.hackrf_set_baseband_filter_bandwidth)(self.dev.0, bw),
                    "hackrf_set_baseband_filter_bandwidth",
                )?;
            }
        }
        self.set_freq(freq)
    }

    fn claim(&self) -> Result<StreamClaim<'_>> {
        if self.streaming.swap(true, Ordering::AcqRel) {
            return Err(SdrError::Device(
                "this HackRF is already streaming (it is half duplex)".into(),
            ));
        }
        Ok(StreamClaim(self, true))
    }
}

/// Releases the half-duplex claim unless the stream took it over.
struct StreamClaim<'a>(&'a Handle, bool);
impl StreamClaim<'_> {
    fn keep(mut self) {
        self.1 = false;
    }
}
impl Drop for StreamClaim<'_> {
    fn drop(&mut self) {
        if self.1 {
            self.0.streaming.store(false, Ordering::Release);
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // Close regardless if the lock can't be had: leaking is worse.
        let _usb = super::usb_lock(super::USB_LOCK_WAIT).ok();
        // SAFETY: every stream owns an Arc, so none is running.
        unsafe { (self.api.hackrf_close)(self.dev.0) };
    }
}

pub struct HackrfDevice {
    handle: Arc<Handle>,
    info: DeviceInfo,
}

fn capabilities() -> Capabilities {
    Capabilities {
        frequency: vec![Range::new(1e6, 6e9)],
        sample_rate: vec![Range::new(2e6, 20e6)],
        gain: Some(RX_GAIN),
        agc: false,
        antennas: vec![],
        bandwidth: FILTERS.iter().map(|&f| Range::new(f, f)).collect(),
    }
}

impl Device for HackrfDevice {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn capabilities(&self) -> Capabilities {
        capabilities()
    }

    fn start_rx(&mut self, cfg: &RxConfig) -> Result<Box<dyn RxStream>> {
        let caps = capabilities();
        caps.check_rate(cfg.sample_rate)?;
        caps.check_frequency(cfg.frequency)?;
        check_common(cfg.channel, cfg.antenna.as_deref())?;
        if cfg.ppm != 0.0 {
            return Err(SdrError::Unsupported("frequency correction (ppm)".into()));
        }
        let h = &*self.handle;
        let claim = h.claim()?;
        h.setup(cfg.sample_rate, cfg.bandwidth, cfg.frequency)?;
        h.set_rx_gain(cfg.gain)?;

        let (producer, rx, _error) = RxRing::new(cfg.sample_rate);
        let ctx = Box::into_raw(Box::new(RxPump::new(producer)));
        // SAFETY: `ctx` lives until stop_rx returned (see HackrfRx::drop).
        let r = unsafe { (h.api.hackrf_start_rx)(h.dev.0, rx_callback, ctx.cast()) };
        if r != 0 {
            // SAFETY: streaming didn't start, so the callback never runs.
            drop(unsafe { Box::from_raw(ctx) });
            h.check(r, "hackrf_start_rx")?;
        }
        claim.keep();
        Ok(Box::new(HackrfRx {
            handle: self.handle.clone(),
            rx,
            ctx,
        }))
    }

    /// Never call this against real hardware without authorisation to
    /// transmit; blocks only do after the user armed TX.
    fn start_tx(&mut self, cfg: &TxConfig) -> Result<Box<dyn TxStream>> {
        let caps = capabilities();
        caps.check_rate(cfg.sample_rate)?;
        caps.check_frequency(cfg.frequency)?;
        check_common(cfg.channel, cfg.antenna.as_deref())?;
        let h = &*self.handle;
        if !TX_GAIN.contains(cfg.gain) {
            return Err(SdrError::Range(format!(
                "HackRF TX gain {} dB is out of range (0–47 dB)",
                cfg.gain
            )));
        }
        let claim = h.claim()?;
        h.setup(cfg.sample_rate, cfg.bandwidth, cfg.frequency)?;
        h.set_tx_gain(cfg.gain)?;

        let (producer, consumer) = ring::ring(crate::ring::rx_capacity(cfg.sample_rate));
        let underflows = Arc::new(AtomicU64::new(0));
        let ctx = Box::into_raw(Box::new(TxPump::new(consumer, underflows.clone())));
        // SAFETY: `ctx` lives until stop_tx returned (see HackrfTx::drop).
        let r = unsafe { (h.api.hackrf_start_tx)(h.dev.0, tx_callback, ctx.cast()) };
        if r != 0 {
            // SAFETY: streaming didn't start.
            drop(unsafe { Box::from_raw(ctx) });
            h.check(r, "hackrf_start_tx")?;
        }
        claim.keep();
        Ok(Box::new(HackrfTx {
            handle: self.handle.clone(),
            ring: producer,
            underflows,
            ctx,
        }))
    }
}

fn check_common(channel: usize, antenna: Option<&str>) -> Result<()> {
    if channel != 0 {
        return Err(SdrError::Range(format!(
            "channel {channel} (a HackRF has only channel 0)"
        )));
    }
    if antenna.is_some() {
        return Err(SdrError::Unsupported("antenna selection".into()));
    }
    Ok(())
}

unsafe extern "C" fn rx_callback(t: *mut Transfer) -> c_int {
    // SAFETY: libhackrf passes a valid transfer whose rx_ctx is our RxPump,
    // alive until stop_rx returns; callbacks come from one thread.
    let t = unsafe { &*t };
    let pump = unsafe { &mut *t.rx_ctx.cast::<RxPump>() };
    if !t.buffer.is_null() && t.valid_length > 0 {
        // SAFETY: `valid_length` bytes of `buffer` were filled.
        let bytes = unsafe { std::slice::from_raw_parts(t.buffer, t.valid_length as usize) };
        pump.push_with(bytes, cs8_to_c32);
    }
    0
}

/// The TX callback's state: pulls samples from the ring into USB buffers.
pub(crate) struct TxPump {
    ring: Consumer<Complex32>,
    scratch: Box<[Complex32]>,
    underflows: Arc<AtomicU64>,
}

impl TxPump {
    fn new(ring: Consumer<Complex32>, underflows: Arc<AtomicU64>) -> Self {
        TxPump {
            ring,
            scratch: vec![Complex32::default(); 16384].into_boxed_slice(),
            underflows,
        }
    }

    /// Fills `buf` (interleaved i8 IQ) from the ring; what the ring can't
    /// supply is sent as zeros and counted as underflow.
    pub(crate) fn fill(&mut self, buf: &mut [u8]) {
        let mut pos = 0;
        while pos + 2 <= buf.len() {
            let want = ((buf.len() - pos) / 2).min(self.scratch.len());
            let got = self.ring.pop(&mut self.scratch[..want]);
            if got == 0 {
                break;
            }
            pos += 2 * c32_to_cs8(&self.scratch[..got], &mut buf[pos..]);
        }
        if pos < buf.len() {
            buf[pos..].fill(0);
            self.underflows
                .fetch_add(((buf.len() - pos) / 2) as u64, Ordering::Relaxed);
        }
    }
}

unsafe extern "C" fn tx_callback(t: *mut Transfer) -> c_int {
    // SAFETY: as for rx_callback; tx_ctx is our TxPump, `buffer_length`
    // bytes of `buffer` are ours to fill.
    let t = unsafe { &mut *t };
    let pump = unsafe { &mut *t.tx_ctx.cast::<TxPump>() };
    if t.buffer.is_null() || t.buffer_length <= 0 {
        return 0;
    }
    let buf = unsafe { std::slice::from_raw_parts_mut(t.buffer, t.buffer_length as usize) };
    pump.fill(buf);
    t.valid_length = t.buffer_length;
    0
}

pub struct HackrfRx {
    handle: Arc<Handle>,
    rx: RxRing,
    ctx: *mut RxPump,
}

// SAFETY: `ctx` is only touched by libhackrf's thread until stop_rx, then
// freed by us; everything else is Send.
unsafe impl Send for HackrfRx {}

impl RxStream for HackrfRx {
    fn read(&mut self, out: &mut [Complex32]) -> Result<usize> {
        self.rx.read(out)
    }

    fn set_waker(&mut self, thread: Thread) {
        self.rx.set_waker(thread);
    }

    fn set_frequency(&mut self, freq: f64) -> Result<()> {
        self.handle.set_freq(freq)
    }

    fn set_gain(&mut self, gain: Gain) -> Result<()> {
        self.handle.set_rx_gain(gain)
    }

    fn overflows(&self) -> u64 {
        self.rx.dropped()
    }

    fn finished(&self) -> bool {
        self.rx.finished()
    }
}

impl Drop for HackrfRx {
    fn drop(&mut self) {
        let h = &self.handle;
        // SAFETY: stop_rx waits for in-flight transfers, after which the
        // callback no longer runs and `ctx` can be freed.
        unsafe {
            let _g = h.lock();
            (h.api.hackrf_stop_rx)(h.dev.0);
            drop(Box::from_raw(self.ctx));
        }
        h.streaming.store(false, Ordering::Release);
    }
}

pub struct HackrfTx {
    handle: Arc<Handle>,
    ring: Producer<Complex32>,
    underflows: Arc<AtomicU64>,
    ctx: *mut TxPump,
}

// SAFETY: as for HackrfRx.
unsafe impl Send for HackrfTx {}

impl TxStream for HackrfTx {
    fn write(&mut self, samples: &[Complex32]) -> Result<usize> {
        Ok(self.ring.push_partial(samples))
    }

    fn set_waker(&mut self, thread: Thread) {
        self.ring.set_waker(thread);
    }

    fn set_frequency(&mut self, freq: f64) -> Result<()> {
        self.handle.set_freq(freq)
    }

    fn set_gain(&mut self, gain: f64) -> Result<()> {
        self.handle.set_tx_gain(gain)
    }

    fn underflows(&self) -> u64 {
        self.underflows.load(Ordering::Relaxed)
    }
}

impl Drop for HackrfTx {
    fn drop(&mut self) {
        let h = &self.handle;
        // SAFETY: as in HackrfRx::drop.
        unsafe {
            let _g = h.lock();
            (h.api.hackrf_stop_tx)(h.dev.0);
            drop(Box::from_raw(self.ctx));
        }
        h.streaming.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_layouts_match_the_header() {
        // Checked against libhackrf/hackrf.h with a C compiler (x86_64).
        if size_of::<usize>() == 8 {
            assert_eq!(size_of::<Transfer>(), 40);
            assert_eq!(std::mem::offset_of!(Transfer, valid_length), 20);
            assert_eq!(std::mem::offset_of!(Transfer, rx_ctx), 24);
            assert_eq!(size_of::<DeviceList>(), 48);
            assert_eq!(std::mem::offset_of!(DeviceList, devicecount), 24);
            assert_eq!(std::mem::offset_of!(DeviceList, usb_devices), 32);
        }
    }

    #[test]
    fn loads_library_and_discovers_nothing() {
        let d = HackrfDriver::new();
        match d.discover() {
            Ok(list) => assert!(list.iter().all(|i| i.driver == "hackrf" && i.tx)),
            Err(e) => assert_eq!(e.to_string(), MISSING),
        }
    }

    #[test]
    fn missing_library_is_reported() {
        let d = HackrfDriver::with_sonames(&["/nonexistent/libhackrf.so.0"]);
        assert_eq!(d.discover().unwrap_err().to_string(), MISSING);
    }

    #[test]
    fn gain_split() {
        assert_eq!(split_gain(0.0), (0, 0));
        assert_eq!(split_gain(30.0), (24, 6));
        assert_eq!(split_gain(40.0), (40, 0));
        assert_eq!(split_gain(70.0), (40, 30));
        assert_eq!(split_gain(102.0), (40, 62));
        assert_eq!(split_gain(500.0), (40, 62));
        assert_eq!(split_gain(-3.0), (0, 0));
        for db in 0..=102 {
            let (l, v) = split_gain(db as f64);
            assert!(l % 8 == 0 && l <= 40 && v % 2 == 0 && v <= 62);
            assert!(((l + v) as f64 - db as f64).abs() <= 1.0, "{db}: {l}+{v}");
        }
    }

    #[test]
    fn tx_pump_pads_and_counts_underflow() {
        let (mut p, c) = ring::ring::<Complex32>(16);
        let under = Arc::new(AtomicU64::new(0));
        let mut pump = TxPump::new(c, under.clone());
        p.push(&[Complex32::new(1.0, -0.5), Complex32::new(0.0, 0.25)]);
        let mut buf = [0xAAu8; 8];
        pump.fill(&mut buf);
        assert_eq!(buf, [127, (-64i8) as u8, 0, 32, 0, 0, 0, 0]);
        assert_eq!(under.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn labels() {
        let i = info_for(&Listed {
            index: 0,
            serial: "0000000000000000457863c82a4b6e5f".into(),
            board: "HackRF One".into(),
        });
        assert_eq!(i.args, "hackrf:serial=0000000000000000457863c82a4b6e5f");
        assert_eq!(i.label, "HackRF One (…2a4b6e5f)");
    }
}
