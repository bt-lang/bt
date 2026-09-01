//! Adapter from `net.listen({type:'web'})` to the Web service runner.

use crate::net::traits::{
    object_array, object_bool, object_get, object_i64, object_string, parse_bind, BtNetServer,
};
use crate::path as bt_path;
use crate::value::Value;
use crate::web::{WebConfig, WebSiteConfig};
use std::path::Path;

/// Parsed web site configuration.
struct ParsedWebSite {
    /// The site configuration used by the runner.
    site: WebSiteConfig,
    /// TLS certificate configuration.
    ssl: Option<SslConfig>,
}

/// Static file configuration.
struct StaticConfig {
    /// Whether to enable static file service.
    open: bool,
    /// Static file routing.
    route: String,
    /// Static file directory.
    path: String,
    /// Default file.
    default_file: String,
    /// Whether directory listings are allowed.
    list: bool,
    /// Static response Cache-Control header.
    cache_control: String,
    /// The number of bytes read in static files in blocks; 0 means using the framework default value.
    chunk_size: u64,
}

/// TLS certificate configuration.
struct SslConfig {
    /// Certificate file path.
    cert: String,
    /// Private key file path.
    key: String,
}

/// Web service handle.
#[derive(Debug, Clone, PartialEq)]
pub struct WebServerHandle {
    /// Server ID.
    id: usize,
    /// The listening address visible to the script.
    addr: String,
}

impl WebServerHandle {
    /// Creates a Web service handle.
    pub fn new(id: usize, addr: String) -> Self {
        Self { id, addr }
    }

    /// Calls the web service method.
    pub fn call_method(&self, method: &str, _args: Vec<Value>) -> Result<Value, String> {
        match method {
            "close" => {
                self.close()?;
                Ok(Value::Bool(true))
            }
            _ => Err(format!("web server has no method `{}`", method)),
        }
    }
}

impl BtNetServer for WebServerHandle {
    /// Closes the Web server.
    fn close(&self) -> Result<(), String> {
        crate::net::close_web_service(self.id)
    }

    /// Returns the Web server's listening address.
    fn addr(&self) -> String {
        self.addr.clone()
    }

    /// Returns the server type name.
    fn kind(&self) -> &'static str {
        "web"
    }
}

/// Starts a Web server.
pub fn listen(config: &Value, source_dir: &Path, project_root: &Path) -> Result<Value, String> {
    let bind = object_string(config, "bind", "0.0.0.0:8080");
    let (bind_host, bind_port) = parse_bind(&bind)?;
    let sites = object_array(config, "sites");
    if sites.is_empty() {
        return Err(
            "net.listen({type:'web'}) requires at least one site configuration".to_string(),
        );
    }
    let mut parsed_sites = Vec::with_capacity(sites.len());
    let mut ssl_config = None;
    for site_value in sites {
        let parsed = parse_site(&site_value, source_dir, project_root);
        if ssl_config.is_none() {
            ssl_config = parsed.ssl;
        }
        parsed_sites.push(parsed.site);
    }
    let ssl_open = ssl_config.is_some();
    let web_config = WebConfig {
        bind_host: bind_host.clone(),
        web_port: bind_port,
        ssl_open,
        ssl_cert_file: ssl_config
            .as_ref()
            .map(|ssl| ssl.cert.clone())
            .unwrap_or_else(|| "ssl/cert.pem".to_string()),
        ssl_key_file: ssl_config
            .as_ref()
            .map(|ssl| ssl.key.clone())
            .unwrap_or_else(|| "ssl/key.pem".to_string()),
        sites: parsed_sites,
    };
    let id = crate::net::spawn_web_service(web_config)?;
    Ok(Value::NetWebServer(WebServerHandle::new(
        id,
        format!("{}:{}", bind_host, bind_port),
    )))
}

/// Parses a Web site configuration.
fn parse_site(value: &Value, source_dir: &Path, project_root: &Path) -> ParsedWebSite {
    let domains = object_array(value, "domains")
        .iter()
        .map(Value::to_string)
        .map(|domain| normalize_domain(&domain))
        .filter(|domain| !domain.is_empty())
        .collect();
    let root = listen_path(
        source_dir,
        project_root,
        &object_string(value, "root", "web/"),
    );
    let entry = object_string(value, "entry", "main.bt");
    let static_value = object_get(value, "static");
    let upload_value = object_get(value, "upload");
    let ssl_value = object_get(value, "ssl");
    let static_config = parse_static(static_value.as_ref(), source_dir, project_root, &root);
    ParsedWebSite {
        site: WebSiteConfig {
            domains,
            project_path: root,
            entry_file: entry,
            file_temp_path: listen_path(
                source_dir,
                project_root,
                &ssl_or_upload_string(upload_value.as_ref(), "temp", "temp/"),
            ),
            web_static_open: static_config.open,
            web_static_path: static_config.path,
            web_static_default: static_config.default_file,
            web_static_router: static_config.route,
            web_static_list: static_config.list,
            web_static_cache_control: static_config.cache_control,
            web_static_chunk_size: static_config.chunk_size,
        },
        ssl: parse_ssl(ssl_value.as_ref(), source_dir, project_root),
    }
}

