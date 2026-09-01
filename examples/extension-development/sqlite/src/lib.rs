//! SQLite extension library.
//!
//! This extension stores SQLite connections in a shared WASM worker and provides persistent
//! connections, result limits, object disposal, and BtValueBinary type boundaries through a
//! coarse-grained SQL API.

use std::cell::RefCell;
use std::time::Duration;

use bt_extension_sdk::{
    bt_extension, bt_extension_init, bt_extension_shutdown, bt_extension_stats, expect_arg_count,
    expect_ext_object_type, expect_string, BtResult, BtValue, ExtObject, ObjectStore,
};
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{params_from_iter, Connection, Row};

/// The `type_id` declared for the Sqlite object in bindings.json.
const DB_TYPE_ID: u32 = 1;
/// The type name declared for the Sqlite object in bindings.json.
const DB_TYPE_NAME: &str = "Sqlite";
/// The `type_id` declared for the SqliteQuery object in bindings.json.
const QUERY_TYPE_ID: u32 = 2;
/// The type name declared for the SqliteQuery object in bindings.json.
const QUERY_TYPE_NAME: &str = "SqliteQuery";
/// Maximum number of SQLite connections retained in a single worker.
const MAX_CONNECTIONS: usize = 64;
/// Maximum number of chained query objects retained in a single worker.
const MAX_QUERIES: usize = 4096;
/// Default SQLite batch size; 0 means use all bound rows.
const DEFAULT_BATCH_SIZE: usize = 0;
/// Migration-compatible default for SQLite workers; writes on one connection are always serial.
const DEFAULT_SQLITE_WORKERS: usize = 1;
/// Hard limit for the SQLite workers parameter, matching the MySQL migration boundary.
const MAX_SQLITE_WORKERS: usize = 4096;
/// Default maximum number of rows returned by all().
const DEFAULT_MAX_ROWS: usize = 1000;
/// Hard limit on the number of rows returned by all().
const HARD_MAX_ROWS: usize = 100_000;
/// Default maximum estimated result size for all().
const DEFAULT_MAX_RESULT_BYTES: usize = 4 * 1024 * 1024;
/// Hard limit on the estimated result size for all().
const HARD_MAX_RESULT_BYTES: usize = 16 * 1024 * 1024;
/// Default SQLite busy timeout.
const DEFAULT_BUSY_TIMEOUT_MS: u64 = 1000;
/// Hard limit for the SQLite busy timeout.
const HARD_BUSY_TIMEOUT_MS: u64 = 300_000;

thread_local! {
    /// SQLite connection table in the current WASM worker.
    static DATABASES: RefCell<ObjectStore<DbState>> =
        RefCell::new(ObjectStore::new(MAX_CONNECTIONS));
    /// Chained query object table in the current WASM worker.
    static QUERIES: RefCell<ObjectStore<QueryState>> =
        RefCell::new(ObjectStore::new(MAX_QUERIES));
    /// Lightweight statistics for the current WASM worker.
    static STATS: RefCell<SqliteStats> = RefCell::new(SqliteStats::default());
}

/// State for a single SQLite connection.
struct DbState {
    /// SQLite connection.
    conn: Connection,
    /// Resource limits used by the current connection.
    options: DbOptions,
}

/// State for a chained query object.
#[derive(Clone)]
struct QueryState {
    /// ID of the database object bound to the query.
    db_id: u64,
    /// SQL to execute.
    sql: String,
    /// Single set of parameters bound by `bind()`.
    params: Vec<SqlValue>,
    /// Multiple parameter sets bound by `binds()`.
    bind_rows: Vec<Vec<SqlValue>>,
    /// Batch size set by `batch()`; affects only statistics and the SQL preview.
    batch_size: usize,
    /// Worker count set by `workers()`; writes on one SQLite connection are normalized but not concurrent.
    workers: usize,
}

/// SQLite connection-level configuration.
#[derive(Debug, Clone, Copy)]
struct DbOptions {
    /// SQLite busy timeout in milliseconds.
    busy_timeout_ms: u64,
    /// Maximum number of rows returned by all().
    max_rows: usize,
    /// Maximum estimated result size for all() and one().
    max_result_bytes: usize,
    /// Whether WAL journal_mode is enabled.
    wal: bool,
}

impl Default for DbOptions {
    /// Returns conservative defaults.
    fn default() -> Self {
        Self {
            busy_timeout_ms: DEFAULT_BUSY_TIMEOUT_MS,
            max_rows: DEFAULT_MAX_ROWS,
            max_result_bytes: DEFAULT_MAX_RESULT_BYTES,
            wal: false,
        }
    }
}

/// Extension-side statistics for a single worker.
#[derive(Debug, Default, Clone)]
struct SqliteStats {
    /// Number of worker initializations.
    init_calls: u64,
    /// Number of times SQLite was opened.
    opens: u64,
    /// Number of times SQLite was closed.
    closes: u64,
    /// Number of SQL executions.
    execs: u64,
    /// Number of one() queries.
    one_calls: u64,
    /// Number of all() queries.
    all_calls: u64,
    /// Number of transaction() calls.
    transactions: u64,
    /// Total number of rows returned.
    rows_returned: u64,
    /// Total estimated bytes returned.
    bytes_returned: u64,
}

/// An SQL plan within transaction().
struct StatementPlan {
    /// SQL to execute.
    sql: String,
    /// SQL parameters.
    params: Vec<SqlValue>,
}

bt_extension!(
    1 => entry_open,
    2 => method_db_one,
    3 => method_db_all,
    4 => method_db_exec,
    5 => method_db_transaction,
    6 => method_db_query,
    7 => method_db_close,
    8 => method_query_bind,
    9 => method_query_one,
    10 => method_query_all,
    11 => method_query_exec,
    12 => method_query_close,
    13 => method_query_binds,
    14 => method_query_batch,
    15 => method_query_workers,
    16 => method_query_sql,
);

bt_extension_init!(lifecycle_init);
bt_extension_shutdown!(lifecycle_shutdown);
bt_extension_stats!(lifecycle_stats);

/// Initializes the current shared worker.
fn lifecycle_init(config: BtValue) -> BtResult<BtValue> {
    let _ = config;
    STATS.with(|stats| {
        let mut stats = stats.borrow_mut();
        stats.init_calls = stats.init_calls.saturating_add(1);
    });
    Ok(BtValue::Bool(true))
}

/// Shuts down the current shared worker and releases all worker state.
fn lifecycle_shutdown() -> BtResult<BtValue> {
    reset_state();
    Ok(BtValue::Bool(true))
}

