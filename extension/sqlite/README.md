# sqlite

BT official SQLite shared WASM extension, used in BT projects to access SQLite database files in the project directory. It is suitable for scenarios such as small websites, desktop applications, local tools, caches, logs and configuration data that do not want to deploy separate database services.

SQLite is a file database, and a `.db` file is a database. After BT opens the database through `sqlite_open()`, it will return a `Sqlite` connection object; all subsequent queries, writes and transactions start from this object.

## Installation

```text
bt install sqlite
```

Specify version:

```text
bt install sqlite 1.0.0
```

You can also select **Download extension** on the extension details page to get the `.bts` package manually. Place the downloaded file in the current project's `extensions/sqlite/` directory and keep the filename `sqlite-1.0.0.bts`.

The installed extension package is stored at the following path in the current project:

```text
extensions/sqlite/sqlite-1.0.0.bts
```

## File access

This extension reads and writes database files in the project directory. BT extensions do not declare per-package permissions; file access follows the process-wide BT policy.

## Minimal example

```bt
fs('@/data').create_dir()

db = sqlite_open('@/data/app.db', {})

db.query('CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT)').exec()
ret = db.query('INSERT INTO users (name) VALUES (?)').bind('Alice').exec()
row = db.query('SELECT name FROM users WHERE id = ?').bind(1).one()

// Output: 1
print ret.rows_affected

// Output: Alice
print row.name

db.close()
```

## Common configuration examples

```bt
db = sqlite_open('@/data/app.db', {
    wal: true,
    busy_timeout_ms: 1000,
    max_rows: 100,
    max_result_bytes: 1048576
})
```

You do not need to specify every option. Start with `{}` to use the defaults; configure individual options when result sets may be large, reads and writes may be concurrent, or memory usage needs a limit.

| Field | Type | Required | Default value | Valid range | Description |
| ------ | ------ | ------ | ------ | ------ | ------ |
| `wal` | Bool | No | `false` | `true` or `false` | Enables SQLite WAL journal mode. `true` executes `PRAGMA journal_mode = WAL`; it generally improves concurrent read/write behavior, but creates `-wal` and `-shm` sidecar files next to the database. It is unnecessary for a temporary single-script database, but recommended for web services, desktop applications, and long-running projects. |
| `busy_timeout_ms` | Int | No | `1000` | `0..=300000` | When the database is locked by other connections, the maximum number of milliseconds to wait before reporting an error. `1000` means waiting at most 1 second; `0` means no waiting. It only handles SQLite file lock waits, not SQL query execution timeouts. |
| `max_rows` | Int | No | `1000` | `1..=100000` | The maximum number of rows that `all()` may return at once. Exceeding it raises an error, preventing a large result set from being loaded into the BT VM at once. For one page of data, add a SQL `LIMIT`, for example `LIMIT 20`. |
| `max_result_bytes` | Int | No | `4194304` | `1..=16777216` | `one()` and `all()` are the estimated upper bound in bytes for the returned result. Default is about 4MB, maximum is 16MB. It is used to protect resident process memory and avoid returning too many large texts or large BLOBs at one time. |

## API list

| API | Description |
| ------ | ------ |
| `sqlite_open(path, options)` | Open the SQLite database file and return the `Sqlite` connection object. |
| `db.query(sql)` | Create a chain query object, only save the SQL, not execute it immediately. |
| `query.bind(value)` | Appends a `?` placeholder parameter. |
| `query.binds(rows)` | Append multi-row batch parameters, can only be used for `exec()`. |
| `query.batch(size)` | Set the batch size statistics for batch execution. |
| `query.workers(count)` | MySQL migration compatibility parameters; SQLite currently still writes serially. |
| `query.one()` | Executes the query and returns the first row object; returns `empty` when no result is found. |
| `query.all()` | Execute the query and return an array of row objects. |
| `query.exec()` | Execute write or DDL SQL and return the execution statistics object. |
| `query.sql()` | Returns SQL debugging text with parameter preview. |
| `query.close()` | Release the chained query object. |
| `db.transaction(statements)` | Execute multiple SQLs serially in one transaction. |
| `db.close()` | Close the database connection. |

## sqlite_open

### Function

