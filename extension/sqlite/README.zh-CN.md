# sqlite

BT 官方 SQLite shared WASM 扩展，用于在 BT 项目中访问项目目录内的 SQLite 数据库文件。它适合小型网站、桌面应用、本地工具、缓存、日志和配置数据这类不想单独部署数据库服务的场景。

SQLite 是文件数据库，一个 `.db` 文件就是一个数据库。BT 通过 `sqlite_open()` 打开数据库后，会返回一个 `Sqlite` 连接对象；后续所有查询、写入和事务都从这个对象开始。

## 安装

```text
bt install sqlite
```

指定版本：

```text
bt install sqlite 1.0.0
```

也可以点击扩展详情页的“下载扩展”按钮手动获取 `.bts` 包；下载后请将文件放入当前项目的 `extensions/sqlite/` 目录，并保持文件名为 `sqlite-1.0.0.bts`。

安装后的扩展包位于当前项目的以下路径：

```text
extensions/sqlite/sqlite-1.0.0.bts
```

## 文件访问

该扩展读写项目目录内的数据库文件。BT 扩展不声明包级权限；文件访问统一服从 BT 进程级策略。

## 最小示例

```bt
fs('@/data').create_dir()

db = sqlite_open('@/data/app.db', {})

db.query('CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT)').exec()
ret = db.query('INSERT INTO users (name) VALUES (?)').bind('Alice').exec()
row = db.query('SELECT name FROM users WHERE id = ?').bind(1).one()

// 输出：1
print ret.rows_affected

// 输出：Alice
print row.name

db.close()
```

## 常用配置示例

```bt
db = sqlite_open('@/data/app.db', {
    wal: true,
    busy_timeout_ms: 1000,
    max_rows: 100,
    max_result_bytes: 1048576
})
```

这些配置不是必须都写。新手可以先传 `{}` 使用默认值；当你知道结果集可能很大、会有并发读写或想限制内存占用时，再按需配置。

| 字段 | 类型 | 必填 | 默认值 | 有效范围 | 说明 |
| ------ | ------ | ------ | ------ | ------ | ------ |
| `wal` | Bool | 否 | `false` | `true` 或 `false` | 是否启用 SQLite WAL 日志模式。`true` 会执行 `PRAGMA journal_mode = WAL`，读写并发体验通常更好，但数据库旁边会出现 `-wal`、`-shm` 辅助文件。单脚本临时数据库可以不启用；Web 服务、桌面应用、长期运行项目建议启用。 |
| `busy_timeout_ms` | Int | 否 | `1000` | `0..=300000` | 数据库被其他连接锁住时，最多等待多少毫秒再报错。`1000` 表示最多等 1 秒；`0` 表示不等待。它只处理 SQLite 文件锁等待，不是 SQL 查询执行超时。 |
| `max_rows` | Int | 否 | `1000` | `1..=100000` | `all()` 单次最多允许返回多少行。超过后会报错，防止一次把大量数据读进 BT VM。只想取一页数据时，建议 SQL 自己加 `LIMIT`，例如 `LIMIT 20`。 |
| `max_result_bytes` | Int | 否 | `4194304` | `1..=16777216` | `one()` 和 `all()` 返回结果的字节估算上限。默认约 4MB，最大 16MB。它用来保护常驻进程内存，避免大文本或大 BLOB 一次性返回过多。 |

## API 列表

| API | 说明 |
| ------ | ------ |
| `sqlite_open(path, options)` | 打开 SQLite 数据库文件，返回 `Sqlite` 连接对象。 |
| `db.query(sql)` | 创建链式查询对象，只保存 SQL，不立即执行。 |
| `query.bind(value)` | 追加一个 `?` 占位符参数。 |
| `query.binds(rows)` | 追加多行批量参数，只能用于 `exec()`。 |
| `query.batch(size)` | 设置批量执行的批大小统计值。 |
| `query.workers(count)` | MySQL 迁移兼容参数；SQLite 当前仍串行写入。 |
| `query.one()` | 执行查询，返回第一行对象；无结果返回 `empty`。 |
| `query.all()` | 执行查询，返回行对象数组。 |
| `query.exec()` | 执行写入或 DDL SQL，返回执行统计对象。 |
| `query.sql()` | 返回带参数预览的 SQL 调试文本。 |
| `query.close()` | 释放链式查询对象。 |
| `db.transaction(statements)` | 在一个事务中串行执行多条 SQL。 |
| `db.close()` | 关闭数据库连接。 |

