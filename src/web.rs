//! BT Web service runner.
//!
//! The Web layer handles only the network protocol, request context injection, and response
//! writing; business code is still executed by the new bytecode VM.
//! This keeps the BT language core independent of any specific framework, so switching from
//! Salvo to the underlying Hyper in the future would require replacing only this file.

use crate::compiler::Compiler;
use crate::lexer::{tokenize, Token};
use crate::libs::fs::BtFs;
use crate::libs::web::BtWebResponse;
use crate::parser::{Parser, Statement};
use crate::path as bt_path;
use crate::source::{analyze_source, SourceMode};
use crate::value::Value;
use crate::vm::{compile_cached_file, Vm};
use indexmap::IndexMap;
use salvo::catcher::Catcher;
use salvo::conn::rustls::{Keycert, RustlsConfig};
use salvo::conn::Listener;
use salvo::http::cookie::time::{Duration, OffsetDateTime};
use salvo::http::cookie::Cookie;
use salvo::http::header::{HeaderName, CACHE_CONTROL};
use salvo::http::{HeaderValue, Response, StatusCode};
use salvo::prelude::*;
use salvo::Request;
use salvo_extra::affix_state;
use salvo_serve_static::StaticDir;
use salvo_session::{CookieStore, Session, SessionDepotExt, SessionHandler};
use std::cell::RefCell;
use std::fmt::Display;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::sync::OnceLock;

/// Default Web request body limit, including forms and multipart uploads.
const DEFAULT_WEB_REQUEST_BODY_LIMIT: usize = 16 * 1024 * 1024;
/// Hard upper bound for the Web request body limit.
const MAX_WEB_REQUEST_BODY_LIMIT: usize = 512 * 1024 * 1024;
/// Default dynamic response body limit.
const DEFAULT_WEB_RESPONSE_BODY_LIMIT: usize = 64 * 1024 * 1024;
/// Hard upper bound for the Web dynamic response body limit.
const MAX_WEB_RESPONSE_BODY_LIMIT: usize = 1024 * 1024 * 1024;
/// Default HTTP header count limit.
const DEFAULT_WEB_HEADER_COUNT_LIMIT: usize = 128;
/// Hard upper bound for the HTTP header count limit.
const MAX_WEB_HEADER_COUNT_LIMIT: usize = 4096;
/// Default total HTTP header byte limit.
const DEFAULT_WEB_HEADER_BYTES_LIMIT: usize = 64 * 1024;
/// Hard upper bound for the total HTTP header byte limit.
const MAX_WEB_HEADER_BYTES_LIMIT: usize = 1024 * 1024;
/// Public BT session cookie name used by every Web listener.
const BT_SESSION_COOKIE_NAME: &str = "bt.session.id";
/// Previous framework-derived session cookie name, expired during migration.
const LEGACY_SESSION_COOKIE_NAME: &str = "salvo.session.id";
/// Default size limit for a single uploaded file.
const DEFAULT_WEB_UPLOAD_FILE_LIMIT: usize = 32 * 1024 * 1024;
/// Hard upper bound for the size limit of a single uploaded file.
const MAX_WEB_UPLOAD_FILE_LIMIT: usize = 512 * 1024 * 1024;

/// Cached Web resource limit configuration.
static WEB_RESOURCE_LIMITS: OnceLock<Result<WebResourceLimits, String>> = OnceLock::new();

/// Web configuration.
#[derive(Clone, Debug)]
pub struct WebConfig {
    /// Actual host address to listen on.
    pub(crate) bind_host: String,
    /// HTTP listening port.
    pub(crate) web_port: u16,
    /// Whether HTTPS is enabled.
    pub(crate) ssl_open: bool,
    /// TLS certificate path.
    pub(crate) ssl_cert_file: String,
    /// TLS private key path.
    pub(crate) ssl_key_file: String,
    /// Site configurations served by this listener.
    pub(crate) sites: Vec<WebSiteConfig>,
}

/// Web site runtime configuration.
#[derive(Clone, Debug)]
pub(crate) struct WebSiteConfig {
    /// Site domain list; an empty list makes this the default site without domain filtering.
    pub domains: Vec<String>,
    /// BT project directory.
    pub project_path: String,
    /// Entry script file.
    pub entry_file: String,
    /// Temporary directory for uploaded files.
    pub file_temp_path: String,
    /// Whether static file serving is enabled.
    pub web_static_open: bool,
    /// Static file directory.
    pub web_static_path: String,
    /// Default file for the static directory.
    pub web_static_default: String,
    /// Static file route.
    pub web_static_router: String,
    /// Whether to show static directory listings.
    pub web_static_list: bool,
    /// Cache-Control header for static responses; an empty value means it is not set explicitly.
    pub web_static_cache_control: String,
    /// Static file read chunk size in bytes; 0 uses the framework default.
    pub web_static_chunk_size: u64,
}

/// Shared state for Salvo requests.
#[derive(Clone, Debug)]
struct SiteState {
    /// Project root of the current Web site.
    project_root: PathBuf,
    /// BT entry script executed for each request.
    entry_path: PathBuf,
    /// Preloaded extension manager for the current site.
    #[cfg(feature = "extensions")]
    extension_manager: Option<std::sync::Arc<crate::extensions::manager::ExtensionManager>>,
    /// Temporary directory for uploaded files.
    file_temp_path: PathBuf,
}

