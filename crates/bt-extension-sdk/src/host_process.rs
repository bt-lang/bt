//! Bounded asynchronous process requests shared by native tests and WASM extensions.
//!
//! The host import accepts UTF-8 JSON and returns `{ "ok": value }` or
//! `{ "error": "message" }`. Only explicit callers enable this optional capability.
//!
//! `close` takes `id` and an optional boolean `discard_output` (default false).
//! Setting it removes only the caller-owned `cleanup_paths` declared during spawn,
//! even when the process succeeded before close was observed. If the process is
//! still active, its worker reaps it before removing those files. Normal close
//! preserves successful outputs. Declare only files reserved with create-new.

#[cfg(not(target_arch = "wasm32"))]
#[path = "host_process_native.rs"]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub use native::ProcessHost;

/// Maximum encoded request size, including all arguments and declared paths.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// Maximum response size; two one-MiB tails fit even after JSON escaping.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "bts_host")]
extern "C" {
    /// Write one envelope to caller memory, returning its length or a negative error.
    fn process_request(request: u32, length: u32, output: u32, capacity: u32) -> i32;
}

#[cfg(target_arch = "wasm32")]
thread_local! {
    /// Reuse response storage across polling calls without retaining decoded replies.
    static RESPONSE: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(not(target_arch = "wasm32"))]
thread_local! {
    /// Native extension tests retain the same service ownership as one WASM instance.
    static HOST: std::cell::RefCell<Option<ProcessHost>> = const { std::cell::RefCell::new(None) };
}

/// Execute a JSON service request without waiting for the spawned process to finish.
///
/// Native callers use the current directory captured by the first request on their
/// thread. WASM callers use the project root and permission checks of their host.
pub fn request(raw: &str) -> crate::BtResult<String> {
    if raw.len() > MAX_REQUEST_BYTES {
        return Err("Host process request exceeds 64 KiB".into());
    }
    #[cfg(target_arch = "wasm32")]
    {
        RESPONSE.with(|storage| {
            let mut storage = storage.borrow_mut();
            storage.resize(MAX_RESPONSE_BYTES, 0);
            // Both slices remain allocated and exclusively borrowed during the import.
            let length = unsafe {
                process_request(
                    raw.as_ptr() as u32,
                    raw.len() as u32,
                    storage.as_mut_ptr() as u32,
                    storage.len() as u32,
                )
            };
            if length <= 0 || length as usize > storage.len() {
                return Err("Host process transport failed".into());
            }
            let envelope: serde_json::Value =
                serde_json::from_slice(&storage[..length as usize])
                    .map_err(|error| format!("Invalid host process response: {error}"))?;
            if let Some(error) = envelope.get("error").and_then(serde_json::Value::as_str) {
                return Err(error.to_owned());
            }
            envelope
                .get("ok")
                .map(serde_json::Value::to_string)
                .ok_or_else(|| "Host process response has no result".into())
        })
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        HOST.with(|host| {
            let mut host = host.borrow_mut();
            if host.is_none() {
                *host = Some(ProcessHost::new(std::env::current_dir().map_err(
                    |error| format!("Cannot read current directory: {error}"),
                )?)?);
            }
            host.as_mut()
                .expect("initialized process host")
                .request(raw)
        })
    }
}