/// Parses static file configuration.
fn parse_static(
    value: Option<&Value>,
    source_dir: &Path,
    project_root: &Path,
    root: &str,
) -> StaticConfig {
    let open = value.is_some();
    let route = value
        .map(|value| object_string(value, "route", "/static/{**}"))
        .unwrap_or_else(|| "/static/{**}".to_string());
    let path = value
        .map(|value| object_string(value, "path", ""))
        .filter(|path| !path.is_empty())
        .map(|path| listen_path(source_dir, project_root, &path))
        .unwrap_or_else(|| {
            bt_path::path_text(&bt_path::normalize_path(Path::new(root).join("static")))
        });
    let default_file = value
        .map(|value| object_string(value, "default", "index.html"))
        .unwrap_or_else(|| "index.html".to_string());
    let list = value
        .map(|value| object_bool(value, "list", false))
        .unwrap_or(false);
    let cache_control = value
        .map(|value| object_string(value, "cache_control", ""))
        .unwrap_or_default();
    let chunk_size = value
        .map(|value| object_i64(value, "chunk_size", 0).max(0) as u64)
        .unwrap_or(0);
    StaticConfig {
        open,
        route,
        path,
        default_file,
        list,
        cache_control,
        chunk_size,
    }
}

/// Parses TLS configuration.
fn parse_ssl(value: Option<&Value>, source_dir: &Path, project_root: &Path) -> Option<SslConfig> {
    let value = value?;
    let cert = object_string(value, "cert", "");
    let key = object_string(value, "key", "");
    if cert.is_empty() || key.is_empty() {
        None
    } else {
        Some(SslConfig {
            cert: listen_path(source_dir, project_root, &cert),
            key: listen_path(source_dir, project_root, &key),
        })
    }
}

/// Read the upload configuration string field.
fn ssl_or_upload_string(value: Option<&Value>, key: &str, default: &str) -> String {
    value
        .map(|value| object_string(value, key, default))
        .unwrap_or_else(|| default.to_string())
}

/// Resolves a configured path against the current source directory or project root.
fn listen_path(source_dir: &Path, project_root: &Path, path: &str) -> String {
    bt_path::path_text(&bt_path::resolve_path(path, project_root, source_dir))
}

/// Normalizes a domain to match Salvo's portless `Host` filtering rules.
fn normalize_domain(domain: &str) -> String {
    let domain = domain.trim();
    if domain.starts_with('[') {
        return domain
            .find(']')
            .map(|end| domain[1..end].to_string())
            .unwrap_or_else(|| domain.to_string());
    }
    match domain.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.chars().all(|ch| ch.is_ascii_digit()) => {
            host.to_string()
        }
        _ => domain.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;

    /// The relative path of the Web site should be bound to the current directory when the listen is created to avoid the request entry being lost after the desktop package is run to restore the working directory.
    #[test]
    fn parse_site_binds_relative_paths_to_listen_base_dir() {
        let base_dir = PathBuf::from(r"C:\bt-temp\bundle");
        let project_root = base_dir.clone();
        let mut site = IndexMap::new();
        site.insert("root".to_string(), Value::Str("www/".to_string()));
        site.insert("entry".to_string(), Value::Str("main.bt".to_string()));

        let mut upload = IndexMap::new();
        upload.insert("temp".to_string(), Value::Str("upload/".to_string()));
        site.insert(
            "upload".to_string(),
            Value::Object(Rc::new(RefCell::new(upload))),
        );

        let mut static_config = IndexMap::new();
        static_config.insert("path".to_string(), Value::Str("assets/".to_string()));
        site.insert(
            "static".to_string(),
            Value::Object(Rc::new(RefCell::new(static_config))),
        );

        let parsed = parse_site(
            &Value::Object(Rc::new(RefCell::new(site))),
            &base_dir,
            &project_root,
        );

        assert_eq!(
            parsed.site.project_path,
            bt_path::path_text(&bt_path::normalize_path(base_dir.join("www/")))
        );
        assert_eq!(parsed.site.entry_file, "main.bt");
        assert_eq!(
            parsed.site.file_temp_path,
            bt_path::path_text(&bt_path::normalize_path(base_dir.join("upload/")))
        );
        assert_eq!(
            parsed.site.web_static_path,
            bt_path::path_text(&bt_path::normalize_path(base_dir.join("assets/")))
        );
    }

    /// Static directory defaults should be based on the pinned site root rather than continuing to concatenate raw strings.
    #[test]
    fn parse_static_default_path_uses_bound_root_dir() {
        let base_dir = PathBuf::from(r"C:\bt-temp\bundle");
        let project_root = base_dir.clone();
        let root = listen_path(&base_dir, &project_root, "www");
        let static_config = parse_static(
            Some(&Value::Object(Rc::new(RefCell::new(IndexMap::new())))),
            &base_dir,
            &project_root,
            &root,
        );

        assert_eq!(
            static_config.path,
            bt_path::path_text(&bt_path::normalize_path(Path::new(&root).join("static")))
        );
    }

    /// Static file cache headers and chunk sizes should be read from the static configuration.
    #[test]
    fn parse_static_reads_cache_and_chunk_options() {
        let base_dir = PathBuf::from(r"C:\bt-temp\bundle");
        let project_root = base_dir.clone();
        let root = listen_path(&base_dir, &project_root, "www");
        let mut value = IndexMap::new();
        value.insert(
            "cache_control".to_string(),
            Value::Str("public, max-age=3600".to_string()),
        );
        value.insert("chunk_size".to_string(), Value::Int(262_144));

        let static_config = parse_static(
            Some(&Value::Object(Rc::new(RefCell::new(value)))),
            &base_dir,
            &project_root,
            &root,
        );

        assert_eq!(static_config.cache_control, "public, max-age=3600");
        assert_eq!(static_config.chunk_size, 262_144);
    }
}