/// Resource limits for Web requests and responses.
#[derive(Clone, Debug)]
struct WebResourceLimits {
    /// Maximum request body size in bytes.
    request_body_bytes: usize,
    /// Maximum dynamic response body size in bytes.
    response_body_bytes: usize,
    /// Maximum number of HTTP headers.
    header_count: usize,
    /// Maximum combined size of HTTP header names and values in bytes.
    header_bytes: usize,
    /// Maximum size of a single uploaded file in bytes.
    upload_file_bytes: usize,
}

/// Parses a BT file.
#[allow(dead_code)]
pub fn parse_file(path: &Path) -> Result<Vec<Statement>, String> {
    let source = fs::read_to_string(path)
        .map_err(|err| format!("Failed to read `{}`: {}", path.display(), err))?;
    let display_path = path.to_string_lossy().to_string();
    let document = analyze_source(&display_path, &source)?;
    if document.mode != SourceMode::Script {
        return Err(format!(
            "{}:1:1: The Web entry file must be a regular BT script",
            display_path
        ));
    }
    let tokens: Vec<Token> = tokenize(&document.body).collect();
    let mut parser = Parser::new(display_path, &document.body, tokens);
    parser.parse().map_err(|err| err.to_string())
}

/// Compiles and executes a BT file, returning the VM, chunk, and output.
#[allow(dead_code)]
pub fn run_file_with_chunk(path: &Path) -> Result<(Vm, crate::bytecode::Chunk, String), String> {
    let path = path
        .canonicalize()
        .map_err(|err| format!("Failed to resolve path `{}`: {}", path.display(), err))?;
    let statements = parse_file(&path)?;
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let source_file = bt_path::path_text(&path);
    let chunk = Compiler::with_source_file(source_file, base_dir)
        .compile(&statements)
        .map_err(|err| err.to_string())?;
    let mut vm = Vm::with_project_root(base_dir);
    vm.load_project_extensions()?;
    let output = match vm.run(&chunk) {
        Ok(output) => output,
        Err(err) => return Err(format_error_after_output(vm.output(), err)),
    };
    Ok((vm, chunk, output))
}

/// Calculates the number of Web runner threads.
///
/// The default retains the previous `CPU * 2` policy, so a 2-core server continues to use 4
/// workers. Set `BT_WEB_WORKERS` to override it explicitly; invalid values or values below 1 fall
/// back to the default policy.
pub fn worker_threads() -> usize {
    std::env::var("BT_WEB_WORKERS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or_else(|| num_cpus::get().max(1) * 2)
}

/// Starts the Web service.
#[allow(dead_code)]
pub async fn serve(config: WebConfig) -> Result<(), String> {
    serve_with_start_signal(config, None).await
}

/// Starts the Web service and notifies the caller after the listener has been bound.
pub async fn serve_with_start_signal(
    config: WebConfig,
    started: Option<Sender<Result<(), String>>>,
) -> Result<(), String> {
    let mut started = started;
    crate::io::ensure_rustls_provider();
    if config.sites.is_empty() {
        return startup_error(
            &mut started,
            "The Web service requires at least one site configuration".to_string(),
        );
    }

    let session_handler = match SessionHandler::builder(
        CookieStore::new(),
        b"btlangbtlangbtlangbtlangbtlangbtlangbtlangbtlangbtlangbtlangborg",
    )
    .cookie_name(BT_SESSION_COOKIE_NAME)
    .build()
    {
        Ok(handler) => handler,
        Err(err) => {
            return startup_error(
                &mut started,
                format!("Failed to create session handler: {}", err),
            );
        }
    };

    let mut router = Router::new().hoop(session_handler);
    for site in &config.sites {
        if site.domains.is_empty() {
            let site_router = match site_router(site) {
                Ok(router) => router,
                Err(err) => return startup_error(&mut started, err),
            };
            router = router.push(site_router);
        } else {
            for domain in &site.domains {
                let site_router = match site_router(site) {
                    Ok(router) => router,
                    Err(err) => return startup_error(&mut started, err),
                };
                router = router.push(Router::with_host(domain.clone()).push(site_router));
            }
        }
    }

    let service = Service::new(router).catcher(Catcher::default().hoop(handle404));
    let bind_addr = socket_bind_addr(&config.bind_host, config.web_port);
    if config.ssl_open {
        let cert = match fs::read(&config.ssl_cert_file) {
            Ok(cert) => cert,
            Err(err) => {
                return startup_error(
                    &mut started,
                    format!(
                        "Failed to read TLS certificate `{}`: {}",
                        config.ssl_cert_file, err
                    ),
                )
            }
        };
        let key = match fs::read(&config.ssl_key_file) {
            Ok(key) => key,
            Err(err) => {
                return startup_error(
                    &mut started,
                    format!(
                        "Failed to read TLS private key `{}`: {}",
                        config.ssl_key_file, err
                    ),
                )
            }
        };
        let tls = RustlsConfig::new(Keycert::new().cert(cert.as_slice()).key(key.as_slice()));
        let listener = match TcpListener::new(bind_addr.clone())
            .rustls(tls)
            .try_bind()
            .await
        {
            Ok(listener) => listener,
            Err(err) => {
                return startup_error(
                    &mut started,
                    bind_error_message(&bind_addr, config.web_port, err),
                )
            }
        };
        print_web_service_started(&config);
        notify_startup(&mut started, Ok(()));
        Server::new(listener).serve(service).await;
    } else {
        let listener = match TcpListener::new(bind_addr.clone()).try_bind().await {
            Ok(listener) => listener,
            Err(err) => {
                return startup_error(
                    &mut started,
                    bind_error_message(&bind_addr, config.web_port, err),
                )
            }
        };
        print_web_service_started(&config);
        notify_startup(&mut started, Ok(()));
        Server::new(listener).serve(service).await;
    }
    Ok(())
}

/// Prints the Web service address after the listener starts successfully.
fn print_web_service_started(config: &WebConfig) {
    println!(
        "Web service: {}://{}:{}",
        if config.ssl_open { "https" } else { "http" },
        display_host(config),
        config.web_port
    );
}

/// Builds the routing tree for a single site.
fn site_router(site: &WebSiteConfig) -> Result<Router, String> {
    let project_root = PathBuf::from(&site.project_path);
    #[cfg(feature = "extensions")]
    let extension_manager = Vm::project_extension_manager(&project_root)?;
    #[cfg(not(feature = "extensions"))]
    Vm::check_project_extensions_available(&project_root)?;
    let state = SiteState {
        entry_path: project_root.join(&site.entry_file),
        #[cfg(feature = "extensions")]
        extension_manager,
        project_root,
        file_temp_path: PathBuf::from(&site.file_temp_path),
    };
    let mut router = Router::new().hoop(affix_state::inject(state));

    if site.web_static_open
        && !site.web_static_path.is_empty()
        && !site.web_static_router.is_empty()
    {
        let mut dir = StaticDir::new(site.web_static_path.clone())
            .include_dot_files(false)
            .auto_list(site.web_static_list);
        if site.web_static_chunk_size > 0 {
            dir = dir.chunk_size(site.web_static_chunk_size);
        }
        if !site.web_static_default.is_empty() {
            dir = dir.defaults(site.web_static_default.clone());
        }
        let mut static_router = Router::with_path(site.web_static_router.clone()).get(dir);
        if !site.web_static_cache_control.trim().is_empty() {
            static_router =
                static_router.hoop(StaticCacheControl::new(&site.web_static_cache_control)?);
        }
        router = router.push(static_router);
    }

    Ok(router.push(
        Router::with_path("{**}")
            .get(main_page)
            .post(main_page)
            .put(main_page)
            .delete(main_page)
            .patch(main_page)
            .head(main_page)
            .options(main_page),
    ))
}

/// Cache-Control response header handler for static files.
#[derive(Clone, Debug)]
struct StaticCacheControl {
    /// Validated Cache-Control header value.
    value: HeaderValue,
}

impl StaticCacheControl {
    /// Creates a static file Cache-Control handler.
    fn new(value: &str) -> Result<Self, String> {
        Ok(Self {
            value: value.parse().map_err(|err| {
                format!("Invalid static file cache_control configuration: {}", err)
            })?,
        })
    }
}

#[async_trait]
impl Handler for StaticCacheControl {
    /// Writes Cache-Control after the static file response has been generated.
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        ctrl.call_next(req, depot, res).await;
        if should_write_static_cache_control(res.status_code) {
            res.headers_mut().insert(CACHE_CONTROL, self.value.clone());
        }
    }
}

