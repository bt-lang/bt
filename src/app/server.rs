use crate::app::resource::ResourceSource;
use crate::app::runtime::AppRuntime;
use crate::error::BtError;
use crate::web;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

/// Handle for the desktop server-mode web service.
///
/// The Salvo service currently runs until the process exits. This handle keeps the service
/// thread and temporary directory alive so extracted bundle files are not removed while the
/// packaged application window is open.
#[derive(Clone)]
pub struct AppServerHandle {
    /// Local service URL loaded by the WebView in server mode; absent for a background `server.bt`.
    url: Option<String>,
    /// Shared state for the service thread and temporary directory.
    #[allow(dead_code)]
    inner: Arc<AppServerInner>,
}

impl std::fmt::Debug for AppServerHandle {
    /// Emits concise debug output without exposing internal thread state.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppServerHandle")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl AppServerHandle {
    /// Returns the local service URL loaded by the WebView in server mode.
    pub fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }
}

/// Internal resources kept alive while server mode is running.
#[derive(Debug)]
struct AppServerInner {
    /// Temporary directory containing the extracted bundle; absent in development mode.
    temp_dir: Option<PathBuf>,
}

impl Drop for AppServerInner {
    /// Makes a best effort to remove the packaged-mode temporary directory on shutdown or drop.
    fn drop(&mut self) {
        if let Some(path) = &self.temp_dir {
            let _ = fs::remove_dir_all(path);
        }
    }
}

/// Starts the local web service for desktop server mode.
pub fn start_app_server(runtime: &AppRuntime) -> Result<AppServerHandle, BtError> {
    let url = if runtime.config.app.mode == "server" {
        let entry = runtime.config.app.entry.trim();
        validate_server_entry_url(entry)?;
        Some(entry.to_string())
    } else {
        None
    };

    let prepared = prepare_server_project(runtime)?;
    let script_path = prepared.root.join("server.bt");
    if !script_path.is_file() {
        cleanup_prepared_temp(&prepared);
        return Err(BtError::Config(format!(
            "Required server-mode script does not exist: {}",
            script_path.display()
        )));
    }

    let run_result = web::run_file_with_chunk(&script_path);
    let (_, _, output) = match run_result {
        Ok(result) => result,
        Err(err) => {
            cleanup_prepared_temp(&prepared);
            return Err(BtError::Config(err));
        }
    };
    if !output.is_empty() {
        print!("{}", output);
    }

    Ok(AppServerHandle {
        url,
        inner: Arc::new(AppServerInner {
            temp_dir: prepared.temp_dir,
        }),
    })
}

/// Removes an extracted temporary directory not yet owned by a live handle.
fn cleanup_prepared_temp(prepared: &PreparedServerProject) {
    if let Some(path) = &prepared.temp_dir {
        let _ = fs::remove_dir_all(path);
    }
}

/// Ensures the server-mode window entry is an HTTP or HTTPS URL.
fn validate_server_entry_url(entry: &str) -> Result<(), BtError> {
    if entry.is_empty() {
        return Err(BtError::Config(
            "app.entry cannot be empty in server mode".to_string(),
        ));
    }
    let url = Url::parse(entry)
        .map_err(|err| BtError::Config(format!("Invalid server-mode entry URL: {}", err)))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(BtError::Config(
            "Server-mode entry must start with http:// or https://".to_string(),
        ));
    }
    Ok(())
}

/// Prepares a project directory the web service can access directly through the filesystem.
fn prepare_server_project(runtime: &AppRuntime) -> Result<PreparedServerProject, BtError> {
    match &runtime.resource {
        ResourceSource::Directory(project_dir) => Ok(PreparedServerProject {
            root: project_dir.clone(),
            temp_dir: None,
        }),
        ResourceSource::Bundle(_) | ResourceSource::Btr(_) => {
            let temp_dir = create_server_temp_dir(&runtime.config.app.name)?;
            materialize_resource_source(&runtime.resource, &temp_dir)?;
            Ok(PreparedServerProject {
                root: temp_dir.clone(),
                temp_dir: Some(temp_dir),
            })
        }
        ResourceSource::Embedded(_) => Err(BtError::Config(
            "The embedded setup page does not support server mode".to_string(),
        )),
    }
}

/// Prepared server project directory.
struct PreparedServerProject {
    /// Root directory served by the web service.
    root: PathBuf,
    /// Temporary directory kept alive and removed on exit.
    temp_dir: Option<PathBuf>,
}

/// Extracts bundle resources into a temporary directory for the existing web runner and `include`/`fs` support.
fn materialize_resource_source(resource: &ResourceSource, root: &Path) -> Result<(), BtError> {
    for name in resource.list() {
        validate_materialized_path(&name)?;
        let bytes = resource.read(&name)?;
        let output = root.join(Path::new(&name));
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(output, bytes)?;
    }
    Ok(())
}

/// Creates a dedicated temporary directory for server mode.
fn create_server_temp_dir(app_name: &str) -> Result<PathBuf, BtError> {
    let base = std::env::temp_dir();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    for index in 0..100u32 {
        let dir = base.join(format!(
            "bt-app-server-{}-{}-{}",
            safe_temp_name(app_name),
            std::process::id(),
            stamp + index as u128
        ));
        match fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(BtError::Io(err)),
        }
    }
    Err(BtError::Runtime(
        "Failed to create a temporary directory for the desktop web service".to_string(),
    ))
}

/// Converts the application name into a safe, compact temporary-directory component.
fn safe_temp_name(name: &str) -> String {
    let mut output = String::with_capacity(name.len().min(32));
    for ch in name.chars().take(32) {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            output.push(ch);
        } else {
            output.push('_');
        }
    }
    if output.is_empty() {
        "app".to_string()
    } else {
        output
    }
}

/// Ensures an extracted path retains the bundle's safe relative-path semantics.
fn validate_materialized_path(path: &str) -> Result<(), BtError> {
    let path = Path::new(path);
    if path.is_absolute() {
        return Err(BtError::Bundle(format!(
            "Bundle extraction does not allow absolute paths: {}",
            path.display()
        )));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                return Err(BtError::Bundle(format!(
                    "Bundle extraction does not allow unsafe paths: {}",
                    path.display()
                )))
            }
        }
    }
    Ok(())
}
