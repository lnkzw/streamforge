//! Quit signals for programs that own an engine: the first asks the
//! program to finish (stop and finalize its outputs), the second exits at
//! once.
//!
//! Quit signals are SIGINT (Ctrl+C) and SIGTERM on Unix, Ctrl+C and
//! Ctrl+Break on Windows. SIGHUP keeps its default, so a program started
//! with `nohup` still outlives its terminal.

use std::io::{self, Write};

/// Exit status after a second quit signal: 128 + SIGINT, as a shell
/// reports a program ended by Ctrl+C.
pub const EXIT_AT_ONCE: i32 = 130;

/// Install the handler for this process (once). `on_first` runs on a
/// handler thread at the first quit signal and must return quickly (set a
/// flag, send on a channel; do that before anything that may block, such
/// as writing to stderr); at the second, the process exits with
/// [`EXIT_AT_ONCE`] after `program` says so on stderr. Without a handler,
/// as before this call or after it fails, a quit signal ends the process
/// at once.
pub fn on_quit(program: &'static str, on_first: impl FnOnce() + Send + 'static) -> io::Result<()> {
    let mut first = Some(on_first);
    platform::install(move || match first.take() {
        Some(on_first) => on_first(),
        None => {
            // Not `eprintln!`: a closed stderr must not panic the handler.
            let _ = writeln!(
                io::stderr(),
                "{program}: quitting at once; open outputs are not finalized"
            );
            std::process::exit(EXIT_AT_ONCE);
        }
    })
}

#[cfg(unix)]
mod platform {
    use std::io;
    use std::sync::mpsc;
    use std::thread;

    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    /// The signals are taken over on the handler thread, once it runs: if
    /// it cannot start, they keep their defaults (signal-hook does not
    /// restore those when its `Signals` is dropped).
    pub fn install(mut handle: impl FnMut() + Send + 'static) -> io::Result<()> {
        let (installed, outcome) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("quit-signal".to_owned())
            .spawn(move || {
                let mut signals = match Signals::new([SIGINT, SIGTERM]) {
                    Ok(signals) => signals,
                    Err(err) => {
                        let _ = installed.send(Err(err));
                        return;
                    }
                };
                let _ = installed.send(Ok(()));
                for _ in signals.forever() {
                    handle();
                }
            })?;
        outcome
            .recv()
            .unwrap_or_else(|_| Err(io::Error::other("the signal thread ended")))
    }
}

#[cfg(windows)]
mod platform {
    use std::io;

    pub fn install(handle: impl FnMut() + Send + 'static) -> io::Result<()> {
        ctrlc::set_handler(handle).map_err(io::Error::other)
    }
}
