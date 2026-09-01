//! BT HTTP request standard library.
//!
//! `reqwest(url)` creates a request builder. Configure it through chained `method/header/cookie/body/json/form/multipart/
//! timeout/query/proxy/redirect_policy/cookie_store` calls, then use `send()` to wait synchronously for a response object.
//! Because the VM is synchronous, reqwest futures run through the process-level I/O runtime.

use crate::value::Value;
use indexmap::IndexMap;
use reqwest::multipart::{Form, Part};
use reqwest::redirect::Policy;
use reqwest::{Client, Method, Proxy};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The default maximum number of entries in the HTTP client pool.
const DEFAULT_HTTP_CLIENT_POOL_LIMIT: usize = 32;
/// The maximum number of allowed entries in the HTTP client pool.
const MAX_HTTP_CLIENT_POOL_LIMIT: usize = 1024;
/// HTTP client pool default idle retention time.
const DEFAULT_HTTP_CLIENT_IDLE_TTL_MS: u64 = 300_000;
/// HTTP slow call log default threshold, 0 means closed.
const DEFAULT_HTTP_SLOW_MS: u64 = 0;

/// HTTP client pool configuration cache.
static HTTP_CLIENT_POOL_CONFIG: OnceLock<Result<HttpClientPoolConfig, String>> = OnceLock::new();
/// HTTP client pool global status.
static HTTP_CLIENT_POOL: OnceLock<Mutex<HttpClientPool>> = OnceLock::new();
/// The number of HTTP client pool hits.
static HTTP_POOL_HITS: AtomicUsize = AtomicUsize::new(0);
/// The number of HTTP client pool misses.
static HTTP_POOL_MISSES: AtomicUsize = AtomicUsize::new(0);
/// The number of HTTP client creation times.
static HTTP_POOL_CREATED: AtomicUsize = AtomicUsize::new(0);
/// The number of HTTP client pool eliminations.
static HTTP_POOL_EVICTED: AtomicUsize = AtomicUsize::new(0);
/// HTTP client pool bypass count.
static HTTP_POOL_BYPASSED: AtomicUsize = AtomicUsize::new(0);
/// The number of HTTP client creation failures.
static HTTP_POOL_BUILD_FAILED: AtomicUsize = AtomicUsize::new(0);
/// The number of HTTP slow calls.
static HTTP_SLOW_CALLS: AtomicUsize = AtomicUsize::new(0);

/// HTTP request body configuration.
#[derive(Debug, Clone, PartialEq)]
enum ReqwestBody {
    /// String request body.
    Text(String),
    /// JSON request body.
    Json(Box<Value>),
    /// `application/x-www-form-urlencoded` request body.
    Form(IndexMap<String, String>),
    /// `multipart/form-data` request body.
    Multipart(IndexMap<String, MultipartField>),
}

/// Multipart field configuration.
#[derive(Debug, Clone, PartialEq)]
enum MultipartField {
    /// Normal text field.
    Text(String),
    /// File field.
    File {
        /// Local file path.
        path: String,
        /// The file name exposed to the server when uploading.
        file_name: Option<String>,
        /// File MIME type.
        mime: Option<String>,
    },
}

/// HTTP redirection policy configuration.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RedirectPolicyConfig {
    /// Use reqwest default policy and follow up to 10 redirects.
    Default,
    /// Do not follow redirects.
    None,
    /// Follow up to the specified number of redirects.
    Limited(usize),
}

/// HTTP client pool configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HttpClientPoolConfig {
    /// Whether to enable HTTP client pool.
    enabled: bool,
    /// The maximum number of client entries retained in the pool.
    pool_limit: usize,
    /// Client idle lifetime; `0` disables idle eviction.
    idle_ttl_ms: u64,
    /// Slow-call threshold; `0` disables tracking.
    slow_ms: u64,
}

