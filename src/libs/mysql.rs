//! BT MySQL standard library.
//!
//! The new version of VM still maintains the synchronous register execution model; the MySQL driver uses SQLx asynchronous connection, so here, like the
//! `reqwest` standard library, it waits synchronously for SQL execution to complete through the process-level shared I/O runtime.
//! Ordinary queries reuse the bounded connection pool according to DSN; batch writing still creates a temporary pool according to this `workers()` to avoid changing the global pool
//! The concurrency boundary of batch execution.

use crate::value::Value;
use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use indexmap::IndexMap;
use sqlx::mysql::{MySqlArguments, MySqlConnection, MySqlPool, MySqlPoolOptions, MySqlRow};
use sqlx::query::Query;
use sqlx::{
    AssertSqlSafe, Column, Connection, MySql, Row, SqlSafeStr, SqlStr, Transaction, TypeInfo,
    ValueRef,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::task::JoinSet;

/// MySQL dynamic query object alias.
type BtMysqlQuery<'q> = Query<'q, MySql, MySqlArguments>;

/// Marks BT's explicitly supplied MySQL statement text as an audited SQLx query string.
///
/// BT deliberately accepts complete SQL statements through `mysql.query()`. Dynamic values added
/// through `bind()` or `binds()` remain prepared-statement parameters and are never interpolated by
/// this boundary. The returned owned SQL string also avoids borrowing query objects from VM state.
#[inline]
fn audited_mysql_sql(sql: &str) -> SqlStr {
    AssertSqlSafe(sql).into_sql_str()
}

/// MySQL batch execution default batch size.
const DEFAULT_BATCH_SIZE: usize = 0;

/// MySQL executes the default number of jobs in batches.
const DEFAULT_MYSQL_WORKERS: usize = 1;

/// The maximum number of jobs allowed for MySQL batch execution.
const MAX_MYSQL_WORKERS: usize = 4096;

/// The maximum number of system threads for the MySQL batch execution runner.
#[cfg(test)]
const MAX_MYSQL_WORKER_THREADS: usize = 64;
/// MySQL global connection pool default DSN grouping upper limit.
const DEFAULT_MYSQL_POOL_LIMIT: usize = 16;
/// MySQL global connection pool maximum DSN grouping limit.
const MAX_MYSQL_POOL_LIMIT: usize = 256;
/// The default minimum number of connections in the MySQL global connection pool.
const DEFAULT_MYSQL_POOL_MIN_CONNECTIONS: usize = 0;
/// The default maximum number of connections in the MySQL global connection pool.
const DEFAULT_MYSQL_POOL_MAX_CONNECTIONS: usize = 8;
/// The hard upper limit for the maximum number of connections in the MySQL global connection pool.
const MAX_MYSQL_POOL_CONNECTIONS: usize = 1024;
/// MySQL global connection pool default idle retention time.
const DEFAULT_MYSQL_POOL_IDLE_TTL_MS: u64 = 300_000;
/// MySQL default connection acquisition timeout.
const DEFAULT_MYSQL_CONNECT_TIMEOUT_MS: u64 = 5_000;
/// MySQL default query timeout.
const DEFAULT_MYSQL_QUERY_TIMEOUT_MS: u64 = 30_000;
/// MySQL slow call log default threshold, 0 means closed.
const DEFAULT_MYSQL_SLOW_MS: u64 = 0;

/// MySQL connection pool configuration cache.
static MYSQL_POOL_CONFIG: OnceLock<Result<MysqlPoolConfig, String>> = OnceLock::new();
/// MySQL global connection pool status.
static MYSQL_POOL_STORE: OnceLock<Mutex<MysqlPoolStore>> = OnceLock::new();
/// The number of MySQL connection pool hits.
static MYSQL_POOL_HITS: AtomicUsize = AtomicUsize::new(0);
/// The number of MySQL connection pool misses.
static MYSQL_POOL_MISSES: AtomicUsize = AtomicUsize::new(0);
/// The number of MySQL connection pool creation times.
static MYSQL_POOL_CREATED: AtomicUsize = AtomicUsize::new(0);
/// The number of MySQL connection pool eliminations.
static MYSQL_POOL_EVICTED: AtomicUsize = AtomicUsize::new(0);
/// The number of MySQL connection pool bypasses.
static MYSQL_POOL_BYPASSED: AtomicUsize = AtomicUsize::new(0);
/// The number of MySQL connection pool creation failures.
static MYSQL_POOL_BUILD_FAILED: AtomicUsize = AtomicUsize::new(0);
/// The number of MySQL slow calls.
static MYSQL_SLOW_CALLS: AtomicUsize = AtomicUsize::new(0);
/// MySQL's current number of active transactions.
static MYSQL_TRANSACTION_ACTIVE: AtomicUsize = AtomicUsize::new(0);
/// The number of MySQL transaction starts.
static MYSQL_TRANSACTION_STARTED: AtomicUsize = AtomicUsize::new(0);
/// The number of MySQL transaction commits.
static MYSQL_TRANSACTION_COMMITTED: AtomicUsize = AtomicUsize::new(0);
/// The number of MySQL transaction rollbacks.
static MYSQL_TRANSACTION_ROLLED_BACK: AtomicUsize = AtomicUsize::new(0);
/// The number of MySQL transaction closures.
static MYSQL_TRANSACTION_CLOSED: AtomicUsize = AtomicUsize::new(0);
/// The number of MySQL transaction failures.
static MYSQL_TRANSACTION_FAILED: AtomicUsize = AtomicUsize::new(0);

/// MySQL global connection pool configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MysqlPoolConfig {
    /// Whether to enable the global connection pool for normal queries.
    enabled: bool,
    /// The maximum number of DSN packets to retain.
    pool_limit: usize,
    /// Minimum number of connections per connection pool.
    min_connections: usize,
    /// The maximum number of connections per connection pool.
    max_connections: usize,
    /// Idle connection pool retention time, 0 means not to be eliminated according to idle time.
    idle_ttl_ms: u64,
    /// Connection acquisition timeout, in milliseconds.
    connect_timeout_ms: u64,
    /// Synchronization wait timeout for a single SQL call, in milliseconds.
    query_timeout_ms: u64,
    /// Slow call log threshold, in milliseconds, 0 means closed.
    slow_ms: u64,
}

/// MySQL global connection pool configuration snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MysqlPoolConfigSnapshot {
    /// Whether to enable the global connection pool for normal queries.
    pub enabled: bool,
    /// The maximum number of DSN packets to retain.
    pub pool_limit: usize,
    /// Minimum number of connections per connection pool.
    pub min_connections: usize,
    /// The maximum number of connections per connection pool.
    pub max_connections: usize,
    /// Idle connection pool retention time, in milliseconds.
    pub idle_ttl_ms: u64,
    /// Connection acquisition timeout, in milliseconds.
    pub connect_timeout_ms: u64,
    /// Synchronization wait timeout for a single SQL call, in milliseconds.
    pub query_timeout_ms: u64,
    /// Slow call log threshold, in milliseconds, 0 means closed.
    pub slow_ms: u64,
    /// Configuration parsing error; None if there is no error.
    pub config_error: Option<String>,
}

/// MySQL global connection pool statistics snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MysqlPoolStats {
    /// Current configuration snapshot.
    pub config: MysqlPoolConfigSnapshot,
    /// Whether the MySQL global connection pool has been initialized.
    pub pool_started: bool,
    /// The number of DSN packets in the current pool.
    pub entries: usize,
    /// The total number of open connections in the current SQLx pool.
    pub connections: usize,
    /// The total number of idle connections in the current SQLx pool.
    pub idle_connections: usize,
    /// Number of pool hits.
    pub hits: usize,
    /// Number of pool misses.
    pub misses: usize,
    /// Number of pool creation times.
    pub created: usize,
    /// Number of pool eliminations.
    pub evicted: usize,
    /// The number of pool bypasses due to closing the global pool.
    pub bypassed: usize,
    /// Number of pool creation failures.
    pub build_failed: usize,
    /// The number of slow calls.
    pub slow_calls: usize,
    /// The current number of active transactions.
    pub transactions_active: usize,
    /// Number of transaction starts.
    pub transactions_started: usize,
    /// Number of transaction commits.
    pub transactions_committed: usize,
    /// Number of transaction rollbacks.
    pub transactions_rolled_back: usize,
    /// The number of times the transaction was explicitly closed.
    pub transactions_closed: usize,
    /// Number of transaction failures.
    pub transactions_failed: usize,
}

/// MySQL global connection pool entry.
struct MysqlPoolEntry {
    /// SQLx MySQL connection pool.
    pool: MySqlPool,
    /// Last used time.
    last_used: Instant,
}

/// MySQL global connection pool storage.
struct MysqlPoolStore {
    /// Connection pool entries saved by DSN.
    entries: HashMap<String, MysqlPoolEntry>,
}

impl MysqlPoolConfig {
    /// Reads the MySQL connection pool configuration from environment variables.
    fn from_env() -> Result<Self, String> {
        let min_connections = read_usize_env(
            "BT_MYSQL_POOL_MIN_CONNECTIONS",
            DEFAULT_MYSQL_POOL_MIN_CONNECTIONS,
            0,
            MAX_MYSQL_POOL_CONNECTIONS,
        )?;
        let max_connections = read_usize_env(
            "BT_MYSQL_POOL_MAX_CONNECTIONS",
            DEFAULT_MYSQL_POOL_MAX_CONNECTIONS,
            1,
            MAX_MYSQL_POOL_CONNECTIONS,
        )?;
        if min_connections > max_connections {
            return Err(
                "BT_MYSQL_POOL_MIN_CONNECTIONS cannot be greater than BT_MYSQL_POOL_MAX_CONNECTIONS".to_string(),
            );
        }
        Ok(Self {
            enabled: read_bool_env("BT_MYSQL_POOL", true)?,
            pool_limit: read_usize_env(
                "BT_MYSQL_POOL_LIMIT",
                DEFAULT_MYSQL_POOL_LIMIT,
                1,
                MAX_MYSQL_POOL_LIMIT,
            )?,
            min_connections,
            max_connections,
            idle_ttl_ms: read_u64_env("BT_MYSQL_POOL_IDLE_TTL_MS", DEFAULT_MYSQL_POOL_IDLE_TTL_MS)?,
            connect_timeout_ms: read_u64_env(
                "BT_MYSQL_CONNECT_TIMEOUT_MS",
                DEFAULT_MYSQL_CONNECT_TIMEOUT_MS,
            )?,
            query_timeout_ms: read_u64_env(
                "BT_MYSQL_QUERY_TIMEOUT_MS",
                DEFAULT_MYSQL_QUERY_TIMEOUT_MS,
            )?,
            slow_ms: read_u64_env("BT_MYSQL_SLOW_MS", DEFAULT_MYSQL_SLOW_MS)?,
        })
    }

