//! The process-wide "TX armed" switch.
//!
//! Transmitting must never happen by accident. Whoever runs a graph (the
//! server for one run the user confirmed, `foxygnal run --arm-tx`) holds a
//! [`TxArm`] for exactly the lifetime of that run; blocks that transmit check
//! [`tx_armed`] in `start()` and refuse otherwise. That is the second line of
//! defence: the server and the CLI already refuse to start a graph with TX
//! blocks unless TX was allowed and armed.
//!
//! The switch is process-wide (a counter of live guards), which matches how
//! foxygnal runs graphs: one session per server, one graph per CLI process.

use std::sync::atomic::{AtomicUsize, Ordering};

static ARMED: AtomicUsize = AtomicUsize::new(0);

/// Transmitting is allowed while this lives. Drop it when the run ends.
#[must_use = "TX is disarmed as soon as the guard is dropped"]
#[derive(Debug)]
pub struct TxArm(());

impl Drop for TxArm {
    fn drop(&mut self) {
        ARMED.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Arm TX until the returned guard is dropped.
pub fn arm_tx() -> TxArm {
    ARMED.fetch_add(1, Ordering::SeqCst);
    TxArm(())
}

/// Whether some run armed TX right now.
pub fn tx_armed() -> bool {
    ARMED.load(Ordering::SeqCst) > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guards_arm_and_disarm() {
        assert!(!tx_armed());
        let a = arm_tx();
        let b = arm_tx();
        assert!(tx_armed());
        drop(a);
        assert!(tx_armed());
        drop(b);
        assert!(!tx_armed());
    }
}