## sqlite_open

### 功能

打开 SQLite 数据库文件，并返回 `Sqlite` 连接对象。文件不存在时，SQLite 会自动创建文件；但父目录必须已存在，所以示例里通常先写 `fs('@/data').create_dir()`。

### 语法

```bt
db = sqlite_open(path, options)
```

### 参数

| 参数 | 类型 | 必填 | 默认值 | 说明 |
| ------ | ------ | ------ | ------ | ------ |
| `path` | String | 是 | 无 | SQLite 数据库文件路径。建议使用 `@/data/app.db` 这类项目根路径。扩展声明为 `path_write`，路径必须落在项目允许的写入范围内。 |
| `options` | Object | 是 | `{}` | 连接配置对象。没有特殊需求时传 `{}`。字段见“常用配置示例”。 |

### 返回值

| 类型 | 说明 |
| ------ | ------ |
| `Sqlite` | SQLite 连接对象。后续通过 `db.query()`、`db.transaction()`、`db.close()` 使用。 |

### 示例

```bt
fs('@/data').create_dir()
db = sqlite_open('@/data/app.db', {})

// 输出：Sqlite
print type(db)

db.close()
```

## db.query

### 功能

创建链式查询对象。`query()` 本身只保存 SQL，不会连接表、不读取数据、不写入数据；真正执行发生在后面的 `one()`、`all()` 或 `exec()`。

### 语法

```bt
query = db.query(sql)
```

### 参数

| 参数 | 类型 | 必填 | 默认值 | 说明 |
| ------ | ------ | ------ | ------ | ------ |
| `sql` | String | 是 | 无 | 待执行 SQL。需要参数时使用 `?` 占位符，例如 `WHERE id = ?`。 |

### 返回值

| 类型 | 说明 |
| ------ | ------ |
| `SqliteQuery` | 链式查询对象，可以继续调用 `bind()`、`binds()`、`one()`、`all()`、`exec()` 等方法。 |

### 示例

```bt
db = sqlite_open('@/data/app.db', {})
query = db.query('SELECT 1 AS value')

// 输出：SqliteQuery
print type(query)

query.close()
db.close()
```

## query.bind

### 功能

追加一个普通绑定参数。每调用一次 `bind(value)`，就按顺序绑定到 SQL 中下一个 `?` 占位符。

绑定参数可以避免手动拼接 SQL，能减少引号转义错误，也能避免 SQL 注入风险。

### 语法

```bt
query = db.query(sql).bind(value)
```

### 参数

| 参数 | 类型 | 必填 | 默认值 | 说明 |
| ------ | ------ | ------ | ------ | ------ |
| `value` | Null/Bool/Int/Float/String/Bytes | 是 | 无 | 要绑定到 `?` 的值。`null` 会写入 SQLite `NULL`；Bool 会按 `0` 或 `1` 写入；Bytes 会写入 BLOB。 |

### 返回值

| 类型 | 说明 |
| ------ | ------ |
| `SqliteQuery` | 返回同一个查询对象，便于继续链式调用。 |

### 示例

```bt
db = sqlite_open('@/data/app.db', {})
db.query('CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT)').exec()
db.query('INSERT INTO users (name) VALUES (?)').bind('Alice').exec()

row = db.query('SELECT name FROM users WHERE name = ?').bind('Alice').one()

// 输出：Alice
print row.name

db.close()
```

## query.binds

### 功能

追加多行批量绑定参数，只用于 `exec()`。常见用途是一次插入多行数据。

`binds(rows)` 的参数通常是二维数组：外层数组表示多行，内层数组表示这一行要绑定到 SQL 占位符的多个值。

### 语法

```bt
query = db.query(sql).binds(rows)
query = db.query(sql).bind(prefix).binds(rows)
```

### 参数

| 参数 | 类型 | 必填 | 默认值 | 说明 |
| ------ | ------ | ------ | ------ | ------ |
| `rows` | Array | 是 | 无 | 批量绑定行数组。推荐传二维数组，例如 `[['Alice', 18], ['Bob', 20]]`。如果某个行元素不是数组，会按单值行处理。 |