    /// Returns the idle TTL.
    fn idle_ttl(&self) -> Option<Duration> {
        (self.idle_ttl_ms > 0).then(|| Duration::from_millis(self.idle_ttl_ms))
    }

    /// returns the connection acquisition timeout.
    fn connect_timeout(&self) -> Duration {
        Duration::from_millis(self.connect_timeout_ms)
    }

    /// returns the query synchronization waiting timeout.
    fn query_timeout(&self) -> Duration {
        Duration::from_millis(self.query_timeout_ms)
    }
}

impl MysqlPoolStore {
    /// Creates an empty MySQL pool storage.
    fn new() -> Self {
        Self {
            entries: HashMap::with_capacity(DEFAULT_MYSQL_POOL_LIMIT),
        }
    }

    /// Clean up expired pool entries.
    fn prune_idle(&mut self, now: Instant, config: &MysqlPoolConfig) {
        let Some(idle_ttl) = config.idle_ttl() else {
            return;
        };
        let before = self.entries.len();
        self.entries
            .retain(|_, entry| now.duration_since(entry.last_used) < idle_ttl);
        let evicted = before.saturating_sub(self.entries.len());
        if evicted > 0 {
            MYSQL_POOL_EVICTED.fetch_add(evicted, Ordering::Relaxed);
        }
    }

    /// Insert into the connection pool and evict the oldest unused entry when the limit is exceeded.
    fn insert(&mut self, dsn: String, pool: MySqlPool, now: Instant, config: &MysqlPoolConfig) {
        self.prune_idle(now, config);
        if !self.entries.contains_key(&dsn) && self.entries.len() >= config.pool_limit {
            if let Some(oldest_key) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
            {
                self.entries.remove(&oldest_key);
                MYSQL_POOL_EVICTED.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.entries.insert(
            dsn,
            MysqlPoolEntry {
                pool,
                last_used: now,
            },
        );
    }
}

/// MySQL bind values that can move safely across asynchronous tasks.
///
/// BT arrays and objects use `Rc<RefCell<_>>`, so they cannot enter `tokio::spawn` directly.
/// Before concurrent execution, arguments are reduced to this scalar-and-string-only enum. This keeps VM references out of background tasks and avoids cross-thread references in long-running processes.
#[derive(Debug, Clone, PartialEq)]
enum MysqlBindValue {
    /// SQL NULL.
    Null,
    /// Boolean value.
    Bool(bool),
    /// Integer value.
    Int(i64),
    /// Floating point value.
    Float(f64),
    /// String value.
    String(String),
}

impl MysqlBindValue {
    /// Creates a sendable binding value from a BT value.
    fn from_value(value: &Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Empty => Self::String(String::new()),
            Value::Bool(value) => Self::Bool(*value),
            Value::Int(value) => Self::Int(*value),
            Value::Float(value) if value.is_finite() => Self::Float(*value),
            Value::Float(_) => Self::Null,
            Value::Str(value) => Self::String(value.clone()),
            Value::Array(_) | Value::Object(_) | Value::Instance(_) => {
                Self::String(value.to_json_string())
            }
            other => Self::String(other.to_string()),
        }
    }

    /// Writes bound values to SQLx queries.
    fn bind<'q>(&self, query: BtMysqlQuery<'q>) -> BtMysqlQuery<'q> {
        match self {
            Self::Null => query.bind(Option::<String>::None),
            Self::Bool(value) => query.bind(*value),
            Self::Int(value) => query.bind(*value),
            Self::Float(value) => query.bind(*value),
            Self::String(value) => query.bind(value.clone()),
        }
    }

    /// Format the bound value as an SQL literal for debugging display only.
    fn sql_literal(&self) -> String {
        match self {
            Self::Null => "NULL".to_string(),
            Self::Bool(value) => {
                if *value {
                    "1".to_string()
                } else {
                    "0".to_string()
                }
            }
            Self::Int(value) => value.to_string(),
            Self::Float(value) if value.is_finite() => value.to_string(),
            Self::Float(_) => "NULL".to_string(),
            Self::String(value) => Self::quote_sql_string(value),
        }
    }

    /// escapes and wraps single quotes according to MySQL string literal rules.
    fn quote_sql_string(value: &str) -> String {
        let mut output = String::with_capacity(value.len() + 2);
        output.push('\'');
        for ch in value.chars() {
            match ch {
                '\'' => output.push_str("''"),
                '\\' => output.push_str("\\\\"),
                _ => output.push(ch),
            }
        }
        output.push('\'');
        output
    }
}

/// Simple INSERT multi-value optimization template.
#[derive(Debug, Clone)]
struct MysqlInsertTemplate {
    /// `VALUES` SQL text preceding the value group.
    prefix: String,
    /// Single-line `VALUES` value group, including outer brackets and placeholders.
    value_group: String,
    /// SQL text following the value group.
    suffix: String,
    /// The number of placeholders in a single-line value group.
    placeholders: usize,
}

/// MySQL query object.
#[derive(Debug, Clone, PartialEq)]
pub struct BtMysql {
    /// Database connection address.
    dsn: String,
    /// Current SQL.
    sql: String,
    /// A single set of parameters bound to `bind()`.
    binds: Vec<Value>,
    /// Multiple sets of parameters bound to `binds()`.
    bind_rows: Vec<Vec<Value>>,
    /// The batch size set by `batch()`, 0 means no batching.
    batch_size: usize,
    /// The number of concurrent jobs set by `workers()`.
    workers: usize,
}

/// MySQL transaction object.
///
/// Transaction objects share the same underlying state; chained `query()` and `bind()` calls copy only lightweight query configuration.
/// The SQLx transaction is taken exclusively only when executing SQL, committing, rolling back, or closing, so a synchronous lock is never held across `await`.
#[derive(Debug, Clone)]
pub struct BtMysqlTransaction {
    /// Shared transaction status.
    state: Arc<Mutex<MysqlTransactionInner>>,
    /// Current SQL.
    sql: String,
    /// Current query binding parameters.
    binds: Vec<Value>,
}

/// MySQL transaction shared state.
#[derive(Debug)]
struct MysqlTransactionInner {
    /// SQLx transaction handle; it will be temporarily taken out during execution to prevent concurrent reuse of the same transaction.
    transaction: Option<Transaction<'static, MySql>>,
    /// Transaction status.
    status: MysqlTransactionStatus,
}

/// MySQL transaction status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MysqlTransactionStatus {
    /// The transaction can still execute SQL.
    Active,
    /// The transaction has been committed.
    Committed,
    /// The transaction has been rolled back.
    RolledBack,
    /// The transaction has been closed via close().
    Closed,
    /// The transaction execution failed to commit, rollback, or close and the underlying transaction was discarded.
    Failed,
}

impl PartialEq for BtMysqlTransaction {
    /// Compares whether two transaction builders point to the same transaction state and the same query configuration.
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state) && self.sql == other.sql && self.binds == other.binds
    }
}

impl MysqlTransactionStatus {
    /// Returns the status text used by scripts and error messages.
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Committed => "committed",
            Self::RolledBack => "rolled_back",
            Self::Closed => "closed",
            Self::Failed => "failed",
        }
    }
}

impl BtMysql {
    /// Creates a MySQL object.
    pub fn new(args: Vec<Value>) -> Result<Value, String> {
        let dsn = args
            .first()
            .map(Value::to_string)
            .ok_or_else(|| "mysql() requires the DSN parameter".to_string())?;
        Ok(Value::Mysql(Self {
            dsn,
            sql: String::new(),
            binds: Vec::new(),
            bind_rows: Vec::new(),
            batch_size: DEFAULT_BATCH_SIZE,
            workers: DEFAULT_MYSQL_WORKERS,
        }))
    }

    /// calls the MySQL method.
    pub fn call_method(&self, method: &str, args: Vec<Value>) -> Result<Value, String> {
        match method {
            "query" => {
                let mut next = self.clone();
                next.sql = args.first().map(Value::to_string).unwrap_or_default();
                Ok(Value::Mysql(next))
            }
            "bind" => {
                let mut next = self.clone();
                next.binds.extend(args);
                Ok(Value::Mysql(next))
            }
            "binds" => {
                let mut next = self.clone();
                next.append_bind_rows(args)?;
                Ok(Value::Mysql(next))
            }
            "batch" => {
                let mut next = self.clone();
                next.batch_size = Self::batch_arg(&args);
                Ok(Value::Mysql(next))
            }
            "workers" => {
                let mut next = self.clone();
                next.workers = Self::workers_arg(&args);
                Ok(Value::Mysql(next))
            }
            "begin" => self.begin(),
            "all" => self.all(),
            "one" => self.one(),
            "exec" => self.exec(),
            "sql" => self.sql_text(&args).map(Value::Str),
            _ => Err(format!("mysql has no method `{}`", method)),
        }
    }

    /// queries multiple rows of data.
    fn all(&self) -> Result<Value, String> {
        self.validate_query_method("all")?;
        self.run_async("all", self.all_async())
    }

    /// Asynchronously queries multiple rows of data.
    async fn all_async(&self) -> Result<Value, String> {
        let rows = if let Some(pool) = mysql_pool(&self.dsn).await? {
            self.bind_query(sqlx::query::<MySql>(audited_mysql_sql(&self.sql)))
                .fetch_all(&pool)
                .await
        } else {
            let mut conn = self.connect().await?;
            self.bind_query(sqlx::query::<MySql>(audited_mysql_sql(&self.sql)))
                .fetch_all(&mut conn)
                .await
        }
        .map_err(|err| self.sql_error("Failed to execute MySQL query", err))?;
        let mut values = Vec::with_capacity(rows.len());
        for row in rows {
            values.push(Self::row_to_value(&row)?);
        }
        Ok(Value::Array(Rc::new(RefCell::new(values))))
    }

    /// Query a single row of data.
    fn one(&self) -> Result<Value, String> {
        self.validate_query_method("one")?;
        self.run_async("one", self.one_async())
    }

