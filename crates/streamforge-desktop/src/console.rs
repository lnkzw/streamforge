//! The console of a window program on Windows (ADR 0020).
//!
//! `streamforge-desktop.exe` uses the Windows subsystem, so Windows opens
//! no console window for it. Started from a console without redirected
//! output, it has no standard output or error at first:
//! [`ParentConsole::borrow`] attaches it to the console of the program that
//! started it, so `--version`, `--help` and startup errors print there, and
//! [`ParentConsole::give_back`] detaches it before the window opens, so
//! Ctrl+C at that console reaches the shell, not the desktop (which closes
//! with its window). A standard handle that already leads somewhere (a
//! pipe or a file, as in the tests) keeps going there. On other systems
//! both do nothing.

use std::io::{self, Write};

pub struct ParentConsole {
    attached: Option<imp::Attached>,
}

impl ParentConsole {
    pub fn borrow() -> Self {
        Self {
            attached: imp::attach(),
        }
    }

    /// Whether this attached to a console (only when standard output or
    /// error led nowhere).
    pub fn is_attached(&self) -> bool {
        self.attached.is_some()
    }

    pub fn give_back(self) {
        if let Some(attached) = self.attached {
            let _ = io::stdout().flush();
            let _ = io::stderr().flush();
            imp::detach(attached);
        }
    }
}

/// Which of the standard handles (input, output, error) the console
/// provides: those that led nowhere before. With both output and error
/// leading somewhere, no console is needed.
#[cfg(any(windows, test))]
fn console_handles(usable: [bool; 3]) -> Option<[bool; 3]> {
    let [_, output, error] = usable;
    (!output || !error).then(|| usable.map(|usable| !usable))
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_UNKNOWN, GetFileType};
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, FreeConsole, GetStdHandle, STD_ERROR_HANDLE,
        STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };

    const IDS: [STD_HANDLE; 3] = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];

    /// The standard handles the console provides (see `console_handles`).
    pub struct Attached {
        from_console: [bool; 3],
    }

    /// Attaches to the parent's console when standard output or standard
    /// error leads nowhere; the handles that did lead somewhere are kept.
    pub fn attach() -> Option<Attached> {
        // SAFETY: these calls take plain values and act on this process's
        // own standard handles and console; a handle that is not open makes
        // GetFileType report FILE_TYPE_UNKNOWN. A handle put back with
        // SetStdHandle is the one the process had a moment before.
        unsafe {
            let before = IDS.map(|id| GetStdHandle(id));
            let from_console = super::console_handles(before.map(|handle| usable(handle)))?;
            if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
                return None;
            }
            // AttachConsole may replace every standard handle.
            for ((id, handle), console) in IDS.into_iter().zip(before).zip(from_console) {
                if !console {
                    SetStdHandle(id, handle);
                }
            }
            Some(Attached { from_console })
        }
    }

    /// Detaches from the console `attach` attached to.
    pub fn detach(attached: Attached) {
        // SAFETY: as in `attach`. FreeConsole closes the console handles
        // that became standard ones; clearing them keeps a later write from
        // reaching whatever reuses those handle values (a write to no
        // handle is ignored).
        unsafe {
            FreeConsole();
            for (id, console) in IDS.into_iter().zip(attached.from_console) {
                if console {
                    SetStdHandle(id, std::ptr::null_mut());
                }
            }
        }
    }

    /// Whether a standard handle is open (a console, pipe or file).
    ///
    /// # Safety
    ///
    /// Any handle value may be passed; GetFileType only inspects it.
    unsafe fn usable(handle: HANDLE) -> bool {
        !handle.is_null()
            && handle != INVALID_HANDLE_VALUE
            && unsafe { GetFileType(handle) } != FILE_TYPE_UNKNOWN
    }

    #[cfg(test)]
    pub fn handles() -> [HANDLE; 3] {
        // SAFETY: GetStdHandle only reads this process's standard handles.
        IDS.map(|id| unsafe { GetStdHandle(id) })
    }
}

#[cfg(not(windows))]
mod imp {
    pub struct Attached;

    pub fn attach() -> Option<Attached> {
        None
    }

    pub fn detach(_: Attached) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_console_provides_only_the_handles_that_led_nowhere() {
        // [input, output, error]
        assert_eq!(console_handles([true, true, true]), None);
        assert_eq!(console_handles([false, true, true]), None);
        assert_eq!(
            console_handles([false, false, false]),
            Some([true, true, true])
        );
        // Output redirected to a file: the console prints the errors.
        assert_eq!(
            console_handles([true, true, false]),
            Some([false, false, true])
        );
        assert_eq!(
            console_handles([false, false, true]),
            Some([true, true, false])
        );
    }

    /// The test harness's output leads somewhere (cargo's pipe or the
    /// terminal), so borrowing changes nothing, as for the desktop started
    /// with redirected output.
    #[test]
    fn with_output_and_errors_going_somewhere_nothing_is_attached() {
        #[cfg(windows)]
        let before = imp::handles();
        let console = ParentConsole::borrow();
        assert!(!console.is_attached());
        console.give_back();
        #[cfg(windows)]
        assert_eq!(imp::handles(), before);
    }
}