### 返回值

| 类型 | 说明 |
| ------ | ------ |
| `SqliteQuery` | 返回同一个查询对象。 |

### 示例

```bt
db = sqlite_open('@/data/app.db', {})
db.query('CREATE TABLE IF NOT EXISTS users (name TEXT, age INTEGER)').exec()

ret = db.query('INSERT INTO users (name, age) VALUES (?, ?)')
    .binds([
        ['Alice', 18],
        ['Bob', 20]
    ])
    .exec()

// 输出：2
print ret.rows_affected

db.close()
```

### 注意事项

- `binds()` 只能配合 `exec()` 使用。
- `binds()` 不能配合 `one()` 或 `all()`，否则会报错。
- 如果每一行前面有固定参数，可以先 `bind(prefix)`，再 `binds(rows)`；执行时会把固定参数拼到每一行前面。

## query.batch

### 功能

设置批量 `exec()` 的批大小统计值。它主要用于和 MySQL 标准库的批量写入写法保持一致。

SQLite 当前会在同一个事务里串行执行这些绑定行；`batch(size)` 会影响返回对象里的 `batch_count` 和 `batch_size`，方便迁移代码和观察批量规模。

### 语法

```bt
query = db.query(sql).binds(rows).batch(size)
```

### 参数

| 参数 | 类型 | 必填 | 默认值 | 有效范围 | 说明 |
| ------ | ------ | ------ | ------ | ------ | ------ |
| `size` | Int | 是 | 未调用时为 `0` | `0` 或正整数 | 每批行数。小于 `0` 会按 `0` 处理；`0` 表示使用全部绑定行作为一批。 |

### 返回值

| 类型 | 说明 |
| ------ | ------ |
| `SqliteQuery` | 返回同一个查询对象。 |

### 示例

```bt
db = sqlite_open('@/data/app.db', {})
db.query('CREATE TABLE IF NOT EXISTS logs (text TEXT)').exec()

ret = db.query('INSERT INTO logs (text) VALUES (?)')
    .binds([['a'], ['b'], ['c']])
    .batch(2)
    .exec()

// 输出：2
print ret.batch_count

db.close()
```

## query.workers

### 功能

设置批量 `exec()` 的工作数统计值。这个方法是为了让从 MySQL 标准库迁移过来的代码可以保留相近写法。

SQLite 的同一个连接不能像 MySQL 连接池那样并发写入；当前实现仍会在一个事务内串行执行。也就是说，`workers(4)` 不会让同一个 SQLite 连接同时并发写 4 条 SQL。

### 语法

```bt
query = db.query(sql).binds(rows).workers(count)
```

### 参数

| 参数 | 类型 | 必填 | 默认值 | 有效范围 | 说明 |
| ------ | ------ | ------ | ------ | ------ | ------ |
| `count` | Int | 是 | 未调用时为 `1` | `1..=4096` | 迁移兼容工作数。小于 `1` 会按 `1` 处理，大于 `4096` 会按 `4096` 处理。 |

### 返回值

| 类型 | 说明 |
| ------ | ------ |
| `SqliteQuery` | 返回同一个查询对象。 |

### 示例

```bt
db = sqlite_open('@/data/app.db', {})
db.query('CREATE TABLE IF NOT EXISTS logs (text TEXT)').exec()

ret = db.query('INSERT INTO logs (text) VALUES (?)')
    .binds([['a'], ['b']])
    .workers(4)
    .exec()

// 输出：4
print ret.workers

db.close()
```

## query.one

### 功能

执行查询并返回第一行。适合按主键查询、查询一条配置、查询计数等只需要一行的场景。

### 语法

```bt
row = db.query(sql).bind(value).one()
```

### 参数

无参数。

### 返回值

| 类型 | 说明 |
| ------ | ------ |
| Object/Empty | 查询到行时返回对象，字段名来自 SQL 返回列名；没有任何结果时返回 `empty`。SQLite `NULL` 字段返回 BT `null`，BLOB 字段返回 BT Bytes。 |

### 示例

```bt
db = sqlite_open('@/data/app.db', {})
db.query('CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT)').exec()
db.query('INSERT INTO users (name) VALUES (?)').bind('Alice').exec()

row = db.query('SELECT id, name FROM users WHERE name = ?').bind('Alice').one()
missing = db.query('SELECT id, name FROM users WHERE name = ?').bind('Missing').one()

// 输出：Alice
print row.name

// 输出：true
print is_empty(missing)

db.close()
```