/// Returns SQLite extension statistics for the current worker.
fn lifecycle_stats() -> BtResult<BtValue> {
    let stats = STATS.with(|stats| stats.borrow().clone());
    let active_connections = DATABASES.with(|databases| databases.borrow().len());
    let query_objects = QUERIES.with(|queries| queries.borrow().len());
    Ok(object_value(vec![
        ("active_connections", usize_value(active_connections)?),
        ("query_objects", usize_value(query_objects)?),
        ("init_calls", u64_value(stats.init_calls)?),
        ("opens", u64_value(stats.opens)?),
        ("closes", u64_value(stats.closes)?),
        ("execs", u64_value(stats.execs)?),
        ("one_calls", u64_value(stats.one_calls)?),
        ("all_calls", u64_value(stats.all_calls)?),
        ("transactions", u64_value(stats.transactions)?),
        ("rows_returned", u64_value(stats.rows_returned)?),
        ("bytes_returned", u64_value(stats.bytes_returned)?),
    ]))
}

/// Opens a SQLite database and returns a connection object.
fn entry_open(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 2, "sqlite_open")?;
    let path = expect_string(&args, 0, "path")?;
    if path.is_empty() {
        return Err("sqlite_open path must not be empty".to_string());
    }
    let options = parse_options(args.get(1), "options")?;
    let conn = Connection::open(&path).map_err(sqlite_error)?;
    conn.busy_timeout(Duration::from_millis(options.busy_timeout_ms))
        .map_err(sqlite_error)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(sqlite_error)?;
    if options.wal {
        conn.execute_batch("PRAGMA journal_mode = WAL;")
            .map_err(sqlite_error)?;
    }

    let object_id =
        DATABASES.with(|databases| databases.borrow_mut().insert(DbState { conn, options }))?;
    bump_stat(|stats| stats.opens = stats.opens.saturating_add(1));
    Ok(BtValue::ExtObject(ExtObject::new(
        DB_TYPE_ID,
        object_id,
        DB_TYPE_NAME,
    )))
}

/// Executes SQL and returns the first row as an object, or empty when there is no result.
fn method_db_one(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 3, "Sqlite.one")?;
    let object = db_receiver(&args, "self")?;
    let sql = expect_string(&args, 1, "sql")?;
    let params = parse_params(args.get(2), "params")?;
    with_db_mut(&object, |db| run_one(db, &sql, &params))
}

/// Executes SQL and returns an array of objects.
fn method_db_all(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 3, "Sqlite.all")?;
    let object = db_receiver(&args, "self")?;
    let sql = expect_string(&args, 1, "sql")?;
    let params = parse_params(args.get(2), "params")?;
    with_db_mut(&object, |db| run_all(db, &sql, &params))
}

/// Executes a single SQL write and returns the number of affected rows.
fn method_db_exec(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 3, "Sqlite.exec")?;
    let object = db_receiver(&args, "self")?;
    let sql = expect_string(&args, 1, "sql")?;
    let params = parse_params(args.get(2), "params")?;
    with_db_mut(&object, |db| run_exec(db, &sql, &params))
}

/// Executes multiple SQL statements serially in a SQLite transaction.
fn method_db_transaction(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 2, "Sqlite.transaction")?;
    let object = db_receiver(&args, "self")?;
    let plans = parse_transaction_plans(args.get(1), "statements")?;
    with_db_mut(&object, |db| run_transaction(db, &plans))
}

/// Creates a chained query object.
fn method_db_query(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 2, "Sqlite.query")?;
    let object = db_receiver(&args, "self")?;
    let sql = expect_string(&args, 1, "sql")?;
    with_db_mut(&object, |_| Ok(()))?;
    let object_id = QUERIES.with(|queries| {
        queries.borrow_mut().insert(QueryState {
            db_id: object.object_id,
            sql,
            params: Vec::new(),
            bind_rows: Vec::new(),
            batch_size: DEFAULT_BATCH_SIZE,
            workers: DEFAULT_SQLITE_WORKERS,
        })
    })?;
    Ok(BtValue::ExtObject(ExtObject::new(
        QUERY_TYPE_ID,
        object_id,
        QUERY_TYPE_NAME,
    )))
}

/// Closes a SQLite connection.
fn method_db_close(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 1, "Sqlite.close")?;
    let object = db_receiver(&args, "self")?;
    DATABASES.with(|databases| {
        databases
            .borrow_mut()
            .remove_required(object.object_id, DB_TYPE_NAME)
            .map(|_| ())
    })?;
    bump_stat(|stats| stats.closes = stats.closes.saturating_add(1));
    Ok(BtValue::Bool(true))
}

/// Appends a bound value to a chained query.
fn method_query_bind(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 2, "SqliteQuery.bind")?;
    let object = query_receiver(&args, "self")?;
    let value = args
        .get(1)
        .ok_or_else(|| "SqliteQuery.bind is missing the value argument".to_string())?;
    let sql_value = bt_value_to_sql(value)?;
    QUERIES.with(|queries| {
        let mut queries = queries.borrow_mut();
        let query = queries.get_mut_required(object.object_id, QUERY_TYPE_NAME)?;
        query.params.push(sql_value);
        Ok::<(), String>(())
    })?;
    Ok(BtValue::ExtObject(ExtObject::new(
        QUERY_TYPE_ID,
        object.object_id,
        QUERY_TYPE_NAME,
    )))
}

/// Appends multiple rows of bound parameters to a chained query.
fn method_query_binds(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 2, "SqliteQuery.binds")?;
    let object = query_receiver(&args, "self")?;
    let rows = parse_bind_rows(
        args.get(1)
            .ok_or_else(|| "SqliteQuery.binds is missing the rows argument".to_string())?,
        "rows",
    )?;
    QUERIES.with(|queries| {
        let mut queries = queries.borrow_mut();
        let query = queries.get_mut_required(object.object_id, QUERY_TYPE_NAME)?;
        query.bind_rows.extend(rows);
        Ok::<(), String>(())
    })?;
    Ok(BtValue::ExtObject(ExtObject::new(
        QUERY_TYPE_ID,
        object.object_id,
        QUERY_TYPE_NAME,
    )))
}

