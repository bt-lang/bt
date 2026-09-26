//! Per-user Linux single-instance ownership and bounded launch forwarding.

use crate::error::BtError;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::{
    fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    net::{UnixListener, UnixStream},
};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use tauri::{Emitter, Manager};

/// Hard bounds on one request and the queue waiting for the frontend.
const MAX_BYTES: usize = 32 * 1024;
const MAX_PENDING: usize = 64;

/// Holds the exclusive file lock and stops the listener before releasing ownership.
pub struct InstanceGuard {
    /// The stable lock inode is deliberately retained across process restarts.
    _lock: File,
    /// Private socket path, removed on normal shutdown or the next successful claim.
    socket: PathBuf,
    /// Shared listener shutdown flag.
    stopped: Arc<AtomicBool>,
}

impl InstanceGuard {
    /// Clone the shutdown token without transferring ownership of the process lock.
    pub fn listener_stop(&self) -> Arc<AtomicBool> {
        self.stopped.clone()
    }
}

impl Drop for InstanceGuard {
    /// Wake a blocking accept and remove only this owner's socket.
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = UnixStream::connect(&self.socket);
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Bounded queue preserving launches received before frontend event registration.
#[derive(Clone, Default)]
pub struct OpenRequests(Arc<Mutex<VecDeque<Vec<String>>>>);

impl OpenRequests {
    /// Drain launch arguments in arrival order.
    pub fn take(&self) -> Vec<Vec<String>> {
        self.0
            .lock()
            .map(|mut values| values.drain(..).collect())
            .unwrap_or_default()
    }

    /// Retain at most the most recent 64 requests.
    fn push(&self, args: Vec<String>) {
        if let Ok(mut values) = self.0.lock() {
            if values.len() == MAX_PENDING {
                values.pop_front();
            }
            values.push_back(args);
        }
    }
}

/// Claim one canonical executable or deliver an acknowledged request to its owner.
pub fn claim_or_forward(args: &[String]) -> Result<Option<(InstanceGuard, UnixListener)>, BtError> {
    let uid = std::fs::metadata("/proc/self")?.uid();
    let directory = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{uid}")))
        .join("bt-instances");
    match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error.into()),
    }
    let metadata = std::fs::symlink_metadata(&directory)?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(BtError::Config(
            "Single-instance directory must be private to the current user".into(),
        ));
    }
    let executable = std::env::current_exe()?.canonicalize()?;
    let digest = Sha256::digest(executable.as_os_str().as_encoded_bytes());
    let key = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let socket = directory.join(format!("{key}.sock"));
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(directory.join(format!("{key}.lock")))?;
    match lock.try_lock() {
        Ok(()) => (),
        Err(std::fs::TryLockError::WouldBlock) => {
            forward(&socket, args)?;
            return Ok(None);
        }
        Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
    }
    // Only the lock owner may remove a stale socket left by an interrupted process.
    if socket.exists() {
        std::fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    Ok(Some((
        InstanceGuard {
            _lock: lock,
            socket,
            stopped: Arc::new(AtomicBool::new(false)),
        },
        listener,
    )))
}

/// Receive bounded requests on one worker and restore the original window on GTK's thread.
pub fn start_listener(
    listener: UnixListener,
    app: tauri::AppHandle,
    pending: OpenRequests,
    stopped: Arc<AtomicBool>,
) -> Result<(), BtError> {
    std::thread::Builder::new()
        .name("bt-open-requests".into())
        .spawn(move || {
            for incoming in listener.incoming() {
                if stopped.load(Ordering::Acquire) {
                    break;
                }
                let Ok(mut stream) = incoming else {
                    break;
                };
                let result = receive(&mut stream);
                let Ok(args) = result else {
                    continue;
                };
                pending.push(args);
                let handle = app.clone();
                let _ = app.run_on_main_thread(move || {
                    if let Some(window) = handle.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.unminimize();
                        let _ = window.set_focus();
                    }
                    let _ = handle.emit("bt://app/open_request", ());
                });
                let _ = stream.write_all(&[1]);
            }
        })?;
    Ok(())
}

/// Read a length-prefixed request with a deadline, avoiding unbounded client allocations.
fn receive(stream: &mut UnixStream) -> Result<Vec<String>, std::io::Error> {
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    let mut header = [0; 4];
    stream.read_exact(&mut header)?;
    let size = u32::from_le_bytes(header) as usize;
    if size > MAX_BYTES {
        return Err(std::io::Error::other(
            "Launch arguments exceed the single-instance limit",
        ));
    }
    let mut data = vec![0; size];
    stream.read_exact(&mut data)?;
    serde_json::from_slice(&data).map_err(std::io::Error::other)
}

/// Wait for startup and require acknowledgement before a secondary launch exits.
fn forward(path: &std::path::Path, args: &[String]) -> Result<(), BtError> {
    let payload = serde_json::to_vec(args).map_err(|error| BtError::Config(error.to_string()))?;
    if payload.len() > MAX_BYTES {
        return Err(BtError::Config(
            "Launch arguments exceed the single-instance limit".into(),
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream = loop {
        match UnixStream::connect(path) {
            Ok(stream) => break stream,
            Err(error) if Instant::now() >= deadline => return Err(error.into()),
            Err(_) => std::thread::sleep(Duration::from_millis(40)),
        }
    };
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    stream.write_all(&(payload.len() as u32).to_le_bytes())?;
    stream.write_all(&payload)?;
    let mut ack = [0];
    stream.read_exact(&mut ack)?;
    if ack != [1] {
        return Err(BtError::Config("Invalid launch acknowledgement".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reject oversized frames before allocating and preserve Unicode arguments.
    #[test]
    fn bounded_launch_protocol() {
        let (mut sender, mut receiver) = UnixStream::pair().unwrap();
        sender
            .write_all(&((MAX_BYTES + 1) as u32).to_le_bytes())
            .unwrap();
        assert!(receive(&mut receiver).is_err());
        let args = vec!["截图.png".to_string(), "--show".to_string()];
        let payload = serde_json::to_vec(&args).unwrap();
        sender
            .write_all(&(payload.len() as u32).to_le_bytes())
            .unwrap();
        sender.write_all(&payload).unwrap();
        assert_eq!(receive(&mut receiver).unwrap(), args);
        let pending = OpenRequests::default();
        for i in 0..100 {
            pending.push(vec![i.to_string()]);
        }
        assert_eq!(pending.take().len(), MAX_PENDING);
        assert!(pending.take().is_empty());
    }
}
