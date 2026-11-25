//! Hardware drivers that load their vendor C library at run time.
//!
//! Nothing here links against a C library: each driver `dlopen`s its library
//! (via `libloading`) the first time it is used, trying the usual sonames. A
//! missing library only makes that driver's `discover()` / `open()` fail with
//! a message saying what to install, so `cargo build` never needs C headers.
//!
//! The FFI is hand-written from the installed headers (`SoapySDR/*.h`,
//! `rtl-sdr.h`, `libhackrf/hackrf.h`); struct layouts are checked by size in
//! the tests. All `unsafe` stays inside these modules, behind safe types: a
//! device handle is owned by an `Arc` that closes it on drop, so a stream keeps
//! its device open even when the [`Device`](crate::Device) is dropped first.
//!
//! Libraries are never unloaded once a symbol table was built from them:
//! libusb (pulled in by librtlsdr / libhackrf) and SoapySDR modules start
//! threads, and unloading code under a running thread crashes.

pub mod hackrf;
pub mod pluto;
pub mod rtlsdr;
pub mod soapy;

use std::sync::{Arc, Mutex, OnceLock};

use crate::ring::{self, Consumer, Producer};
use crate::{Complex32, Result, SdrError};

pub use hackrf::HackrfDriver;
pub use pluto::PlutoDriver;
pub use rtlsdr::RtlSdrDriver;
pub use soapy::SoapyDriver;