    /// Query a single row of data asynchronously.
    async fn one_async(&self) -> Result<Value, String> {
        let row = if let Some(pool) = mysql_pool(&self.dsn).await? {
            self.bind_query(sqlx::query::<MySql>(audited_mysql_sql(&self.sql)))
                .fetch_optional(&pool)
                .await
        } else {
            let mut conn = self.connect().await?;
            self.bind_query(sqlx::query::<MySql>(audited_mysql_sql(&self.sql)))
                .fetch_optional(&mut conn)
                .await
        }
        .map_err(|err| self.sql_error("Failed to execute MySQL query", err))?;
        row.as_ref()
            .map(Self::row_to_value)
            .unwrap_or(Ok(Value::Empty))
    }

    /// executes the write statement.
    fn exec(&self) -> Result<Value, String> {
        self.validate("exec")?;
        if self.bind_rows.is_empty() {
            return self.run_async("exec", self.exec_single_async());
        }

        let rows = self.materialize_bind_rows();
        let total = rows.len();
        let workers = self.normalized_workers();
        if total == 0 {
            return Ok(Self::exec_result(0, 0, 0, 0, self.batch_size, workers));
        }

        let insert_template = Self::insert_template(&self.sql);
        let batch_unit_size = self.batch_unit_size(total, insert_template.is_some());
        let batch_count = Self::batch_count(total, batch_unit_size);
        let active_workers = workers.min(batch_count).max(1);
        self.run_async(
            "exec",
            self.exec_batch_async(
                rows,
                insert_template,
                workers,
                active_workers,
                batch_unit_size,
                batch_count,
            ),
        )
    }

    /// starts a MySQL transaction.
    fn begin(&self) -> Result<Value, String> {
        self.validate_dsn("begin")?;
        let config = mysql_pool_config()?;
        let start = Instant::now();
        let result = crate::io::run_async(self.begin_async(), Some(config.query_timeout()));
        self.record_slow_call("begin", start.elapsed());
        result
    }

    /// starts a MySQL transaction asynchronously.
    async fn begin_async(&self) -> Result<Value, String> {
        match begin_mysql_transaction(&self.dsn).await {
            Ok(transaction) => {
                MYSQL_TRANSACTION_STARTED.fetch_add(1, Ordering::Relaxed);
                MYSQL_TRANSACTION_ACTIVE.fetch_add(1, Ordering::Relaxed);
                Ok(Value::MysqlTransaction(BtMysqlTransaction::new(
                    transaction,
                )))
            }
            Err(err) => {
                MYSQL_TRANSACTION_FAILED.fetch_add(1, Ordering::Relaxed);
                Err(err)
            }
        }
    }

    /// executes a single write statement asynchronously.
    async fn exec_single_async(&self) -> Result<Value, String> {
        let result = if let Some(pool) = mysql_pool(&self.dsn).await? {
            self.bind_query(sqlx::query::<MySql>(audited_mysql_sql(&self.sql)))
                .execute(&pool)
                .await
        } else {
            let mut conn = self.connect().await?;
            self.bind_query(sqlx::query::<MySql>(audited_mysql_sql(&self.sql)))
                .execute(&mut conn)
                .await
        }
        .map_err(|err| self.sql_error("Failed to execute MySQL statement", err))?;
        Ok(Self::exec_result(
            1,
            result.rows_affected(),
            result.last_insert_id(),
            1,
            self.batch_size,
            self.normalized_workers(),
        ))
    }

    /// Asynchronously execute multiple sets of bound parameters in batches.
    async fn exec_batch_async(
        &self,
        rows: Vec<Vec<MysqlBindValue>>,
        insert_template: Option<MysqlInsertTemplate>,
        workers: usize,
        active_workers: usize,
        batch_unit_size: usize,
        batch_count: usize,
    ) -> Result<Value, String> {
        let total = rows.len();
        let pool = MySqlPoolOptions::new()
            .max_connections(active_workers as u32)
            .connect(&self.dsn)
            .await
            .map_err(|err| self.sql_error("Failed to create MySQL connection pool", err))?;
        Self::warm_pool(&pool, active_workers)
            .await
            .map_err(|err| self.sql_error("Failed to warm up MySQL connection pool", err))?;
        let sql = Arc::new(self.sql.clone());
        let insert_template = insert_template.map(Arc::new);
        let mut pending = rows.into_iter();
        let mut tasks = JoinSet::new();
        let mut completed_rows = 0usize;
        let mut rows_affected = 0u64;
        let mut last_insert_id = 0u64;

        for _ in 0..active_workers {
            let Some(batch) = Self::next_exec_batch(&mut pending, batch_unit_size) else {
                break;
            };
            Self::spawn_exec_batch_task(
                &mut tasks,
                pool.clone(),
                sql.clone(),
                insert_template.clone(),
                batch,
            );
        }

        while let Some(result) = tasks.join_next().await {
            let (batch_rows, batch_affected, batch_last_id) =
                result.map_err(|err| format!("MySQL batch task exception: {}", err))??;
            completed_rows += batch_rows;
            rows_affected = rows_affected.saturating_add(batch_affected);
            last_insert_id = last_insert_id.max(batch_last_id);

            if let Some(batch) = Self::next_exec_batch(&mut pending, batch_unit_size) {
                Self::spawn_exec_batch_task(
                    &mut tasks,
                    pool.clone(),
                    sql.clone(),
                    insert_template.clone(),
                    batch,
                );
            }
        }

        if completed_rows != total {
            return Err(format!(
                "mysql.exec() batch execution quantity exception: completed {} items, plan {} entries",
                completed_rows, total
            ));
        }
        Ok(Self::exec_result(
            total,
            rows_affected,
            last_insert_id,
            batch_count,
            self.batch_size,
            workers,
        ))
    }

    /// Sequentially warm up the connection pool.
    ///
    /// During high concurrency stress testing, if all tasks trigger connection creation at the same time, both Windows and MySQL servers may return underlying socket errors
    /// under a transient connection storm. Here, connections are first established one by one according to the number of concurrencies and returned to the pool, so that subsequent tasks can focus on executing SQL.
    async fn warm_pool(pool: &MySqlPool, concurrency: usize) -> Result<(), sqlx::Error> {
        let mut connections = Vec::with_capacity(concurrency);
        for _ in 0..concurrency {
            connections.push(pool.acquire().await?);
        }
        drop(connections);
        Ok(())
    }

    /// starts a batch execution task.
    fn spawn_exec_batch_task(
        tasks: &mut JoinSet<Result<(usize, u64, u64), String>>,
        pool: MySqlPool,
        sql: Arc<String>,
        insert_template: Option<Arc<MysqlInsertTemplate>>,
        batch: Vec<Vec<MysqlBindValue>>,
    ) {
        tasks.spawn(async move {
            if let Some(template) = insert_template {
                return Self::execute_insert_batch(&pool, &template, batch).await;
            }
            Self::execute_rows_batch(&pool, &sql, batch).await
        });
    }

    /// Performs an INSERT multi-value batch.
    async fn execute_insert_batch(
        pool: &MySqlPool,
        template: &MysqlInsertTemplate,
        batch: Vec<Vec<MysqlBindValue>>,
    ) -> Result<(usize, u64, u64), String> {
        for row in &batch {
            if row.len() != template.placeholders {
                return Err(format!(
                    "mysql.exec() INSERT batch parameter count mismatch: SQL expects {}, but the row contains {}",
                    template.placeholders,
                    row.len()
                ));
            }
        }

        let sql = Self::build_insert_batch_sql(template, batch.len());
        let mut query = sqlx::query::<MySql>(AssertSqlSafe(sql));
        for row in &batch {
            for value in row {
                query = value.bind(query);
            }
        }
        let result = query
            .execute(pool)
            .await
            .map_err(|err| format!("Failed to execute MySQL INSERT batch statement: {}", err))?;
        Ok((batch.len(), result.rows_affected(), result.last_insert_id()))
    }

    /// Execute a non-INSERT one by one batch.
    async fn execute_rows_batch(
        pool: &MySqlPool,
        sql: &str,
        batch: Vec<Vec<MysqlBindValue>>,
    ) -> Result<(usize, u64, u64), String> {
        let mut conn = pool
            .acquire()
            .await
            .map_err(|err| format!("Failed to obtain MySQL batch connection: {}", err))?;
        let sql = audited_mysql_sql(sql);
        let mut rows_affected = 0u64;
        let mut last_insert_id = 0u64;
        for binds in &batch {
            let mut query = sqlx::query::<MySql>(sql.clone());
            for value in binds {
                query = value.bind(query);
            }
            let result = query
                .execute(&mut *conn)
                .await
                .map_err(|err| format!("Failed to execute MySQL batch statement: {}", err))?;
            rows_affected = rows_affected.saturating_add(result.rows_affected());
            last_insert_id = last_insert_id.max(result.last_insert_id());
        }
        Ok((batch.len(), rows_affected, last_insert_id))
    }

    /// Take out a batch from the parameter iterator to be executed.
    fn next_exec_batch(
        pending: &mut std::vec::IntoIter<Vec<MysqlBindValue>>,
        batch_unit_size: usize,
    ) -> Option<Vec<Vec<MysqlBindValue>>> {
        let mut batch = Vec::with_capacity(batch_unit_size.min(pending.len()));
        for _ in 0..batch_unit_size {
            let Some(row) = pending.next() else {
                break;
            };
            batch.push(row);
        }
        if batch.is_empty() {
            None
        } else {
            Some(batch)
        }
    }

    /// Calculates the number of runner worker threads required for concurrent execution.
    ///
    /// `workers` controls the MySQL connection pool and the number of SQLs executed simultaneously, and should not be directly equal to the number of OS threads; SQLx
    /// Most of the time in asynchronous I/O waiting, a small number of worker threads can drive a large number of connections, avoiding the need to create 1000 threads when 1000 is passed in.
    #[cfg(test)]
    fn mysql_worker_threads(workers: usize) -> usize {
        let parallel = std::thread::available_parallelism()
            .map(|value| value.get())
            .unwrap_or(4);
        workers
            .min(parallel.saturating_mul(2))
            .clamp(1, MAX_MYSQL_WORKER_THREADS)
    }

