//! Discovery from the UI can race a block opening its device. Native library
//! init (libhackrf's `hackrf_init`, for one) isn't thread-safe, so the first
//! load must happen once, whichever thread gets there first.

use std::sync::Barrier;

#[test]
fn concurrent_first_discovery_is_safe() {
    // Fresh driver sets so each thread can be the first to load a library.
    let sets: Vec<_> = (0..8).map(|_| foxygnal_sdr::default_drivers()).collect();
    let barrier = Barrier::new(sets.len() * 2);
    std::thread::scope(|s| {
        for d in &sets {
            let barrier = &barrier;
            s.spawn(move || {
                barrier.wait();
                d.discover()
            });
            s.spawn(move || {
                barrier.wait();
                foxygnal_sdr::drivers().discover()
            });
        }
    });
}