Opens a SQLite database file and returns a `Sqlite` connection object. When the file does not exist, SQLite will automatically create the file; but the parent directory must already exist, so the example usually writes `fs('@/data').create_dir()` first.

### Syntax

```bt
db = sqlite_open(path, options)
```

### Parameters

| Parameters | Type | Required | Default value | Description |
| ------ | ------ | ------ | ------ | ------ |
| `path` | String | Yes | None | SQLite database file path. It is recommended to use a project root path such as `@/data/app.db`. The extension is declared as `path_write` and the path must fall within the write range allowed by the project. |
| `options` | Object | Yes | `{}` | Connection configuration object. Pass `{}` when there are no special requirements. See "Common Configuration Examples" for fields. |

### Return value

| Type | Description |
| ------ | ------ |
| `Sqlite` | SQLite connection object. Subsequent use via `db.query()`, `db.transaction()`, `db.close()`. |

### Example

```bt
fs('@/data').create_dir()
db = sqlite_open('@/data/app.db', {})

// Output: Sqlite
print type(db)

db.close()
```
## db.query

### Function

Creates a chained query object. `query()` only stores SQL; it does not access tables, read data, or write data. Execution occurs later in `one()`, `all()`, or `exec()`.

### Syntax

```bt
query = db.query(sql)
```

### Parameters

| Parameters | Type | Required | Default value | Description |
| ------ | ------ | ------ | ------ | ------ |
| `sql` | String | Yes | None | SQL to be executed. Use the `?` placeholder when parameters are required, for example `WHERE id = ?`. |

### Return value

| Type | Description |
| ------ | ------ |
| `SqliteQuery` | Chain query object, you can continue to call methods such as `bind()`, `binds()`, `one()`, `all()`, `exec()`, etc. |

### Example

```bt
db = sqlite_open('@/data/app.db', {})
query = db.query('SELECT 1 AS value')

// Output: SqliteQuery
print type(query)

query.close()
db.close()
```

## query.bind

### Function

Append a normal binding parameter. Each time `bind(value)` is called, it is bound to the next `?` placeholder in SQL in sequence.

Binding parameters can avoid manual SQL splicing, reduce quote escape errors, and avoid SQL injection risks.

### Syntax

```bt
query = db.query(sql).bind(value)
```

### Parameters

| Parameters | Type | Required | Default value | Description |
| ------ | ------ | ------ | ------ | ------ |
| `value` | Null/Bool/Int/Float/String/Bytes | Yes | None | The value to bind to `?`. `null` will be written to SQLite `NULL`; Bool will be written as `0` or `1`; Bytes will be written to BLOB. |

### Return value

| Type | Description |
| ------ | ------ |
| `SqliteQuery` | Returns the same query object to facilitate continued chain calls. |

### Example

```bt
db = sqlite_open('@/data/app.db', {})
db.query('CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT)').exec()
db.query('INSERT INTO users (name) VALUES (?)').bind('Alice').exec()

row = db.query('SELECT name FROM users WHERE name = ?').bind('Alice').one()

// Output: Alice
print row.name

db.close()
```

## query.binds

### Function

Append multiple rows of batch binding parameters, only used for `exec()`. A common use is to insert multiple rows of data at once.

The parameters of `binds(rows)` are usually two-dimensional arrays: the outer array represents multiple rows, and the inner array represents multiple values for this row to be bound to SQL placeholders.

### Syntax

```bt
query = db.query(sql).binds(rows)
query = db.query(sql).bind(prefix).binds(rows)
```

### Parameters

| Parameters | Type | Required | Default value | Description |
| ------ | ------ | ------ | ------ | ------ |
| `rows` | Array | Yes | None | An array of rows to batch bind. It is recommended to pass a two-dimensional array, such as `[['Alice', 18], ['Bob', 20]]`. If a row element is not an array, it will be treated as a single-valued row. |

### Return value

| Type | Description |
| ------ | ------ |
| `SqliteQuery` | Returns the same query object. |

### Example

```bt
db = sqlite_open('@/data/app.db', {})
db.query('CREATE TABLE IF NOT EXISTS users (name TEXT, age INTEGER)').exec()

ret = db.query('INSERT INTO users (name, age) VALUES (?, ?)')
    .binds([
        ['Alice', 18],
        ['Bob', 20]
    ])
    .exec()

// Output: 2
print ret.rows_affected

db.close()
```