/// HTTP client pool configuration snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpPoolConfigSnapshot {
    /// Whether to enable HTTP client pool.
    pub enabled: bool,
    /// The maximum number of client entries retained in the pool.
    pub pool_limit: usize,
    /// Client idle lifetime in milliseconds.
    pub idle_ttl_ms: u64,
    /// Slow-call threshold in milliseconds; `0` disables tracking.
    pub slow_ms: u64,
    /// Configuration error, or `None` when valid.
    pub config_error: Option<String>,
}

/// HTTP client pool statistics snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpPoolStats {
    /// Current configuration snapshot.
    pub config: HttpPoolConfigSnapshot,
    /// Whether the HTTP client pool has been initialized.
    pub pool_started: bool,
    /// The number of client entries in the current pool.
    pub entries: usize,
    /// Number of pool hits.
    pub hits: usize,
    /// Number of pool misses.
    pub misses: usize,
    /// Number of clients created.
    pub created: usize,
    /// Number of clients evicted.
    pub evicted: usize,
    /// Number of requests that bypassed pooling because reuse was unsafe.
    pub bypassed: usize,
    /// The number of client creation failures.
    pub build_failed: usize,
    /// The number of slow calls.
    pub slow_calls: usize,
}

/// HTTP client pool key.
///
/// The key includes only settings that affect a reusable reqwest `Client`.
/// Per-request timeouts stay on `Request`, avoiding needless pool fragmentation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct HttpClientPoolKey {
    /// Proxy address.
    proxy: Option<String>,
    /// Whether to enable cookie storage.
    cookie_store: bool,
    /// Redirect strategy.
    redirect_policy: RedirectPolicyConfig,
}

/// HTTP client pool entry.
struct HttpClientEntry {
    /// Reusable reqwest Client.
    client: Client,
    /// Last used time.
    last_used: Instant,
}

/// HTTP client bounded cache.
struct HttpClientPool {
    /// Saved client entries grouped by configuration.
    entries: HashMap<HttpClientPoolKey, HttpClientEntry>,
}

impl HttpClientPoolConfig {
    /// Reads the HTTP client pool configuration from environment variables.
    fn from_env() -> Result<Self, String> {
        Ok(Self {
            enabled: read_bool_env("BT_HTTP_CLIENT_POOL", true)?,
            pool_limit: read_usize_env(
                "BT_HTTP_CLIENT_POOL_LIMIT",
                DEFAULT_HTTP_CLIENT_POOL_LIMIT,
                1,
                MAX_HTTP_CLIENT_POOL_LIMIT,
            )?,
            idle_ttl_ms: read_u64_env(
                "BT_HTTP_CLIENT_IDLE_TTL_MS",
                DEFAULT_HTTP_CLIENT_IDLE_TTL_MS,
            )?,
            slow_ms: read_u64_env("BT_HTTP_SLOW_MS", DEFAULT_HTTP_SLOW_MS)?,
        })
    }

    /// Returns the idle TTL.
    fn idle_ttl(&self) -> Duration {
        Duration::from_millis(self.idle_ttl_ms)
    }
}

impl HttpClientPool {
    /// Creates an empty HTTP client pool.
    fn new() -> Self {
        Self {
            entries: HashMap::with_capacity(DEFAULT_HTTP_CLIENT_POOL_LIMIT),
        }
    }

    /// Clean up expired entries.
    fn prune_idle(&mut self, now: Instant, config: &HttpClientPoolConfig) {
        if config.idle_ttl_ms == 0 {
            return;
        }
        let before = self.entries.len();
        let idle_ttl = config.idle_ttl();
        self.entries
            .retain(|_, entry| now.duration_since(entry.last_used) < idle_ttl);
        let evicted = before.saturating_sub(self.entries.len());
        if evicted > 0 {
            HTTP_POOL_EVICTED.fetch_add(evicted, Ordering::Relaxed);
        }
    }

