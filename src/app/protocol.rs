use crate::app::runtime::{AppRuntime, AppState};
use std::path::{Component, Path};
use std::sync::OnceLock;
use tauri::http::{header, Request, Response, StatusCode, Uri};
use tauri::{Builder, Manager, Runtime};

/// Registers the `bt://` desktop resource protocol.
///
/// The protocol maps paths such as `bt://app/index.html` and `bt://app/assets/app.css`
/// to the `AppRuntime` resource source. Development builds read the project directory;
/// packaged builds read the bundle appended to the executable.
pub fn register_bt_protocol<R: Runtime>(builder: Builder<R>) -> Builder<R> {
    builder.register_uri_scheme_protocol("bt", |ctx, request| {
        let state = ctx.app_handle().state::<AppState>();
        let response = match state.lock_runtime() {
            Ok(runtime) => match build_response(&runtime, &request) {
                Ok(response) => response,
                Err(response) => response,
            },
            Err(err) => error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Desktop runtime unavailable: {}", err),
            ),
        };
        response
    })
}

/// Builds a protocol response for the request.
fn build_response(
    runtime: &AppRuntime,
    request: &Request<Vec<u8>>,
) -> Result<Response<Vec<u8>>, Response<Vec<u8>>> {
    let requested_path = match resource_path_from_uri(request.uri()) {
        Ok(path) => path,
        Err(status) => return Err(error_response(status, "Invalid resource path")),
    };
    let path = resolve_protocol_resource_path(runtime, requested_path);

    if trace_protocol() {
        println!("bt:// request: {} -> {}", request.uri(), path);
    }

    if path == crate::app::html::ERROR_ENTRY {
        if let Some(html) = runtime.startup_error_html.as_ref() {
            return Ok(html_response(html.as_bytes().to_vec()));
        }
    }

    if path == crate::app::api::screen::OVERLAY_ENTRY {
        return Ok(html_response(
            crate::app::api::screen::overlay_html().as_bytes().to_vec(),
        ));
    }

    match runtime.resource.read(&path) {
        Ok(bytes) => Ok(resource_response(&path, bytes)),
        Err(err) => {
            if let Some(entry) = static_spa_fallback_entry(runtime, &path) {
                if let Ok(bytes) = runtime.resource.read(&entry) {
                    return Ok(resource_response(&entry, bytes));
                }
            }
            Err(error_response(
                StatusCode::NOT_FOUND,
                &format!(
                    "Resource `{}` does not exist or cannot be read: {}",
                    path, err
                ),
            ))
        }
    }
}

/// Builds a standard resource response.
fn resource_response(path: &str, bytes: Vec<u8>) -> Response<Vec<u8>> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type(path))
        .header(header::CACHE_CONTROL, "no-cache")
        .body(bytes)
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to build resource response",
            )
        })
}

/// Maps the protocol root path to the static entry file.
///
/// This allows the default `index.html` to open at `bt://app/`, so frontend history
/// routing sees `/` as the initial path.
fn resolve_protocol_resource_path(runtime: &AppRuntime, path: String) -> String {
    if path.is_empty() && runtime.config.app.mode == "static" {
        return runtime.config.app.entry.trim_start_matches('/').to_string();
    }
    path
}

/// Returns the fallback entry for static SPA routes.
///
/// Only extensionless paths fall back after the requested resource cannot be read,
/// preventing missing JS, CSS, or images from being served as HTML.
fn static_spa_fallback_entry(runtime: &AppRuntime, path: &str) -> Option<String> {
    if runtime.config.app.mode != "static"
        || path.is_empty()
        || Path::new(path).extension().is_some()
    {
        return None;
    }
    let entry = runtime.config.app.entry.trim_start_matches('/');
    if entry.is_empty() || path == entry {
        None
    } else {
        Some(entry.to_string())
    }
}

/// Builds a standard HTML response.
fn html_response(body: Vec<u8>) -> Response<Vec<u8>> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(body)
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to build resource response",
            )
        })
}

/// Returns whether protocol request tracing is enabled.
fn trace_protocol() -> bool {
    static TRACE: OnceLock<bool> = OnceLock::new();
    *TRACE.get_or_init(|| std::env::var("BT_APP_TRACE_PROTOCOL").as_deref() == Ok("1"))
}