/// Sets the batch size for a chained query.
fn method_query_batch(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 2, "SqliteQuery.batch")?;
    let object = query_receiver(&args, "self")?;
    let batch_size = batch_arg(args.get(1), "size")?;
    QUERIES.with(|queries| {
        let mut queries = queries.borrow_mut();
        let query = queries.get_mut_required(object.object_id, QUERY_TYPE_NAME)?;
        query.batch_size = batch_size;
        Ok::<(), String>(())
    })?;
    Ok(BtValue::ExtObject(ExtObject::new(
        QUERY_TYPE_ID,
        object.object_id,
        QUERY_TYPE_NAME,
    )))
}

/// Sets the migration-compatible worker count for a chained query.
fn method_query_workers(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 2, "SqliteQuery.workers")?;
    let object = query_receiver(&args, "self")?;
    let workers = workers_arg(args.get(1), "count")?;
    QUERIES.with(|queries| {
        let mut queries = queries.borrow_mut();
        let query = queries.get_mut_required(object.object_id, QUERY_TYPE_NAME)?;
        query.workers = workers;
        Ok::<(), String>(())
    })?;
    Ok(BtValue::ExtObject(ExtObject::new(
        QUERY_TYPE_ID,
        object.object_id,
        QUERY_TYPE_NAME,
    )))
}

/// Executes a chained query and returns the first row as an object, or empty if there is no result.
fn method_query_one(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 1, "SqliteQuery.one")?;
    let query = query_state(&args)?;
    validate_query_read_method(&query, "one")?;
    let db_object = ExtObject::new(DB_TYPE_ID, query.db_id, DB_TYPE_NAME);
    with_db_mut(&db_object, |db| run_one(db, &query.sql, &query.params))
}

/// Executes a chained query and returns an array of objects.
fn method_query_all(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 1, "SqliteQuery.all")?;
    let query = query_state(&args)?;
    validate_query_read_method(&query, "all")?;
    let db_object = ExtObject::new(DB_TYPE_ID, query.db_id, DB_TYPE_NAME);
    with_db_mut(&db_object, |db| run_all(db, &query.sql, &query.params))
}

/// Executes a chained SQL write and returns the number of affected rows.
fn method_query_exec(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 1, "SqliteQuery.exec")?;
    let query = query_state(&args)?;
    let db_object = ExtObject::new(DB_TYPE_ID, query.db_id, DB_TYPE_NAME);
    with_db_mut(&db_object, |db| run_query_exec(db, &query))
}

/// Returns the SQL debug text for a chained query.
fn method_query_sql(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 1, "SqliteQuery.sql")?;
    let query = query_state(&args)?;
    Ok(BtValue::String(sql_text(&query)))
}

/// Closes a chained query object.
fn method_query_close(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 1, "SqliteQuery.close")?;
    let object = query_receiver(&args, "self")?;
    QUERIES.with(|queries| {
        queries
            .borrow_mut()
            .remove_required(object.object_id, QUERY_TYPE_NAME)
            .map(|_| ())
    })?;
    Ok(BtValue::Bool(true))
}

/// Executes the main one() path.
fn run_one(db: &mut DbState, sql: &str, params: &[SqlValue]) -> BtResult<BtValue> {
    let mut statement = db.conn.prepare(sql).map_err(sqlite_error)?;
    let column_names = statement_column_names(&statement);
    let mut rows = statement
        .query(params_from_iter(params.iter()))
        .map_err(sqlite_error)?;
    let Some(row) = rows.next().map_err(sqlite_error)? else {
        bump_stat(|stats| stats.one_calls = stats.one_calls.saturating_add(1));
        return Ok(BtValue::Empty);
    };
    let value = row_to_object(&column_names, row)?;
    let bytes = estimate_value_bytes(&value);
    if bytes > db.options.max_result_bytes {
        return Err(format!(
            "SQLite one() estimated result size {} exceeds max_result_bytes {}",
            bytes, db.options.max_result_bytes
        ));
    }
    bump_stat(|stats| {
        stats.one_calls = stats.one_calls.saturating_add(1);
        stats.rows_returned = stats.rows_returned.saturating_add(1);
        stats.bytes_returned = stats.bytes_returned.saturating_add(bytes as u64);
    });
    Ok(value)
}

/// Executes the main all() path.
fn run_all(db: &mut DbState, sql: &str, params: &[SqlValue]) -> BtResult<BtValue> {
    let mut statement = db.conn.prepare(sql).map_err(sqlite_error)?;
    let column_names = statement_column_names(&statement);
    let mut rows = statement
        .query(params_from_iter(params.iter()))
        .map_err(sqlite_error)?;
    let mut values = Vec::new();
    let mut total_bytes = 0usize;
    while let Some(row) = rows.next().map_err(sqlite_error)? {
        if values.len() >= db.options.max_rows {
            return Err(format!(
                "SQLite all() returned more than max_rows {} rows",
                db.options.max_rows
            ));
        }
        let value = row_to_object(&column_names, row)?;
        total_bytes = total_bytes.saturating_add(estimate_value_bytes(&value));
        if total_bytes > db.options.max_result_bytes {
            return Err(format!(
                "SQLite all() estimated result size {} exceeds max_result_bytes {}",
                total_bytes, db.options.max_result_bytes
            ));
        }
        values.push(value);
    }
    let row_count = values.len() as u64;
    bump_stat(|stats| {
        stats.all_calls = stats.all_calls.saturating_add(1);
        stats.rows_returned = stats.rows_returned.saturating_add(row_count);
        stats.bytes_returned = stats.bytes_returned.saturating_add(total_bytes as u64);
    });
    Ok(BtValue::Array(values))
}

/// Executes the main exec() path.
fn run_exec(db: &mut DbState, sql: &str, params: &[SqlValue]) -> BtResult<BtValue> {
    let changed = db
        .conn
        .execute(sql, params_from_iter(params.iter()))
        .map_err(sqlite_error)?;
    bump_stat(|stats| stats.execs = stats.execs.saturating_add(1));
    exec_result(
        1,
        changed as u64,
        db.conn.last_insert_rowid(),
        1,
        DEFAULT_BATCH_SIZE,
        DEFAULT_SQLITE_WORKERS,
    )
}