## query.all

### 功能

执行查询并返回多行对象数组。适合列表页、分页查询和导出少量数据。

### 语法

```bt
rows = db.query(sql).bind(value).all()
```

### 参数

无参数。

### 返回值

| 类型 | 说明 |
| ------ | ------ |
| Array | 返回行对象数组。没有结果时返回空数组 `[]`。单次返回受 `sqlite_open()` 的 `max_rows` 和 `max_result_bytes` 限制。 |

### 示例

```bt
db = sqlite_open('@/data/app.db', {
    max_rows: 100,
    max_result_bytes: 1048576
})
db.query('CREATE TABLE IF NOT EXISTS users (name TEXT)').exec()
db.query('INSERT INTO users (name) VALUES (?)').bind('Alice').exec()
db.query('INSERT INTO users (name) VALUES (?)').bind('Bob').exec()

rows = db.query('SELECT name FROM users ORDER BY name LIMIT 20').all()

// 输出：2
print rows.len()

db.close()
```

## query.exec

### 功能

执行不需要返回结果集的 SQL，例如 `CREATE TABLE`、`INSERT`、`UPDATE`、`DELETE`。普通 `bind()` 执行一次；`binds()` 会在 SQLite 事务中按绑定行串行执行。

### 语法

```bt
ret = db.query(sql).exec()
ret = db.query(sql).bind(value).exec()
ret = db.query(sql).binds(rows).batch(size).workers(count).exec()
```

### 参数

无参数。

### 返回值

| 类型 | 说明 |
| ------ | ------ |
| Object | 返回 SQL 执行统计对象。 |

### 执行结果字段

| 字段 | 类型 | 必定存在 | 说明 |
| ------ | ------ | ------ | ------ |
| `total` | Int | 是 | 本次执行处理的绑定行数。普通执行为 `1`；`binds()` 批量执行时为绑定数组行数；空绑定行时为 `0`。 |
| `rows_affected` | Int | 是 | SQLite 报告的受影响行数。批量执行时为逐行累加值。 |
| `last_insert_id` | Int | 是 | SQLite 当前连接的 `last_insert_rowid()`，通常用于读取最近一次自增主键。 |
| `batch_count` | Int | 是 | 按 `batch()` 计算出的批次数。普通执行通常为 `1`；空批量为 `0`。 |
| `batch_size` | Int | 是 | 当前查询对象配置的批大小。未调用 `batch()` 时为 `0`。 |
| `workers` | Int | 是 | 当前查询对象配置并规范化后的工作数。SQLite 当前不并发写同一个连接。 |

### 示例

```bt
db = sqlite_open('@/data/app.db', {})
db.query('CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT)').exec()

ret = db.query('INSERT INTO users (name) VALUES (?)').bind('Alice').exec()

// 输出：1
print ret.rows_affected

// 输出示例：1
print ret.last_insert_id

db.close()
```

## query.sql

### 功能

返回当前 SQL 的调试预览文本。它会把已绑定参数渲染到 `?` 占位符位置，方便你打印检查 SQL。

这个文本只用于调试，不参与真实执行。真实执行仍使用 SQLite 参数绑定。

### 语法

```bt
text = db.query(sql).bind(value).sql()
```

### 参数

无参数。

### 返回值

| 类型 | 说明 |
| ------ | ------ |
| String | SQL 调试预览文本。 |

### 示例

```bt
db = sqlite_open('@/data/app.db', {})
text = db.query('SELECT name FROM users WHERE id = ?').bind(1001).sql()

// 输出：SELECT name FROM users WHERE id = 1001
print text

db.close()
```

## query.close

### 功能

释放链式查询对象。查询对象保存了 SQL、绑定参数和批量配置；长时间运行的服务里，如果你创建了很多临时查询对象，可以显式释放。

### 语法

```bt
query.close()
```

### 参数

无参数。

### 返回值

| 类型 | 说明 |
| ------ | ------ |
| Bool | 成功释放返回 `true`。释放后旧查询对象失效，不能继续调用。 |

## db.transaction

### 功能