/// Determines whether Cache-Control should be added to a static response.
fn should_write_static_cache_control(status: Option<StatusCode>) -> bool {
    matches!(
        status.unwrap_or(StatusCode::OK),
        StatusCode::OK | StatusCode::PARTIAL_CONTENT | StatusCode::NOT_MODIFIED
    )
}

/// Sends the Web service startup result.
fn notify_startup(started: &mut Option<Sender<Result<(), String>>>, result: Result<(), String>) {
    if let Some(sender) = started.take() {
        let _ = sender.send(result);
    }
}

/// Returns a startup error and notifies the caller at the same time.
fn startup_error<T>(
    started: &mut Option<Sender<Result<(), String>>>,
    message: String,
) -> Result<T, String> {
    notify_startup(started, Err(message.clone()));
    Err(message)
}

/// Formats a listener binding error.
fn bind_error_message(bind_addr: &str, port: u16, err: salvo::Error) -> String {
    if matches!(&err, salvo::Error::Io(io) if io.kind() == ErrorKind::AddrInUse) {
        return format!(
            "Port {} is already in use; close the program using it",
            port
        );
    }
    format!(
        "Failed to start Web service listener on `{}`: {}",
        bind_addr, err
    )
}

/// Returns the host name displayed in the console.
fn display_host(config: &WebConfig) -> &str {
    config
        .sites
        .first()
        .and_then(|site| site.domains.first())
        .map(String::as_str)
        .filter(|domain| !domain.is_empty())
        .unwrap_or(&config.bind_host)
}

/// Custom 404 response.
#[handler]
async fn handle404(res: &mut Response, ctrl: &mut FlowCtrl) {
    if StatusCode::NOT_FOUND == res.status_code.unwrap_or(StatusCode::NOT_FOUND) {
        res.render("BT 404");
        ctrl.skip_rest();
    }
}

/// Web request entry point.
#[handler]
async fn main_page(req: &mut Request, res: &mut Response, depot: &mut Depot) {
    match eval_request(req, res, depot).await {
        Ok(()) => {}
        Err(err) => {
            res.status_code(web_error_status(&err));
            write_utf8_error_headers(res);
            let _ = res.write_body(err);
        }
    }
}

/// Executes one BT Web request.
async fn eval_request(
    req: &mut Request,
    res: &mut Response,
    depot: &mut Depot,
) -> Result<(), String> {
    let state = depot
        .get_typed::<SiteState>()
        .map_err(|_| "Web state was not injected".to_string())?
        .clone();
    let limits = web_resource_limits()?;
    let snapshot = build_http_snapshot(req, depot, &state, limits).await?;
    let result =
        crate::io::run_blocking_async(None, move || eval_request_blocking(state, snapshot)).await?;
    if result.response.file.is_none() && result.content.len() > limits.response_body_bytes {
        return Err(format!(
            "Web response body size of {} bytes exceeds the {}-byte limit",
            result.content.len(),
            limits.response_body_bytes
        ));
    }

    let response_file = result.response.file.clone();
    write_response_controls(res, &result.response, response_file.is_none())?;
    write_cookies(req, res, result.cookies.as_ref())?;
    write_session(depot, result.session)?;
    if let Some(path) = response_file {
        res.send_file(PathBuf::from(path), req.headers()).await;
    } else if let Err(err) = res.write_body(result.content) {
        eprintln!("Failed to write response body: {}", err);
    }
    Ok(())
}

