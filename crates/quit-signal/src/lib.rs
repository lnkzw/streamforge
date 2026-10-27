//! Quit signals for programs that own an engine: the first asks the
//! program to finish (stop and finalize its outputs), the second exits at
//! once.
//!
//! Quit signals are SIGINT (Ctrl+C) and SIGTERM on Unix, Ctrl+C and
//! Ctrl+Break on Windows. SIGHUP keeps its default, so a program started
//! with `nohup` still outlives its terminal.

use std::io;

/// Exit status after a second quit signal: 128 + SIGINT, as a shell
/// reports a program ended by Ctrl+C.
pub const EXIT_AT_ONCE: i32 = 130;

/// Install the handler for this process (once). `on_first` runs on a
/// handler thread at the first quit signal and must return quickly (set a
/// flag, send on a channel); at the second, the process exits with
/// [`EXIT_AT_ONCE`] after `program` says so on stderr. Without a handler,
/// as before this call, a quit signal ends the process at once.
pub fn on_quit(program: &'static str, on_first: impl FnOnce() + Send + 'static) -> io::Result<()> {
    let mut first = Some(on_first);
    platform::install(move || match first.take() {
        Some(on_first) => on_first(),
        None => {
            eprintln!("{program}: quitting at once; open outputs are not finalized");
            std::process::exit(EXIT_AT_ONCE);
        }
    })
}

#[cfg(unix)]
mod platform {
    use std::io;
    use std::thread;

    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    pub fn install(mut handle: impl FnMut() + Send + 'static) -> io::Result<()> {
        let mut signals = Signals::new([SIGINT, SIGTERM])?;
        thread::Builder::new()
            .name("quit-signal".to_owned())
            .spawn(move || {
                for _ in signals.forever() {
                    handle();
                }
            })?;
        Ok(())
    }
}

#[cfg(windows)]
mod platform {
    use std::io;

    pub fn install(handle: impl FnMut() + Send + 'static) -> io::Result<()> {
        ctrlc::set_handler(handle).map_err(io::Error::other)
    }
}