在同一个 SQLite 事务中串行执行多条写语句。所有语句都成功时提交；中途任何一条失败时，SQLite 会回滚本次事务。

适合“多步写入必须一起成功或一起失败”的场景，例如创建订单和扣库存。

### 语法

```bt
changed = db.transaction(statements)
```

### 参数

| 参数 | 类型 | 必填 | 默认值 | 说明 |
| ------ | ------ | ------ | ------ | ------ |
| `statements` | Array | 是 | 无 | 事务语句数组。元素可以是 SQL 字符串，也可以是 `{ sql, binds }` 对象。 |

### statements 对象字段

| 字段 | 类型 | 必填 | 默认值 | 说明 |
| ------ | ------ | ------ | ------ | ------ |
| `sql` | String | 是 | 无 | 待执行 SQL。 |
| `binds` | Array | 否 | `[]` | SQL 参数数组。支持 `null`、Bool、Int、Float、String、Bytes；不允许 `empty`。 |
| `params` | Array | 否 | `[]` | `binds` 的旧字段别名，建议新代码使用 `binds`。如果同时写了 `binds` 和 `params`，优先使用 `binds`。 |

### 返回值

| 类型 | 说明 |
| ------ | ------ |
| Int | 事务内累计影响行数。 |

### 示例

```bt
db = sqlite_open('@/data/app.db', {})
db.query('CREATE TABLE IF NOT EXISTS users (name TEXT, note TEXT)').exec()
db.query('INSERT INTO users (name, note) VALUES (?, ?)').bind('Bob').bind('new').exec()
db.query('INSERT INTO users (name, note) VALUES (?, ?)').bind('Carol').bind('new').exec()

changed = db.transaction([
    { sql: 'UPDATE users SET note = ? WHERE name = ?', binds: ['reader', 'Bob'] },
    { sql: 'UPDATE users SET note = ? WHERE name = ?', binds: ['reader', 'Carol'] }
])

// 输出：2
print changed

db.close()
```

## db.close

### 功能

关闭 SQLite 数据库连接。BT 是常驻内存语言，服务或桌面应用可能运行很久；不用的连接应主动关闭，避免连接对象一直留在 shared worker 中。

### 语法

```bt
db.close()
```

### 参数

无参数。

### 返回值

| 类型 | 说明 |
| ------ | ------ |
| Bool | 成功关闭返回 `true`。关闭后旧连接对象失效，不能继续查询。 |

## 完整示例

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

// 输出：1
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

// 输出：Alice
print row.name

// 输出：true
print is_null(row.note)

missing = db.query('SELECT name FROM users WHERE name = ?').bind('Missing').one()

// 输出：true
print is_empty(missing)

rows = db.query('SELECT id, name FROM users ORDER BY id LIMIT 20').all()

// 输出：3
print rows.len()

changed = db.transaction([
    { sql: 'UPDATE users SET note = ? WHERE name = ?', binds: ['reader', 'Bob'] },
    { sql: 'UPDATE users SET note = ? WHERE name = ?', binds: ['reader', 'Carol'] }
])

// 输出：2
print changed

db.close()
```

## 数据类型对应关系

| BT 值 | SQLite 参数或返回值 |
| ------ | ------ |
| `null` | SQLite `NULL` |
| Bool | 整数 `0` 或 `1` |
| Int | INTEGER |
| Float | REAL |
| String | TEXT |
| Bytes | BLOB |
| `empty` | 不允许作为 SQL 参数；请显式传 `null` |

## 注意事项

- 推荐所有用户都用 `query(sql).bind(...).one/all/exec()` 写法，不要手动拼接用户输入到 SQL 字符串里。
- `bind()` 每次只绑定一个值，需要多个参数就连续调用多次。
- `binds()`、`batch()`、`workers()` 是批量写入用法，只支持 `exec()`，不支持 `one()` 和 `all()`。
- `workers()` 是 MySQL 迁移兼容接口，SQLite 当前不会并发写同一个连接。
- `all()` 适合读取有限结果集；大列表建议 SQL 加 `LIMIT/OFFSET`，并设置合理的 `max_rows` 和 `max_result_bytes`。
- `one()` 无结果返回 `empty`，数据库字段值为 SQL `NULL` 时返回 BT `null`，两者含义不同。
- `close()` 和 `query.close()` 调用成功后旧句柄会失效。