### Notes

- `binds()` can only be used with `exec()`.
- `binds()` cannot be used with `one()` or `all()`, otherwise an error will be reported.
- If every row has fixed leading parameters, call `bind(prefix)` before `binds(rows)`; execution prepends the fixed parameters to each row.

## query.batch

### Function

Set batch size statistics for batch `exec()`. It is mainly used to be consistent with the batch writing method of the MySQL standard library.

SQLite currently executes these bound rows serially in the same transaction; `batch(size)` will affect `batch_count` and `batch_size` in the returned object, making it easier to migrate code and observe batch size.

### Syntax

```bt
query = db.query(sql).binds(rows).batch(size)
```

### Parameters

| Parameters | Type | Required | Default value | Valid range | Description |
| ------ | ------ | ------ | ------ | ------ | ------ |
| `size` | Int | Yes | `0` when not called | `0` or a positive integer | The number of rows per batch. If it is less than `0`, it will be processed as `0`; `0` means using all bound rows as a batch. |

### Return value

| Type | Description |
| ------ | ------ |
| `SqliteQuery` | Returns the same query object. |

### Example

```bt
db = sqlite_open('@/data/app.db', {})
db.query('CREATE TABLE IF NOT EXISTS logs (text TEXT)').exec()

ret = db.query('INSERT INTO logs (text) VALUES (?)')
    .binds([['a'], ['b'], ['c']])
    .batch(2)
    .exec()

// Output: 2
print ret.batch_count

db.close()
```

## query.workers

### Function

Sets the worker-count statistic for batch `exec()`. This method lets code migrated from the MySQL standard library retain a similar call style.

SQLite cannot write concurrently to the same connection like the MySQL connection pool; the current implementation still executes serially within a transaction. In other words, `workers(4)` will not allow the same SQLite connection to write 4 SQL statements concurrently.

### Syntax

```bt
query = db.query(sql).binds(rows).workers(count)
```

### Parameters
| Parameters | Type | Required | Default value | Valid range | Description |
| ------ | ------ | ------ | ------ | ------ | ------ |
| `count` | Int | Yes | `1` when not called | `1..=4096` | Migration-compatibility worker count. Values below `1` are treated as `1`; values above `4096` are treated as `4096`. |

### Return value

| Type | Description |
| ------ | ------ |
| `SqliteQuery` | Returns the same query object. |

### Example

```bt
db = sqlite_open('@/data/app.db', {})
db.query('CREATE TABLE IF NOT EXISTS logs (text TEXT)').exec()

ret = db.query('INSERT INTO logs (text) VALUES (?)')
    .binds([['a'], ['b']])
    .workers(4)
    .exec()

// Output: 4
print ret.workers

db.close()
```

## query.one

### Function

Execute the query and return the first row. It is suitable for scenarios where only one row is required, such as querying by primary key, querying a configuration, querying count, etc.

### Syntax

```bt
row = db.query(sql).bind(value).one()
```

### Parameters

No parameters.

### Return value

| Type | Description |
| ------ | ------ |
| Object/Empty | Returns an object when a row is found; field names come from the SQL result columns. Returns `empty` when no result is found. SQLite `NULL` fields return BT `null`, and BLOB fields return BT Bytes. |

### Example

```bt
db = sqlite_open('@/data/app.db', {})
db.query('CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT)').exec()
db.query('INSERT INTO users (name) VALUES (?)').bind('Alice').exec()

row = db.query('SELECT id, name FROM users WHERE name = ?').bind('Alice').one()
missing = db.query('SELECT id, name FROM users WHERE name = ?').bind('Missing').one()

// Output: Alice
print row.name

// Output: true
print is_empty(missing)

db.close()
```

## query.all

### Function

Execute a query and return a multi-row array of objects. Suitable for list pages, paging queries and exporting small amounts of data.

### Syntax

```bt
rows = db.query(sql).bind(value).all()
```

### Parameters

No parameters.

### Return value

| Type | Description |
| ------ | ------ |
| Array | Returns an array of row objects. Returns an empty array `[]` when there are no results. Each call is limited by the `max_rows` and `max_result_bytes` options of `sqlite_open()`. |

### Example

