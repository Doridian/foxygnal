//! The C ABI of the WebAssembly build, for `ui/src/lib/video/mp2.ts`.
//!
//! JavaScript owns one handle per stream. To feed it a PES packet it asks
//! for room (`mp2a_input`), copies the bytes into the module's memory there
//! and commits them (`mp2a_push`). `mp2a_decode` then returns 1 while frames
//! are ready; each stays readable through the accessors until the next
//! `mp2a_decode` or `mp2a_push`. Memory may grow during any call, so views on
//! `memory.buffer` must be made afresh after each one.

use crate::{Header, MAX_PCM, Stream};

pub struct Handle {
    stream: Stream,
    /// The packet being fed, how far, and its PTS.
    input: Vec<u8>,
    at: usize,
    pts: Option<u64>,
    /// The frame out: interleaved PCM and its header and PTS.
    pcm: Vec<f32>,
    frame: Option<(Header, Option<u64>)>,
}

/// # Safety
/// `h` must come from `mp2a_new` and not have been freed.
unsafe fn handle<'a>(h: *mut Handle) -> &'a mut Handle {
    // SAFETY: the caller passes a live handle from `mp2a_new`; wasm is single
    // threaded, so nothing else holds a reference to it during the call.
    unsafe { &mut *h }
}

#[unsafe(no_mangle)]
pub extern "C" fn mp2a_new() -> *mut Handle {
    Box::into_raw(Box::new(Handle {
        stream: Stream::new(),
        input: Vec::new(),
        at: 0,
        pts: None,
        pcm: Vec::with_capacity(MAX_PCM),
        frame: None,
    }))
}

/// # Safety
/// `h` must come from `mp2a_new`; it is invalid afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mp2a_free(h: *mut Handle) {
    if !h.is_null() {
        // SAFETY: made by `Box::into_raw` in `mp2a_new`, freed once.
        drop(unsafe { Box::from_raw(h) });
    }
}

/// Room for `len` bytes of input, to be committed with `mp2a_push`. Input
/// of the previous packet not decoded yet is dropped.
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mp2a_input(h: *mut Handle, len: u32) -> *mut u8 {
    // SAFETY: see `handle`.
    let h = unsafe { handle(h) };
    h.input.resize(len as usize, 0);
    h.at = h.input.len();
    h.input.as_mut_ptr()
}

/// Commit `len` bytes written at `mp2a_input`'s pointer; `pts` < 0: none.
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mp2a_push(h: *mut Handle, len: u32, pts: f64) {
    // SAFETY: see `handle`.
    let h = unsafe { handle(h) };
    h.input.truncate(len as usize);
    h.at = 0;
    h.pts = (pts >= 0.0).then_some(pts as u64);
    h.frame = None;
}

/// 1 if a frame is ready (see the accessors), 0 if more input is needed.
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mp2a_decode(h: *mut Handle) -> i32 {
    // SAFETY: see `handle`.
    let h = unsafe { handle(h) };
    h.frame = None;
    loop {
        if let Some(f) = h.stream.decode() {
            h.pcm.clear();
            h.pcm.extend_from_slice(f.pcm);
            h.frame = Some((f.header, f.pts));
            return 1;
        }
        if h.at >= h.input.len() {
            return 0;
        }
        let pts = if h.at == 0 { h.pts } else { None };
        let n = h.stream.push(&h.input[h.at..], pts);
        if n == 0 {
            // Can't happen: decode() leaves less than a frame and a header.
            h.at = h.input.len();
            return 0;
        }
        h.at += n;
    }
}

/// End of stream: the last frame needn't be followed by another.
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mp2a_flush(h: *mut Handle) {
    // SAFETY: see `handle`.
    unsafe { handle(h) }.stream.flush();
}

/// After a discontinuity: find the frame grid again.
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mp2a_reset(h: *mut Handle) {
    // SAFETY: see `handle`.
    let h = unsafe { handle(h) };
    h.frame = None;
    h.at = h.input.len();
    h.stream.reset();
}

macro_rules! accessor {
    ($name:ident -> $t:ty, $none:expr, |$h:ident, $p:ident| $e:expr) => {
        /// # Safety
        /// `h` must be a live handle.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(h: *mut Handle) -> $t {
            // SAFETY: see `handle`.
            let $h = unsafe { handle(h) };
            match $h.frame {
                Some($p) => $e,
                None => $none,
            }
        }
    };
}

accessor!(mp2a_pcm -> *const f32, std::ptr::null(), |h, _f| h.pcm.as_ptr());
accessor!(mp2a_pcm_len -> u32, 0, |h, _f| h.pcm.len() as u32);
accessor!(mp2a_channels -> u32, 0, |_h, f| f.0.channels() as u32);
accessor!(mp2a_sample_rate -> u32, 0, |_h, f| f.0.sample_rate);
accessor!(mp2a_bitrate -> u32, 0, |_h, f| f.0.bitrate);
accessor!(mp2a_pts -> f64, -1.0, |_h, f| f.1.map_or(-1.0, |v| v as f64));

/// The layer of the last frame header seen (decoded or not), 0 if none: a
/// Layer III stream shows up as 3 and is never decoded.
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mp2a_layer(h: *mut Handle) -> u32 {
    // SAFETY: see `handle`.
    unsafe { handle(h) }
        .stream
        .last_header()
        .map_or(0, |h| u32::from(h.layer))
}

/// Frames that failed their CRC, and times sync was lost.
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mp2a_errors(h: *mut Handle) -> u32 {
    // SAFETY: see `handle`.
    unsafe { handle(h) }.stream.errors()
}