    /// Constructs `exec()` Return object.
    fn exec_result(
        total: usize,
        rows_affected: u64,
        last_insert_id: u64,
        batch_count: usize,
        batch_size: usize,
        workers: usize,
    ) -> Value {
        let mut object = IndexMap::new();
        object.insert("total".to_string(), Self::u64_to_value(total as u64));
        object.insert(
            "rows_affected".to_string(),
            Self::u64_to_value(rows_affected),
        );
        object.insert(
            "last_insert_id".to_string(),
            Self::u64_to_value(last_insert_id),
        );
        object.insert(
            "batch_count".to_string(),
            Self::u64_to_value(batch_count as u64),
        );
        object.insert("batch_size".to_string(), Value::Int(batch_size as i64));
        object.insert("workers".to_string(), Value::Int(workers as i64));
        Value::Object(Rc::new(RefCell::new(object)))
    }

    /// appends multiple sets of binding parameters passed in by `binds()`.
    fn append_bind_rows(&mut self, args: Vec<Value>) -> Result<(), String> {
        if args.is_empty() {
            return Err("mysql.binds() requires a 2D array argument".to_string());
        }
        for value in args {
            Self::append_bind_rows_value(&mut self.bind_rows, value)?;
        }
        Ok(())
    }

    /// Appends a single 2D array to the bulk bind list.
    fn append_bind_rows_value(output: &mut Vec<Vec<Value>>, value: Value) -> Result<(), String> {
        let Value::Array(rows) = value else {
            return Err("The mysql.binds() parameter must be a two-dimensional array".to_string());
        };
        let rows = rows.borrow();
        output.reserve(rows.len());
        for row in rows.iter() {
            match row {
                Value::Array(values) => {
                    let values = values.borrow();
                    output.push(values.iter().cloned().collect());
                }
                value => output.push(vec![value.clone()]),
            }
        }
        Ok(())
    }

    /// Parses the batch size of `batch()`.
    fn batch_arg(args: &[Value]) -> usize {
        args.first()
            .map(Value::to_i64_lossy)
            .unwrap_or(DEFAULT_BATCH_SIZE as i64)
            .max(0) as usize
    }

    /// Number of concurrent jobs parsing `workers()`.
    fn workers_arg(args: &[Value]) -> usize {
        args.first()
            .map(Value::to_i64_lossy)
            .unwrap_or(DEFAULT_MYSQL_WORKERS as i64)
            .clamp(DEFAULT_MYSQL_WORKERS as i64, MAX_MYSQL_WORKERS as i64) as usize
    }

    /// Returns the number of valid jobs under the current configuration.
    fn normalized_workers(&self) -> usize {
        self.workers.clamp(DEFAULT_MYSQL_WORKERS, MAX_MYSQL_WORKERS)
    }

    /// Combines the `bind()` prefix parameters and `binds()` row parameters into a bound value that can be moved across threads.
    fn materialize_bind_rows(&self) -> Vec<Vec<MysqlBindValue>> {
        let mut rows = Vec::with_capacity(self.bind_rows.len());
        for row in &self.bind_rows {
            let mut values = Vec::with_capacity(self.binds.len() + row.len());
            values.extend(self.binds.iter().map(MysqlBindValue::from_value));
            values.extend(row.iter().map(MysqlBindValue::from_value));
            rows.push(values);
        }
        rows
    }

    /// Calculates the number of rows each execution task should consume.
    fn batch_unit_size(&self, total: usize, insert_optimized: bool) -> usize {
        if self.batch_size > 0 {
            return self.batch_size.min(total).max(1);
        }
        if insert_optimized {
            total.max(1)
        } else {
            1
        }
    }

    /// Calculate the number of batches.
    fn batch_count(total: usize, batch_unit_size: usize) -> usize {
        if total == 0 {
            0
        } else {
            (total - 1) / batch_unit_size + 1
        }
    }

    /// Returns the debug text of the current SQL.
    fn sql_text(&self, args: &[Value]) -> Result<String, String> {
        let render_binds = args.first().map(Value::is_truthy).unwrap_or(true);
        if !render_binds {
            return Ok(self.sql.clone());
        }
        if self.bind_rows.is_empty() {
            let binds: Vec<_> = self.binds.iter().map(MysqlBindValue::from_value).collect();
            return Ok(Self::format_sql_with_binds(&self.sql, &binds));
        }

        let rows = self.materialize_bind_rows();
        if rows.is_empty() {
            return Ok(self.sql.clone());
        }
        let preview_size = if self.batch_size > 0 {
            self.batch_size.min(rows.len()).max(1)
        } else {
            rows.len()
        };
        let mut sql = if let Some(template) = Self::insert_template(&self.sql) {
            let preview_rows = &rows[..preview_size];
            if preview_rows
                .iter()
                .all(|row| row.len() == template.placeholders)
            {
                let batch_sql = Self::build_insert_batch_sql(&template, preview_size);
                let mut binds = Vec::with_capacity(template.placeholders * preview_size);
                for row in preview_rows {
                    binds.extend(row.iter().cloned());
                }
                Self::format_sql_with_binds(&batch_sql, &binds)
            } else {
                Self::format_sql_with_binds(&self.sql, &rows[0])
            }
        } else {
            Self::format_sql_with_binds(&self.sql, &rows[0])
        };
        sql.push_str(&format!(
            " /* binds: {} rows, batch: {}, workers: {} */",
            rows.len(),
            self.batch_size,
            self.normalized_workers()
        ));
        Ok(sql)
    }

    /// Replaces the `?` placeholder outside a string literal in SQL with a bound value.
    fn format_sql_with_binds(sql: &str, binds: &[MysqlBindValue]) -> String {
        let mut output = String::with_capacity(sql.len().saturating_add(binds.len() * 8));
        let mut bind_index = 0usize;
        let mut in_single = false;
        let mut in_double = false;
        let mut escaped = false;

        for ch in sql.chars() {
            if escaped {
                output.push(ch);
                escaped = false;
                continue;
            }

            if in_single || in_double {
                output.push(ch);
                if ch == '\\' {
                    escaped = true;
                } else if in_single && ch == '\'' {
                    in_single = false;
                } else if in_double && ch == '"' {
                    in_double = false;
                }
                continue;
            }

            match ch {
                '\'' => {
                    in_single = true;
                    output.push(ch);
                }
                '"' => {
                    in_double = true;
                    output.push(ch);
                }
                '?' => {
                    if let Some(value) = binds.get(bind_index) {
                        output.push_str(&value.sql_literal());
                        bind_index += 1;
                    } else {
                        output.push('?');
                    }
                }
                _ => output.push(ch),
            }
        }

        if bind_index < binds.len() {
            output.push_str(&format!(" /* extra binds: {} */", binds.len() - bind_index));
        }
        output
    }

    /// Identifies a simple INSERT statement whose `VALUES` group can be repeated safely.
    fn insert_template(sql: &str) -> Option<MysqlInsertTemplate> {
        let trimmed = sql.trim_start();
        if !Self::starts_with_keyword(trimmed, "insert") {
            return None;
        }
        let mut search_start = 0usize;
        while let Some(relative_index) =
            Self::find_keyword_outside_strings(&sql[search_start..], "values")
        {
            let values_index = search_start + relative_index;
            let group_start = Self::first_non_whitespace(sql, values_index + "values".len())?;
            if sql[group_start..].chars().next()? != '(' {
                search_start = values_index + "values".len();
                continue;
            }
            let group_end = Self::find_matching_paren(sql, group_start)?;
            let suffix = &sql[group_end + 1..];
            let suffix_trimmed = suffix.trim();
            if !suffix_trimmed.is_empty() && suffix_trimmed != ";" {
                return None;
            }
            let value_group = &sql[group_start..=group_end];
            let placeholders = Self::count_sql_placeholders(value_group);
            if placeholders == 0 {
                return None;
            }
            return Some(MysqlInsertTemplate {
                prefix: sql[..group_start].to_string(),
                value_group: value_group.to_string(),
                suffix: suffix.to_string(),
                placeholders,
            });
        }
        None
    }

    /// Builds INSERT SQL after repeating the values group.
    fn build_insert_batch_sql(template: &MysqlInsertTemplate, batch_len: usize) -> String {
        let mut sql = String::with_capacity(
            template
                .prefix
                .len()
                .saturating_add(template.value_group.len().saturating_mul(batch_len))
                .saturating_add(batch_len.saturating_sub(1) * 2)
                .saturating_add(template.suffix.len()),
        );
        sql.push_str(&template.prefix);
        for index in 0..batch_len {
            if index > 0 {
                sql.push_str(", ");
            }
            sql.push_str(&template.value_group);
        }
        sql.push_str(&template.suffix);
        sql
    }

    /// Counts the number of placeholders outside string literals in SQL.
    fn count_sql_placeholders(sql: &str) -> usize {
        let mut count = 0usize;
        let mut in_single = false;
        let mut in_double = false;
        let mut escaped = false;

        for ch in sql.chars() {
            if escaped {
                escaped = false;
                continue;
            }
            if in_single || in_double {
                if ch == '\\' {
                    escaped = true;
                } else if in_single && ch == '\'' {
                    in_single = false;
                } else if in_double && ch == '"' {
                    in_double = false;
                }
                continue;
            }
            match ch {
                '\'' => in_single = true,
                '"' => in_double = true,
                '?' => count += 1,
                _ => {}
            }
        }
        count
    }

    /// Determines whether the beginning of the text is the specified SQL keyword.
    fn starts_with_keyword(text: &str, keyword: &str) -> bool {
        if text.len() < keyword.len() {
            return false;
        }
        let end = keyword.len();
        text[..end].eq_ignore_ascii_case(keyword)
            && text
                .as_bytes()
                .get(end)
                .map_or(true, |byte| !Self::is_sql_ident_byte(*byte))
    }

