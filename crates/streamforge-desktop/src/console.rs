//! The console of a window program on Windows (ADR 0020).
//!
//! `streamforge-desktop.exe` uses the Windows subsystem, so Windows opens
//! no console window for it. Started from a console without redirected
//! output, it has no standard output or error at first:
//! [`ParentConsole::borrow`] attaches it to the console of the program that
//! started it, so `--version`, `--help` and startup errors print there, and
//! [`ParentConsole::give_back`] detaches it before the window opens, so
//! Ctrl+C at that console reaches the shell, not the desktop (which closes
//! with its window). A standard handle redirected to a pipe or a file (as
//! in the tests) keeps going there. On other systems both do nothing.

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
    /// error was not redirected to a pipe or a file).
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
/// provides, given which ones are redirected to a pipe or a file: all the
/// others. With both output and error redirected, no console is needed.
#[cfg(any(windows, test))]
fn console_handles(redirected: [bool; 3]) -> Option<[bool; 3]> {
    let [_, output, error] = redirected;
    (!output || !error).then(|| redirected.map(|redirected| !redirected))
}

#[cfg(windows)]
mod imp {
    use std::ptr;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_DISK, FILE_TYPE_PIPE,
        GetFileType, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, FreeConsole, GetStdHandle, STD_ERROR_HANDLE,
        STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };

    const IDS: [STD_HANDLE; 3] = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
    const DEVICES: [&str; 3] = ["CONIN$", "CONOUT$", "CONOUT$"];

    /// The console handles this opened as standard handles (null where a
    /// handle was redirected, or the console could not be opened).
    pub struct Attached {
        opened: [HANDLE; 3],
    }

    /// Attaches to the parent's console when standard output or standard
    /// error is not redirected, and makes the console every standard
    /// handle that is not; redirected ones are kept.
    pub fn attach() -> Option<Attached> {
        // SAFETY: these calls take plain values (and CreateFileW a
        // NUL-terminated name that outlives the call) and act on this
        // process's own standard handles and console. A handle put back
        // with SetStdHandle is the one the process had a moment before; a
        // handle that is not open makes GetFileType report
        // FILE_TYPE_UNKNOWN.
        unsafe {
            let before = IDS.map(|id| GetStdHandle(id));
            let from_console = super::console_handles(before.map(|handle| redirected(handle)))?;
            // Fails when there is no parent console, or this process
            // already has one.
            if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
                return None;
            }
            let mut opened = [ptr::null_mut(); 3];
            for index in 0..IDS.len() {
                if from_console[index] {
                    // Opened here rather than relying on AttachConsole to
                    // replace the handle, which it does not do for every
                    // value a parent may have passed.
                    let handle = open(DEVICES[index]);
                    if handle != INVALID_HANDLE_VALUE {
                        opened[index] = handle;
                    }
                    SetStdHandle(IDS[index], opened[index]);
                } else {
                    // AttachConsole may have replaced it.
                    SetStdHandle(IDS[index], before[index]);
                }
            }
            Some(Attached { opened })
        }
    }

    /// Detaches from the console `attach` attached to.
    pub fn detach(attached: Attached) {
        // SAFETY: as in `attach`. The handles closed here are the ones
        // `attach` opened, cleared from the standard handles first, so no
        // later write reaches whatever reuses those values (a write to no
        // handle is ignored).
        unsafe {
            for (id, handle) in IDS.into_iter().zip(attached.opened) {
                if !handle.is_null() {
                    SetStdHandle(id, ptr::null_mut());
                    CloseHandle(handle);
                }
            }
            FreeConsole();
        }
    }

    /// Whether a standard handle is redirected: open, and a pipe or a
    /// file (not a console or another character device).
    ///
    /// # Safety
    ///
    /// Any handle value may be passed; GetFileType only inspects it.
    unsafe fn redirected(handle: HANDLE) -> bool {
        !handle.is_null()
            && handle != INVALID_HANDLE_VALUE
            && matches!(
                unsafe { GetFileType(handle) },
                FILE_TYPE_DISK | FILE_TYPE_PIPE
            )
    }

    /// Opens the attached console's input or screen buffer.
    ///
    /// # Safety
    ///
    /// The returned handle (unless INVALID_HANDLE_VALUE) is owned by the
    /// caller, which must close it once.
    unsafe fn open(device: &str) -> HANDLE {
        let name: Vec<u16> = device.encode_utf16().chain([0]).collect();
        unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            )
        }
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
    fn a_console_provides_every_handle_that_is_not_redirected() {
        // [input, output, error] redirected to a pipe or a file
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

    /// The test harness's output is redirected (cargo's pipe), or it is a
    /// console program already attached to a console (run in a terminal),
    /// so borrowing changes nothing either way.
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