```bt
db = sqlite_open('@/data/app.db', {
    max_rows: 100,
    max_result_bytes: 1048576
})
db.query('CREATE TABLE IF NOT EXISTS users (name TEXT)').exec()
db.query('INSERT INTO users (name) VALUES (?)').bind('Alice').exec()
db.query('INSERT INTO users (name) VALUES (?)').bind('Bob').exec()

rows = db.query('SELECT name FROM users ORDER BY name LIMIT 20').all()

// Output: 2
print rows.len()

db.close()
```

## query.exec

### Function

Executes SQL that does not return a result set, such as `CREATE TABLE`, `INSERT`, `UPDATE`, or `DELETE`. A normal `bind()` call executes once; `binds()` executes each bound row serially in a SQLite transaction.

### Syntax

```bt
ret = db.query(sql).exec()
ret = db.query(sql).bind(value).exec()
ret = db.query(sql).binds(rows).batch(size).workers(count).exec()
```

### Parameters

No parameters.

### Return value

| Type | Description |
| ------ | ------ |
| Object | Returns the SQL execution statistics object. |

### Execution result field

| Field | Type | Must exist | Description |
| ------ | ------ | ------ | ------ |
| `total` | Int | Yes | Number of bound rows processed by this execution. A normal execution is `1`; for `binds()`, it is the number of rows in the bound array; an empty batch is `0`. |
| `rows_affected` | Int | Yes | The number of affected rows reported by SQLite. When executed in batches, the values are accumulated row by row. |
| `last_insert_id` | Int | Yes | `last_insert_rowid()` of the current SQLite connection, usually used to read the latest auto-incremented primary key. |
| `batch_count` | Int | Yes | Number of batches calculated by `batch()`. Normal execution is usually `1`; empty batch is `0`. |
| `batch_size` | Int | Yes | The batch size configured for the current query object. `0` when `batch()` is not called. |
| `workers` | Int | Yes | The number of jobs after the current query object is configured and normalized. SQLite currently does not write concurrently to the same connection. |

### Example

```bt
db = sqlite_open('@/data/app.db', {})
db.query('CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT)').exec()

ret = db.query('INSERT INTO users (name) VALUES (?)').bind('Alice').exec()

// Output: 1
print ret.rows_affected

// Example output: 1
print ret.last_insert_id

db.close()
```

## query.sql

### Function

Returns the debug preview text of the current SQL. It will render the bound parameters to the `?` placeholder position, allowing you to print and check the SQL.

This text is only used for debugging and does not participate in actual execution. Real execution still uses SQLite parameter binding.

### Syntax

```bt
text = db.query(sql).bind(value).sql()
```

### Parameters

No parameters.

### Return value

| Type | Description |
| ------ | ------ |
| String | SQL debugging preview text. |

### Example

```bt
db = sqlite_open('@/data/app.db', {})
text = db.query('SELECT name FROM users WHERE id = ?').bind(1001).sql()

// Output: SELECT name FROM users WHERE id = 1001
print text

db.close()
```

## query.close

### Function

Release the chained query object. The query object saves SQL, binding parameters and batch configuration; in a long-running service, if you create many temporary query objects, you can explicitly release them.

### Syntax

```bt
query.close()
```

### Parameters

No parameters.

### Return value

| Type | Description |
| ------ | ------ |
| Bool | Successful release returns `true`. After being released, the old query object becomes invalid and cannot be called further. |

## db.transaction

### Function

Executes multiple write statements serially within one SQLite transaction. The transaction is committed only when all statements succeed; if any statement fails, SQLite rolls it back.

Suitable for scenarios where "multi-step writes must succeed or fail together", such as creating an order and deducting inventory.

### Syntax

```bt
changed = db.transaction(statements)
```

### Parameters

| Parameters | Type | Required | Default value | Description |
| ------ | ------ | ------ | ------ | ------ |
| `statements` | Array | Yes | None | Array of transaction statements. The element can be a SQL string or a `{ sql, binds }` object. |

### statements object fields

| Field | Type | Required | Default value | Description |
| ------ | ------ | ------ | ------ | ------ |
| `sql` | String | Yes | None | SQL to be executed. |
| `binds` | Array | No | `[]` | SQL parameter array. Supports `null`, Bool, Int, Float, String, Bytes; `empty` is not allowed. |
| `params` | Array | No | `[]` | The old field alias of `binds`, it is recommended that new code use `binds`. If `binds` and `params` are written at the same time, `binds` will take precedence. |