/// Extracts a bundle or directory resource path from the protocol request URI.
fn resource_path_from_uri(uri: &Uri) -> Result<String, StatusCode> {
    let scheme = uri.scheme_str().unwrap_or_default();
    let host = uri.host().unwrap_or_default();

    if scheme == "bt" && host != "app" {
        return Err(StatusCode::FORBIDDEN);
    }

    if (scheme == "http" || scheme == "https") && host != "bt.localhost" {
        return Err(StatusCode::FORBIDDEN);
    }

    if scheme != "bt" && scheme != "http" && scheme != "https" {
        return Err(StatusCode::FORBIDDEN);
    }

    let raw_path = uri.path().trim_start_matches('/');
    let path = percent_decode(raw_path).map_err(|_| StatusCode::BAD_REQUEST)?;
    if path.is_empty() {
        return Ok(path);
    }
    validate_protocol_path(&path)?;
    Ok(path)
}

/// Ensures the protocol path is a safe relative resource path.
fn validate_protocol_path(path: &str) -> Result<(), StatusCode> {
    if path.is_empty() || path.as_bytes().contains(&0) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let path = Path::new(path);
    if path.is_absolute() {
        return Err(StatusCode::FORBIDDEN);
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                return Err(StatusCode::FORBIDDEN);
            }
        }
    }
    Ok(())
}

/// Performs minimal percent-decoding on a URI path without another runtime dependency.
fn percent_decode(input: &str) -> Result<String, ()> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(());
            }
            let high = hex_value(bytes[i + 1])?;
            let low = hex_value(bytes[i + 2])?;
            out.push((high << 4) | low);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| ())
}

/// Decodes a hexadecimal digit.
fn hex_value(byte: u8) -> Result<u8, ()> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(()),
    }
}

/// Builds the standard HTML error response.
fn error_response(status: StatusCode, message: &str) -> Response<Vec<u8>> {
    let html = crate::app::html::render_error_html("BT App Resource Error", message);
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(html.into_bytes())
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Vec::new())
                .unwrap()
        })
}

/// Returns the Content-Type for a file extension.
fn content_type(path: &str) -> &'static str {
    let ext = Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "application/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::config::AppJson;
    use crate::app::resource::ResourceSource;
    use crate::app::runtime::AppRuntime;
    use std::path::PathBuf;

    /// The protocol accepts the root path so the runtime can map it to the static entry.
    #[test]
    fn protocol_allows_root_path() {
        let uri: Uri = "bt://app/".parse().unwrap();

        assert_eq!(resource_path_from_uri(&uri).unwrap(), "");
    }

    /// In static mode, the root path maps to the current `app.entry`.
    #[test]
    fn protocol_root_maps_to_static_entry() {
        let runtime = static_runtime("index.html");

        assert_eq!(
            resolve_protocol_resource_path(&runtime, String::new()),
            "index.html"
        );
    }

    /// Non-root paths retain their original resource path.
    #[test]
    fn protocol_non_root_keeps_resource_path() {
        let runtime = static_runtime("index.html");

        assert_eq!(
            resolve_protocol_resource_path(&runtime, "assets/app.js".to_string()),
            "assets/app.js"
        );
    }

    /// In static mode, extensionless routes fall back to the entry HTML for SPA history reloads.
    #[test]
    fn static_spa_fallback_accepts_extensionless_route() {
        let runtime = static_runtime("index.html");

        assert_eq!(
            static_spa_fallback_entry(&runtime, "dashboard/list"),
            Some("index.html".to_string())
        );
    }

    /// Missing resources with extensions do not fall back to the entry HTML, keeping frontend asset errors visible.
    #[test]
    fn static_spa_fallback_rejects_extension_resource() {
        let runtime = static_runtime("index.html");

        assert_eq!(static_spa_fallback_entry(&runtime, "assets/app.js"), None);
    }

    /// Builds a minimal static runtime for protocol path-mapping tests.
    fn static_runtime(entry: &str) -> AppRuntime {
        let mut config = AppJson::default();
        config.app.entry = entry.to_string();
        AppRuntime {
            project_dir: PathBuf::from("."),
            source: crate::app::runtime::AppSource::EmbeddedPage,
            app_args: Vec::new(),
            config,
            resource: ResourceSource::Embedded(&[]),
            bt_vm: None,
            server: None,
            startup_error_html: None,
            startup_error_message: None,
        }
    }
}
