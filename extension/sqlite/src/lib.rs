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
    17 => entry_open,
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
fn lifecycle_init(_config: BtValue) -> BtResult<BtValue> {
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

/// Opens a database for both `sqlite` and its deprecated `sqlite_open` alias.
fn entry_open(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 2, "sqlite")?;
    let path = expect_string(&args, 0, "path")?;
    if path.is_empty() {
        return Err("sqlite path must not be empty".to_string());
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

/// Executes multiple SQL statements serially in a SQLite transaction.
fn method_db_transaction(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 2, "Sqlite.transaction")?;
    let object = db_receiver(&args, "self")?;
    let plans = parse_transaction_plans(args.get(1), "statements")?;
    with_db_mut(object.object_id, |db| run_transaction(db, &plans))
}

/// Creates a chained query object.
fn method_db_query(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 2, "Sqlite.query")?;
    let object = db_receiver(&args, "self")?;
    let sql = expect_string(&args, 1, "sql")?;
    with_db_mut(object.object_id, |_| Ok(()))?;
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
    with_query(&args, |query| {
        validate_query_read_method(query, "one")?;
        with_db_mut(query.db_id, |db| run_one(db, &query.sql, &query.params))
    })
}

/// Executes a chained query and returns an array of objects.
fn method_query_all(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 1, "SqliteQuery.all")?;
    with_query(&args, |query| {
        validate_query_read_method(query, "all")?;
        with_db_mut(query.db_id, |db| run_all(db, &query.sql, &query.params))
    })
}

/// Executes a chained SQL write and returns execution statistics.
fn method_query_exec(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 1, "SqliteQuery.exec")?;
    with_query(&args, |query| {
        with_db_mut(query.db_id, |db| run_query_exec(db, query))
    })
}

/// Returns the SQL debug text for a chained query.
fn method_query_sql(args: Vec<BtValue>) -> BtResult<BtValue> {
    expect_arg_count(&args, 1, "SqliteQuery.sql")?;
    with_query(&args, |query| Ok(BtValue::String(sql_text(query))))
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
    let total = query.bind_rows.len();
    let batch_count = if query.batch_size == 0 {
        1
    } else {
        total.div_ceil(query.batch_size)
    };
    let tx = db.conn.transaction().map_err(sqlite_error)?;
    let mut rows_affected = 0u64;
    {
        let mut statement = tx.prepare(&query.sql).map_err(sqlite_error)?;
        // Borrow the common prefix and each row directly; large BLOBs must not be
        // copied into a second batch before executing the transaction.
        for row in &query.bind_rows {
            let changed = statement
                .execute(params_from_iter(query.params.iter().chain(row.iter())))
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
        query.workers,
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

/// Borrows query state for synchronous work without cloning SQL or bound values.
fn with_query<T>(args: &[BtValue], body: impl FnOnce(&QueryState) -> BtResult<T>) -> BtResult<T> {
    let object = query_receiver(args, "self")?;
    QUERIES.with(|queries| {
        let queries = queries.borrow();
        let query = queries.get_required(object.object_id, QUERY_TYPE_NAME)?;
        // Execution only borrows DATABASES and STATS, with no callbacks into BT.
        body(query)
    })
}

/// Mutably accesses a SQLite connection in the connection table.
fn with_db_mut<T>(object_id: u64, body: impl FnOnce(&mut DbState) -> BtResult<T>) -> BtResult<T> {
    DATABASES.with(|databases| {
        let mut databases = databases.borrow_mut();
        let db = databases.get_mut_required(object_id, DB_TYPE_NAME)?;
        body(db)
    })
}

/// Parses connection options shared by both public entry points.
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
    if query.workers != DEFAULT_SQLITE_WORKERS {
        return Err(format!("sqlite.{}() does not support workers()", method));
    }
    Ok(())
}

/// Previews only the first execution row, borrowing its bound values.
fn sql_text(query: &QueryState) -> String {
    let Some(first_row) = query.bind_rows.first() else {
        return format_sql_with_binds(&query.sql, query.params.iter());
    };
    let mut sql = format_sql_with_binds(&query.sql, query.params.iter().chain(first_row.iter()));
    sql.push_str(&format!(
        " /* binds: {} rows, batch: {}, workers: {} */",
        query.bind_rows.len(),
        query.batch_size,
        query.workers
    ));
    sql
}

/// Replaces `?` placeholders outside SQL string literals with bound values.
fn format_sql_with_binds<'a>(sql: &str, mut binds: impl Iterator<Item = &'a SqlValue>) -> String {
    let mut output = String::with_capacity(
        sql.len()
            .saturating_add(binds.size_hint().0.saturating_mul(8)),
    );
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
                if let Some(value) = binds.next() {
                    output.push_str(&sql_literal(value));
                } else {
                    output.push('?');
                }
            }
            _ => output.push(ch),
        }
    }

    let extra_binds = binds.count();
    if extra_binds > 0 {
        output.push_str(&format!(" /* extra binds: {} */", extra_binds));
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
        ("last_insert_id", BtValue::Int(last_insert_id)),
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
mod tests;