    /// Finds SQL keywords outside of string literals.
    fn find_keyword_outside_strings(sql: &str, keyword: &str) -> Option<usize> {
        let first = keyword.as_bytes().first().copied()?;
        let mut in_single = false;
        let mut in_double = false;
        let mut escaped = false;

        for (index, ch) in sql.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            if in_single || in_double {
                if ch == '\\' {
                    escaped = true;
                } else if in_single && ch == '\'' {
                    in_single = false;
                } else if in_double && ch == '"' {
                    in_double = false;
                }
                continue;
            }
            match ch {
                '\'' => {
                    in_single = true;
                    continue;
                }
                '"' => {
                    in_double = true;
                    continue;
                }
                _ => {}
            }

            if !ch.is_ascii() || !ch.eq_ignore_ascii_case(&(first as char)) {
                continue;
            }
            let end = index + keyword.len();
            if end > sql.len() || !sql.is_char_boundary(end) {
                continue;
            }
            let before_ok = index == 0 || !Self::is_sql_ident_byte(sql.as_bytes()[index - 1]);
            let after_ok = sql
                .as_bytes()
                .get(end)
                .map_or(true, |byte| !Self::is_sql_ident_byte(*byte));
            if before_ok && after_ok && sql[index..end].eq_ignore_ascii_case(keyword) {
                return Some(index);
            }
        }
        None
    }

    /// returns the index of the first non-whitespace character after the specified position.
    fn first_non_whitespace(sql: &str, start: usize) -> Option<usize> {
        sql.get(start..)?
            .char_indices()
            .find_map(|(offset, ch)| (!ch.is_whitespace()).then_some(start + offset))
    }

    /// Finds a right parenthesis that matches the specified left parenthesis.
    fn find_matching_paren(sql: &str, open_index: usize) -> Option<usize> {
        let mut depth = 0usize;
        let mut in_single = false;
        let mut in_double = false;
        let mut escaped = false;

        for (offset, ch) in sql.get(open_index..)?.char_indices() {
            let index = open_index + offset;
            if escaped {
                escaped = false;
                continue;
            }
            if in_single || in_double {
                if ch == '\\' {
                    escaped = true;
                } else if in_single && ch == '\'' {
                    in_single = false;
                } else if in_double && ch == '"' {
                    in_double = false;
                }
                continue;
            }
            match ch {
                '\'' => in_single = true,
                '"' => in_double = true,
                '(' => depth += 1,
                ')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(index);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Determines whether the byte is an SQL identifier character.
    fn is_sql_ident_byte(byte: u8) -> bool {
        byte.is_ascii_alphanumeric() || byte == b'_'
    }

    /// The verification query interface does not use batch execution configuration.
    fn validate_query_method(&self, method: &str) -> Result<(), String> {
        self.validate(method)?;
        if !self.bind_rows.is_empty() {
            return Err(format!("mysql.{}() does not support binds()", method));
        }
        if self.batch_size > 0 {
            return Err(format!("mysql.{}() does not support batch()", method));
        }
        if self.normalized_workers() != DEFAULT_MYSQL_WORKERS {
            return Err(format!("mysql.{}() does not support workers()", method));
        }
        Ok(())
    }

    /// Synchronously wait for a MySQL asynchronous task.
    fn run_async<Fut>(&self, method: &str, task: Fut) -> Result<Value, String>
    where
        Fut: std::future::Future<Output = Result<Value, String>>,
    {
        self.validate(method)?;
        let config = mysql_pool_config()?;
        let start = Instant::now();
        let result = crate::io::run_async(task, Some(config.query_timeout()));
        self.record_slow_call(method, start.elapsed());
        result
    }

    /// Verify necessary status before execution.
    fn validate(&self, method: &str) -> Result<(), String> {
        self.validate_dsn(method)?;
        if self.sql.trim().is_empty() {
            return Err(format!(
                "mysql.{}() requires query(sql) to be called first",
                method
            ));
        }
        Ok(())
    }

    /// first to verify the MySQL DSN.
    fn validate_dsn(&self, method: &str) -> Result<(), String> {
        if self.dsn.trim().is_empty() {
            return Err(format!("mysql.{}() requires a valid DSN", method));
        }
        Ok(())
    }

    /// to create a MySQL connection.
    async fn connect(&self) -> Result<MySqlConnection, String> {
        crate::io::ensure_rustls_provider();
        MySqlConnection::connect(&self.dsn).await.map_err(|err| {
            format!(
                "Failed to connect to MySQL `{}`: {}",
                redact_mysql_dsn(&self.dsn),
                sanitize_mysql_error_text(&self.dsn, err)
            )
        })
    }

    /// Logs MySQL slow calls that exceed the threshold.
    fn record_slow_call(&self, method: &str, elapsed: Duration) {
        let Ok(config) = mysql_pool_config() else {
            return;
        };
        if config.slow_ms == 0 || elapsed < Duration::from_millis(config.slow_ms) {
            return;
        }
        MYSQL_SLOW_CALLS.fetch_add(1, Ordering::Relaxed);
        eprintln!(
            "BT slow MySQL call: mysql.{}() takes {} milliseconds; binds={} bind_rows={} batch={} workers={}",
            method,
            elapsed.as_millis(),
            self.binds.len(),
            self.bind_rows.len(),
            self.batch_size,
            self.normalized_workers()
        );
    }

    /// binds the parameters saved by the current query object.
    fn bind_query<'q>(&self, mut query: BtMysqlQuery<'q>) -> BtMysqlQuery<'q> {
        for value in &self.binds {
            query = Self::bind_value(query, value);
        }
        query
    }

    /// Binds BT values as SQLx parameters.
    fn bind_value<'q>(query: BtMysqlQuery<'q>, value: &Value) -> BtMysqlQuery<'q> {
        MysqlBindValue::from_value(value).bind(query)
    }

    /// Converts a row of SQLx results to a BT object.
    fn row_to_value(row: &MySqlRow) -> Result<Value, String> {
        let columns = row.columns();
        let mut object = IndexMap::with_capacity(columns.len());
        for (index, column) in columns.iter().enumerate() {
            let value = Self::column_to_value(row, index, column.type_info().name())?;
            object.insert(column.name().to_string(), value);
        }
        Ok(Value::Object(Rc::new(RefCell::new(object))))
    }

    /// Converts a single field to a BT value by MySQL column type.
    fn column_to_value(row: &MySqlRow, index: usize, type_name: &str) -> Result<Value, String> {
        let raw = row
            .try_get_raw(index)
            .map_err(|err| Self::decode_error(index, type_name, err))?;
        if raw.is_null() {
            return Ok(Value::Null);
        }

        match type_name {
            "BOOLEAN" => row
                .try_get::<bool, _>(index)
                .map(Value::Bool)
                .map_err(|err| Self::decode_error(index, type_name, err)),
            "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "BIGINT" => row
                .try_get::<i64, _>(index)
                .map(Value::Int)
                .map_err(|err| Self::decode_error(index, type_name, err)),
            "TINYINT UNSIGNED" | "SMALLINT UNSIGNED" | "MEDIUMINT UNSIGNED" | "INT UNSIGNED"
            | "BIGINT UNSIGNED" | "YEAR" | "BIT" => row
                .try_get::<u64, _>(index)
                .map(Self::u64_to_value)
                .map_err(|err| Self::decode_error(index, type_name, err)),
            "FLOAT" | "DOUBLE" => row
                .try_get::<f64, _>(index)
                .map(Value::Float)
                .map_err(|err| Self::decode_error(index, type_name, err)),
            "DECIMAL" => row
                .try_get_unchecked::<String, _>(index)
                .map(Value::Str)
                .map_err(|err| Self::decode_error(index, type_name, err)),
            "DATE" => row
                .try_get::<NaiveDate, _>(index)
                .map(|value| Value::Str(value.format("%Y-%m-%d").to_string()))
                .map_err(|err| Self::decode_error(index, type_name, err)),
            "TIME" => row
                .try_get::<NaiveTime, _>(index)
                .map(|value| Value::Str(value.format("%H:%M:%S%.f").to_string()))
                .map_err(|err| Self::decode_error(index, type_name, err)),
            "DATETIME" | "TIMESTAMP" => row
                .try_get::<NaiveDateTime, _>(index)
                .map(|value| Value::Str(value.format("%Y-%m-%d %H:%M:%S%.f").to_string()))
                .map_err(|err| Self::decode_error(index, type_name, err)),
            "JSON" => row
                .try_get_unchecked::<String, _>(index)
                .map(Self::json_text_to_value)
                .map_err(|err| Self::decode_error(index, type_name, err)),
            "BINARY" | "VARBINARY" | "TINYBLOB" | "BLOB" | "MEDIUMBLOB" | "LONGBLOB" => row
                .try_get::<Vec<u8>, _>(index)
                .map(|value| Value::Str(STANDARD.encode(&value)))
                .map_err(|err| Self::decode_error(index, type_name, err)),
            _ => row
                .try_get_unchecked::<String, _>(index)
                .map(Value::Str)
                .or_else(|_| {
                    row.try_get::<Vec<u8>, _>(index)
                        .map(|value| Value::Str(STANDARD.encode(&value)))
                })
                .map_err(|err| Self::decode_error(index, type_name, err)),
        }
    }

    /// Converts JSON text to BT values, retaining the original string on failure.
    fn json_text_to_value(text: String) -> Value {
        serde_json::from_str::<serde_json::Value>(&text)
            .map(Self::json_value_to_bt)
            .unwrap_or(Value::Str(text))
    }

    /// Convert serde_json value to BT value.
    fn json_value_to_bt(value: serde_json::Value) -> Value {
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
                values.into_iter().map(Self::json_value_to_bt).collect(),
            ))),
            serde_json::Value::Object(values) => Value::Object(Rc::new(RefCell::new(
                values
                    .into_iter()
                    .map(|(key, value)| (key, Self::json_value_to_bt(value)))
                    .collect(),
            ))),
        }
    }

    /// Convert u64 to BT value, using string to preserve precision on overflow.
    fn u64_to_value(value: u64) -> Value {
        i64::try_from(value)
            .map(Value::Int)
            .unwrap_or_else(|_| Value::Str(value.to_string()))
    }

    /// Format SQLx execution error.
    fn sql_error(&self, prefix: &str, err: sqlx::Error) -> String {
        format!(
            "{}: {}; sql=`{}` binds={} bind_rows={} batch={} workers={}",
            prefix,
            err,
            self.sql,
            self.binds.len(),
            self.bind_rows.len(),
            self.batch_size,
            self.normalized_workers()
        )
    }

    /// Format column decoding error.
    fn decode_error(index: usize, type_name: &str, err: sqlx::Error) -> String {
        format!(
            "Failed to read MySQL column {} ({}): {}",
            index + 1,
            type_name,
            err
        )
    }
}

impl BtMysqlTransaction {
    /// Create a MySQL transaction object.
    fn new(transaction: Transaction<'static, MySql>) -> Self {
        Self {
            state: Arc::new(Mutex::new(MysqlTransactionInner {
                transaction: Some(transaction),
                status: MysqlTransactionStatus::Active,
            })),
            sql: String::new(),
            binds: Vec::new(),
        }
    }

    /// creates a transaction object for testing and does not hold a real database connection.
    #[cfg(test)]
    fn new_for_test(status: MysqlTransactionStatus, sql: &str, binds: Vec<Value>) -> Self {
        Self {
            state: Arc::new(Mutex::new(MysqlTransactionInner {
                transaction: None,
                status,
            })),
            sql: sql.to_string(),
            binds,
        }
    }