### Return value

| Type | Description |
| ------ | ------ |
| Int | The cumulative number of affected rows within the transaction. |

### Example

```bt
db = sqlite_open('@/data/app.db', {})
db.query('CREATE TABLE IF NOT EXISTS users (name TEXT, note TEXT)').exec()
db.query('INSERT INTO users (name, note) VALUES (?, ?)').bind('Bob').bind('new').exec()
db.query('INSERT INTO users (name, note) VALUES (?, ?)').bind('Carol').bind('new').exec()

changed = db.transaction([
    { sql: 'UPDATE users SET note = ? WHERE name = ?', binds: ['reader', 'Bob'] },
    { sql: 'UPDATE users SET note = ? WHERE name = ?', binds: ['reader', 'Carol'] }
])

// Output: 2
print changed

db.close()
```

## db.close

### Function

Close the SQLite database connection. BT is a memory-resident language, and services or desktop applications may run for a long time; unused connections should be actively closed to prevent connection objects from remaining in the shared worker.

### Syntax

```bt
db.close()
```

### Parameters

No parameters.

### Return value

| Type | Description |
| ------ | ------ |
| Bool | Returns `true` when the connection is closed successfully. After closing, the old connection object becomes invalid and cannot be queried further. |

## Complete example

```bt
fs('@/data').create_dir()

db = sqlite_open('@/data/sqlite-demo.db', {
    wal: true,
    busy_timeout_ms: 1000,
    max_rows: 100,
    max_result_bytes: 1048576
})

db.query('CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT, payload BLOB, note TEXT)').exec()
db.query('DELETE FROM users').exec()

ret = db.query('INSERT INTO users (name, payload, note) VALUES (?, ?, ?)')
    .bind('Alice')
    .bind(bytes('4254', 'hex'))
    .bind(null)
    .exec()

// Output: 1
print ret.rows_affected

db.query('INSERT INTO users (name, payload, note) VALUES (?, ?, ?)')
    .binds([
        ['Bob', bytes('0102', 'hex'), 'writer'],
        ['Carol', bytes('0304', 'hex'), 'writer']
    ])
    .batch(2)
    .workers(1)
    .exec()

row = db.query('SELECT name, payload, note FROM users WHERE name = ?').bind('Alice').one()

// Output: Alice
print row.name

// Output: true
print is_null(row.note)

missing = db.query('SELECT name FROM users WHERE name = ?').bind('Missing').one()

// Output: true
print is_empty(missing)

rows = db.query('SELECT id, name FROM users ORDER BY id LIMIT 20').all()

// Output: 3
print rows.len()

changed = db.transaction([
    { sql: 'UPDATE users SET note = ? WHERE name = ?', binds: ['reader', 'Bob'] },
    { sql: 'UPDATE users SET note = ? WHERE name = ?', binds: ['reader', 'Carol'] }
])

// Output: 2
print changed

db.close()
```

## Data type correspondence

| BT value | SQLite parameter or return value |
| ------ | ------ |
| `null` | SQLite `NULL` |
| Bool | Integer `0` or `1` |
| Int | INTEGER |
| Float | REAL |
| String | TEXT |
| Bytes | BLOB |
| `empty` | Not allowed as SQL parameter; please pass it explicitly `null` |

## Notes

- Use the `query(sql).bind(...).one/all/exec()` style rather than manually concatenating user input into SQL strings.
- `bind()` Only binds one value at a time. If multiple parameters are required, call it multiple times in succession.
- `binds()`, `batch()`, `workers()` are batch write usages. Only `exec()` is supported, `one()` and `all()` are not supported.
- `workers()` is a MySQL migration compatible interface. SQLite currently does not write to the same connection concurrently.
- `all()` is suitable for reading limited result sets; for large lists, it is recommended to add `LIMIT/OFFSET` to SQL and set reasonable `max_rows` and `max_result_bytes`.
- `one()` returns `empty` if there is no result. When the database field value is SQL `NULL`, it returns BT `null`. The two have different meanings.
- The old handle will become invalid after successful calls to `close()` and `query.close()`.
