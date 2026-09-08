//! SQLite regression tests through the public chained query interface.

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
        panic!("sqlite should return a Sqlite object");
    };
    object
}

/// Creates, binds, executes, and closes a query through the public methods.
fn run_query(
    db: &ExtObject,
    sql: &str,
    params: Vec<BtValue>,
    method: fn(Vec<BtValue>) -> BtResult<BtValue>,
) -> BtResult<BtValue> {
    let value = method_db_query(vec![
        BtValue::ExtObject(db.clone()),
        BtValue::String(sql.to_string()),
    ])?;
    let BtValue::ExtObject(query) = value else {
        panic!("query should return a SqliteQuery object");
    };
    let result = (|| {
        for value in params {
            method_query_bind(vec![BtValue::ExtObject(query.clone()), value])?;
        }
        method(vec![BtValue::ExtObject(query.clone())])
    })();
    method_query_close(vec![BtValue::ExtObject(query)]).unwrap();
    result
}

/// Executes Sqlite.query().exec().
fn exec(db: &ExtObject, sql: &str, params: Vec<BtValue>) -> BtValue {
    run_query(db, sql, params, method_query_exec).unwrap()
}

/// Executes Sqlite.query().one().
fn one(db: &ExtObject, sql: &str, params: Vec<BtValue>) -> BtValue {
    run_query(db, sql, params, method_query_one).unwrap()
}

/// Executes Sqlite.query().all().
fn all(db: &ExtObject, sql: &str, params: Vec<BtValue>) -> BtValue {
    run_query(db, sql, params, method_query_all).unwrap()
}

/// Closes a test database.
fn close_db(db: &ExtObject) {
    method_db_close(vec![BtValue::ExtObject(db.clone())]).unwrap();
}

/// Rejected connection arguments must not retain native database handles.
#[test]
fn connection_entry_rejects_invalid_arguments_without_allocating() {
    reset_state();
    assert!(entry_open(vec![BtValue::String(":memory:".into())]).is_err());
    assert!(entry_open(vec![BtValue::String(String::new()), object(vec![])]).is_err());
    assert!(entry_open(vec![BtValue::String(":memory:".into()), BtValue::Null]).is_err());
    assert_eq!(DATABASES.with(|databases| databases.borrow().len()), 0);
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

    let row_err = run_query(
        &db,
        "SELECT name FROM items ORDER BY name",
        vec![],
        method_query_all,
    )
    .unwrap_err();
    assert!(row_err.contains("max_rows"));

    let bytes_err = run_query(
        &db,
        "SELECT 'abcdefghijklmnopqrstuvwxyz' AS name",
        vec![],
        method_query_one,
    )
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

/// Batch execution must retain its BLOB prefix and rows for repeated use.
#[test]
fn batch_query_can_reuse_bound_blob_and_rows() {
    reset_state();
    let db = open_db(":memory:", object(vec![]));
    exec(&db, "CREATE TABLE items (payload BLOB, name TEXT)", vec![]);
    let query = method_db_query(vec![
        BtValue::ExtObject(db.clone()),
        BtValue::String("INSERT INTO items (payload, name) VALUES (?, ?)".to_string()),
    ])
    .unwrap();
    let payload = BtValue::Bytes(vec![0x42; 64 * 1024]);
    method_query_bind(vec![query.clone(), payload.clone()]).unwrap();
    method_query_binds(vec![
        query.clone(),
        array(vec![
            array(vec![BtValue::String("Alice".to_string())]),
            array(vec![BtValue::String("Bob".to_string())]),
            array(vec![BtValue::Null]),
        ]),
    ])
    .unwrap();

    for (size, batches) in [(2, 2), (0, 1)] {
        method_query_batch(vec![query.clone(), BtValue::Int(size)]).unwrap();
        let result = method_query_exec(vec![query.clone()]).unwrap();
        let BtValue::Object(fields) = result else {
            panic!("exec should return an object");
        };
        assert_eq!(object_field(&fields, "total"), Some(&BtValue::Int(3)));
        assert_eq!(
            object_field(&fields, "rows_affected"),
            Some(&BtValue::Int(3))
        );
        assert_eq!(
            object_field(&fields, "batch_count"),
            Some(&BtValue::Int(batches))
        );
    }

    let BtValue::Array(rows) = all(
        &db,
        "SELECT payload, name FROM items ORDER BY rowid",
        vec![],
    ) else {
        panic!("all should return an array");
    };
    assert_eq!(rows.len(), 6);
    for (index, row) in rows.iter().enumerate() {
        let BtValue::Object(fields) = row else {
            panic!("row should be an object");
        };
        assert_eq!(object_field(fields, "payload"), Some(&payload));
        let name = match index % 3 {
            0 => BtValue::String("Alice".to_string()),
            1 => BtValue::String("Bob".to_string()),
            _ => BtValue::Null,
        };
        assert_eq!(object_field(fields, "name"), Some(&name));
    }
    method_query_close(vec![query]).unwrap();
    close_db(&db);
}

/// A failed batch must roll back all rows and release its borrows for subsequent calls.
#[test]
fn failed_batch_rolls_back_and_preserves_query() {
    reset_state();
    let db = open_db(":memory:", object(vec![]));
    exec(&db, "CREATE TABLE items (id INTEGER PRIMARY KEY)", vec![]);
    let query = method_db_query(vec![
        BtValue::ExtObject(db.clone()),
        BtValue::String("INSERT INTO items (id) VALUES (?)".to_string()),
    ])
    .unwrap();
    method_query_binds(vec![
        query.clone(),
        array(vec![BtValue::Int(1), BtValue::Int(1)]),
    ])
    .unwrap();

    for _ in 0..2 {
        let err = method_query_exec(vec![query.clone()]).unwrap_err();
        assert!(err.contains("UNIQUE constraint failed"));
        assert_eq!(all(&db, "SELECT id FROM items", vec![]), array(vec![]));
        assert_eq!(
            method_query_sql(vec![query.clone()]).unwrap(),
            BtValue::String(
                "INSERT INTO items (id) VALUES (1) /* binds: 2 rows, batch: 0, workers: 1 */"
                    .to_string()
            )
        );
    }
    method_query_close(vec![query]).unwrap();
    exec(
        &db,
        "INSERT INTO items (id) VALUES (?)",
        vec![BtValue::Int(2)],
    );
    assert_eq!(
        one(&db, "SELECT id FROM items", vec![]),
        object(vec![("id", BtValue::Int(2))])
    );
    close_db(&db);
}

/// An old connection handle should become invalid after close().
#[test]
fn close_invalidates_database_handle() {
    reset_state();
    let path = test_path("close");
    cleanup_path(&path);
    let db = open_db(&path, object(vec![]));
    close_db(&db);
    let err = method_db_query(vec![
        BtValue::ExtObject(db),
        BtValue::String("SELECT 1".to_string()),
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
    let err = run_query(
        &db,
        "INSERT INTO items (name) VALUES (?)",
        vec![BtValue::String("blocked".to_string())],
        method_query_exec,
    )
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