/// Executes the main chained exec() path.
fn run_query_exec(db: &mut DbState, query: &QueryState) -> BtResult<BtValue> {
    validate_query_sql(query, "exec")?;
    if query.bind_rows.is_empty() {
        return run_exec(db, &query.sql, &query.params);
    }
    let rows = materialize_bind_rows(query);
    let total = rows.len();
    let workers = normalized_workers(query.workers);
    if total == 0 {
        return exec_result(0, 0, 0, 0, query.batch_size, workers);
    }

    let batch_count = batch_count(total, batch_unit_size(total, query.batch_size));
    let tx = db.conn.transaction().map_err(sqlite_error)?;
    let mut rows_affected = 0u64;
    {
        let mut statement = tx.prepare(&query.sql).map_err(sqlite_error)?;
        for row in &rows {
            let changed = statement
                .execute(params_from_iter(row.iter()))
                .map_err(sqlite_error)?;
            rows_affected = rows_affected.saturating_add(changed as u64);
        }
    }
    tx.commit().map_err(sqlite_error)?;
    bump_stat(|stats| stats.execs = stats.execs.saturating_add(total as u64));
    exec_result(
        total,
        rows_affected,
        db.conn.last_insert_rowid(),
        batch_count,
        query.batch_size,
        workers,
    )
}

/// Executes the main transaction() path.
fn run_transaction(db: &mut DbState, plans: &[StatementPlan]) -> BtResult<BtValue> {
    let tx = db.conn.transaction().map_err(sqlite_error)?;
    let mut changed = 0usize;
    for plan in plans {
        let count = tx
            .execute(&plan.sql, params_from_iter(plan.params.iter()))
            .map_err(sqlite_error)?;
        changed = changed.saturating_add(count);
    }
    tx.commit().map_err(sqlite_error)?;
    bump_stat(|stats| stats.transactions = stats.transactions.saturating_add(1));
    usize_value(changed)
}

/// Reads and validates the Sqlite receiver.
fn db_receiver(args: &[BtValue], name: &str) -> BtResult<ExtObject> {
    expect_ext_object_type(args, 0, name, DB_TYPE_ID, DB_TYPE_NAME)
}

/// Reads and validates the SqliteQuery receiver.
fn query_receiver(args: &[BtValue], name: &str) -> BtResult<ExtObject> {
    expect_ext_object_type(args, 0, name, QUERY_TYPE_ID, QUERY_TYPE_NAME)
}

/// Reads a snapshot of the chained query state.
fn query_state(args: &[BtValue]) -> BtResult<QueryState> {
    let object = query_receiver(args, "self")?;
    QUERIES.with(|queries| {
        queries
            .borrow()
            .get_required(object.object_id, QUERY_TYPE_NAME)
            .cloned()
    })
}

/// Mutably accesses a SQLite connection in the connection table.
fn with_db_mut<T>(
    object: &ExtObject,
    body: impl FnOnce(&mut DbState) -> BtResult<T>,
) -> BtResult<T> {
    DATABASES.with(|databases| {
        let mut databases = databases.borrow_mut();
        let db = databases.get_mut_required(object.object_id, DB_TYPE_NAME)?;
        body(db)
    })
}

/// Parses sqlite_open options.
fn parse_options(value: Option<&BtValue>, name: &str) -> BtResult<DbOptions> {
    let Some(value) = value else {
        return Ok(DbOptions::default());
    };
    let BtValue::Object(fields) = value else {
        return Err(format!("argument `{}` must be an object", name));
    };
    let mut options = DbOptions::default();
    if let Some(value) = object_field(fields, "busy_timeout_ms") {
        options.busy_timeout_ms = bounded_u64(value, "busy_timeout_ms", 0, HARD_BUSY_TIMEOUT_MS)?;
    }
    if let Some(value) = object_field(fields, "max_rows") {
        options.max_rows = bounded_usize(value, "max_rows", 1, HARD_MAX_ROWS)?;
    }
    if let Some(value) = object_field(fields, "max_result_bytes") {
        options.max_result_bytes =
            bounded_usize(value, "max_result_bytes", 1, HARD_MAX_RESULT_BYTES)?;
    }
    if let Some(value) = object_field(fields, "wal") {
        options.wal = expect_bool_value(value, "wal")?;
    }
    Ok(options)
}

/// Parses an array of SQL parameters.
fn parse_params(value: Option<&BtValue>, name: &str) -> BtResult<Vec<SqlValue>> {
    let Some(BtValue::Array(values)) = value else {
        return Err(format!("argument `{}` must be an array", name));
    };
    let mut params = Vec::with_capacity(values.len());
    for value in values {
        params.push(bt_value_to_sql(value)?);
    }
    Ok(params)
}

/// Parses the two-dimensional bound parameters for binds().
fn parse_bind_rows(value: &BtValue, name: &str) -> BtResult<Vec<Vec<SqlValue>>> {
    let BtValue::Array(rows) = value else {
        return Err(format!("argument `{}` must be an array", name));
    };
    let mut output = Vec::with_capacity(rows.len());
    for row in rows {
        match row {
            BtValue::Array(values) => {
                let mut params = Vec::with_capacity(values.len());
                for value in values {
                    params.push(bt_value_to_sql(value)?);
                }
                output.push(params);
            }
            value => output.push(vec![bt_value_to_sql(value)?]),
        }
    }
    Ok(output)
}

/// Parses the transaction() plan list.
fn parse_transaction_plans(value: Option<&BtValue>, name: &str) -> BtResult<Vec<StatementPlan>> {
    let Some(BtValue::Array(values)) = value else {
        return Err(format!("argument `{}` must be an array", name));
    };
    let mut plans = Vec::with_capacity(values.len());
    for value in values {
        match value {
            BtValue::String(sql) => plans.push(StatementPlan {
                sql: sql.clone(),
                params: Vec::new(),
            }),
            BtValue::Object(fields) => {
                let sql = expect_object_string(fields, "sql")?;
                let params = object_field(fields, "binds")
                    .or_else(|| object_field(fields, "params"))
                    .map(|value| parse_params(Some(value), "params"))
                    .transpose()?
                    .unwrap_or_default();
                plans.push(StatementPlan { sql, params });
            }
            other => {
                return Err(format!(
                    "transaction statement elements must be strings or objects, got {}",
                    other.type_name()
                ));
            }
        }
    }
    Ok(plans)
}

/// Reads the batch size for batch().
fn batch_arg(value: Option<&BtValue>, name: &str) -> BtResult<usize> {
    let Some(BtValue::Int(value)) = value else {
        return Err(format!("argument `{}` must be an int", name));
    };
    Ok((*value).max(0) as usize)
}

/// Reads the worker count for workers().
fn workers_arg(value: Option<&BtValue>, name: &str) -> BtResult<usize> {
    let Some(BtValue::Int(value)) = value else {
        return Err(format!("argument `{}` must be an int", name));
    };
    Ok((*value).clamp(DEFAULT_SQLITE_WORKERS as i64, MAX_SQLITE_WORKERS as i64) as usize)
}

