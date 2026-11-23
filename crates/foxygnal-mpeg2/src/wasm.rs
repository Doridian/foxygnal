//! The C ABI of the WebAssembly build, for `ui/src/lib/video/`.
//!
//! JavaScript owns one handle per stream. To feed it, it asks for room
//! (`mp2v_input`), copies the bytes into the module's memory there and
//! commits them (`mp2v_push`). `mp2v_decode` then returns 1 while pictures
//! are ready; each stays readable through the accessors until the next
//! `mp2v_decode` or `mp2v_push`. Memory may grow during any call, so views
//! on `memory.buffer` must be made afresh after each one.

use crate::{Decoder, Picture};

pub struct Handle {
    dec: Decoder,
    input: Vec<u8>,
    pic: Option<Picture>,
}

/// # Safety
/// `h` must come from `mp2v_new` and not have been freed.
unsafe fn handle<'a>(h: *mut Handle) -> &'a mut Handle {
    // SAFETY: the caller passes a live handle from `mp2v_new`; wasm is single
    // threaded, so nothing else holds a reference to it during the call.
    unsafe { &mut *h }
}

#[unsafe(no_mangle)]
pub extern "C" fn mp2v_new() -> *mut Handle {
    Box::into_raw(Box::new(Handle {
        dec: Decoder::new(),
        input: Vec::new(),
        pic: None,
    }))
}

/// # Safety
/// `h` must come from `mp2v_new`; it is invalid afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mp2v_free(h: *mut Handle) {
    if !h.is_null() {
        // SAFETY: made by `Box::into_raw` in `mp2v_new`, freed once.
        drop(unsafe { Box::from_raw(h) });
    }
}

/// Room for `len` bytes of input, to be committed with `mp2v_push`.
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mp2v_input(h: *mut Handle, len: u32) -> *mut u8 {
    // SAFETY: see `handle`.
    let h = unsafe { handle(h) };
    h.input.resize(len as usize, 0);
    h.input.as_mut_ptr()
}

/// Commit `len` bytes written at `mp2v_input`'s pointer; `pts` < 0: none.
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mp2v_push(h: *mut Handle, len: u32, pts: f64) {
    // SAFETY: see `handle`.
    let h = unsafe { handle(h) };
    let len = (len as usize).min(h.input.len());
    let pts = (pts >= 0.0).then_some(pts as u64);
    h.pic = None;
    h.dec.push(&h.input[..len], pts);
}

/// 1 if a picture is ready (see the accessors), 0 if more input is needed.
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mp2v_decode(h: *mut Handle) -> i32 {
    // SAFETY: see `handle`.
    let h = unsafe { handle(h) };
    h.pic = h.dec.decode();
    i32::from(h.pic.is_some())
}

/// End of stream: the held picture becomes available to `mp2v_decode`.
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mp2v_flush(h: *mut Handle) {
    // SAFETY: see `handle`.
    unsafe { handle(h) }.dec.flush();
}

/// After a discontinuity: wait for a sequence header and an I picture.
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mp2v_reset(h: *mut Handle) {
    // SAFETY: see `handle`.
    let h = unsafe { handle(h) };
    h.pic = None;
    h.dec.reset();
}

macro_rules! accessor {
    ($name:ident -> $t:ty, $none:expr, |$p:ident| $e:expr) => {
        /// # Safety
        /// `h` must be a live handle.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(h: *mut Handle) -> $t {
            // SAFETY: see `handle`.
            match &unsafe { handle(h) }.pic {
                Some($p) => $e,
                None => $none,
            }
        }
    };
}

accessor!(mp2v_width -> u32, 0, |p| p.width as u32);
accessor!(mp2v_height -> u32, 0, |p| p.height as u32);
accessor!(mp2v_data -> *const u8, std::ptr::null(), |p| p.data.as_ptr());
accessor!(mp2v_data_len -> u32, 0, |p| p.data.len() as u32);
accessor!(mp2v_pts -> f64, -1.0, |p| p.pts.map_or(-1.0, |v| v as f64));
accessor!(mp2v_frame_rate_num -> u32, 0, |p| p.frame_rate.0);
accessor!(mp2v_frame_rate_den -> u32, 1, |p| p.frame_rate.1);
accessor!(mp2v_aspect -> u32, 0, |p| u32::from(p.aspect));
accessor!(mp2v_picture_type -> u32, 0, |p| u32::from(p.picture_type));
accessor!(mp2v_progressive -> u32, 0, |p| u32::from(p.progressive));

/// Slices concealed or pictures dropped so far.
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mp2v_errors(h: *mut Handle) -> u32 {
    // SAFETY: see `handle`.
    unsafe { handle(h) }.dec.errors()
}
