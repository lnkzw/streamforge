//! CPU time used by this process so far (user plus system, all threads).
//!
//! The only `unsafe` in this crate: one call into the OS per reading. The
//! invariants are stated at each call.

use std::time::Duration;

/// User plus system time of every thread of this process.
pub fn process_time() -> std::io::Result<Duration> {
    imp::process_time()
}

#[cfg(unix)]
mod imp {
    use std::time::Duration;

    pub fn process_time() -> std::io::Result<Duration> {
        // SAFETY: `getrusage` only writes into the `rusage` we pass; a
        // zeroed `rusage` is a valid value of that plain C struct, and the
        // pointer is valid for the duration of the call.
        let usage = unsafe {
            let mut usage: libc::rusage = std::mem::zeroed();
            if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            usage
        };
        Ok(timeval(usage.ru_utime) + timeval(usage.ru_stime))
    }

    fn timeval(time: libc::timeval) -> Duration {
        let seconds = u64::try_from(time.tv_sec).unwrap_or(0);
        let micros = u32::try_from(time.tv_usec).unwrap_or(0);
        Duration::from_secs(seconds) + Duration::from_micros(u64::from(micros))
    }
}

#[cfg(windows)]
mod imp {
    use std::time::Duration;

    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    pub fn process_time() -> std::io::Result<Duration> {
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut creation, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
        // SAFETY: `GetCurrentProcess` returns a pseudo handle that needs no
        // closing and is always valid for this process; `GetProcessTimes`
        // only writes the four `FILETIME`s, which live for the whole call.
        let ok = unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(filetime(kernel) + filetime(user))
    }

    /// A `FILETIME` duration counts 100 ns intervals.
    fn filetime(time: FILETIME) -> Duration {
        let ticks = (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
        Duration::from_nanos(ticks.saturating_mul(100))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn busy_work_adds_cpu_time() {
        let before = process_time().expect("cpu time");
        let start = Instant::now();
        let mut x = 0u64;
        while start.elapsed().as_millis() < 200 {
            x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
        }
        let used = process_time().expect("cpu time") - before;
        // Spinning for 200 ms costs most of that in CPU time; the bound is
        // loose for busy CI runners.
        assert!(used >= std::time::Duration::from_millis(50), "{used:?}");
    }
}