/// Executes one Web request script in the BT blocking pool.
fn eval_request_blocking(
    state: SiteState,
    snapshot: WebRequestSnapshot,
) -> Result<WebEvalResult, String> {
    let context = request_context_from_snapshot(snapshot)?;
    let response_state = Rc::new(RefCell::new(BtWebResponse::new()));
    let mut vm = Vm::with_project_root(&state.project_root);
    #[cfg(feature = "extensions")]
    vm.set_extension_manager(state.extension_manager.clone())?;
    vm.set_web_response(response_state.clone());
    inject_context(&mut vm, context);

    let chunk = compile_cached_file(&state.entry_path, false)?;
    let (output, value) = match vm.run_with_value_owned(chunk) {
        Ok(result) => result,
        Err(err) => return Err(format_error_after_output(vm.output(), err)),
    };
    let content = if output.is_empty() {
        value.to_string()
    } else {
        output
    };
    let response = response_state.borrow().clone();
    let cookies = cookie_map_from_value(web_context_field(&vm, "cookie"));
    let session = web_context_field(&vm, "session")
        .as_ref()
        .map(value_to_json)
        .unwrap_or(serde_json::Value::Null);

    Ok(WebEvalResult {
        content,
        response,
        cookies,
        session,
    })
}

/// Places runtime errors after existing script output so printed content appears first during Web debugging.
fn format_error_after_output(output: &str, err: impl Display) -> String {
    if output.is_empty() {
        return err.to_string();
    }
    let err = err.to_string();
    let mut text = String::with_capacity(output.len() + 1 + err.len());
    text.push_str(output);
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&err);
    text
}

/// Web request context.
struct WebRequestContext {
    /// The `web` request context object injected into the script.
    web: Value,
}

/// Result of executing a Web request script.
struct WebEvalResult {
    /// Response body.
    content: String,
    /// Response control state set by the script.
    response: BtWebResponse,
    /// Cookie field after the script finishes; `None` for non-objects means cookies are unchanged.
    cookies: Option<IndexMap<String, String>>,
    /// Session JSON value after the script finishes.
    session: serde_json::Value,
}

/// Web request snapshot that can be moved across threads.
struct WebRequestSnapshot {
    /// Request method.
    method: String,
    /// Request path without the leading slash.
    url: String,
    /// Raw query string.
    query: String,
    /// Form field snapshot.
    post: Vec<WebFormFieldSnapshot>,
    /// Uploaded file snapshot.
    files: Vec<WebUploadFileSnapshot>,
    /// Cookie snapshot.
    cookies: Vec<(String, String)>,
    /// Raw Session JSON text.
    session_json: String,
    /// Server and connection information snapshot.
    server: WebServerSnapshot,
}

/// Web form field snapshot.
struct WebFormFieldSnapshot {
    /// Field name.
    name: String,
    /// Field value list.
    values: Vec<String>,
}

/// Web uploaded file snapshot.
struct WebUploadFileSnapshot {
    /// Form field name.
    field_name: String,
    /// Salvo temporary file path.
    source: PathBuf,
    /// Destination path in the BT Web temporary directory.
    dest: PathBuf,
    /// File name submitted by the browser.
    filename: String,
    /// File size.
    size: u64,
    /// File MIME type.
    content_type: String,
}

/// Web server information snapshot.
struct WebServerSnapshot {
    /// Request method.
    method: String,
    /// HTTP version text.
    version: String,
    /// Request scheme.
    scheme: String,
    /// Request headers.
    headers: Vec<(String, String)>,
    /// Local listening address.
    local_addr: String,
    /// Remote address.
    remote_addr: String,
    /// Remote IP address.
    ip: Option<String>,
    /// Remote port.
    port: Option<u16>,
}

/// Builds an HTTP request snapshot that can be moved across threads.
async fn build_http_snapshot(
    req: &mut Request,
    depot: &mut Depot,
    state: &SiteState,
    limits: &WebResourceLimits,
) -> Result<WebRequestSnapshot, String> {
    validate_request_headers(req, limits)?;
    validate_content_length(req, limits)?;
    req.set_secure_max_size(limits.request_body_bytes);
    let form = if should_parse_form_data(req.headers().get("content-type")) {
        Some(req.form_data().await.map_err(|err| {
            format!(
                "Web request body parsing failed or exceeded the {}-byte limit: {}",
                limits.request_body_bytes, err
            )
        })?)
    } else {
        None
    };
    let post = post_snapshot(form);
    let files = files_snapshot(form, &state.file_temp_path, limits)?;
    let cookies = cookie_snapshot(req);
    let session_json = depot
        .session()
        .and_then(|session| session.get::<String>("__bt_session_json"))
        .unwrap_or_default();
    let method = req.method().to_string();
    let url = req.uri().path().trim_start_matches('/').to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let server = server_snapshot(req);

    Ok(WebRequestSnapshot {
        method,
        url,
        query,
        post,
        files,
        cookies,
        session_json,
        server,
    })
}