/// Declares a table of C function pointers resolved from a library.
///
/// `required` symbols must all exist; `optional` ones become `Option`s (for
/// functions newer than what distributions commonly ship).
macro_rules! dl_api {
    (
        $(#[$m:meta])*
        $vis:vis struct $name:ident {
            $( fn $f:ident ( $($a:ty),* $(,)? ) $(-> $r:ty)? ; )*
        }
        $( optional {
            $( fn $of:ident ( $($oa:ty),* $(,)? ) $(-> $or:ty)? ; )*
        } )?
    ) => {
        $(#[$m])*
        #[allow(non_snake_case)]
        $vis struct $name {
            $( pub(crate) $f: unsafe extern "C" fn($($a),*) $(-> $r)?, )*
            $( $( pub(crate) $of: Option<unsafe extern "C" fn($($oa),*) $(-> $or)?>, )* )?
        }

        impl $name {
            /// Resolves every symbol from `lib`. On success the library is
            /// leaked (never unloaded) so the pointers stay valid forever.
            fn resolve(lib: libloading::Library, what: &str) -> std::result::Result<&'static Self, String> {
                // SAFETY: the signatures above are copied from the C headers;
                // the pointers are only used while the library is loaded,
                // which is forever once we return Ok.
                let api = unsafe {
                    Self {
                        $( $f: *lib
                            .get::<unsafe extern "C" fn($($a),*) $(-> $r)?>(
                                concat!(stringify!($f), "\0").as_bytes(),
                            )
                            .map_err(|_| format!(
                                "{what} is too old or not compatible (missing `{}`)",
                                stringify!($f)
                            ))?, )*
                        $( $( $of: lib
                            .get::<unsafe extern "C" fn($($oa),*) $(-> $or)?>(
                                concat!(stringify!($of), "\0").as_bytes(),
                            )
                            .ok()
                            .map(|s| *s), )* )?
                    }
                };
                std::mem::forget(lib);
                Ok(Box::leak(Box::new(api)))
            }
        }
    };
}
pub(crate) use dl_api;

/// Opens the first of `names` that loads. `missing` is the whole error when
/// none exists, e.g. "librtlsdr not found (install rtl-sdr)".
pub(crate) fn open_library<S: AsRef<str>>(
    names: &[S],
    missing: &str,
) -> std::result::Result<libloading::Library, String> {
    let mut other = Vec::new();
    for name in names {
        let name = name.as_ref();
        // SAFETY: loading runs the library's initialisers. These are the
        // vendor libraries named in our soname lists (or a test path).
        match unsafe { libloading::Library::new(name) } {
            Ok(lib) => return Ok(lib),
            Err(e) => {
                // Since libloading 0.9 the error itself only says e.g.
                // "dlopen failed"; the loader's message is its source.
                let e = match std::error::Error::source(&e) {
                    Some(source) => source.to_string(),
                    None => e.to_string(),
                };
                // `name` itself missing just means try the next one; anything
                // else (a missing dependency such as libusb, the wrong
                // architecture) is worth showing. glibc names the file it
                // couldn't find first, so a missing dependency doesn't match.
                let not_found = e.starts_with(&format!("{name}: cannot open shared object file"))
                    || e.contains("(no such file)")
                    || e.contains("image not found")
                    || e.contains("os error 126");
                if !not_found {
                    other.push(e);
                }
            }
        }
    }
    if other.is_empty() {
        Err(missing.to_string())
    } else {
        Err(format!("{missing}: {}", other.join("; ")))
    }
}

/// Serializes calls into libraries that share process-wide USB state.
///
/// SoapySDR loads its own modules, which link the same `libhackrf.so` we
/// dlopen; SoapyHackRF calls `hackrf_init`/`hackrf_exit` around every scan,
/// and `hackrf_exit` frees the libusb context libhackrf keeps in a global.
/// So every libhackrf list/open/close and every SoapySDR enumerate/make/unmake
/// holds this lock, and libhackrf users re-run `hackrf_init` under it.
static USB_LIBS: Mutex<()> = Mutex::new(());

/// How long [`usb_lock`] waits before giving up (a SoapySDR module may be
/// stuck scanning the network while holding it).
pub(crate) const USB_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Takes [`USB_LIBS`], waiting at most `wait`.
pub(crate) fn usb_lock(wait: std::time::Duration) -> Result<std::sync::MutexGuard<'static, ()>> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        match USB_LIBS.try_lock() {
            Ok(g) => return Ok(g),
            Err(std::sync::TryLockError::Poisoned(p)) => return Ok(p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        if std::time::Instant::now() >= deadline {
            return Err(SdrError::Device(
                "another driver is busy with USB (a SoapySDR scan may be stuck); try again".into(),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Returns the cached symbol table, loading it on first use. Failures aren't
/// cached, so installing the library later works without a restart.
pub(crate) fn cached<A: Sync + 'static>(
    cell: &OnceLock<&'static A>,
    load: impl FnOnce() -> std::result::Result<&'static A, String>,
) -> Result<&'static A> {
    if let Some(a) = cell.get() {
        return Ok(a);
    }
    // One load at a time, process-wide: library init functions (hackrf_init's
    // libusb_init into a global context, for one) are not thread-safe, and
    // discovery from the UI can race a block opening its device.
    static LOADING: Mutex<()> = Mutex::new(());
    let _guard = LOADING.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(a) = cell.get() {
        return Ok(a);
    }
    let a = load().map_err(SdrError::Unavailable)?;
    Ok(cell.get_or_init(|| a))
}

/// Converts a C string the library owns into an owned `String` (`""` for null).
///
/// # Safety
/// `p` must be null or point to a NUL-terminated string.
pub(crate) unsafe fn cstr(p: *const std::ffi::c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: the caller promises a valid C string.
    unsafe { std::ffi::CStr::from_ptr(p) }
        .to_string_lossy()
        .into_owned()
}

/// A C string for a call, or a `BadArgs` error if `s` contains a NUL.
pub(crate) fn c_string(s: &str) -> Result<std::ffi::CString> {
    std::ffi::CString::new(s)
        .map_err(|_| SdrError::BadArgs(s.into(), "contains a NUL character".into()))
}

/// A raw device handle that may move between threads. The owner serialises
/// control calls; the vendor libraries allow streaming on another thread.
pub(crate) struct RawPtr<T>(pub *mut T);
impl<T> Clone for RawPtr<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for RawPtr<T> {}
// SAFETY: see above; each driver documents how it uses its handle.
unsafe impl<T> Send for RawPtr<T> {}
unsafe impl<T> Sync for RawPtr<T> {}

/// Unsigned 8-bit IQ (RTL-SDR) to complex float: `(x − 127.5) / 127.5`.
/// Converts `min(src.len() / 2, dst.len())` samples and returns that count.
#[inline]
pub(crate) fn cu8_to_c32(src: &[u8], dst: &mut [Complex32]) -> usize {
    const K: f32 = 1.0 / 127.5;
    let n = (src.len() / 2).min(dst.len());
    for (d, s) in dst[..n].iter_mut().zip(src.as_chunks::<2>().0) {
        *d = Complex32::new((s[0] as f32 - 127.5) * K, (s[1] as f32 - 127.5) * K);
    }
    n
}

/// Signed 8-bit IQ (HackRF) to complex float: `x / 128`.
#[inline]
pub(crate) fn cs8_to_c32(src: &[u8], dst: &mut [Complex32]) -> usize {
    const K: f32 = 1.0 / 128.0;
    let n = (src.len() / 2).min(dst.len());
    for (d, s) in dst[..n].iter_mut().zip(src.as_chunks::<2>().0) {
        *d = Complex32::new((s[0] as i8) as f32 * K, (s[1] as i8) as f32 * K);
    }
    n
}

/// Complex float to signed 8-bit IQ: `round(x · 127)`, clamped to ±127.
#[inline]
pub(crate) fn c32_to_cs8(src: &[Complex32], dst: &mut [u8]) -> usize {
    #[inline]
    fn q(x: f32) -> u8 {
        ((x * 127.0).round().clamp(-127.0, 127.0) as i8) as u8
    }
    let n = src.len().min(dst.len() / 2);
    for (s, d) in src[..n].iter().zip(dst.as_chunks_mut::<2>().0) {
        d[0] = q(s.re);
        d[1] = q(s.im);
    }
    n
}

/// The producer side of a receive stream as used inside a C callback:
/// converts raw bytes into a preallocated scratch buffer and pushes it into
/// the ring in chunks. No allocation after construction.
pub(crate) struct RxPump {
    ring: Producer<Complex32>,
    scratch: Box<[Complex32]>,
}

impl RxPump {
    pub(crate) fn new(ring: Producer<Complex32>) -> Self {
        RxPump {
            ring,
            scratch: vec![Complex32::default(); 16384].into_boxed_slice(),
        }
    }

    /// Converts and pushes `bytes` with `conv` (one of the `*_to_c32`).
    pub(crate) fn push_with(
        &mut self,
        mut bytes: &[u8],
        conv: fn(&[u8], &mut [Complex32]) -> usize,
    ) {
        while bytes.len() >= 2 {
            let n = conv(bytes, &mut self.scratch);
            self.ring.push(&self.scratch[..n]);
            bytes = &bytes[n * 2..];
        }
    }
}

/// The consumer side of a receive stream, shared by the native drivers.
/// Items are samples, or `[Complex32; 2]` for a two-channel stream.
pub(crate) struct RxRing<T = Complex32> {
    ring: Consumer<T>,
    /// Why the driver's thread stopped, if it did on its own.
    error: Arc<Mutex<Option<String>>>,
}

impl<T: Copy + Default + Send> RxRing<T> {
    pub(crate) fn new(rate: f64) -> (Producer<T>, Self, Arc<Mutex<Option<String>>>) {
        let (p, c) = ring::ring(ring::rx_capacity(rate));
        let error = Arc::new(Mutex::new(None));
        (
            p,
            RxRing {
                ring: c,
                error: error.clone(),
            },
            error,
        )
    }

    pub(crate) fn read(&mut self, out: &mut [T]) -> Result<usize> {
        let n = self.ring.pop(out);
        self.ended(n)
    }

    /// `n` items were read: an error once nothing is left of a stream that
    /// ended, else `Ok(n)`.
    fn ended(&self, n: usize) -> Result<usize> {
        if n == 0 && self.ring.finished() {
            let msg = self.error.lock().unwrap_or_else(|e| e.into_inner()).clone();
            return Err(SdrError::Device(
                msg.unwrap_or_else(|| "the device stopped streaming".into()),
            ));
        }
        Ok(n)
    }

    pub(crate) fn set_waker(&self, t: std::thread::Thread) {
        self.ring.set_waker(t);
    }

    pub(crate) fn dropped(&self) -> u64 {
        self.ring.dropped()
    }

    pub(crate) fn finished(&self) -> bool {
        self.ring.finished()
    }
}

impl RxRing<[Complex32; 2]> {
    /// Splits pairs into channel 0 (`a`) and channel 1 (`b`).
    pub(crate) fn read_pair(&mut self, a: &mut [Complex32], b: &mut [Complex32]) -> Result<usize> {
        let max = a.len().min(b.len());
        let n = self.ring.pop_with(max, |i, [x, y]| {
            a[i] = x;
            b[i] = y;
        });
        self.ended(n)
    }
}

/// Picks the value in `options` nearest to `want`.
pub(crate) fn nearest<T: Copy + Into<f64>>(options: &[T], want: f64) -> Option<T> {
    options.iter().copied().min_by(|a, b| {
        let da = ((*a).into() - want).abs();
        let db = ((*b).into() - want).abs();
        da.total_cmp(&db)
    })
}

/// Parses a boolean device-string option (`1`/`0`, `true`/`false`, `on`/`off`).
pub(crate) fn flag(args: &crate::DeviceArgs, key: &str) -> Result<bool> {
    match args.get(key) {
        None => Ok(false),
        Some("" | "1" | "true" | "on" | "yes") => Ok(true),
        Some("0" | "false" | "off" | "no") => Ok(false),
        Some(v) => Err(SdrError::BadArgs(
            args.to_string(),
            format!("`{key}={v}` should be 0 or 1"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_8_bit_samples() {
        let mut out = [Complex32::default(); 4];
        assert_eq!(cu8_to_c32(&[0, 255, 127, 128, 9], &mut out), 2);
        assert_eq!(out[0], Complex32::new(-1.0, 1.0));
        assert!((out[1].re + 0.5 / 127.5).abs() < 1e-7);
        assert!((out[1].im - 0.5 / 127.5).abs() < 1e-7);

        assert_eq!(cs8_to_c32(&[0x80, 0x7f, 0, 0xff], &mut out), 2);
        assert_eq!(out[0], Complex32::new(-1.0, 127.0 / 128.0));
        assert_eq!(out[1], Complex32::new(0.0, -1.0 / 128.0));

        let mut bytes = [0u8; 6];
        let src = [
            Complex32::new(1.0, -1.0),
            Complex32::new(5.0, -5.0),
            Complex32::new(0.5, 0.0),
        ];
        assert_eq!(c32_to_cs8(&src, &mut bytes), 3);
        assert_eq!(bytes, [127, (-127i8) as u8, 127, (-127i8) as u8, 64, 0]);
        // Round trip within one LSB.
        let mut back = [Complex32::default(); 3];
        cs8_to_c32(&bytes, &mut back);
        assert!((back[2].re - 0.5).abs() < 1.0 / 128.0);
    }

    #[test]
    fn pump_converts_in_chunks() {
        let (p, mut rx, _) = RxRing::new(0.0);
        let mut pump = RxPump::new(p);
        let bytes: Vec<u8> = (0..100_000u32).map(|i| (i % 256) as u8).collect();
        pump.push_with(&bytes, cu8_to_c32);
        let mut out = vec![Complex32::default(); 60_000];
        assert_eq!(rx.read(&mut out).unwrap(), 50_000);
        let mut expect = [Complex32::default(); 1];
        cu8_to_c32(&[2, 3], &mut expect);
        assert_eq!(out[1], expect[0]);
        // Samples straddling the scratch-buffer boundary arrive in order.
        let mut expect = [Complex32::default(); 1];
        cu8_to_c32(&bytes[2 * 16384..], &mut expect);
        assert_eq!(out[16384], expect[0]);
        drop(pump);
        let e = rx.read(&mut out).unwrap_err();
        assert!(e.to_string().contains("stopped streaming"), "{e}");
    }

    #[test]
    fn missing_library_message() {
        let e = open_library(
            &["/nonexistent/libnothing.so.0"],
            "libnothing not found (install nothing)",
        )
        .unwrap_err();
        assert_eq!(e, "libnothing not found (install nothing)");
    }

    #[test]
    fn registered_first_and_discovery_never_fails_hard() {
        let d = crate::default_drivers();
        let names: Vec<_> = d.iter().map(|d| d.name()).collect();
        assert_eq!(&names[..4], ["rtlsdr", "hackrf", "pluto", "soapy"]);
        // Each driver either lists devices or says why not.
        assert!(d.discover().len() >= 4);
    }

    #[test]
    fn nearest_gain() {
        let gains = [0, 9, 14, 27, 37, 77, 87];
        assert_eq!(nearest(&gains, 30.0), Some(27));
        assert_eq!(nearest(&gains, 1000.0), Some(87));
        assert_eq!(nearest::<i32>(&[], 3.0), None);
    }
}
