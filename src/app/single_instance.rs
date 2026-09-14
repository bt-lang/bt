//! Opt-in Windows single-instance launch forwarding for packaged desktop apps.

use crate::error::BtError;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::os::windows::ffi::OsStrExt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{Emitter, Manager};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_PIPE_CONNECTED, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE,
    OPEN_EXISTING, PIPE_ACCESS_INBOUND,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::CreateMutexW;

/// Maximum forwarded launch size and pending request count bound IPC memory use.
const MAX_REQUEST_BYTES: usize = 32 * 1024;
const MAX_PENDING_REQUESTS: usize = 64;

/// Owns the process mutex until the Tauri event loop exits.
pub struct InstanceGuard(HANDLE);

impl Drop for InstanceGuard {
    /// Release the named mutex handle when the primary application exits.
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// A small queue bridges requests that arrive before the web page installs its event listener.
#[derive(Clone, Default)]
pub struct OpenRequests(Arc<Mutex<VecDeque<Vec<String>>>>);

impl OpenRequests {
    /// Drain all requests in arrival order for the frontend.
    pub fn take(&self) -> Vec<Vec<String>> {
        let Ok(mut pending) = self.0.lock() else {
            return Vec::new();
        };
        pending.drain(..).collect()
    }

    /// Add one bounded launch request without allowing an unattended process to grow indefinitely.
    fn push(&self, args: Vec<String>) {
        if let Ok(mut pending) = self.0.lock() {
            if pending.len() == MAX_PENDING_REQUESTS {
                pending.pop_front();
            }
            pending.push_back(args);
        }
    }
}

/// Claim the executable-specific mutex or send arguments to the existing process.
///
/// None means the secondary launch was delivered and should exit without creating a window.
pub fn claim_or_forward(args: &[String]) -> Result<Option<(InstanceGuard, String)>, BtError> {
    let executable = std::env::current_exe()?.canonicalize()?;
    let identity = executable.to_string_lossy().to_lowercase();
    let digest = Sha256::digest(identity.as_bytes());
    let suffix = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let mutex_name = wide(&format!(r"Local\BTApp-{suffix}"));
    let pipe_name = format!(r"\\.\pipe\BTApp-{suffix}");
    let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr()) };
    if mutex.is_null() {
        return Err(BtError::Io(std::io::Error::last_os_error()));
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe { CloseHandle(mutex) };
        forward(&pipe_name, args)?;
        return Ok(None);
    }
    Ok(Some((InstanceGuard(mutex), pipe_name)))
}

/// Listen on an inbound, local-only named pipe and wake the existing main window for each launch.
pub fn start_listener(
    pipe_name: String,
    app: tauri::AppHandle,
    pending: OpenRequests,
) -> Result<(), BtError> {
    let name = wide(&pipe_name);
    // The handle is exclusively owned by the spawned listener; transfer its pointer-sized value.
    let first_pipe = create_pipe(&name, true)? as usize;
    std::thread::spawn(move || {
        let mut pipe = first_pipe as HANDLE;
        loop {
            let connected = unsafe { ConnectNamedPipe(pipe, std::ptr::null_mut()) } != 0
                || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
            if connected {
                let mut bytes = [0u8; MAX_REQUEST_BYTES];
                let mut length = 0u32;
                let received = unsafe {
                    ReadFile(
                        pipe,
                        bytes.as_mut_ptr(),
                        bytes.len() as u32,
                        &mut length,
                        std::ptr::null_mut(),
                    )
                } != 0;
                if received {
                    if let Ok(args) =
                        serde_json::from_slice::<Vec<String>>(&bytes[..length as usize])
                    {
                        if args.is_empty() {
                            if let Ok(runtime) =
                                app.state::<crate::app::runtime::AppState>().lock_runtime()
                            {
                                let config = runtime.config.clone();
                                drop(runtime);
                                let _ = crate::app::file_association::register(&config);
                            }
                        }
                        pending.push(args);
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.unminimize();
                            let _ = window.set_focus();
                        }
                        let _ = app.emit("bt://app/open_request", ());
                    }
                }
                unsafe { DisconnectNamedPipe(pipe) };
            }
            unsafe { CloseHandle(pipe) };
            match create_pipe(&name, false) {
                Ok(next) => pipe = next,
                Err(_) => break,
            }
        }
    });
    Ok(())
}

/// Reserve the first pipe instance before startup succeeds so later launches cannot be silently lost.
fn create_pipe(name: &[u16], first: bool) -> Result<HANDLE, BtError> {
    let access = PIPE_ACCESS_INBOUND
        | if first {
            FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            0
        };
    let pipe = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            access,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            MAX_REQUEST_BYTES as u32,
            MAX_REQUEST_BYTES as u32,
            0,
            std::ptr::null(),
        )
    };
    if pipe == INVALID_HANDLE_VALUE {
        return Err(BtError::Io(std::io::Error::last_os_error()));
    }
    Ok(pipe)
}

/// Retry until the primary process has created its pipe, then send one complete JSON message.
fn forward(pipe_name: &str, args: &[String]) -> Result<(), BtError> {
    let payload = serde_json::to_vec(args).map_err(|err| BtError::Config(err.to_string()))?;
    if payload.len() > MAX_REQUEST_BYTES {
        return Err(BtError::Config(
            "App launch arguments exceed the single-instance limit".to_string(),
        ));
    }
    let name = wide(pipe_name);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let pipe = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if pipe != INVALID_HANDLE_VALUE {
            let mut written = 0u32;
            let sent = unsafe {
                WriteFile(
                    pipe,
                    payload.as_ptr(),
                    payload.len() as u32,
                    &mut written,
                    std::ptr::null_mut(),
                )
            } != 0;
            unsafe { CloseHandle(pipe) };
            if sent && written as usize == payload.len() {
                return Ok(());
            }
            return Err(BtError::Io(std::io::Error::last_os_error()));
        }
        if Instant::now() >= deadline {
            return Err(BtError::Io(std::io::Error::last_os_error()));
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

/// Encode a Windows kernel object name as NUL-terminated UTF-16.
fn wide(value: &str) -> Vec<u16> {
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