    /// Insert new clients and evict the oldest unused entries when the upper limit is exceeded.
    fn insert(
        &mut self,
        key: HttpClientPoolKey,
        client: Client,
        now: Instant,
        config: &HttpClientPoolConfig,
    ) {
        self.prune_idle(now, config);
        if !self.entries.contains_key(&key) && self.entries.len() >= config.pool_limit {
            if let Some(oldest_key) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
            {
                self.entries.remove(&oldest_key);
                HTTP_POOL_EVICTED.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.entries.insert(
            key,
            HttpClientEntry {
                client,
                last_used: now,
            },
        );
    }
}

/// HTTP request builder.
#[derive(Debug, Clone, PartialEq)]
pub struct BtReqwest {
    /// Request address.
    url: String,
    /// HTTP method.
    method: String,
    /// Request headers.
    headers: IndexMap<String, String>,
    /// Query parameters.
    query: Vec<(String, String)>,
    /// Request body.
    body: Option<ReqwestBody>,
    /// Timeout in milliseconds.
    timeout_ms: Option<u64>,
    /// Proxy address.
    proxy: Option<String>,
    /// Whether to enable cookie storage.
    cookie_store: bool,
    /// Redirect strategy.
    redirect_policy: RedirectPolicyConfig,
}

impl BtReqwest {
    /// Creates an HTTP request object.
    pub fn new(args: Vec<Value>) -> Result<Value, String> {
        let url = args
            .first()
            .map(Value::to_string)
            .ok_or_else(|| "reqwest() requires a URL argument".to_string())?;
        Ok(Value::Reqwest(Self {
            url,
            method: "GET".to_string(),
            headers: IndexMap::new(),
            query: Vec::new(),
            body: None,
            timeout_ms: None,
            proxy: None,
            cookie_store: false,
            redirect_policy: RedirectPolicyConfig::Default,
        }))
    }

    /// Dispatches an HTTP request-builder method.
    pub fn call_method(&self, method: &str, args: Vec<Value>) -> Result<Value, String> {
        match method {
            "method" => Ok(Value::Reqwest(self.with_method(args.first()))),
            "header" => self.with_header(&args).map(Value::Reqwest),
            "cookie" => self.with_cookie(args.first()).map(Value::Reqwest),
            "cookie_store" => Ok(Value::Reqwest(self.with_cookie_store(args.first()))),
            "body" => Ok(Value::Reqwest(self.with_body(args.first()))),
            "json" => Ok(Value::Reqwest(self.with_json(args.first()))),
            "form" => self.with_form(args.first()).map(Value::Reqwest),
            "multipart" => self.with_multipart(args.first()).map(Value::Reqwest),
            "timeout" => Ok(Value::Reqwest(self.with_timeout(args.first()))),
            "query" => Ok(Value::Reqwest(self.with_query(args.first()))),
            "proxy" => Ok(Value::Reqwest(self.with_proxy(args.first()))),
            "redirect_policy" => self.with_redirect_policy(args.first()).map(Value::Reqwest),
            "send" => self.send(),
            _ => Err(format!("reqwest has no method `{}`", method)),
        }
    }

    /// Sets the request method.
    fn with_method(&self, value: Option<&Value>) -> Self {
        let mut next = self.clone();
        next.method = value
            .map(Value::to_string)
            .unwrap_or_else(|| "GET".to_string())
            .to_uppercase();
        next
    }

    /// Set request header.
    fn with_header(&self, args: &[Value]) -> Result<Self, String> {
        let mut next = self.clone();
        match args {
            [Value::Object(values)] => {
                for (key, value) in values.borrow().iter() {
                    next.headers.insert(key.clone(), value.to_string());
                }
            }
            [key, value, ..] => {
                next.headers.insert(key.to_string(), value.to_string());
            }
            _ => return Err("reqwest.header() requires object or key/value parameters".to_string()),
        }
        Ok(next)
    }

    /// Sets the Cookie request header.
    fn with_cookie(&self, value: Option<&Value>) -> Result<Self, String> {
        let mut next = self.clone();
        if let Some(value) = value {
            next.headers.insert("Cookie".to_string(), value.to_string());
        }
        Ok(next)
    }

    /// Sets the string request body.
    fn with_body(&self, value: Option<&Value>) -> Self {
        let mut next = self.clone();
        next.body = Some(ReqwestBody::Text(
            value.map(Value::to_string).unwrap_or_default(),
        ));
        next
    }

    /// Sets the JSON request body.
    fn with_json(&self, value: Option<&Value>) -> Self {
        let mut next = self.clone();
        next.body = Some(ReqwestBody::Json(Box::new(
            value.cloned().unwrap_or(Value::Null),
        )));
        next
    }

    /// Sets the form request body.
    fn with_form(&self, value: Option<&Value>) -> Result<Self, String> {
        let mut next = self.clone();
        let value =
            value.ok_or_else(|| "reqwest.form() requires an object argument".to_string())?;
        next.body = Some(ReqwestBody::Form(Self::string_map(value)?));
        Ok(next)
    }

    /// Sets the multipart request body.
    fn with_multipart(&self, value: Option<&Value>) -> Result<Self, String> {
        let mut next = self.clone();
        let value =
            value.ok_or_else(|| "reqwest.multipart() requires an object argument".to_string())?;
        next.body = Some(ReqwestBody::Multipart(Self::multipart_fields(value)?));
        Ok(next)
    }

    /// Sets the request timeout.
    fn with_timeout(&self, value: Option<&Value>) -> Self {
        let mut next = self.clone();
        next.timeout_ms = value
            .map(Value::to_i64_lossy)
            .filter(|v| *v > 0)
            .map(|v| v as u64);
        next
    }

    /// Set query parameters.
    fn with_query(&self, value: Option<&Value>) -> Self {
        let mut next = self.clone();
        if let Some(value) = value {
            let map = match Self::string_map(value) {
                Ok(map) => map,
                Err(_) => return next,
            };
            next.query.extend(map);
        }
        next
    }

    /// Set proxy address.
    fn with_proxy(&self, value: Option<&Value>) -> Self {
        let mut next = self.clone();
        next.proxy = value
            .map(Value::to_string)
            .filter(|value| !value.trim().is_empty());
        next
    }

    /// Set the cookie storage switch.
    fn with_cookie_store(&self, value: Option<&Value>) -> Self {
        let mut next = self.clone();
        next.cookie_store = value.map(Value::is_truthy).unwrap_or(true);
        next
    }

    /// Set redirection policy.
    fn with_redirect_policy(&self, value: Option<&Value>) -> Result<Self, String> {
        let mut next = self.clone();
        next.redirect_policy = match value {
            None => RedirectPolicyConfig::Default,
            Some(Value::Int(limit)) if *limit <= 0 => RedirectPolicyConfig::None,
            Some(Value::Int(limit)) => RedirectPolicyConfig::Limited(*limit as usize),
            Some(Value::Float(limit)) if *limit <= 0.0 => RedirectPolicyConfig::None,
            Some(Value::Float(limit)) => RedirectPolicyConfig::Limited(*limit as usize),
            Some(value) => Self::redirect_policy_from_text(&value.to_string())?,
        };
        Ok(next)
    }

    /// Sends the request.
    fn send(&self) -> Result<Value, String> {
        let request = self.clone();
        let start = Instant::now();
        let result = crate::io::run_async(
            async move { request.send_async().await },
            Some(self.request_timeout()),
        );
        self.record_slow_call(start.elapsed());
        result
    }

    /// Sends the request asynchronously.
    async fn send_async(&self) -> Result<Value, String> {
        let client = self.client()?;
        let method = self
            .method
            .parse::<Method>()
            .map_err(|err| format!("Invalid HTTP method `{}`: {}", self.method, err))?;
        let url = if self.query.is_empty() {
            self.url.clone()
        } else {
            let mut url = url::Url::parse(&self.url)
                .map_err(|err| format!("Invalid URL `{}`: {}", self.url, err))?;
            {
                let mut pairs = url.query_pairs_mut();
                for (key, value) in &self.query {
                    pairs.append_pair(key, value);
                }
            }
            url.to_string()
        };
        let mut request = client.request(method, &url).timeout(self.request_timeout());
        for (key, value) in &self.headers {
            request = request.header(key, value);
        }
        if let Some(body) = &self.body {
            request = match body {
                ReqwestBody::Text(body) => request.body(body.clone()),
                ReqwestBody::Json(json) => request.json(&to_json_value(json)),
                ReqwestBody::Form(form) => request.form(form),
                ReqwestBody::Multipart(fields) => {
                    request.multipart(Self::multipart_form(fields).await?)
                }
            };
        }
        let response = request
            .send()
            .await
            .map_err(|err| format!("HTTP request `{}` failed: {}", self.url, err))?;
        let status = response.status().as_u16() as i64;
        let headers = response
            .headers()
            .iter()
            .map(|(key, value)| {
                (
                    key.to_string(),
                    Value::Str(value.to_str().unwrap_or("").to_string()),
                )
            })
            .collect::<IndexMap<_, _>>();
        let text = response
            .text()
            .await
            .map_err(|err| format!("failed to read HTTP response: {}", err))?;
        let mut object = IndexMap::new();
        object.insert("status".to_string(), Value::Int(status));
        object.insert("body".to_string(), Value::Str(text));
        object.insert(
            "headers".to_string(),
            Value::Object(Rc::new(RefCell::new(headers))),
        );
        Ok(Value::Object(Rc::new(RefCell::new(object))))
    }

    /// Returns the HTTP client available for this request.
    fn client(&self) -> Result<Client, String> {
        let config = http_client_pool_config()?;
        let Some(key) = self.pool_key(config) else {
            HTTP_POOL_BYPASSED.fetch_add(1, Ordering::Relaxed);
            return self.build_client();
        };
        let now = Instant::now();
        let pool = HTTP_CLIENT_POOL.get_or_init(|| Mutex::new(HttpClientPool::new()));
        {
            let mut pool = pool
                .lock()
                .map_err(|_| "The HTTP client pool lock is poisoned".to_string())?;
            pool.prune_idle(now, config);
            if let Some(entry) = pool.entries.get_mut(&key) {
                entry.last_used = now;
                HTTP_POOL_HITS.fetch_add(1, Ordering::Relaxed);
                return Ok(entry.client.clone());
            }
        }

        HTTP_POOL_MISSES.fetch_add(1, Ordering::Relaxed);
        let client = self.build_client()?;
        HTTP_POOL_CREATED.fetch_add(1, Ordering::Relaxed);
        let mut pool = pool
            .lock()
            .map_err(|_| "The HTTP client pool lock is poisoned".to_string())?;
        if let Some(entry) = pool.entries.get_mut(&key) {
            entry.last_used = now;
            HTTP_POOL_HITS.fetch_add(1, Ordering::Relaxed);
            return Ok(entry.client.clone());
        }
        pool.insert(key, client.clone(), now, config);
        Ok(client)
    }

    /// Returns a pool key, or `None` when the client cannot be reused safely.
    fn pool_key(&self, config: &HttpClientPoolConfig) -> Option<HttpClientPoolKey> {
        if !config.enabled || self.cookie_store {
            return None;
        }
        Some(HttpClientPoolKey {
            proxy: self.proxy.clone(),
            cookie_store: self.cookie_store,
            redirect_policy: self.redirect_policy.clone(),
        })
    }

    /// Creates a reqwest Client with the current request configuration.
    fn build_client(&self) -> Result<Client, String> {
        crate::io::ensure_rustls_provider();
        let mut client = Client::builder()
            .cookie_store(self.cookie_store)
            .redirect(self.redirect_policy());
        if let Some(proxy) = &self.proxy {
            client = client.proxy(Proxy::all(proxy).map_err(|err| {
                format!("Invalid HTTP proxy `{}`: {}", redact_url_secret(proxy), err)
            })?);
        }
        client.build().map_err(|err| {
            HTTP_POOL_BUILD_FAILED.fetch_add(1, Ordering::Relaxed);
            format!("failed to create HTTP client: {}", err)
        })
    }

    /// Records HTTP calls that exceed the slow-call threshold.
    fn record_slow_call(&self, elapsed: Duration) {
        let Ok(config) = http_client_pool_config() else {
            return;
        };
        if config.slow_ms == 0 || elapsed < Duration::from_millis(config.slow_ms) {
            return;
        }
        HTTP_SLOW_CALLS.fetch_add(1, Ordering::Relaxed);
        eprintln!(
            "Slow BT HTTP call: {} {} took {} milliseconds",
            self.method,
            log_safe_url(&self.url),
            elapsed.as_millis()
        );
    }

    /// Convert object value to string map.
    fn string_map(value: &Value) -> Result<IndexMap<String, String>, String> {
        let Value::Object(values) = value else {
            return Err("The argument must be an object".to_string());
        };
        Ok(values
            .borrow()
            .iter()
            .map(|(key, value)| (key.clone(), value.to_string()))
            .collect())
    }

    /// Converts an object value to a multipart field map.
    fn multipart_fields(value: &Value) -> Result<IndexMap<String, MultipartField>, String> {
        let Value::Object(values) = value else {
            return Err("reqwest.multipart() requires an object argument".to_string());
        };
        let values = values.borrow();
        let mut fields = IndexMap::with_capacity(values.len());
        for (key, value) in values.iter() {
            fields.insert(key.clone(), Self::multipart_field(value));
        }
        Ok(fields)
    }

    /// Converts a single BT value to a multipart field.
    fn multipart_field(value: &Value) -> MultipartField {
        if let Value::Object(values) = value {
            let values = values.borrow();
            if let Some(path) = values.get("path").and_then(Self::optional_string) {
                return MultipartField::File {
                    path,
                    file_name: values.get("file_name").and_then(Self::optional_string),
                    mime: values.get("mime").and_then(Self::optional_string),
                };
            }
        }
        MultipartField::Text(value.to_string())
    }

    /// Convert multipart field mapping to reqwest form.
    async fn multipart_form(fields: &IndexMap<String, MultipartField>) -> Result<Form, String> {
        let mut form = Form::new();
        for (key, field) in fields {
            form = match field {
                MultipartField::Text(value) => form.text(key.clone(), value.clone()),
                MultipartField::File {
                    path,
                    file_name,
                    mime,
                } => {
                    let mut part = Part::file(path).await.map_err(|err| {
                        format!("failed to read multipart file `{}`: {}", path, err)
                    })?;
                    if let Some(file_name) = file_name {
                        part = part.file_name(file_name.clone());
                    }
                    if let Some(mime) = mime {
                        part = part.mime_str(mime).map_err(|err| {
                            format!("Invalid multipart MIME type `{}`: {}", mime, err)
                        })?;
                    }
                    form.part(key.clone(), part)
                }
            };
        }
        Ok(form)
    }

    /// Read optional string configuration.
    fn optional_string(value: &Value) -> Option<String> {
        match value {
            Value::Empty | Value::Null => None,
            other => {
                let text = other.to_string();
                if text.is_empty() {
                    None
                } else {
                    Some(text)
                }
            }
        }
    }

    /// Read request timeout.
    fn request_timeout(&self) -> Duration {
        self.timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(Duration::from_secs(30))
    }

    /// Reads the reqwest redirect policy.
    fn redirect_policy(&self) -> Policy {
        match self.redirect_policy {
            RedirectPolicyConfig::Default => Policy::default(),
            RedirectPolicyConfig::None => Policy::none(),
            RedirectPolicyConfig::Limited(limit) => Policy::limited(limit),
        }
    }

    /// Reads the redirection policy from a string.
    fn redirect_policy_from_text(text: &str) -> Result<RedirectPolicyConfig, String> {
        let text = text.trim().to_lowercase();
        match text.as_str() {
            "" | "default" => Ok(RedirectPolicyConfig::Default),
            "none" => Ok(RedirectPolicyConfig::None),
            _ => text
                .parse::<usize>()
                .map(RedirectPolicyConfig::Limited)
                .map_err(|_| {
                    "reqwest.redirect_policy() accepts `default`, `none`, or a maximum redirect count".to_string()
                }),
        }
    }
}

/// Returns an HTTP client-pool statistics snapshot.
pub fn stats() -> HttpPoolStats {
    let (config, config_error) = match http_client_pool_config() {
        Ok(config) => (config.clone(), None),
        Err(err) => (fallback_http_client_pool_config(), Some(err)),
    };
    let entries = HTTP_CLIENT_POOL
        .get()
        .and_then(|pool| pool.lock().ok().map(|pool| pool.entries.len()))
        .unwrap_or(0);
    HttpPoolStats {
        config: HttpPoolConfigSnapshot {
            enabled: config.enabled,
            pool_limit: config.pool_limit,
            idle_ttl_ms: config.idle_ttl_ms,
            slow_ms: config.slow_ms,
            config_error,
        },
        pool_started: HTTP_CLIENT_POOL.get().is_some(),
        entries,
        hits: HTTP_POOL_HITS.load(Ordering::Relaxed),
        misses: HTTP_POOL_MISSES.load(Ordering::Relaxed),
        created: HTTP_POOL_CREATED.load(Ordering::Relaxed),
        evicted: HTTP_POOL_EVICTED.load(Ordering::Relaxed),
        bypassed: HTTP_POOL_BYPASSED.load(Ordering::Relaxed),
        build_failed: HTTP_POOL_BUILD_FAILED.load(Ordering::Relaxed),
        slow_calls: HTTP_SLOW_CALLS.load(Ordering::Relaxed),
    }
}

/// Returns the HTTP client pool configuration.
fn http_client_pool_config() -> Result<&'static HttpClientPoolConfig, String> {
    match HTTP_CLIENT_POOL_CONFIG.get_or_init(HttpClientPoolConfig::from_env) {
        Ok(config) => Ok(config),
        Err(err) => Err(err.clone()),
    }
}

/// Returns the conservative HTTP client pool configuration used for statistics display.
fn fallback_http_client_pool_config() -> HttpClientPoolConfig {
    HttpClientPoolConfig {
        enabled: true,
        pool_limit: DEFAULT_HTTP_CLIENT_POOL_LIMIT,
        idle_ttl_ms: DEFAULT_HTTP_CLIENT_IDLE_TTL_MS,
        slow_ms: DEFAULT_HTTP_SLOW_MS,
    }
}

/// Reads a Boolean environment variable.
fn read_bool_env(name: &str, default: bool) -> Result<bool, String> {
    let Some(value) = std::env::var(name).ok() else {
        return Ok(default);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(format!("{} must be true/false or 1/0", name)),
    }
}

/// Reads a `usize` environment variable.
fn read_usize_env(name: &str, default: usize, min: usize, max: usize) -> Result<usize, String> {
    let Some(value) = std::env::var(name).ok() else {
        return Ok(default);
    };
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

/// Reads a `u64` environment variable.
fn read_u64_env(name: &str, default: u64) -> Result<u64, String> {
    let Some(value) = std::env::var(name).ok() else {
        return Ok(default);
    };
    value
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("{} must be an integer not less than 0", name))
}

/// Returns the URL text that does not reveal the account password.
fn redact_url_secret(text: &str) -> String {
    let Ok(mut url) = url::Url::parse(text) else {
        return text.to_string();
    };
    if url.password().is_some() {
        let _ = url.set_password(Some("***"));
    }
    if !url.username().is_empty() {
        let _ = url.set_username("***");
    }
    url.to_string()
}

/// Returns the URL text that the slow call log can display.
fn log_safe_url(text: &str) -> String {
    let Ok(mut url) = url::Url::parse(text) else {
        return "<invalid-url>".to_string();
    };
    url.set_query(None);
    url.set_fragment(None);
    redact_url_secret(url.as_str())
}

/// Convert BT value to serde_json value.
fn to_json_value(value: &Value) -> serde_json::Value {
    match value {
        Value::Null | Value::Empty => serde_json::Value::Null,
        Value::Bool(value) => serde_json::Value::Bool(*value),
        Value::Int(value) => serde_json::Value::Number((*value).into()),
        Value::Float(value) => serde_json::Number::from_f64(*value)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Str(value) => serde_json::Value::String(value.clone()),
        Value::Array(values) => {
            serde_json::Value::Array(values.borrow().iter().map(to_json_value).collect())
        }
        Value::Object(values) => serde_json::Value::Object(
            values
                .borrow()
                .iter()
                .map(|(key, value)| (key.clone(), to_json_value(value)))
                .collect(),
        ),
        other => serde_json::Value::String(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a request builder for testing.
    fn reqwest_builder() -> BtReqwest {
        match BtReqwest::new(vec![Value::Str("https://example.com".to_string())]).unwrap() {
            Value::Reqwest(request) => request,
            other => panic!("expected Reqwest, received {}", other.type_name()),
        }
    }

    /// Creates an object value for request-builder tests.
    fn object(values: Vec<(&str, Value)>) -> Value {
        Value::Object(Rc::new(RefCell::new(
            values
                .into_iter()
                .map(|(key, value)| (key.to_string(), value))
                .collect(),
        )))
    }

    #[test]
    fn proxy_redirect_and_cookie_store_are_real_config() {
        let proxy = Value::Str("http://127.0.0.1:7890".to_string());
        let limit = Value::Int(3);
        let request = reqwest_builder()
            .with_cookie_store(None)
            .with_proxy(Some(&proxy))
            .with_redirect_policy(Some(&limit))
            .unwrap();

        assert!(request.cookie_store);
        assert_eq!(request.proxy.as_deref(), Some("http://127.0.0.1:7890"));
        assert_eq!(request.redirect_policy, RedirectPolicyConfig::Limited(3));
    }

    /// Requests that enable cookie storage should not enter the shared pool to avoid contaminating cookies across requests.
    #[test]
    fn pooled_client_key_bypasses_cookie_store_requests() {
        let config = HttpClientPoolConfig {
            enabled: true,
            pool_limit: 8,
            idle_ttl_ms: 1_000,
            slow_ms: 0,
        };
        let pooled = reqwest_builder();
        let cookie_store = pooled.with_cookie_store(None);

        assert!(pooled.pool_key(&config).is_some());
        assert!(cookie_store.pool_key(&config).is_none());
    }

    /// URL desensitization should hide the account password, and the slow call log does not retain the query.
    #[test]
    fn url_redaction_hides_credentials_and_query() {
        assert_eq!(
            redact_url_secret("http://user:pass@127.0.0.1:8080/proxy"),
            "http://***:***@127.0.0.1:8080/proxy"
        );
        assert_eq!(
            log_safe_url("https://user:pass@example.com/path?a=1#frag"),
            "https://***:***@example.com/path"
        );
    }

    #[test]
    fn request_body_setters_keep_last_body_kind() {
        let form = object(vec![("name", Value::Str("BT".to_string()))]);
        let request = reqwest_builder()
            .with_body(Some(&Value::Str("raw".to_string())))
            .with_json(Some(&Value::Int(1)))
            .with_form(Some(&form))
            .unwrap()
            .with_multipart(Some(&form))
            .unwrap();

        assert!(matches!(request.body, Some(ReqwestBody::Multipart(_))));
    }

    #[test]
    fn multipart_file_descriptor_uses_snake_case_fields() {
        let values = object(vec![(
            "file",
            object(vec![
                ("path", Value::Str("demo.txt".to_string())),
                ("file_name", Value::Str("upload.txt".to_string())),
                ("mime", Value::Str("text/plain".to_string())),
            ]),
        )]);
        let request = reqwest_builder().with_multipart(Some(&values)).unwrap();

        let Some(ReqwestBody::Multipart(fields)) = request.body else {
            panic!("expected multipart request body");
        };
        assert_eq!(
            fields.get("file"),
            Some(&MultipartField::File {
                path: "demo.txt".to_string(),
                file_name: Some("upload.txt".to_string()),
                mime: Some("text/plain".to_string()),
            })
        );
    }
}