/// Determines whether the current request body should be parsed as form data.
fn should_parse_form_data(content_type: Option<&HeaderValue>) -> bool {
    let Some(content_type) = content_type else {
        return false;
    };
    let Ok(content_type) = content_type.to_str() else {
        return false;
    };
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    matches!(
        media_type.as_str(),
        "application/x-www-form-urlencoded" | "multipart/form-data"
    )
}

/// Validates the request header count and cumulative byte size.
fn validate_request_headers(req: &Request, limits: &WebResourceLimits) -> Result<(), String> {
    let count = req.headers().len();
    let bytes = req.headers().iter().fold(0usize, |total, (key, value)| {
        total
            .saturating_add(key.as_str().len())
            .saturating_add(value.as_bytes().len())
    });
    validate_header_totals(count, bytes, limits)
}

/// Validates aggregate request header information.
fn validate_header_totals(
    count: usize,
    bytes: usize,
    limits: &WebResourceLimits,
) -> Result<(), String> {
    if count > limits.header_count {
        return Err(format!(
            "Web request header count of {} exceeds the limit of {}",
            count, limits.header_count
        ));
    }
    if bytes > limits.header_bytes {
        return Err(format!(
            "Web request headers have a total size of {} bytes, exceeding the {}-byte limit",
            bytes, limits.header_bytes
        ));
    }
    Ok(())
}

/// Validates the request body size declared by Content-Length.
fn validate_content_length(req: &Request, limits: &WebResourceLimits) -> Result<(), String> {
    let Some(value) = req.headers().get("content-length") else {
        return Ok(());
    };
    let text = value
        .to_str()
        .map_err(|_| "Web request Content-Length is not valid text".to_string())?;
    let length = text.trim().parse::<usize>().map_err(|_| {
        format!(
            "Web request Content-Length `{}` is not a valid integer",
            text
        )
    })?;
    if length > limits.request_body_bytes {
        return Err(format!(
            "Web request body size of {} bytes exceeds the {}-byte limit",
            length, limits.request_body_bytes
        ));
    }
    Ok(())
}

/// Returns an HTTP status code based on the error type.
fn web_error_status(message: &str) -> StatusCode {
    if message.starts_with("Web request") || message.starts_with("Web upload") {
        StatusCode::PAYLOAD_TOO_LARGE
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

/// Returns the Web resource limit configuration.
fn web_resource_limits() -> Result<&'static WebResourceLimits, String> {
    match WEB_RESOURCE_LIMITS.get_or_init(WebResourceLimits::from_env) {
        Ok(limits) => Ok(limits),
        Err(err) => Err(err.clone()),
    }
}

impl WebResourceLimits {
    /// Reads Web resource limits from environment variables.
    fn from_env() -> Result<Self, String> {
        Ok(Self {
            request_body_bytes: read_web_usize_env(
                "BT_WEB_REQUEST_BODY_LIMIT",
                DEFAULT_WEB_REQUEST_BODY_LIMIT,
                1024,
                MAX_WEB_REQUEST_BODY_LIMIT,
            )?,
            response_body_bytes: read_web_usize_env(
                "BT_WEB_RESPONSE_BODY_LIMIT",
                DEFAULT_WEB_RESPONSE_BODY_LIMIT,
                1024,
                MAX_WEB_RESPONSE_BODY_LIMIT,
            )?,
            header_count: read_web_usize_env(
                "BT_WEB_HEADER_COUNT_LIMIT",
                DEFAULT_WEB_HEADER_COUNT_LIMIT,
                1,
                MAX_WEB_HEADER_COUNT_LIMIT,
            )?,
            header_bytes: read_web_usize_env(
                "BT_WEB_HEADER_BYTES_LIMIT",
                DEFAULT_WEB_HEADER_BYTES_LIMIT,
                1024,
                MAX_WEB_HEADER_BYTES_LIMIT,
            )?,
            upload_file_bytes: read_web_usize_env(
                "BT_WEB_UPLOAD_FILE_LIMIT",
                DEFAULT_WEB_UPLOAD_FILE_LIMIT,
                1024,
                MAX_WEB_UPLOAD_FILE_LIMIT,
            )?,
        })
    }
}

/// Reads a Web `usize` environment variable.
fn read_web_usize_env(name: &str, default: usize, min: usize, max: usize) -> Result<usize, String> {
    let Some(value) = std::env::var(name).ok() else {
        return Ok(default);
    };
    parse_web_usize(name, &value, min, max)
}

/// Parses a Web `usize` configuration value.
fn parse_web_usize(name: &str, value: &str, min: usize, max: usize) -> Result<usize, String> {
    let parsed = value
        .trim()
        .parse::<usize>()
        .map_err(|_| format!("{} must be an integer between {} and {}", name, min, max))?;
    if parsed < min || parsed > max {
        return Err(format!(
            "{} must be an integer between {} and {}",
            name, min, max
        ));
    }
    Ok(parsed)
}

/// Builds the script-visible HTTP context from a request snapshot.
fn request_context_from_snapshot(
    snapshot: WebRequestSnapshot,
) -> Result<WebRequestContext, String> {
    let get = query_object(&snapshot.query);
    let post = post_value(snapshot.post);
    let files = files_value(snapshot.files)?;
    let cookie = pair_object_value(snapshot.cookies);
    let session = session_value(snapshot.session_json);
    let method = Value::Str(snapshot.method.clone());
    let url = Value::Str(snapshot.url);
    let server = server_value(snapshot.server);

    let mut web = IndexMap::new();
    web.insert("url".to_string(), url.clone());
    web.insert("get".to_string(), get.clone());
    web.insert("post".to_string(), post.clone());
    web.insert("method".to_string(), method.clone());
    web.insert("server".to_string(), server.clone());
    web.insert("files".to_string(), files.clone());
    web.insert("cookie".to_string(), cookie.clone());
    web.insert("session".to_string(), session.clone());
    web.insert(
        "header".to_string(),
        Value::NativeFunction("header".to_string()),
    );
    web.insert(
        "status_code".to_string(),
        Value::NativeFunction("status_code".to_string()),
    );
    web.insert(
        "redirect".to_string(),
        Value::NativeFunction("redirect".to_string()),
    );
    web.insert(
        "send_file".to_string(),
        Value::NativeFunction("send_file".to_string()),
    );

    Ok(WebRequestContext {
        web: object_value(web),
    })
}

/// Injects the request context.
fn inject_context(vm: &mut Vm, context: WebRequestContext) {
    vm.set_global("web", context.web);
}

/// Reads a field from the `web` request context.
fn web_context_field(vm: &Vm, key: &str) -> Option<Value> {
    let Value::Object(values) = vm.get_global("web")? else {
        return None;
    };
    values.borrow().get(key).cloned()
}

/// Captures a Cookie snapshot.
fn cookie_snapshot(req: &Request) -> Vec<(String, String)> {
    req.cookies()
        .iter()
        .filter(|cookie| !is_reserved_session_cookie(cookie.name()))
        .map(|cookie| (cookie.name().to_string(), cookie.value().to_string()))
        .collect()
}

/// Returns whether a cookie name belongs to the BT session middleware rather than user code.
fn is_reserved_session_cookie(name: &str) -> bool {
    matches!(name, BT_SESSION_COOKIE_NAME | LEGACY_SESSION_COOKIE_NAME)
}

/// Converts key-value pairs into a BT object.
fn pair_object_value(items: Vec<(String, String)>) -> Value {
    object_value(
        items
            .into_iter()
            .map(|(key, value)| (key, Value::Str(value)))
            .collect(),
    )
}

/// Builds a Session object.
fn session_value(text: String) -> Value {
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .map(json_to_value)
        .unwrap_or_else(|| object_value(IndexMap::new()))
}

/// Builds a GET parameter object.
fn query_object(query: &str) -> Value {
    let mut values = IndexMap::new();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        values.insert(key.to_string(), Value::Str(value.to_string()));
    }
    object_value(values)
}

