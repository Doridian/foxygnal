//! What the operating system knows about this process: CPU time, memory and
//! threads. Linux gives everything (`/proc/self`); other Unixes get CPU time
//! and peak RSS from `getrusage`; elsewhere every field is `None`.

use std::time::Duration;

/// CPU time of the whole process so far, threads that already exited included.
#[derive(Debug, Clone, Copy, Default)]
pub struct CpuTime {
    pub user: Duration,
    pub system: Duration,
}

impl CpuTime {
    pub fn total(&self) -> Duration {
        self.user + self.system
    }
}

#[cfg(unix)]
pub fn cpu_time() -> Option<CpuTime> {
    let ru = rusage()?;
    let tv = |t: libc::timeval| {
        Duration::from_secs(t.tv_sec as u64) + Duration::from_micros(t.tv_usec as u64)
    };
    Some(CpuTime {
        user: tv(ru.ru_utime),
        system: tv(ru.ru_stime),
    })
}

#[cfg(not(unix))]
pub fn cpu_time() -> Option<CpuTime> {
    None
}

#[cfg(unix)]
fn rusage() -> Option<libc::rusage> {
    let mut ru = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage only writes the struct we pass.
    let r = unsafe { libc::getrusage(libc::RUSAGE_SELF, ru.as_mut_ptr()) };
    // SAFETY: zero-initialized and filled in by a successful call.
    (r == 0).then(|| unsafe { ru.assume_init() })
}

/// Memory and threads.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct Memory {
    /// Peak resident set size, bytes.
    pub peak_rss: Option<u64>,
    /// Current resident set size, bytes.
    pub rss: Option<u64>,
    pub threads: Option<u64>,
}

pub fn memory() -> Memory {
    proc_status().unwrap_or_else(|| Memory {
        peak_rss: max_rss(),
        ..Memory::default()
    })
}

#[cfg(target_os = "linux")]
fn proc_status() -> Option<Memory> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let field = |name: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
            .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
    };
    Some(Memory {
        peak_rss: field("VmHWM").map(|kb| kb * 1024),
        rss: field("VmRSS").map(|kb| kb * 1024),
        threads: field("Threads"),
    })
}

#[cfg(not(target_os = "linux"))]
fn proc_status() -> Option<Memory> {
    None
}

#[cfg(unix)]
fn max_rss() -> Option<u64> {
    // ru_maxrss is KiB on Linux and the BSDs, bytes on macOS.
    let scale = if cfg!(target_vendor = "apple") {
        1
    } else {
        1024
    };
    rusage().map(|r| r.ru_maxrss as u64 * scale)
}

#[cfg(not(unix))]
fn max_rss() -> Option<u64> {
    None
}

/// CPU time per thread (by thread name), Linux only.
#[derive(Debug, Clone)]
pub struct ThreadTime {
    pub tid: u64,
    /// The kernel's name for it: at most 15 bytes (block threads are named
    /// `fox:<block id>`, cut off there).
    pub name: String,
    pub cpu: Duration,
}

#[cfg(target_os = "linux")]
pub fn thread_times() -> Vec<ThreadTime> {
    // SAFETY: sysconf has no preconditions.
    let tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let tck = if tck > 0 { tck as u64 } else { 100 };
    let Ok(dir) = std::fs::read_dir("/proc/self/task") else {
        return Vec::new();
    };
    dir.filter_map(|e| {
        let e = e.ok()?;
        let tid = e.file_name().to_str()?.parse().ok()?;
        let stat = std::fs::read_to_string(e.path().join("stat")).ok()?;
        // `tid (name) S …`: the name may contain spaces and parentheses, so
        // split at the last ')'. utime and stime are fields 14 and 15.
        let (head, rest) = stat.rsplit_once(')')?;
        let name = head.split_once('(')?.1.to_owned();
        // `schedstat` has the run time in ns (stat only in clock ticks, 10 ms),
        // but needs CONFIG_SCHED_INFO.
        let ns = std::fs::read_to_string(e.path().join("schedstat"))
            .ok()
            .and_then(|s| s.split_whitespace().next()?.parse::<u64>().ok())
            .or_else(|| {
                let mut f = rest.split_whitespace().skip(11);
                let ticks = f.next()?.parse::<u64>().ok()? + f.next()?.parse::<u64>().ok()?;
                Some(ticks * 1_000_000_000 / tck)
            })?;
        Some(ThreadTime {
            tid,
            name,
            cpu: Duration::from_nanos(ns),
        })
    })
    .collect()
}

#[cfg(not(target_os = "linux"))]
pub fn thread_times() -> Vec<ThreadTime> {
    Vec::new()
}

/// A one-line description of the CPU, e.g. "Raspberry Pi 4 Model B Rev 1.4
/// (Cortex-A72), 4 cores".
pub fn cpu_model() -> String {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let model = cpuinfo_model().unwrap_or_else(|| std::env::consts::ARCH.to_owned());
    format!("{model}, {cores} cores")
}

#[cfg(not(target_os = "linux"))]
fn cpuinfo_model() -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
fn cpuinfo_model() -> Option<String> {
    let info = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    {
        let field = |name: &str| {
            info.lines().find_map(|l| {
                let (k, v) = l.split_once(':')?;
                (k.trim() == name).then(|| v.trim().to_owned())
            })
        };
        // x86: "model name". Raspberry Pi: "Model" (the board), and an ARM
        // "CPU part" number for the core.
        field("model name").or_else(|| {
            let board = field("Model");
            let core = field("CPU part").map(|p| match p.as_str() {
                "0xd03" => "Cortex-A53".to_owned(),
                "0xd08" => "Cortex-A72".to_owned(),
                "0xd0b" => "Cortex-A76".to_owned(),
                _ => format!("ARM part {p}"),
            });
            match (board, core) {
                (Some(b), Some(c)) => Some(format!("{b} ({c})")),
                (b, c) => b.or(c),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_something_on_linux() {
        if cfg!(target_os = "linux") {
            let m = memory();
            assert!(m.rss.unwrap() > 0 && m.peak_rss.unwrap() >= m.rss.unwrap());
            assert!(m.threads.unwrap() >= 1);
            assert!(!thread_times().is_empty());
        }
        if cfg!(unix) {
            assert!(cpu_time().is_some());
        }
        assert!(!cpu_model().is_empty());
    }
}
