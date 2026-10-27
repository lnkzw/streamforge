//! Resources the engine process holds, and the host's load, read from
//! Linux's `/proc`. Other systems report nothing yet: the soak test runs on
//! Linux for 0.1.0 (Windows and macOS follow with their own readings).

use serde::Serialize;

/// One reading of the engine process.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ProcessReading {
    /// Resident memory.
    pub rss_bytes: u64,
    /// User plus system CPU time of all its threads so far.
    pub cpu_s: f64,
    pub threads: u64,
    /// Open file descriptors (files, sockets, pipes, DMA-BUFs).
    pub fds: u64,
}

/// The process `pid` now; `None` where not measured or once it is gone.
pub fn read(pid: u32) -> Option<ProcessReading> {
    imp::read(pid)
}

/// The host's 1-minute load average.
pub fn load_average() -> Option<f64> {
    imp::load_average()
}

#[cfg(target_os = "linux")]
mod imp {
    use super::ProcessReading;

    pub fn read(pid: u32) -> Option<ProcessReading> {
        let dir = format!("/proc/{pid}");
        let status = std::fs::read_to_string(format!("{dir}/status")).ok()?;
        let stat = std::fs::read_to_string(format!("{dir}/stat")).ok()?;
        let fds = std::fs::read_dir(format!("{dir}/fd")).ok()?.count();
        let (rss_bytes, threads) = super::parse_status(&status)?;
        let ticks = super::parse_cpu_ticks(&stat)?;
        Some(ProcessReading {
            rss_bytes,
            cpu_s: ticks as f64 / clock_ticks_per_second(),
            threads,
            fds: fds as u64,
        })
    }

    pub fn load_average() -> Option<f64> {
        let text = std::fs::read_to_string("/proc/loadavg").ok()?;
        text.split_whitespace().next()?.parse().ok()
    }

    fn clock_ticks_per_second() -> f64 {
        // SAFETY: `sysconf` only reads a configuration value; it takes no
        // pointers and has no preconditions.
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        // 100 is the usual value; -1 means it cannot be read.
        if ticks > 0 { ticks as f64 } else { 100.0 }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::ProcessReading;

    pub fn read(_pid: u32) -> Option<ProcessReading> {
        None
    }

    pub fn load_average() -> Option<f64> {
        None
    }
}

/// `VmRSS` (in bytes) and `Threads` from `/proc/<pid>/status`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_status(status: &str) -> Option<(u64, u64)> {
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
    };
    // VmRSS is in kB (KiB).
    Some((field("VmRSS:")?.checked_mul(1024)?, field("Threads:")?))
}

/// User plus system time, in clock ticks, from `/proc/<pid>/stat`. The
/// command name (field 2) is in parentheses and may contain spaces and
/// parentheses itself, so the fields are counted after its last `)`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_cpu_ticks(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    // Field 3 (state) comes first; utime and stime are fields 14 and 15.
    let utime: u64 = fields.nth(11)?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    utime.checked_add(stime)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_gives_resident_memory_and_threads() {
        let status = "Name:\tstreamforge-en\nVmPeak:\t 900000 kB\nVmRSS:\t  412300 kB\n\
                      RssAnon:\t 300000 kB\nThreads:\t48\n";
        assert_eq!(parse_status(status), Some((412_300 * 1024, 48)));
        assert_eq!(parse_status("Name:\tx\nThreads:\t3\n"), None);
    }

    #[test]
    fn cpu_ticks_are_counted_after_the_command_name() {
        // A name with a space and a parenthesis must not shift the fields.
        let stat = "4242 (stream (forge) x) S 1 4242 4242 0 -1 4194560 100 0 0 0 \
                    700 55 0 0 20 0 48 0 1234 900000000 103075";
        assert_eq!(parse_cpu_ticks(stat), Some(755));
        assert_eq!(parse_cpu_ticks("4242 (x) S 1"), None);
    }
}