    /// Calls the MySQL transaction method.
    pub fn call_method(&self, method: &str, args: Vec<Value>) -> Result<Value, String> {
        match method {
            "query" => {
                let mut next = self.clone();
                next.sql = args.first().map(Value::to_string).unwrap_or_default();
                Ok(Value::MysqlTransaction(next))
            }
            "bind" => {
                let mut next = self.clone();
                next.binds.extend(args);
                Ok(Value::MysqlTransaction(next))
            }
            "all" => self.all(),
            "one" => self.one(),
            "exec" => self.exec(),
            "sql" => self.sql_text(&args).map(Value::Str),
            "commit" => self.commit(),
            "rollback" => self.rollback(),
            "close" => self.close(),
            "status" => Ok(Value::Str(self.status_text()?)),
            _ => Err(format!("mysql transaction has no method `{}`", method)),
        }
    }

    /// queries multiple rows of data within the transaction.
    fn all(&self) -> Result<Value, String> {
        self.validate_query("all")?;
        self.run_async("all", self.all_async())
    }

    /// Asynchronously queries multiple rows of data within a transaction.
    async fn all_async(&self) -> Result<Value, String> {
        let mut transaction = self.take_transaction("all")?;
        let result = self
            .bind_query(sqlx::query::<MySql>(audited_mysql_sql(&self.sql)))
            .fetch_all(&mut *transaction)
            .await;
        self.restore_transaction(transaction)?;
        let rows = result
            .map_err(|err| self.sql_error("Failed to execute MySQL transaction query", err))?;
        let mut values = Vec::with_capacity(rows.len());
        for row in rows {
            values.push(BtMysql::row_to_value(&row)?);
        }
        Ok(Value::Array(Rc::new(RefCell::new(values))))
    }

    /// Query a single row of data within the transaction.
    fn one(&self) -> Result<Value, String> {
        self.validate_query("one")?;
        self.run_async("one", self.one_async())
    }

    /// Asynchronously queries a single row of data within a transaction.
    async fn one_async(&self) -> Result<Value, String> {
        let mut transaction = self.take_transaction("one")?;
        let result = self
            .bind_query(sqlx::query::<MySql>(audited_mysql_sql(&self.sql)))
            .fetch_optional(&mut *transaction)
            .await;
        self.restore_transaction(transaction)?;
        let row = result
            .map_err(|err| self.sql_error("Failed to execute MySQL transaction query", err))?;
        row.as_ref()
            .map(BtMysql::row_to_value)
            .unwrap_or(Ok(Value::Empty))
    }

    /// executes the write statement within the transaction.
    fn exec(&self) -> Result<Value, String> {
        self.validate_query("exec")?;
        self.run_async("exec", self.exec_async())
    }

    /// executes write statements within a transaction asynchronously.
    async fn exec_async(&self) -> Result<Value, String> {
        let mut transaction = self.take_transaction("exec")?;
        let result = self
            .bind_query(sqlx::query::<MySql>(audited_mysql_sql(&self.sql)))
            .execute(&mut *transaction)
            .await;
        self.restore_transaction(transaction)?;
        let result = result.map_err(|err| {
            self.sql_error("failed to execute the MySQL transaction statement", err)
        })?;
        Ok(BtMysql::exec_result(
            1,
            result.rows_affected(),
            result.last_insert_id(),
            1,
            0,
            1,
        ))
    }

    /// committed the transaction.
    fn commit(&self) -> Result<Value, String> {
        self.run_async("commit", self.commit_async())
    }

    /// commits the transaction asynchronously.
    async fn commit_async(&self) -> Result<Value, String> {
        let transaction = self.take_transaction("commit")?;
        match transaction.commit().await {
            Ok(()) => {
                self.finish_transaction(MysqlTransactionStatus::Committed)?;
                MYSQL_TRANSACTION_COMMITTED.fetch_add(1, Ordering::Relaxed);
                Ok(Value::Bool(true))
            }
            Err(err) => {
                self.finish_failed_transaction()?;
                Err(format!("Failed to commit MySQL transaction: {}", err))
            }
        }
    }

    /// Rolling back the transaction.
    fn rollback(&self) -> Result<Value, String> {
        self.run_async("rollback", self.rollback_async())
    }

    /// Asynchronously rolls back the transaction.
    async fn rollback_async(&self) -> Result<Value, String> {
        let transaction = self.take_transaction("rollback")?;
        match transaction.rollback().await {
            Ok(()) => {
                self.finish_transaction(MysqlTransactionStatus::RolledBack)?;
                MYSQL_TRANSACTION_ROLLED_BACK.fetch_add(1, Ordering::Relaxed);
                Ok(Value::Bool(true))
            }
            Err(err) => {
                self.finish_failed_transaction()?;
                Err(format!(
                    "Failed to roll back the MySQL transaction: {}",
                    err
                ))
            }
        }
    }

    /// Close the transaction; active transactions will be rolled back first, and completed transactions will return false.
    fn close(&self) -> Result<Value, String> {
        self.run_async("close", self.close_async())
    }

    /// Closes the transaction asynchronously.
    async fn close_async(&self) -> Result<Value, String> {
        let Some(transaction) = self.take_transaction_for_close()? else {
            return Ok(Value::Bool(false));
        };
        match transaction.rollback().await {
            Ok(()) => {
                self.finish_transaction(MysqlTransactionStatus::Closed)?;
                MYSQL_TRANSACTION_ROLLED_BACK.fetch_add(1, Ordering::Relaxed);
                MYSQL_TRANSACTION_CLOSED.fetch_add(1, Ordering::Relaxed);
                Ok(Value::Bool(true))
            }
            Err(err) => {
                self.finish_failed_transaction()?;
                Err(format!("Failed to close MySQL transaction: {}", err))
            }
        }
    }

    /// Returns the current transaction status text.
    fn status_text(&self) -> Result<String, String> {
        let inner = self
            .state
            .lock()
            .map_err(|_| "MySQL transaction lock is poisoned".to_string())?;
        Ok(inner.status.as_str().to_string())
    }

    /// Returns the debug text of the current SQL.
    fn sql_text(&self, args: &[Value]) -> Result<String, String> {
        let render_binds = args.first().map(Value::is_truthy).unwrap_or(true);
        if !render_binds {
            return Ok(self.sql.clone());
        }
        let binds: Vec<_> = self.binds.iter().map(MysqlBindValue::from_value).collect();
        Ok(BtMysql::format_sql_with_binds(&self.sql, &binds))
    }

    /// binds the parameters saved by the current transaction query object.
    fn bind_query<'q>(&self, mut query: BtMysqlQuery<'q>) -> BtMysqlQuery<'q> {
        for value in &self.binds {
            query = BtMysql::bind_value(query, value);
        }
        query
    }

    /// Synchronously waits for a MySQL transaction asynchronous task.
    fn run_async<Fut>(&self, method: &str, task: Fut) -> Result<Value, String>
    where
        Fut: std::future::Future<Output = Result<Value, String>>,
    {
        let config = mysql_pool_config()?;
        let start = Instant::now();
        let result = crate::io::run_async(task, Some(config.query_timeout()));
        self.record_slow_call(method, start.elapsed());
        result
    }

    /// Verifies the necessary status before transaction query.
    fn validate_query(&self, method: &str) -> Result<(), String> {
        if self.sql.trim().is_empty() {
            return Err(format!(
                "mysql transaction.{}() requires query(sql) to be called first",
                method
            ));
        }
        Ok(())
    }

    /// Retrieve the transaction handle from the shared state.
    fn take_transaction(&self, method: &str) -> Result<Transaction<'static, MySql>, String> {
        let mut inner = self
            .state
            .lock()
            .map_err(|_| "MySQL transaction lock is poisoned".to_string())?;
        if inner.status != MysqlTransactionStatus::Active {
            return Err(format!(
                "mysql transaction.{}() requires an active transaction; current status: {}",
                method,
                inner.status.as_str()
            ));
        }
        inner.transaction.take().ok_or_else(|| {
            format!(
                "mysql transaction.{}() is already in use; the same transaction cannot be reused concurrently",
                method
            )
        })
    }

    /// close() Special transaction removal logic, completed status return None.
    fn take_transaction_for_close(&self) -> Result<Option<Transaction<'static, MySql>>, String> {
        let mut inner = self
            .state
            .lock()
            .map_err(|_| "MySQL transaction lock is poisoned".to_string())?;
        if inner.status != MysqlTransactionStatus::Active {
            return Ok(None);
        }
        inner.transaction.take().map(Some).ok_or_else(|| {
            "mysql transaction.close() is executing other operations and cannot close the same transaction concurrently.".to_string()
        })
    }

    /// puts the completed transaction handle back into the shared state.
    fn restore_transaction(&self, transaction: Transaction<'static, MySql>) -> Result<(), String> {
        let mut inner = self
            .state
            .lock()
            .map_err(|_| "MySQL transaction lock is poisoned".to_string())?;
        if inner.status == MysqlTransactionStatus::Active && inner.transaction.is_none() {
            inner.transaction = Some(transaction);
        }
        Ok(())
    }

    /// marks that the transaction has ended normally.
    fn finish_transaction(&self, status: MysqlTransactionStatus) -> Result<(), String> {
        let mut inner = self
            .state
            .lock()
            .map_err(|_| "MySQL transaction lock is poisoned".to_string())?;
        if inner.status == MysqlTransactionStatus::Active {
            MYSQL_TRANSACTION_ACTIVE.fetch_sub(1, Ordering::Relaxed);
        }
        inner.transaction = None;
        inner.status = status;
        Ok(())
    }

    /// marks the transaction as failed and releases the activity count.
    fn finish_failed_transaction(&self) -> Result<(), String> {
        MYSQL_TRANSACTION_FAILED.fetch_add(1, Ordering::Relaxed);
        self.finish_transaction(MysqlTransactionStatus::Failed)
    }

    /// Logs MySQL transaction slow calls that exceed a threshold.
    fn record_slow_call(&self, method: &str, elapsed: Duration) {
        let Ok(config) = mysql_pool_config() else {
            return;
        };
        if config.slow_ms == 0 || elapsed < Duration::from_millis(config.slow_ms) {
            return;
        }
        MYSQL_SLOW_CALLS.fetch_add(1, Ordering::Relaxed);
        eprintln!(
            "BT slow MySQL transaction call: mysql transaction.{}() takes {} milliseconds; binds={}",
            method,
            elapsed.as_millis(),
            self.binds.len()
        );
    }

    /// Format transaction SQL execution error.
    fn sql_error(&self, prefix: &str, err: sqlx::Error) -> String {
        format!(
            "{}: {}; sql=`{}` binds={}",
            prefix,
            err,
            self.sql,
            self.binds.len()
        )
    }
}

