//! Backing memory for ring buffers.
//!
//! The preferred layout is a *double mapping*: the same physical pages mapped
//! twice, back to back, so byte `i` and byte `i + len` are the same memory.
//! A slice that starts near the end of the ring simply runs on into the second
//! copy, so a ring can hand out every available item as one contiguous slice
//! (GNU Radio's vmcircbuf trick).
//!
//! Where that mapping isn't available (Windows for now, or if the OS refuses),
//! [`Storage`] falls back to one ordinary allocation, and the ring cuts slices
//! at the wrap point instead. The choice is made at run time.

use std::cell::UnsafeCell;

/// Memory a ring stores its items in.
pub(crate) struct Storage {
    ptr: *mut u8,
    /// Size of one copy, in bytes.
    len: usize,
    kind: Kind,
}

enum Kind {
    /// `ptr..ptr + 2 * len` is mapped; the second half mirrors the first.
    #[cfg_attr(not(unix), allow(dead_code))]
    Mirrored,
    /// An ordinary allocation of `len` bytes (u64 words for 8-byte alignment).
    Plain(#[allow(dead_code)] Box<[UnsafeCell<u64>]>),
}

// SAFETY: `Storage` owns its memory exclusively (like a `Box`); concurrent
// access to the bytes is coordinated by the ring's atomic cursors.
unsafe impl Send for Storage {}
unsafe impl Sync for Storage {}

impl Storage {
    /// A plain, single allocation of at least `len` bytes.
    pub(crate) fn plain(len: usize) -> Storage {
        let words: Box<[UnsafeCell<u64>]> = (0..len.div_ceil(8).max(1))
            .map(|_| UnsafeCell::new(0))
            .collect();
        Storage {
            ptr: words.as_ptr() as *mut u8,
            len,
            kind: Kind::Plain(words),
        }
    }

    /// A double mapping of `len` bytes, or `None` if this platform can't make
    /// one. `len` must be a nonzero multiple of [`page_size`].
    pub(crate) fn mirrored(len: usize) -> Option<Storage> {
        if len == 0 || len % page_size() != 0 {
            return None;
        }
        imp::map_mirrored(len).map(|ptr| Storage {
            ptr,
            len,
            kind: Kind::Mirrored,
        })
    }

    /// Start of the memory. Valid for `len` bytes, or `2 * len` if
    /// [`Self::is_mirrored`].
    pub(crate) fn base(&self) -> *mut u8 {
        self.ptr
    }

    pub(crate) fn is_mirrored(&self) -> bool {
        !matches!(self.kind, Kind::Plain(_))
    }
}

impl Drop for Storage {
    fn drop(&mut self) {
        if let Kind::Mirrored = self.kind {
            imp::unmap_mirrored(self.ptr, self.len);
        }
    }
}

/// The virtual memory page size: mirrored storage comes in multiples of it.
pub(crate) fn page_size() -> usize {
    use std::sync::OnceLock;
    static PAGE: OnceLock<usize> = OnceLock::new();
    *PAGE.get_or_init(imp::page_size)
}

#[cfg(unix)]
mod imp {
    use std::ptr;

    pub(super) fn page_size() -> usize {
        // SAFETY: sysconf has no preconditions.
        let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if n > 0 { n as usize } else { 4096 }
    }

    /// An anonymous shared-memory file descriptor of `len` bytes.
    fn shm_fd(len: usize) -> Option<libc::c_int> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        // SAFETY: the name is a valid C string; the flags are valid.
        let fd = unsafe { libc::memfd_create(c"foxygnal-ring".as_ptr(), libc::MFD_CLOEXEC) };
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let fd = {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            // A unique name, unlinked right away: only our mappings keep it.
            let name = format!(
                "/foxygnal-{}-{}\0",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            );
            // SAFETY: `name` is NUL-terminated; the flags and mode are valid.
            unsafe {
                let fd = libc::shm_open(
                    name.as_ptr().cast(),
                    libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                    0o600 as libc::c_uint,
                );
                if fd >= 0 {
                    libc::shm_unlink(name.as_ptr().cast());
                }
                fd
            }
        };
        if fd < 0 {
            return None;
        }
        // SAFETY: `fd` is a valid descriptor we own.
        if unsafe { libc::ftruncate(fd, len as libc::off_t) } != 0 {
            // SAFETY: as above; closed once.
            unsafe { libc::close(fd) };
            return None;
        }
        Some(fd)
    }

    pub(super) fn map_mirrored(len: usize) -> Option<*mut u8> {
        let fd = shm_fd(len)?;
        // SAFETY: reserving fresh address space (PROT_NONE, anonymous) has no
        // preconditions. The two MAP_FIXED mappings replace exactly that
        // reservation, which nobody else can be using: we just created it.
        // On any failure everything mapped so far is unmapped again.
        let base = unsafe {
            let base = libc::mmap(
                ptr::null_mut(),
                2 * len,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            let ok = base != libc::MAP_FAILED && {
                let half = |at: *mut libc::c_void| {
                    libc::mmap(
                        at,
                        len,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_SHARED | libc::MAP_FIXED,
                        fd,
                        0,
                    ) == at
                };
                let ok = half(base) && half(base.cast::<u8>().add(len).cast());
                if !ok {
                    libc::munmap(base, 2 * len);
                }
                ok
            };
            libc::close(fd);
            if !ok {
                return None;
            }
            base.cast::<u8>()
        };
        // Check that the mirror really mirrors before trusting it.
        // SAFETY: both addresses lie inside the 2 * len mapping made above.
        let mirrors = unsafe {
            base.write_volatile(0xA5);
            let seen = base.add(len).read_volatile();
            base.write_volatile(0);
            seen == 0xA5
        };
        if !mirrors {
            unmap_mirrored(base, len);
            return None;
        }
        Some(base)
    }

    pub(super) fn unmap_mirrored(base: *mut u8, len: usize) {
        // SAFETY: `base..base + 2 * len` is a mapping made by `map_mirrored`
        // that nothing references any more.
        unsafe { libc::munmap(base.cast(), 2 * len) };
    }
}

#[cfg(not(unix))]
mod imp {
    // No double mapping yet (Windows would need VirtualAlloc2 placeholders):
    // rings use the plain fallback.
    pub(super) fn page_size() -> usize {
        4096
    }

    pub(super) fn map_mirrored(_len: usize) -> Option<*mut u8> {
        None
    }

    pub(super) fn unmap_mirrored(_base: *mut u8, _len: usize) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn second_half_mirrors_the_first() {
        let len = page_size() * 2;
        // Some containers (CI runners) forbid it; rings then fall back.
        let Some(s) = Storage::mirrored(len) else {
            eprintln!("double mapping isn't available here: skipped");
            return;
        };
        assert!(s.is_mirrored());
        // SAFETY: inside the 2 * len mapping.
        unsafe {
            s.base().add(5).write(42);
            assert_eq!(s.base().add(len + 5).read(), 42);
            s.base().add(2 * len - 1).write(7);
            assert_eq!(s.base().add(len - 1).read(), 7);
        }
    }

    #[test]
    fn mirrored_needs_whole_pages() {
        assert!(Storage::mirrored(page_size() + 1).is_none());
        assert!(Storage::mirrored(0).is_none());
        assert!(!Storage::plain(10).is_mirrored());
    }
}