/// Captures a POST form field snapshot.
fn post_snapshot(form: Option<&salvo::http::form::FormData>) -> Vec<WebFormFieldSnapshot> {
    let Some(form) = form else {
        return Vec::new();
    };
    let mut fields = Vec::new();
    for (name, values) in &form.fields {
        fields.push(WebFormFieldSnapshot {
            name: name.clone(),
            values: values.clone(),
        });
    }
    fields
}

/// Builds a POST form object.
fn post_value(fields: Vec<WebFormFieldSnapshot>) -> Value {
    let mut values = IndexMap::new();
    for field in fields {
        if field.values.len() == 1 {
            values.insert(field.name, Value::Str(field.values[0].clone()));
        } else {
            values.insert(
                field.name,
                Value::Array(Rc::new(RefCell::new(
                    field.values.into_iter().map(Value::Str).collect(),
                ))),
            );
        }
    }
    object_value(values)
}

/// Captures an uploaded file snapshot.
fn files_snapshot(
    form: Option<&salvo::http::form::FormData>,
    temp_dir: &Path,
    limits: &WebResourceLimits,
) -> Result<Vec<WebUploadFileSnapshot>, String> {
    let Some(form) = form else {
        return Ok(Vec::new());
    };
    let mut snapshots = Vec::new();
    for (name, files) in &form.files {
        for file in files {
            if file.size() > limits.upload_file_bytes as u64 {
                return Err(format!(
                    "Web upload file `{}` has a size of {} bytes, exceeding the {}-byte limit",
                    file.name().unwrap_or("upload"),
                    file.size(),
                    limits.upload_file_bytes
                ));
            }
            let source = file.path();
            let file_name = source
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("upload.tmp");
            snapshots.push(WebUploadFileSnapshot {
                field_name: name.clone(),
                source: source.to_path_buf(),
                dest: temp_dir.join(file_name),
                filename: file.name().unwrap_or_default().to_string(),
                size: file.size(),
                content_type: file
                    .content_type()
                    .map(|mime| mime.to_string())
                    .unwrap_or_default(),
            });
        }
    }
    Ok(snapshots)
}

/// Builds an uploaded file object.
fn files_value(files: Vec<WebUploadFileSnapshot>) -> Result<Value, String> {
    let mut values: IndexMap<String, Value> = IndexMap::new();
    if let Some(parent) = files.first().and_then(|file| file.dest.parent()) {
        fs::create_dir_all(parent).map_err(|err| {
            format!(
                "Failed to create upload temporary directory `{}`: {}",
                parent.display(),
                err
            )
        })?;
    }
    for file in files {
        fs::copy(&file.source, &file.dest).map_err(|err| {
            format!(
                "Failed to save uploaded file to `{}`: {}",
                file.dest.display(),
                err
            )
        })?;
        let mut info = IndexMap::new();
        info.insert("filename".to_string(), Value::Str(file.filename));
        info.insert("size".to_string(), Value::Int(file.size as i64));
        info.insert("type".to_string(), Value::Str(file.content_type));
        info.insert(
            "path".to_string(),
            Value::Str(file.dest.to_string_lossy().to_string()),
        );
        info.insert("name".to_string(), Value::Str(file.field_name.clone()));
        info.insert("file".to_string(), Value::Fs(BtFs::from_path(file.dest)));
        let item = object_value(info);
        let entry = values
            .entry(file.field_name)
            .or_insert_with(|| Value::Array(Rc::new(RefCell::new(Vec::new()))));
        if let Value::Array(items) = entry {
            items.borrow_mut().push(item);
        }
    }
    Ok(object_value(values))
}