/// Returns the worker count actually used by SQLite.
fn normalized_workers(workers: usize) -> usize {
    workers.clamp(DEFAULT_SQLITE_WORKERS, MAX_SQLITE_WORKERS)
}

/// Validates that SQL has been set on the query object.
fn validate_query_sql(query: &QueryState, method: &str) -> BtResult<()> {
    if query.sql.trim().is_empty() {
        return Err(format!("sqlite.{}() requires query(sql) first", method));
    }
    Ok(())
}

/// Validates that one/all do not use bulk-write configuration.
fn validate_query_read_method(query: &QueryState, method: &str) -> BtResult<()> {
    validate_query_sql(query, method)?;
    if !query.bind_rows.is_empty() {
        return Err(format!("sqlite.{}() does not support binds()", method));
    }
    if query.batch_size > 0 {
        return Err(format!("sqlite.{}() does not support batch()", method));
    }
    if normalized_workers(query.workers) != DEFAULT_SQLITE_WORKERS {
        return Err(format!("sqlite.{}() does not support workers()", method));
    }
    Ok(())
}

/// Combines bind() prefix parameters with binds() row parameters into execution rows.
fn materialize_bind_rows(query: &QueryState) -> Vec<Vec<SqlValue>> {
    let mut rows = Vec::with_capacity(query.bind_rows.len());
    for row in &query.bind_rows {
        let mut values = Vec::with_capacity(query.params.len() + row.len());
        values.extend(query.params.iter().cloned());
        values.extend(row.iter().cloned());
        rows.push(values);
    }
    rows
}

/// Returns the number of rows per batch under the current configuration.
fn batch_unit_size(total: usize, batch_size: usize) -> usize {
    if total == 0 {
        return 0;
    }
    if batch_size > 0 {
        batch_size.min(total).max(1)
    } else {
        total
    }
}

/// Returns the number of batches.
fn batch_count(total: usize, batch_unit_size: usize) -> usize {
    if total == 0 || batch_unit_size == 0 {
        0
    } else {
        (total - 1) / batch_unit_size + 1
    }
}

/// Returns SQL debug text.
fn sql_text(query: &QueryState) -> String {
    if query.bind_rows.is_empty() {
        return format_sql_with_binds(&query.sql, &query.params);
    }
    let rows = materialize_bind_rows(query);
    if rows.is_empty() {
        return query.sql.clone();
    }
    let mut sql = format_sql_with_binds(&query.sql, &rows[0]);
    sql.push_str(&format!(
        " /* binds: {} rows, batch: {}, workers: {} */",
        rows.len(),
        query.batch_size,
        normalized_workers(query.workers)
    ));
    sql
}

/// Replaces `?` placeholders outside SQL string literals with bound values.
fn format_sql_with_binds(sql: &str, binds: &[SqlValue]) -> String {
    let mut output = String::with_capacity(sql.len().saturating_add(binds.len() * 8));
    let mut bind_index = 0usize;
    let mut in_single = false;
    let mut in_double = false;

    for ch in sql.chars() {
        if in_single || in_double {
            output.push(ch);
            if in_single && ch == '\'' {
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
                    output.push_str(&sql_literal(value));
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

/// Formats a SQLite parameter value as an SQL literal for debugging only.
fn sql_literal(value: &SqlValue) -> String {
    match value {
        SqlValue::Null => "NULL".to_string(),
        SqlValue::Integer(value) => value.to_string(),
        SqlValue::Real(value) if value.is_finite() => value.to_string(),
        SqlValue::Real(_) => "NULL".to_string(),
        SqlValue::Text(value) => quote_sql_string(value),
        SqlValue::Blob(value) => blob_literal(value),
    }
}

/// Escapes and wraps a value in single quotes according to SQLite string literal rules.
fn quote_sql_string(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            output.push_str("''");
        } else {
            output.push(ch);
        }
    }
    output.push('\'');
    output
}

/// Renders a BLOB parameter as a SQLite hexadecimal literal.
fn blob_literal(value: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut output = String::with_capacity(value.len().saturating_mul(2).saturating_add(3));
    output.push_str("X'");
    for byte in value {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output.push('\'');
    output
}

/// Converts a BT value to a SQLite parameter value.
fn bt_value_to_sql(value: &BtValue) -> BtResult<SqlValue> {
    match value {
        BtValue::Empty => Err("SQL parameters cannot be empty; pass null explicitly".to_string()),
        BtValue::Null => Ok(SqlValue::Null),
        BtValue::Bool(value) => Ok(SqlValue::Integer(i64::from(*value))),
        BtValue::Int(value) => Ok(SqlValue::Integer(*value)),
        BtValue::Float(value) => Ok(SqlValue::Real(*value)),
        BtValue::String(value) => Ok(SqlValue::Text(value.clone())),
        BtValue::Bytes(value) => Ok(SqlValue::Blob(value.clone())),
        other => Err(format!(
            "SQL parameters do not support the {} type",
            other.type_name()
        )),
    }
}

/// Converts a SQLite row to a BT object.
fn row_to_object(column_names: &[String], row: &Row<'_>) -> BtResult<BtValue> {
    let mut fields = Vec::with_capacity(column_names.len());
    for (index, name) in column_names.iter().enumerate() {
        let value = row.get_ref(index).map_err(sqlite_error)?;
        fields.push((name.clone(), sqlite_value_to_bt(value)?));
    }
    Ok(BtValue::Object(fields))
}

/// Converts a SQLite field value to a BT value.
fn sqlite_value_to_bt(value: ValueRef<'_>) -> BtResult<BtValue> {
    match value {
        ValueRef::Null => Ok(BtValue::Null),
        ValueRef::Integer(value) => Ok(BtValue::Int(value)),
        ValueRef::Real(value) => Ok(BtValue::Float(value)),
        ValueRef::Text(value) => std::str::from_utf8(value)
            .map(|text| BtValue::String(text.to_string()))
            .map_err(|err| format!("SQLite TEXT is not valid UTF-8: {}", err)),
        ValueRef::Blob(value) => Ok(BtValue::Bytes(value.to_vec())),
    }
}

/// Reads a snapshot of the statement's column names.
fn statement_column_names(statement: &rusqlite::Statement<'_>) -> Vec<String> {
    statement
        .column_names()
        .into_iter()
        .map(|name| name.to_string())
        .collect()
}

/// Reads a string field from an object's fields.
fn expect_object_string(fields: &[(String, BtValue)], key: &str) -> BtResult<String> {
    match object_field(fields, key) {
        Some(BtValue::String(value)) => Ok(value.clone()),
        Some(other) => Err(format!(
            "object field `{}` must be a string, got {}",
            key,
            other.type_name()
        )),
        None => Err(format!("object is missing field `{}`", key)),
    }
}

/// Reads a field reference from an object's fields.
fn object_field<'a>(fields: &'a [(String, BtValue)], key: &str) -> Option<&'a BtValue> {
    fields
        .iter()
        .find_map(|(field, value)| (field == key).then_some(value))
}

/// Reads a Boolean configuration value.
fn expect_bool_value(value: &BtValue, name: &str) -> BtResult<bool> {
    match value {
        BtValue::Bool(value) => Ok(*value),
        other => Err(format!(
            "configuration `{}` must be a bool, got {}",
            name,
            other.type_name()
        )),
    }
}

/// Reads a u64 configuration value with hard bounds.
fn bounded_u64(value: &BtValue, name: &str, min: u64, max: u64) -> BtResult<u64> {
    let BtValue::Int(value) = value else {
        return Err(format!("configuration `{}` must be an int", name));
    };
    let value = u64::try_from(*value)
        .map_err(|_| format!("configuration `{}` cannot be negative", name))?;
    if value < min || value > max {
        return Err(format!(
            "configuration `{}` must be in the range {}..={}",
            name, min, max
        ));
    }
    Ok(value)
}

/// Reads a usize configuration value with hard bounds.
fn bounded_usize(value: &BtValue, name: &str, min: usize, max: usize) -> BtResult<usize> {
    let value = bounded_u64(value, name, min as u64, max as u64)?;
    usize::try_from(value).map_err(|_| {
        format!(
            "configuration `{}` exceeds the usize limit on this platform",
            name
        )
    })
}

/// Estimates the size of a returned value in bytes.
fn estimate_value_bytes(value: &BtValue) -> usize {
    match value {
        BtValue::Empty | BtValue::Null | BtValue::Bool(_) => 1,
        BtValue::Int(_) | BtValue::Float(_) => 8,
        BtValue::String(value) => value.len(),
        BtValue::Bytes(value) => value.len(),
        BtValue::Array(values) => values.iter().map(estimate_value_bytes).sum(),
        BtValue::Object(fields) => fields
            .iter()
            .map(|(key, value)| key.len().saturating_add(estimate_value_bytes(value)))
            .sum(),
        BtValue::ExtObject(object) => object.type_name.len().saturating_add(16),
    }
}

/// Constructs the statistics object returned by exec().
fn exec_result(
    total: usize,
    rows_affected: u64,
    last_insert_id: i64,
    batch_count: usize,
    batch_size: usize,
    workers: usize,
) -> BtResult<BtValue> {
    Ok(object_value(vec![
        ("total", usize_value(total)?),
        ("rows_affected", u64_value(rows_affected)?),
        ("last_insert_id", i64_value(last_insert_id)),
        ("batch_count", usize_value(batch_count)?),
        ("batch_size", usize_value(batch_size)?),
        ("workers", usize_value(workers)?),
    ]))
}

/// Converts a usize to a BT int.
fn usize_value(value: usize) -> BtResult<BtValue> {
    let value = i64::try_from(value).map_err(|_| "usize exceeds the BT int limit".to_string())?;
    Ok(BtValue::Int(value))
}

/// Converts an i64 to a BT int.
fn i64_value(value: i64) -> BtValue {
    BtValue::Int(value)
}

/// Converts a u64 to a BT int.
fn u64_value(value: u64) -> BtResult<BtValue> {
    let value = i64::try_from(value).map_err(|_| "u64 exceeds the BT int limit".to_string())?;
    Ok(BtValue::Int(value))
}

/// Constructs a BT object value.
fn object_value(fields: Vec<(&str, BtValue)>) -> BtValue {
    BtValue::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
    )
}