/// starts a MySQL transaction.
async fn begin_mysql_transaction(dsn: &str) -> Result<Transaction<'static, MySql>, String> {
    if let Some(pool) = mysql_pool(dsn).await? {
        return pool.begin().await.map_err(|err| {
            format!(
                "Failed to open MySQL transaction for `{}`: {}",
                redact_mysql_dsn(dsn),
                sanitize_mysql_error_text(dsn, err)
            )
        });
    }

    let config = mysql_pool_config()?;
    let pool = build_mysql_transaction_pool(dsn, config).await?;
    pool.begin().await.map_err(|err| {
        format!(
            "Failed to open MySQL transaction for `{}`: {}",
            redact_mysql_dsn(dsn),
            sanitize_mysql_error_text(dsn, err)
        )
    })
}

/// Returns MySQL normal query reusable connection pool; returns None when pooling is turned off.
async fn mysql_pool(dsn: &str) -> Result<Option<MySqlPool>, String> {
    let config = mysql_pool_config()?;
    if !config.enabled {
        MYSQL_POOL_BYPASSED.fetch_add(1, Ordering::Relaxed);
        return Ok(None);
    }

    let now = Instant::now();
    let store = MYSQL_POOL_STORE.get_or_init(|| Mutex::new(MysqlPoolStore::new()));
    {
        let mut store = store
            .lock()
            .map_err(|_| "MySQL connection pool lock is poisoned".to_string())?;
        store.prune_idle(now, config);
        if let Some(entry) = store.entries.get_mut(dsn) {
            entry.last_used = now;
            MYSQL_POOL_HITS.fetch_add(1, Ordering::Relaxed);
            return Ok(Some(entry.pool.clone()));
        }
    }

    MYSQL_POOL_MISSES.fetch_add(1, Ordering::Relaxed);
    let pool = build_mysql_pool(dsn, config).await?;
    MYSQL_POOL_CREATED.fetch_add(1, Ordering::Relaxed);
    let mut store = store
        .lock()
        .map_err(|_| "MySQL connection pool lock is poisoned".to_string())?;
    if let Some(entry) = store.entries.get_mut(dsn) {
        entry.last_used = now;
        MYSQL_POOL_HITS.fetch_add(1, Ordering::Relaxed);
        return Ok(Some(entry.pool.clone()));
    }
    store.insert(dsn.to_string(), pool.clone(), now, config);
    Ok(Some(pool))
}

/// Creates a SQLx MySQL connection pool as configured.
async fn build_mysql_pool(dsn: &str, config: &MysqlPoolConfig) -> Result<MySqlPool, String> {
    crate::io::ensure_rustls_provider();
    MySqlPoolOptions::new()
        .min_connections(config.min_connections as u32)
        .max_connections(config.max_connections as u32)
        .acquire_timeout(config.connect_timeout())
        .idle_timeout(config.idle_ttl())
        .connect(dsn)
        .await
        .map_err(|err| {
            MYSQL_POOL_BUILD_FAILED.fetch_add(1, Ordering::Relaxed);
            format!(
                "Create MySQL connection pool `{}` failed: {}",
                redact_mysql_dsn(dsn),
                sanitize_mysql_error_text(dsn, err)
            )
        })
}

/// Create transaction-specific temporary connection pool.
///
/// When the user explicitly turns off the normal query global pool, transactions still require SQLx's `Pool::begin()` to obtain a holdable
/// `'static` transaction handle. A single-connection temporary pool is used here to avoid pre-building multiple additional connections for one transaction.
async fn build_mysql_transaction_pool(
    dsn: &str,
    config: &MysqlPoolConfig,
) -> Result<MySqlPool, String> {
    crate::io::ensure_rustls_provider();
    MySqlPoolOptions::new()
        .min_connections(0)
        .max_connections(1)
        .acquire_timeout(config.connect_timeout())
        .idle_timeout(config.idle_ttl())
        .connect(dsn)
        .await
        .map_err(|err| {
            MYSQL_POOL_BUILD_FAILED.fetch_add(1, Ordering::Relaxed);
            format!(
                "Create MySQL transaction connection `{}` failed: {}",
                redact_mysql_dsn(dsn),
                sanitize_mysql_error_text(dsn, err)
            )
        })
}

/// Returns MySQL connection pool statistics snapshot.
pub fn stats() -> MysqlPoolStats {
    let (config, config_error) = match mysql_pool_config() {
        Ok(config) => (config.clone(), None),
        Err(err) => (fallback_mysql_pool_config(), Some(err)),
    };
    let (entries, connections, idle_connections) = MYSQL_POOL_STORE
        .get()
        .and_then(|store| {
            store.lock().ok().map(|store| {
                let entries = store.entries.len();
                let connections = store
                    .entries
                    .values()
                    .map(|entry| entry.pool.size() as usize)
                    .sum();
                let idle_connections = store
                    .entries
                    .values()
                    .map(|entry| entry.pool.num_idle())
                    .sum();
                (entries, connections, idle_connections)
            })
        })
        .unwrap_or((0, 0, 0));
    MysqlPoolStats {
        config: MysqlPoolConfigSnapshot {
            enabled: config.enabled,
            pool_limit: config.pool_limit,
            min_connections: config.min_connections,
            max_connections: config.max_connections,
            idle_ttl_ms: config.idle_ttl_ms,
            connect_timeout_ms: config.connect_timeout_ms,
            query_timeout_ms: config.query_timeout_ms,
            slow_ms: config.slow_ms,
            config_error,
        },
        pool_started: MYSQL_POOL_STORE.get().is_some(),
        entries,
        connections,
        idle_connections,
        hits: MYSQL_POOL_HITS.load(Ordering::Relaxed),
        misses: MYSQL_POOL_MISSES.load(Ordering::Relaxed),
        created: MYSQL_POOL_CREATED.load(Ordering::Relaxed),
        evicted: MYSQL_POOL_EVICTED.load(Ordering::Relaxed),
        bypassed: MYSQL_POOL_BYPASSED.load(Ordering::Relaxed),
        build_failed: MYSQL_POOL_BUILD_FAILED.load(Ordering::Relaxed),
        slow_calls: MYSQL_SLOW_CALLS.load(Ordering::Relaxed),
        transactions_active: MYSQL_TRANSACTION_ACTIVE.load(Ordering::Relaxed),
        transactions_started: MYSQL_TRANSACTION_STARTED.load(Ordering::Relaxed),
        transactions_committed: MYSQL_TRANSACTION_COMMITTED.load(Ordering::Relaxed),
        transactions_rolled_back: MYSQL_TRANSACTION_ROLLED_BACK.load(Ordering::Relaxed),
        transactions_closed: MYSQL_TRANSACTION_CLOSED.load(Ordering::Relaxed),
        transactions_failed: MYSQL_TRANSACTION_FAILED.load(Ordering::Relaxed),
    }
}

/// Returns the MySQL connection pool configuration.
fn mysql_pool_config() -> Result<&'static MysqlPoolConfig, String> {
    match MYSQL_POOL_CONFIG.get_or_init(MysqlPoolConfig::from_env) {
        Ok(config) => Ok(config),
        Err(err) => Err(err.clone()),
    }
}

/// Returns the conservative MySQL connection pool configuration for statistics display.
fn fallback_mysql_pool_config() -> MysqlPoolConfig {
    MysqlPoolConfig {
        enabled: true,
        pool_limit: DEFAULT_MYSQL_POOL_LIMIT,
        min_connections: DEFAULT_MYSQL_POOL_MIN_CONNECTIONS,
        max_connections: DEFAULT_MYSQL_POOL_MAX_CONNECTIONS,
        idle_ttl_ms: DEFAULT_MYSQL_POOL_IDLE_TTL_MS,
        connect_timeout_ms: DEFAULT_MYSQL_CONNECT_TIMEOUT_MS,
        query_timeout_ms: DEFAULT_MYSQL_QUERY_TIMEOUT_MS,
        slow_ms: DEFAULT_MYSQL_SLOW_MS,
    }
}

/// Read bool type environment variables.
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

/// Read usize type environment variable.
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

/// reads u64 type environment variables.
fn read_u64_env(name: &str, default: u64) -> Result<u64, String> {
    let Some(value) = std::env::var(name).ok() else {
        return Ok(default);
    };
    value
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("{} must be an integer not less than 0", name))
}

/// Returns a MySQL DSN that does not reveal passwords.
fn redact_mysql_dsn(dsn: &str) -> String {
    let Ok(mut url) = url::Url::parse(dsn) else {
        return "<invalid-dsn>".to_string();
    };
    if url.password().is_some() {
        let _ = url.set_password(Some("***"));
    }
    url.to_string()
}

