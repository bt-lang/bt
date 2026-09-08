//! Optional, permission-gated native process imports for WASI extensions.
//!
//! Process execution is a native capability, outside the WASI filesystem sandbox.
//! The import is installed only for extensions declaring `process`; ordinary
//! extension calls and unrelated VM instructions perform no extra work.

use std::path::Path;
use std::sync::{Arc, Mutex};

use bt_extension_sdk::host_process::ProcessHost;
use wasmtime::{Caller, Linker};
use wasmtime_wasi::p1::WasiP1Ctx;

use crate::extensions::manifest::ExtensionPermissions;
use crate::permission::{self, Capability};

/// Largest accepted request, before reading or parsing guest memory.
const MAX_REQUEST: usize = 64 * 1024;
/// Required response capacity, covering worst-case JSON escaping of bounded pipes.
const RESPONSE_CAPACITY: usize = 16 * 1024 * 1024;

/// Installs the generic process import without adding state to unprivileged stores.
pub(crate) fn add_to_linker(
    linker: &mut Linker<WasiP1Ctx>,
    project_root: &Path,
    permissions: ExtensionPermissions,
) -> Result<(), String> {
    if !permissions.process {
        return Ok(());
    }
    let host = Arc::new(Mutex::new(ProcessHost::new(project_root.to_path_buf())?));
    linker
        .func_wrap(
            "bts_host",
            "process_request",
            move |mut caller: Caller<'_, WasiP1Ctx>,
                  request_ptr: u32,
                  request_len: u32,
                  output_ptr: u32,
                  output_cap: u32|
                  -> i32 {
                let Some(memory) = caller
                    .get_export("memory")
                    .and_then(|item| item.into_memory())
                else {
                    return -1;
                };
                // Validate both ranges before executing mutations. A bad output pointer
                // must never create an inaccessible background job.
                let memory_len = memory.data_size(&caller);
                if request_len as usize > MAX_REQUEST
                    || output_cap as usize != RESPONSE_CAPACITY
                    || (request_ptr as usize)
                        .checked_add(request_len as usize)
                        .is_none_or(|end| end > memory_len)
                    || (output_ptr as usize)
                        .checked_add(output_cap as usize)
                        .is_none_or(|end| end > memory_len)
                {
                    return -2;
                }
                let mut request = vec![0; request_len as usize];
                if memory
                    .read(&caller, request_ptr as usize, &mut request)
                    .is_err()
                {
                    return -3;
                }
                let result = execute_request(&host, permissions, &request);
                let envelope = match result {
                    Ok(value) => serde_json::json!({"ok": value}),
                    Err(error) => serde_json::json!({"error": error}),
                };
                let response = envelope.to_string();
                if response.len() > RESPONSE_CAPACITY {
                    return -4;
                }
                if memory
                    .write(&mut caller, output_ptr as usize, response.as_bytes())
                    .is_err()
                {
                    return -5;
                }
                response.len() as i32
            },
        )
        .map_err(|error| format!("Failed to register extension process import: {error}"))?;
    Ok(())
}

/// Checks current process permissions and validates declared filesystem path lists.
fn execute_request(
    host: &Mutex<ProcessHost>,
    permissions: ExtensionPermissions,
    bytes: &[u8],
) -> Result<serde_json::Value, String> {
    let request: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("Invalid process request JSON: {error}"))?;
    // Releasing already-owned resources remains possible after permission revocation.
    if !matches!(
        request.get("op").and_then(|value| value.as_str()),
        Some("cancel" | "close")
    ) {
        permission::check(Capability::Process)?;
    }
    if request.get("op").and_then(|value| value.as_str()) == Some("spawn") {
        for (field, allowed) in [
            ("read_paths", permissions.fs_read),
            ("write_paths", permissions.fs_write),
            ("cleanup_paths", permissions.fs_write),
        ] {
            if request
                .get(field)
                .and_then(|value| value.as_array())
                .is_some_and(|paths| !paths.is_empty())
            {
                if !allowed {
                    return Err(format!(
                        "Process request `{field}` requires a declared filesystem permission"
                    ));
                }
                permission::check(Capability::Fs)?;
            }
        }
    }
    let raw = std::str::from_utf8(bytes)
        .map_err(|error| format!("Process request must be UTF-8: {error}"))?;
    let response = host
        .lock()
        .map_err(|_| "Extension process state lock is poisoned".to_string())?
        .request(raw)?;
    serde_json::from_str(&response)
        .map_err(|error| format!("Invalid process backend response: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::{Engine, Module, Store};
    use wasmtime_wasi::WasiCtxBuilder;

    /// Builds a minimal guest exposing the import without depending on an extension package.
    fn guest(engine: &Engine) -> Module {
        Module::new(engine, wat::parse_str(r#"(module
            (import "bts_host" "process_request" (func $request (param i32 i32 i32 i32) (result i32)))
            (memory (export "memory") 257)
            (data (i32.const 0) "{\22op\22:\22unknown\22}")
            (func (export "request") (param i32 i32 i32 i32) (result i32)
                local.get 0 local.get 1 local.get 2 local.get 3 call $request))"#).unwrap()).unwrap()
    }

    /// Extensions without process permission cannot link the native process import.
    #[test]
    fn undeclared_process_import_is_unavailable() {
        let engine = Engine::default();
        let module = guest(&engine);
        let mut linker = Linker::new(&engine);
        add_to_linker(
            &mut linker,
            &std::env::current_dir().unwrap(),
            ExtensionPermissions::default(),
        )
        .unwrap();
        let mut store = Store::new(&engine, WasiCtxBuilder::new().build_p1());
        assert!(linker.instantiate(&mut store, &module).is_err());
    }

    /// Invalid ranges are rejected before dispatch; business failures use JSON envelopes.
    #[test]
    fn process_import_checks_memory_and_returns_errors() {
        let engine = Engine::default();
        let module = guest(&engine);
        let mut linker = Linker::new(&engine);
        let permissions = ExtensionPermissions {
            process: true,
            ..ExtensionPermissions::default()
        };
        add_to_linker(&mut linker, &std::env::current_dir().unwrap(), permissions).unwrap();
        let mut store = Store::new(&engine, WasiCtxBuilder::new().build_p1());
        let instance = linker.instantiate(&mut store, &module).unwrap();
        let request = instance
            .get_typed_func::<(u32, u32, u32, u32), i32>(&mut store, "request")
            .unwrap();
        assert_eq!(
            request
                .call(&mut store, (0, 16, u32::MAX, RESPONSE_CAPACITY as u32))
                .unwrap(),
            -2
        );
        assert_eq!(request.call(&mut store, (0, 16, 65536, 1024)).unwrap(), -2);
        let length = request
            .call(&mut store, (0, 16, 65536, RESPONSE_CAPACITY as u32))
            .unwrap();
        assert!(length > 0);
        let memory = instance.get_memory(&mut store, "memory").unwrap();
        let mut output = vec![0; length as usize];
        memory.read(&store, 65536, &mut output).unwrap();
        let envelope: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert!(envelope["error"].is_string());
    }
}
