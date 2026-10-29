//! The console of a window program on Windows (ADR 0020).
//!
//! `streamforge-desktop.exe` uses the Windows subsystem, so Windows opens
//! no console window for it. Started from a console without redirected
//! output, it has no standard output at first: [`ParentConsole::borrow`]
//! attaches it to the console of the program that started it, so
//! `--version`, `--help` and startup errors print there, and
//! [`ParentConsole::give_back`] detaches it before the window opens, so
//! Ctrl+C at that console reaches the shell, not the desktop (which closes
//! with its window). With output already going somewhere (a pipe or a
//! file, as in the tests), and on other systems, both do nothing.

use std::io::{self, Write};

pub struct ParentConsole {
    attached: bool,
}

impl ParentConsole {
    pub fn borrow() -> Self {
        Self {
            attached: imp::attach(),
        }
    }

    pub fn give_back(self) {
        if self.attached {
            let _ = io::stdout().flush();
            let _ = io::stderr().flush();
            imp::detach();
        }
    }
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_UNKNOWN, GetFileType};
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, FreeConsole, GetStdHandle, STD_ERROR_HANDLE,
        STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };

    /// Attaches to the parent's console when neither standard output nor
    /// standard error leads anywhere; true if it did.
    pub fn attach() -> bool {
        // SAFETY: these calls take plain values and act on this process's
        // own standard handles and console; a handle that is not open makes
        // GetFileType report FILE_TYPE_UNKNOWN.
        unsafe {
            if usable(GetStdHandle(STD_OUTPUT_HANDLE)) || usable(GetStdHandle(STD_ERROR_HANDLE)) {
                return false;
            }
            AttachConsole(ATTACH_PARENT_PROCESS) != 0
        }
    }

    /// Detaches from the console `attach` attached to.
    pub fn detach() {
        // SAFETY: as in `attach`. FreeConsole closes the console handles
        // that AttachConsole made the standard ones; clearing them keeps a
        // later write from reaching whatever reuses those handle values (a
        // write to no handle is ignored).
        unsafe {
            FreeConsole();
            for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                SetStdHandle(id, std::ptr::null_mut());
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
}

#[cfg(not(windows))]
mod imp {
    pub fn attach() -> bool {
        false
    }

    pub fn detach() {}
}
