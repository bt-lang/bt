//! Desktop app console toggles.

/// Initialize the desktop app console from `app.json`.
pub fn configure_app_console(show: bool) {
    #[cfg(windows)]
    platform::configure_app_console(show);

    #[cfg(not(windows))]
    let _ = show;
}

#[cfg(windows)]
mod platform {
    use std::fs::OpenOptions;
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::{
        AllocConsole, AttachConsole, FreeConsole, GetConsoleWindow, SetConsoleCP,
        SetConsoleOutputCP, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
        STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE, SW_SHOW};

    const CP_UTF8: u32 = 65001;

    /// Show or hide the console based on the app runtime configuration.
    pub fn configure_app_console(show: bool) {
        if show {
            show_console();
        } else {
            disable_console();
        }
    }

    /// Show or create the console and set UTF-8 encoding for `echo()` debug output.
    fn show_console() {
        unsafe {
            if GetConsoleWindow().is_null() {
                let _ = AttachConsole(ATTACH_PARENT_PROCESS);
            }
            if GetConsoleWindow().is_null() {
                let _ = AllocConsole();
            }
            let _ = SetConsoleCP(CP_UTF8);
            let _ = SetConsoleOutputCP(CP_UTF8);
            bind_console_stdio();
            let hwnd = GetConsoleWindow();
            if !hwnd.is_null() {
                let _ = ShowWindow(hwnd, SW_SHOW);
            }
        }
    }

    /// Rebind Rust standard I/O to the current console.
    ///
    /// GUI-subsystem child processes or processes restarted from a console-less bootstrap page may inherit empty standard handles; even after `AllocConsole()` opens a window, `println!` and BT `echo()` still have nowhere writable to go.
    /// Explicitly binding `CONOUT$` / `CONIN$` sends the startup summary and later bridge-call logs to the new console.
    fn bind_console_stdio() {
        bind_console_handle(STD_INPUT_HANDLE, "CONIN$", false);
        bind_console_handle(STD_OUTPUT_HANDLE, "CONOUT$", true);
        bind_console_handle(STD_ERROR_HANDLE, "CONOUT$", true);
    }

    /// Open a console device and attach it to the specified standard handle.
    fn bind_console_handle(std_handle: u32, device: &str, write: bool) {
        let file = if write {
            OpenOptions::new().write(true).open(device)
        } else {
            OpenOptions::new().read(true).open(device)
        };
        if let Ok(file) = file {
            let handle = file.into_raw_handle();
            unsafe {
                let _ = SetStdHandle(std_handle, handle);
            }
        }
    }

    /// Close the desktop app console so packaged GUI builds do not flash a black window.
    fn disable_console() {
        unsafe {
            let hwnd = GetConsoleWindow();
            if !hwnd.is_null() {
                let _ = ShowWindow(hwnd, SW_HIDE);
                let _ = FreeConsole();
            }
        }
    }
}