/// Captures a server information snapshot.
fn server_snapshot(req: &Request) -> WebServerSnapshot {
    let headers = req
        .headers()
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_str().unwrap_or("").to_string()))
        .collect();
    let addr = req.remote_addr();
    WebServerSnapshot {
        method: req.method().to_string(),
        version: format!("{:?}", req.version()),
        scheme: req.scheme().to_string(),
        headers,
        local_addr: req.local_addr().to_string(),
        remote_addr: addr.to_string(),
        ip: addr.ip().map(|ip| ip.to_string()),
        port: addr.port(),
    }
}

/// Builds a server information object.
fn server_value(server: WebServerSnapshot) -> Value {
    let headers = object_value(
        server
            .headers
            .into_iter()
            .map(|(key, value)| (key, Value::Str(value)))
            .collect(),
    );
    let mut values = IndexMap::new();
    values.insert("method".to_string(), Value::Str(server.method));
    values.insert("version".to_string(), Value::Str(server.version));
    values.insert("scheme".to_string(), Value::Str(server.scheme));
    values.insert("headers".to_string(), headers);
    values.insert("local_addr".to_string(), Value::Str(server.local_addr));
    values.insert("remote_addr".to_string(), Value::Str(server.remote_addr));
    values.insert(
        "ip".to_string(),
        server.ip.map(Value::Str).unwrap_or(Value::Null),
    );
    values.insert(
        "port".to_string(),
        server
            .port
            .map(|port| Value::Int(port as i64))
            .unwrap_or(Value::Null),
    );
    object_value(values)
}

/// Writes response controls.
fn write_response_controls(
    res: &mut Response,
    control: &BtWebResponse,
    default_content_type: bool,
) -> Result<(), String> {
    res.headers_mut().insert(
        "Server",
        "BT".parse()
            .map_err(|err| format!("Failed to parse response header: {}", err))?,
    );
    if default_content_type {
        res.headers_mut().insert(
            "Content-Type",
            "text/html; charset=utf-8"
                .parse()
                .map_err(|err| format!("Failed to parse response header: {}", err))?,
        );
    }
    for (key, value) in &control.headers {
        let name = HeaderName::from_bytes(key.as_bytes())
            .map_err(|err| format!("Invalid response header `{}`: {}", key, err))?;
        res.headers_mut().insert(
            name,
            value
                .parse()
                .map_err(|err| format!("Invalid value for response header `{}`: {}", key, err))?,
        );
    }
    if let Some(status) = control.status_code {
        let status = StatusCode::from_u16(status)
            .map_err(|err| format!("Invalid HTTP status code `{}`: {}", status, err))?;
        res.status_code(status);
    }
    if let Some(url) = &control.redirect {
        res.status_code(StatusCode::FOUND);
        res.headers_mut().insert(
            "Location",
            url.parse()
                .map_err(|err| format!("Invalid redirect URL `{}`: {}", url, err))?,
        );
    }
    Ok(())
}

/// Writes UTF-8 error response headers.
///
/// An error may occur before the business script executes, before normal response headers can be
/// written. This explicitly marks error responses with `charset=utf-8` to prevent browsers or
/// proxies from guessing GBK/ANSI and garbling non-ASCII text.
fn write_utf8_error_headers(res: &mut Response) {
    if let Ok(value) = "text/plain; charset=utf-8".parse() {
        res.headers_mut().insert("Content-Type", value);
    }
    if let Ok(value) = "BT".parse() {
        res.headers_mut().insert("Server", value);
    }
}

/// Writes Cookies back to the response.
fn write_cookies(
    req: &Request,
    res: &mut Response,
    values: Option<&IndexMap<String, String>>,
) -> Result<(), String> {
    let Some(values) = values else {
        return Ok(());
    };
    let expired_time = OffsetDateTime::now_utc() - Duration::days(1);
    for cookie in req.cookies().iter() {
        if cookie.name() == BT_SESSION_COOKIE_NAME {
            continue;
        }
        if cookie.name() == LEGACY_SESSION_COOKIE_NAME {
            let mut expired = Cookie::new(LEGACY_SESSION_COOKIE_NAME, "");
            expired.set_path("/");
            expired.set_expires(expired_time);
            res.add_cookie(expired);
            continue;
        }
        if !values.contains_key(cookie.name()) {
            let mut expired = Cookie::new(cookie.name().to_string(), "");
            expired.set_expires(expired_time);
            res.add_cookie(expired);
        }
    }
    for (key, value) in values.iter() {
        if is_reserved_session_cookie(key) {
            continue;
        }
        res.add_cookie(Cookie::new(key.clone(), value.clone()));
    }
    Ok(())
}

/// Writes the Session back to the depot.
fn write_session(depot: &mut Depot, value: serde_json::Value) -> Result<(), String> {
    let mut session = Session::new();
    session
        .insert(
            "__bt_session_json",
            serde_json::to_string(&value).unwrap_or_default(),
        )
        .map_err(|err| format!("Failed to write Session: {}", err))?;
    depot.set_session(session);
    Ok(())
}

/// Converts the Cookie object after script completion into a string map that can cross threads.
fn cookie_map_from_value(value: Option<Value>) -> Option<IndexMap<String, String>> {
    let Some(Value::Object(values)) = value else {
        return None;
    };
    let map = values
        .borrow()
        .iter()
        .map(|(key, value)| (key.clone(), value.to_string()))
        .collect();
    Some(map)
}