/// Updates statistics for the current worker.
fn bump_stat(body: impl FnOnce(&mut SqliteStats)) {
    STATS.with(|stats| body(&mut stats.borrow_mut()));
}

/// Clears all state in the current worker.
fn reset_state() {
    DATABASES.with(|databases| {
        *databases.borrow_mut() = ObjectStore::new(MAX_CONNECTIONS);
    });
    QUERIES.with(|queries| {
        *queries.borrow_mut() = ObjectStore::new(MAX_QUERIES);
    });
    STATS.with(|stats| {
        *stats.borrow_mut() = SqliteStats::default();
    });
}

/// Converts SQLite errors to a consistent format.
fn sqlite_error(err: rusqlite::Error) -> String {
    format!("SQLite error: {}", err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Creates a test database path.
    fn test_path(name: &str) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "bt_sqlite_{}_{}_{}.db",
            name,
            std::process::id(),
            now
        ));
        path.to_string_lossy().replace('\\', "/")
    }

    /// Removes the test database and its WAL sidecar files.
    fn cleanup_path(path: &str) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(format!("{path}-wal"));
        let _ = fs::remove_file(format!("{path}-shm"));
    }

    /// Creates a BT object.
    fn object(fields: Vec<(&str, BtValue)>) -> BtValue {
        object_value(fields)
    }

    /// Creates a BT array.
    fn array(values: Vec<BtValue>) -> BtValue {
        BtValue::Array(values)
    }

    /// Opens a test database.
    fn open_db(path: &str, options: BtValue) -> ExtObject {
        let value = entry_open(vec![BtValue::String(path.to_string()), options]).unwrap();
        let BtValue::ExtObject(object) = value else {
            panic!("sqlite_open should return a Sqlite object");
        };
        object
    }

    /// Executes Sqlite.exec.
    fn exec(db: &ExtObject, sql: &str, params: Vec<BtValue>) -> BtValue {
        method_db_exec(vec![
            BtValue::ExtObject(db.clone()),
            BtValue::String(sql.to_string()),
            array(params),
        ])
        .unwrap()
    }

    /// Executes Sqlite.one.
    fn one(db: &ExtObject, sql: &str, params: Vec<BtValue>) -> BtValue {
        method_db_one(vec![
            BtValue::ExtObject(db.clone()),
            BtValue::String(sql.to_string()),
            array(params),
        ])
        .unwrap()
    }

    /// Executes Sqlite.all.
    fn all(db: &ExtObject, sql: &str, params: Vec<BtValue>) -> BtValue {
        method_db_all(vec![
            BtValue::ExtObject(db.clone()),
            BtValue::String(sql.to_string()),
            array(params),
        ])
        .unwrap()
    }

    /// Closes a test database.
    fn close_db(db: &ExtObject) {
        method_db_close(vec![BtValue::ExtObject(db.clone())]).unwrap();
    }

    /// one() should preserve empty, NULL, and BLOB boundaries.
    #[test]
    fn one_preserves_empty_null_and_blob() {
        reset_state();
        let path = test_path("types");
        cleanup_path(&path);
        let db = open_db(&path, object(vec![("max_rows", BtValue::Int(10))]));
        exec(
            &db,
            "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT, payload BLOB, note TEXT)",
            vec![],
        );
        exec(
            &db,
            "INSERT INTO items (name, payload, note) VALUES (?, ?, ?)",
            vec![
                BtValue::String("Alice".to_string()),
                BtValue::Bytes(vec![0x42, 0x54]),
                BtValue::Null,
            ],
        );

        let row = one(
            &db,
            "SELECT name, payload, note FROM items WHERE name = ?",
            vec![BtValue::String("Alice".to_string())],
        );
        let BtValue::Object(fields) = row else {
            panic!("one should return an object");
        };
        assert_eq!(
            object_field(&fields, "name"),
            Some(&BtValue::String("Alice".to_string()))
        );
        assert_eq!(
            object_field(&fields, "payload"),
            Some(&BtValue::Bytes(vec![0x42, 0x54]))
        );
        assert_eq!(object_field(&fields, "note"), Some(&BtValue::Null));

        let missing = one(
            &db,
            "SELECT name FROM items WHERE name = ?",
            vec![BtValue::String("Missing".to_string())],
        );
        assert_eq!(missing, BtValue::Empty);
        close_db(&db);
        cleanup_path(&path);
    }

    /// all() must enforce maximum row and result-size limits.
    #[test]
    fn all_enforces_rows_and_bytes_limits() {
        reset_state();
        let path = test_path("limits");
        cleanup_path(&path);
        let db = open_db(
            &path,
            object(vec![
                ("max_rows", BtValue::Int(1)),
                ("max_result_bytes", BtValue::Int(12)),
            ]),
        );
        exec(&db, "CREATE TABLE items (name TEXT)", vec![]);
        exec(
            &db,
            "INSERT INTO items (name) VALUES (?)",
            vec![BtValue::String("Alice".to_string())],
        );
        exec(
            &db,
            "INSERT INTO items (name) VALUES (?)",
            vec![BtValue::String("Bob".to_string())],
        );

        let row_err = method_db_all(vec![
            BtValue::ExtObject(db.clone()),
            BtValue::String("SELECT name FROM items ORDER BY name".to_string()),
            array(vec![]),
        ])
        .unwrap_err();
        assert!(row_err.contains("max_rows"));

        let bytes_err = method_db_one(vec![
            BtValue::ExtObject(db.clone()),
            BtValue::String("SELECT 'abcdefghijklmnopqrstuvwxyz' AS name".to_string()),
            array(vec![]),
        ])
        .unwrap_err();
        assert!(bytes_err.contains("max_result_bytes"));
        close_db(&db);
        cleanup_path(&path);
    }

    /// transaction() should execute multiple write statements serially.
    #[test]
    fn transaction_executes_multiple_statements() {
        reset_state();
        let path = test_path("transaction");
        cleanup_path(&path);
        let db = open_db(&path, object(vec![]));
        exec(&db, "CREATE TABLE items (name TEXT)", vec![]);
        let changed = method_db_transaction(vec![
            BtValue::ExtObject(db.clone()),
            array(vec![
                object(vec![
                    (
                        "sql",
                        BtValue::String("INSERT INTO items (name) VALUES (?)".to_string()),
                    ),
                    ("params", array(vec![BtValue::String("Alice".to_string())])),
                ]),
                object(vec![
                    (
                        "sql",
                        BtValue::String("INSERT INTO items (name) VALUES (?)".to_string()),
                    ),
                    ("params", array(vec![BtValue::String("Bob".to_string())])),
                ]),
            ]),
        ])
        .unwrap();
        assert_eq!(changed, BtValue::Int(2));
        let rows = all(&db, "SELECT name FROM items ORDER BY name", vec![]);
        let BtValue::Array(rows) = rows else {
            panic!("all should return an array");
        };
        assert_eq!(rows.len(), 2);
        close_db(&db);
        cleanup_path(&path);
    }

    /// The query().bind() convenience layer should reuse the coarse-grained execution path.
    #[test]
    fn query_bind_chain_runs_queries() {
        reset_state();
        let path = test_path("query");
        cleanup_path(&path);
        let db = open_db(&path, object(vec![]));
        exec(
            &db,
            "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT)",
            vec![],
        );
        exec(
            &db,
            "INSERT INTO items (name) VALUES (?)",
            vec![BtValue::String("Bob".to_string())],
        );
        let query = method_db_query(vec![
            BtValue::ExtObject(db.clone()),
            BtValue::String("SELECT name FROM items WHERE id = ?".to_string()),
        ])
        .unwrap();
        let BtValue::ExtObject(query) = query else {
            panic!("query should return a SqliteQuery object");
        };
        method_query_bind(vec![BtValue::ExtObject(query.clone()), BtValue::Int(1)]).unwrap();
        let row = method_query_one(vec![BtValue::ExtObject(query.clone())]).unwrap();
        let BtValue::Object(fields) = row else {
            panic!("query.one should return an object");
        };
        assert_eq!(
            object_field(&fields, "name"),
            Some(&BtValue::String("Bob".to_string()))
        );
        method_query_close(vec![BtValue::ExtObject(query.clone())]).unwrap();
        let err = method_query_one(vec![BtValue::ExtObject(query)]).unwrap_err();
        assert!(err.contains("is no longer valid"));
        close_db(&db);
        cleanup_path(&path);
    }

    /// query().binds().batch().workers().exec() should return a MySQL-style statistics object.
    #[test]
    fn query_batch_exec_returns_stats_object() {
        reset_state();
        let path = test_path("batch");
        cleanup_path(&path);
        let db = open_db(&path, object(vec![]));
        exec(
            &db,
            "CREATE TABLE items (id INTEGER PRIMARY KEY, group_name TEXT, name TEXT)",
            vec![],
        );
        let query = method_db_query(vec![
            BtValue::ExtObject(db.clone()),
            BtValue::String("INSERT INTO items (group_name, name) VALUES (?, ?)".to_string()),
        ])
        .unwrap();
        let BtValue::ExtObject(query) = query else {
            panic!("query should return a SqliteQuery object");
        };
        method_query_bind(vec![
            BtValue::ExtObject(query.clone()),
            BtValue::String("writer".to_string()),
        ])
        .unwrap();
        method_query_binds(vec![
            BtValue::ExtObject(query.clone()),
            array(vec![
                array(vec![BtValue::String("Alice".to_string())]),
                array(vec![BtValue::String("Bob".to_string())]),
            ]),
        ])
        .unwrap();
        method_query_batch(vec![BtValue::ExtObject(query.clone()), BtValue::Int(1)]).unwrap();
        method_query_workers(vec![BtValue::ExtObject(query.clone()), BtValue::Int(4)]).unwrap();

        let preview = method_query_sql(vec![BtValue::ExtObject(query.clone())]).unwrap();
        assert_eq!(
            preview,
            BtValue::String(
                "INSERT INTO items (group_name, name) VALUES ('writer', 'Alice') /* binds: 2 rows, batch: 1, workers: 4 */"
                    .to_string()
            )
        );

        let result = method_query_exec(vec![BtValue::ExtObject(query.clone())]).unwrap();
        let BtValue::Object(fields) = result else {
            panic!("exec should return an object");
        };
        assert_eq!(object_field(&fields, "total"), Some(&BtValue::Int(2)));
        assert_eq!(
            object_field(&fields, "rows_affected"),
            Some(&BtValue::Int(2))
        );
        assert_eq!(object_field(&fields, "batch_count"), Some(&BtValue::Int(2)));
        assert_eq!(object_field(&fields, "batch_size"), Some(&BtValue::Int(1)));
        assert_eq!(object_field(&fields, "workers"), Some(&BtValue::Int(4)));

        let rows = all(&db, "SELECT name FROM items ORDER BY id", vec![]);
        let BtValue::Array(rows) = rows else {
            panic!("all should return an array");
        };
        assert_eq!(rows.len(), 2);
        method_query_close(vec![BtValue::ExtObject(query)]).unwrap();
        close_db(&db);
        cleanup_path(&path);
    }

    /// An old connection handle should become invalid after close().
    #[test]
    fn close_invalidates_database_handle() {
        reset_state();
        let path = test_path("close");
        cleanup_path(&path);
        let db = open_db(&path, object(vec![]));
        close_db(&db);
        let err = method_db_exec(vec![
            BtValue::ExtObject(db),
            BtValue::String("SELECT 1".to_string()),
            array(vec![]),
        ])
        .unwrap_err();
        assert!(err.contains("is no longer valid"));
        cleanup_path(&path);
    }

    /// WAL mode should be explicitly configurable through options.
    #[test]
    fn wal_mode_can_be_enabled() {
        reset_state();
        let path = test_path("wal");
        cleanup_path(&path);
        let db = open_db(&path, object(vec![("wal", BtValue::Bool(true))]));
        let row = one(&db, "PRAGMA journal_mode", vec![]);
        let BtValue::Object(fields) = row else {
            panic!("PRAGMA journal_mode should return an object");
        };
        let mode = object_field(&fields, "journal_mode")
            .and_then(BtValue::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        assert_eq!(mode, "wal");
        close_db(&db);
        cleanup_path(&path);
    }

    /// Concurrent readers should open independent connections in worker-like threads.
    #[test]
    fn concurrent_readers_can_read_same_database() {
        reset_state();
        let path = test_path("readers");
        cleanup_path(&path);
        let db = open_db(&path, object(vec![("wal", BtValue::Bool(true))]));
        exec(&db, "CREATE TABLE items (name TEXT)", vec![]);
        exec(
            &db,
            "INSERT INTO items (name) VALUES ('a'), ('b'), ('c')",
            vec![],
        );
        close_db(&db);

        let mut handles = Vec::new();
        for _ in 0..4 {
            let path = path.clone();
            handles.push(thread::spawn(move || {
                reset_state();
                let db = open_db(&path, object(vec![("wal", BtValue::Bool(true))]));
                let rows = all(&db, "SELECT name FROM items ORDER BY name", vec![]);
                close_db(&db);
                let BtValue::Array(rows) = rows else {
                    panic!("all should return an array");
                };
                rows.len()
            }));
        }

        for handle in handles {
            assert_eq!(handle.join().unwrap(), 3);
        }
        cleanup_path(&path);
    }

    /// An expired busy_timeout should return a SQLite lock error, and writes should recover after unlocking.
    #[test]
    fn busy_timeout_rejects_locked_write_then_recovers() {
        reset_state();
        let path = test_path("busy");
        cleanup_path(&path);
        let db = open_db(&path, object(vec![("busy_timeout_ms", BtValue::Int(20))]));
        exec(&db, "CREATE TABLE items (name TEXT)", vec![]);
        close_db(&db);

        let locker = Connection::open(&path).unwrap();
        locker
            .execute_batch("BEGIN EXCLUSIVE; INSERT INTO items (name) VALUES ('locked');")
            .unwrap();
        let db = open_db(&path, object(vec![("busy_timeout_ms", BtValue::Int(20))]));
        let err = method_db_exec(vec![
            BtValue::ExtObject(db.clone()),
            BtValue::String("INSERT INTO items (name) VALUES (?)".to_string()),
            array(vec![BtValue::String("blocked".to_string())]),
        ])
        .unwrap_err();
        assert!(err.contains("locked"));
        locker.execute_batch("ROLLBACK;").unwrap();
        exec(
            &db,
            "INSERT INTO items (name) VALUES (?)",
            vec![BtValue::String("ok".to_string())],
        );
        close_db(&db);
        cleanup_path(&path);
    }

    /// Connection objects should not keep growing after repeated open-close cycles.
    #[test]
    fn repeated_open_close_does_not_grow_objects() {
        reset_state();
        let path = test_path("steady");
        cleanup_path(&path);
        lifecycle_init(BtValue::Object(vec![])).unwrap();
        for _ in 0..50 {
            let db = open_db(&path, object(vec![]));
            close_db(&db);
        }
        let stats = lifecycle_stats().unwrap();
        let BtValue::Object(fields) = stats else {
            panic!("stats should return an object");
        };
        assert_eq!(
            object_field(&fields, "active_connections"),
            Some(&BtValue::Int(0))
        );
        assert_eq!(
            object_field(&fields, "query_objects"),
            Some(&BtValue::Int(0))
        );
        assert_eq!(object_field(&fields, "init_calls"), Some(&BtValue::Int(1)));
        cleanup_path(&path);
    }
}