/// Returns MySQL error text that does not reveal the original DSN.
fn sanitize_mysql_error_text(dsn: &str, err: sqlx::Error) -> String {
    let text = err.to_string();
    if text.contains(dsn) {
        text.replace(dsn, &redact_mysql_dsn(dsn))
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::mysql::MySqlConnectOptions;
    use sqlx::ConnectOptions;

    /// SQLx should identify hosts by the last @, allowing passwords containing unescaped @.
    ///
    /// `to_url_lossy()` will encode `@` in the password into `%40` according to URL rules; what is verified here is that the host has not been truncated by `to_url_lossy()` in the
    /// `@` in the password is truncated, while the password content is still retained in encoded form.
    #[test]
    fn sqlx_parses_password_with_at_sign() {
        let options: MySqlConnectOptions = "mysql://user:p@ss@127.0.0.1/test".parse().unwrap();
        let url = options.to_url_lossy();

        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_eq!(url.path(), "/test");
        assert_eq!(url.password(), Some("p%40ss"));
    }

    /// DSN desensitization cannot reveal passwords containing @.
    #[test]
    fn mysql_dsn_redaction_hides_password_with_at_sign() {
        let redacted = redact_mysql_dsn("mysql://user:p@ss@127.0.0.1/test");

        assert!(!redacted.contains("p@ss"));
        assert!(!redacted.contains("p%40ss"));
        assert!(redacted.contains("***"));
        assert!(redacted.contains("127.0.0.1"));
    }

    /// The JSON field should be returned to the BT native object for scripts to continue chaining access.
    #[test]
    fn json_text_to_value_returns_bt_object() {
        let value = BtMysql::json_text_to_value(r#"{"id":1,"ok":true}"#.to_string());
        let Value::Object(values) = value else {
            panic!("JSON objects should be converted to BT objects");
        };
        let values = values.borrow();
        assert_eq!(values.get("id"), Some(&Value::Int(1)));
        assert_eq!(values.get("ok"), Some(&Value::Bool(true)));
    }

    /// Unsigned integers beyond i64 should be converted to strings preserving precision.
    #[test]
    fn u64_to_value_preserves_large_value() {
        assert_eq!(BtMysql::u64_to_value(42), Value::Int(42));
        assert_eq!(
            BtMysql::u64_to_value(u64::MAX),
            Value::Str(u64::MAX.to_string())
        );
    }

    /// Creates a MySQL object for unit testing.
    fn mysql_fixture(sql: &str) -> BtMysql {
        BtMysql {
            dsn: "mysql://user:pass@127.0.0.1/test".to_string(),
            sql: sql.to_string(),
            binds: Vec::new(),
            bind_rows: Vec::new(),
            batch_size: DEFAULT_BATCH_SIZE,
            workers: DEFAULT_MYSQL_WORKERS,
        }
    }

    /// Gets the MySQL object from the method call return value.
    fn unwrap_mysql(value: Value) -> BtMysql {
        let Value::Mysql(mysql) = value else {
            panic!("The return value should be a MySQL object");
        };
        mysql
    }

    /// Get the MySQL transaction object from the method call return value.
    fn unwrap_mysql_transaction(value: Value) -> BtMysqlTransaction {
        let Value::MysqlTransaction(transaction) = value else {
            panic!("The return value should be a MySQL transaction object");
        };
        transaction
    }

    /// sql() should replace ordinary placeholders.
    #[test]
    fn sql_text_replaces_plain_placeholders() {
        let mut mysql = mysql_fixture("select * from user where id=? and name=?");
        mysql.binds.push(Value::Int(1001));
        mysql.binds.push(Value::Str("Zhang San".to_string()));

        assert_eq!(
            mysql.sql_text(&[]).unwrap(),
            "select * from user where id=1001 and name='Zhang San'"
        );
    }

    /// sql() should support multiple bind() append parameters.
    #[test]
    fn sql_text_supports_repeated_bind_calls() {
        let mysql = mysql_fixture("select * from user where id=? and name=? and sex=?");
        let mysql = unwrap_mysql(mysql.call_method("bind", vec![Value::Int(1001)]).unwrap());
        let mysql = unwrap_mysql(
            mysql
                .call_method("bind", vec![Value::Str("Zhang San".to_string())])
                .unwrap(),
        );
        let mysql = unwrap_mysql(mysql.call_method("bind", vec![Value::Int(1)]).unwrap());

        assert_eq!(
            mysql.sql_text(&[]).unwrap(),
            "select * from user where id=1001 and name='Zhang San' and sex=1"
        );
    }

    /// sql() should support mixed calls of one bind() and multiple bind().
    #[test]
    fn sql_text_supports_mixed_bind_calls() {
        let mysql = mysql_fixture("select * from user where id=? and name=? and sex=?");
        let mysql = unwrap_mysql(
            mysql
                .call_method(
                    "bind",
                    vec![Value::Int(1001), Value::Str("Zhang San".to_string())],
                )
                .unwrap(),
        );
        let mysql = unwrap_mysql(mysql.call_method("bind", vec![Value::Int(1)]).unwrap());

        assert_eq!(
            mysql.sql_text(&[]).unwrap(),
            "select * from user where id=1001 and name='Zhang San' and sex=1"
        );
    }

    /// sql() should correctly escape single quotes and backslashes in strings.
    #[test]
    fn sql_text_escapes_string_literal() {
        let mut mysql = mysql_fixture("select * from user where name=? and path=?");
        mysql.binds.push(Value::Str("Tom's cat".to_string()));
        mysql.binds.push(Value::Str(r"C:\tmp".to_string()));

        assert_eq!(
            mysql.sql_text(&[]).unwrap(),
            r"select * from user where name='Tom''s cat' and path='C:\\tmp'"
        );
    }

    /// sql() should not replace question marks in SQL string literals.
    #[test]
    fn sql_text_keeps_question_mark_inside_sql_string() {
        let mut mysql = mysql_fixture("select '?' as mark, name from user where id=?");
        mysql.binds.push(Value::Int(1001));

        assert_eq!(
            mysql.sql_text(&[]).unwrap(),
            "select '?' as mark, name from user where id=1001"
        );
    }

    /// sql() should retain remaining question marks when there are fewer bind parameters than placeholders.
    #[test]
    fn sql_text_keeps_remaining_placeholders() {
        let mut mysql = mysql_fixture("select * from user where a=? and b=?");
        mysql.binds.push(Value::Int(1));

        assert_eq!(
            mysql.sql_text(&[]).unwrap(),
            "select * from user where a=1 and b=?"
        );
    }

    /// sql() should append the extra binds annotation when the bind parameters have more than placeholders.
    #[test]
    fn sql_text_appends_extra_binds_comment() {
        let mut mysql = mysql_fixture("select * from user where a=?");
        mysql.binds.push(Value::Int(1));
        mysql.binds.push(Value::Int(2));
        mysql.binds.push(Value::Int(3));

        assert_eq!(
            mysql.sql_text(&[]).unwrap(),
            "select * from user where a=1 /* extra binds: 2 */"
        );
    }

    /// sql(false) should return raw unbound SQL.
    #[test]
    fn sql_text_false_returns_raw_sql() {
        let mut mysql = mysql_fixture("select * from user where a=?");
        mysql.binds.push(Value::Int(1));

        assert_eq!(
            mysql.sql_text(&[Value::Bool(false)]).unwrap(),
            "select * from user where a=?"
        );
    }

    /// sql() should support binds(), batch() and workers() to generate INSERT batch previews.
    #[test]
    fn sql_text_supports_binds_batch_and_workers() {
        let rows = Value::Array(Rc::new(RefCell::new(vec![
            Value::Array(Rc::new(RefCell::new(vec![
                Value::Str("A".to_string()),
                Value::Int(18),
            ]))),
            Value::Array(Rc::new(RefCell::new(vec![
                Value::Str("B".to_string()),
                Value::Int(20),
            ]))),
            Value::Array(Rc::new(RefCell::new(vec![
                Value::Str("C".to_string()),
                Value::Int(22),
            ]))),
        ])));
        let mysql = mysql_fixture("insert into user(name, age) values (?, ?)");
        let mysql = unwrap_mysql(mysql.call_method("binds", vec![rows]).unwrap());
        let mysql = unwrap_mysql(mysql.call_method("batch", vec![Value::Int(2)]).unwrap());
        let mysql = unwrap_mysql(mysql.call_method("workers", vec![Value::Int(4)]).unwrap());

        assert_eq!(
            mysql.sql_text(&[]).unwrap(),
            "insert into user(name, age) values ('A', 18), ('B', 20) /* binds: 3 rows, batch: 2, workers: 4 */"
        );
    }

    /// to_string() has been removed from the mysql object.
    #[test]
    fn to_string_method_is_removed() {
        let mysql = mysql_fixture("select 1");
        let err = mysql.call_method("to_string", Vec::new()).unwrap_err();

        assert!(err.contains("has no method"));
    }

    /// The query interface should reject batch binding parameters to avoid configurations being silently ignored.
    #[test]
    fn query_methods_reject_batch_options() {
        let rows = Value::Array(Rc::new(RefCell::new(vec![Value::Array(Rc::new(
            RefCell::new(vec![Value::Int(1)]),
        ))])));
        let mysql = mysql_fixture("select * from user where id=?");
        let mysql = unwrap_mysql(mysql.call_method("binds", vec![rows]).unwrap());

        assert_eq!(
            mysql.all().unwrap_err(),
            "mysql.all() does not support binds()"
        );
    }

    /// workers should not be equal to the number of connections to avoid creating a large number of OS threads in high concurrency stress tests.
    #[test]
    fn mysql_worker_threads_stays_bounded() {
        assert!(BtMysql::mysql_worker_threads(1000) <= MAX_MYSQL_WORKER_THREADS);
        assert!(BtMysql::mysql_worker_threads(1000) < 1000);
        assert_eq!(BtMysql::mysql_worker_threads(1), 1);
    }

    /// The transaction query builder should reuse the same transaction state and be able to preview the bound SQL.
    #[test]
    fn transaction_query_builder_formats_sql() {
        let transaction =
            BtMysqlTransaction::new_for_test(MysqlTransactionStatus::Active, "", Vec::new());
        let transaction = unwrap_mysql_transaction(
            transaction
                .call_method(
                    "query",
                    vec![Value::Str("select * from user where id=?".to_string())],
                )
                .unwrap(),
        );
        let transaction = unwrap_mysql_transaction(
            transaction
                .call_method("bind", vec![Value::Int(7)])
                .unwrap(),
        );

        assert_eq!(
            transaction.call_method("sql", Vec::new()).unwrap(),
            Value::Str("select * from user where id=7".to_string())
        );
        assert_eq!(
            transaction.call_method("status", Vec::new()).unwrap(),
            Value::Str("active".to_string())
        );
    }

    /// Calling close() on an ended transaction should return false to avoid misjudgment caused by repeated rollbacks.
    #[test]
    fn transaction_close_inactive_returns_false() {
        let transaction =
            BtMysqlTransaction::new_for_test(MysqlTransactionStatus::Committed, "", Vec::new());

        assert_eq!(
            transaction.call_method("close", Vec::new()).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            transaction.call_method("status", Vec::new()).unwrap(),
            Value::Str("committed".to_string())
        );
    }

    /// When the underlying transaction is fetched during transaction execution, concurrent reuse of the same transaction should be rejected.
    #[test]
    fn transaction_rejects_concurrent_reuse() {
        let transaction = BtMysqlTransaction::new_for_test(
            MysqlTransactionStatus::Active,
            "select * from user where id=?",
            vec![Value::Int(1)],
        );

        let err = transaction.call_method("one", Vec::new()).unwrap_err();

        assert!(err.contains("the same transaction cannot be reused concurrently"));
    }
}