/// Builds the listening address used by Salvo.
fn socket_bind_addr(host: &str, port: u16) -> String {
    let host = host.trim();
    if host.contains(':') && !host.starts_with('[') {
        format!("[{}]:{}", host, port)
    } else {
        format!("{}:{}", host, port)
    }
}

/// Builds an object value.
fn object_value(values: IndexMap<String, Value>) -> Value {
    Value::Object(Rc::new(RefCell::new(values)))
}

/// Converts a JSON value into a BT value.
fn json_to_value(value: serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(value) => Value::Bool(value),
        serde_json::Value::Number(value) => value
            .as_i64()
            .map(Value::Int)
            .or_else(|| value.as_f64().map(Value::Float))
            .unwrap_or(Value::Null),
        serde_json::Value::String(value) => Value::Str(value),
        serde_json::Value::Array(values) => Value::Array(Rc::new(RefCell::new(
            values.into_iter().map(json_to_value).collect(),
        ))),
        serde_json::Value::Object(values) => object_value(
            values
                .into_iter()
                .map(|(key, value)| (key, json_to_value(value)))
                .collect(),
        ),
    }
}

/// Converts a BT value into a JSON value.
fn value_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::Null | Value::Empty => serde_json::Value::Null,
        Value::Bool(value) => serde_json::Value::Bool(*value),
        Value::Int(value) => serde_json::Value::Number((*value).into()),
        Value::Float(value) => serde_json::Number::from_f64(*value)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Str(value) => serde_json::Value::String(value.clone()),
        Value::Array(values) => {
            serde_json::Value::Array(values.borrow().iter().map(value_to_json).collect())
        }
        Value::Object(values) => serde_json::Value::Object(
            values
                .borrow()
                .iter()
                .map(|(key, value)| (key.clone(), value_to_json(value)))
                .collect(),
        ),
        other => serde_json::Value::String(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Session middleware cookies must stay outside the BT user-cookie object during migration.
    #[test]
    fn reserved_session_cookie_names_cover_current_and_legacy_names() {
        assert!(is_reserved_session_cookie("bt.session.id"));
        assert!(is_reserved_session_cookie("salvo.session.id"));
        assert!(!is_reserved_session_cookie("bt_locale"));
    }

    /// Web numeric environment variables should reject non-numeric and out-of-range values.
    #[test]
    fn parse_web_usize_rejects_invalid_values() {
        assert_eq!(parse_web_usize("BT_WEB_TEST", "8", 1, 16).unwrap(), 8);
        assert!(parse_web_usize("BT_WEB_TEST", "0", 1, 16).is_err());
        assert!(parse_web_usize("BT_WEB_TEST", "17", 1, 16).is_err());
        assert!(parse_web_usize("BT_WEB_TEST", "abc", 1, 16).is_err());
    }

    /// Both header count and total size should be limited.
    #[test]
    fn validate_header_totals_rejects_over_limit() {
        let limits = WebResourceLimits {
            request_body_bytes: 1024,
            response_body_bytes: 1024,
            header_count: 2,
            header_bytes: 16,
            upload_file_bytes: 1024,
        };

        assert!(validate_header_totals(2, 16, &limits).is_ok());
        assert!(validate_header_totals(3, 16, &limits).is_err());
        assert!(validate_header_totals(2, 17, &limits).is_err());
    }

    /// Only standard form Content-Types should trigger form parsing.
    #[test]
    fn should_parse_form_data_only_accepts_form_media_types() {
        assert!(!should_parse_form_data(None));
        assert!(!should_parse_form_data(Some(&HeaderValue::from_static(
            "application/json"
        ))));
        assert!(should_parse_form_data(Some(&HeaderValue::from_static(
            "application/x-www-form-urlencoded"
        ))));
        assert!(should_parse_form_data(Some(&HeaderValue::from_static(
            "multipart/form-data; boundary=abc"
        ))));
        assert!(should_parse_form_data(Some(&HeaderValue::from_static(
            "Multipart/Form-Data; boundary=abc"
        ))));
    }

    /// Request resource limit errors should return 413; other errors should still return 500.
    #[test]
    fn web_error_status_maps_payload_errors() {
        assert_eq!(
            web_error_status("Web request body size of 2 bytes exceeds the 1-byte limit"),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            web_error_status(
                "Web upload file `a` has a size of 2 bytes, exceeding the 1-byte limit"
            ),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            web_error_status("Script execution failed"),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    /// Static cache headers should be written only for successful, partial, and not-modified responses.
    #[test]
    fn static_cache_control_only_applies_to_cacheable_static_status() {
        assert!(should_write_static_cache_control(None));
        assert!(should_write_static_cache_control(Some(StatusCode::OK)));
        assert!(should_write_static_cache_control(Some(
            StatusCode::PARTIAL_CONTENT
        )));
        assert!(should_write_static_cache_control(Some(
            StatusCode::NOT_MODIFIED
        )));
        assert!(!should_write_static_cache_control(Some(
            StatusCode::NOT_FOUND
        )));
        assert!(!should_write_static_cache_control(Some(
            StatusCode::INTERNAL_SERVER_ERROR
        )));
    }

    /// Static cache header configuration should be validated during startup construction.
    #[test]
    fn static_cache_control_rejects_invalid_header_value() {
        assert!(StaticCacheControl::new("public, max-age=3600").is_ok());
        assert!(StaticCacheControl::new("bad\r\nvalue").is_err());
    }
}
