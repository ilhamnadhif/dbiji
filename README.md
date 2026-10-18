# DBiji

A lightweight desktop SQL client written 100% in Rust, aiming at feature parity
with DBeaver for SQL databases — without the JVM. One native process, a
GPU-rendered immediate-mode UI (egui), a tokio runtime for all I/O, and
result sets that never have to fit in memory.

| | DBiji target (PRD §2) | Measured (macOS, Apple M1 Pro) |
| --- | --- | --- |
| Idle memory | < 120 MB | see `dbiji --bench memory` (≈ 75 MB debug build) |
| 3 connections + 5 result tabs | < 250 MB | ≈ 94 MB (debug build, 20 000 rows per tab) |
| Cold start to first frame | < 1 s | ≈ 0.1 s |
| 1 M rows × 100 columns scroll | < 16 ms / frame | avg < 3 ms, worst 11.7 ms |
| Idle CPU | ≈ 0 % | repaints only on input or finished background work |

## Databases

| Database | Driver | Notes |
| --- | --- | --- |
| PostgreSQL 12+ | `dbiji-driver-postgres` (tokio-postgres) | TLS (verify-full by default when requested), arrays, JSONB, PostGIS, EXPLAIN (ANALYZE) JSON, pldebugger, pg_dump/pg_restore |
| CockroachDB | same crate | PostgreSQL protocol with CockroachDB catalog adjustments |
| MySQL 5.7 / 8.x, MariaDB 10+ | `dbiji-driver-mysql` (mysql_async) | separate MySQL and MariaDB dialects, EXPLAIN JSON/TREE/ANALYZE, mysqldump |
| SQLite | `dbiji-driver-sqlite` (rusqlite, bundled) | file connections, `.db` files open from the command line, online backup |
| SQL Server | `dbiji-driver-mssql` (tiberius) | TDS 7.3+, XML plans, sessions/locks/KILL, sqlcmd |
| ClickHouse | `dbiji-driver-clickhouse` (HTTP) | streaming, KILL QUERY cancel, EXPLAIN json |
| DuckDB | `dbiji-driver-duckdb` (embedded, optional) | build with `--features duckdb` (adds ≈ 30 MB) |

Oracle, Db2, Firebird, H2 and Trino are recorded exceptions (no pure-Rust
network driver exists); see [docs/PARITY.md](docs/PARITY.md).

## Features

* **Connections** — wizard with every driver property, connection types
  (development / test / production colours, confirmations, auto-commit
  defaults), SSH tunnels (password, keys, agent, jump via proxy), SOCKS5 /
  HTTP proxies, SSL with client certificates and custom CA, network profiles,
  init SQL and shell commands, read-only connections enforced in the client,
  a separate session per editor, keep-alive and reconnect, import from
  DBeaver workspaces, `~/.pgpass` and connection URLs.
* **Secrets** — macOS Keychain (Windows Credential Manager / Secret Service)
  or an encrypted vault (Argon2id + XChaCha20-Poly1305) with a master
  password; never written in plain text; configuration files are 0600.
* **Navigator** — lazy, cached and virtualised object tree with folders,
  filters, simple/advanced views, system objects toggle, bookmarks, projects
  with script folders and saved diagrams; metadata and data search.
* **SQL editor** — virtualised custom editor (50 000-line scripts type in
  < 1 ms), dialect-aware highlighting, schema-aware autocomplete (tables,
  columns, joins from foreign keys, aliases, CTEs), hover help, ⌘-click
  hyperlinks to objects, find/replace with regex, formatter, `:name` /
  `${name}` parameters, `@set` / `@include` / `@export` / `@echo` client
  commands, outline, error markers at the server's error position,
  diagnostics for unknown tables/columns, spell checking of comments and
  strings, statement / selection / script execution, results in several
  tabs or one, statistics, output log, terminal mode, explain plans (text,
  tree, diagram), smart commit, transaction log, query history, SQL
  generation from objects, visual query builder, scripts saved as `.sql`
  files and editors restored on start.
* **Data editor** — custom-painted grid virtualised in both directions,
  incremental fetching into a bounded row store that spills to disk, inline
  editing with type validation, add/duplicate/delete rows, pending changes
  saved in one transaction (or inside your open transaction) with a SQL
  preview, read-only reasons and virtual keys, server- or client-side
  filtering and sorting, grid/text/record/JSON/XML views, value / metadata /
  grouping / calc / references panels, advanced copy (TSV, CSV, Markdown,
  JSON, SQL, HTML, XML), foreign-key navigation with back/forward, auto
  refresh, total row count, charts, spatial (WKT/WKB/EWKB) viewer.
* **Data transfer** — export to CSV, TSV, JSON, JSON Lines, SQL (INSERT /
  UPSERT / CREATE TABLE), XLSX, Markdown, HTML, XML, TXT, DBUnit, PHP and
  Parquet; import CSV/TSV, XLSX, XML and JSON with type inference and column
  mapping; migration between databases; mock data generation; everything
  can be saved as a task.
* **Schema** — object editor (properties, data, DDL, dependencies,
  statistics), table designer with DDL preview, ER diagrams (auto layout,
  saved diagrams, edit mode, SVG/PNG export), schema compare (dependency
  ordered migration script, HTML report, diagram diff, Liquibase changelog),
  data compare with sync script.
* **Administration** — session manager, lock manager, server dashboard,
  backup/restore with the vendor tools, maintenance (VACUUM, ANALYZE,
  OPTIMIZE ...), PostgreSQL debugger, change password.
* **Tasks** — SQL scripts, exports, imports, backups, schema compares,
  shell commands and composite tasks; cron / interval / daily schedules, run
  log, `dbiji --run-task` for headless runs and launchd / cron / schtasks
  integration.
* **AI assistant** (off by default) — chat, SQL generation, explanations,
  error fixes and optimisation with your own Anthropic or OpenAI-compatible
  API key; schema context only when you allow it.
* **Interface** — light/dark/system theme, high contrast, English and
  Indonesian, every shortcut customisable (DBiji or DBeaver preset),
  customisable toolbar, command palette (⇧⌘P), keyboard navigation everywhere.

## Install

* **DMG** — download `DBiji-<version>.dmg` from the releases, drag DBiji to
  Applications.
* **Homebrew** — `brew install --cask dbiji` (cask template in
  `packaging/homebrew/dbiji.rb`).
* **From source** — Rust 1.88+:

  ```sh
  cargo build --release -p dbiji-app            # target/release/dbiji
  cargo build --release -p dbiji-app --features duckdb
  scripts/bundle-macos.sh                        # dist/DBiji.app and DMG
  ```

## Command line

```
dbiji [FILE.sql | DATABASE.db ...]   open scripts / SQLite and DuckDB files
dbiji --run-task <id|name>           run a saved task headless (exit code 0/1/2/3)
dbiji --portable <DIR>               keep settings and data in DIR
dbiji --bench startup|memory|all     benchmarks used by CI
```

Headless task runs read secrets from the Keychain; with the encrypted vault
set `DBIJI_VAULT_PASSWORD`.

## Default shortcuts

| Action | macOS |
| --- | --- |
| Execute statement / selection | ⌘↩ |
| Execute script | ⌥X |
| Execute in a new result tab | ⇧⌘↩ |
| Explain plan | ⇧⌘E |
| Cancel query | ⌘. |
| New SQL editor | ⌘] |
| Active connection / schema | ⌘9 / ⌘0 |
| Format SQL | ⇧⌘F |
| Autocomplete | ⌃Space |
| Command palette | ⇧⌘P |
| Save script / data | ⌘S |

All bindings are listed under *Help → Keyboard shortcuts* and editable in
*Settings → Keyboard shortcuts*.

## Where data lives

| | macOS |
| --- | --- |
| Settings, keymap, connections (no secrets) | `~/Library/Application Support/dev.dbiji.DBiji/` |
| Query history, projects, scripts, tasks | `~/Library/Application Support/dev.dbiji.DBiji/` (data dir) |
| Secrets | Keychain service `dev.dbiji.DBiji`, or `vault.bin` |

Set `DBIJI_HOME` (or `--portable`) to keep everything in one folder.
DBiji has no telemetry; it only connects to the servers you configure and,
when enabled, to your AI provider.

## Development

```sh
cargo test --workspace                       # unit tests
# integration tests against real servers (Docker):
export DBIJI_TEST_PG=postgres://dbiji:dbiji@127.0.0.1:55432/dbiji
export DBIJI_TEST_MYSQL=mysql://root:dbiji@127.0.0.1:53306/dbiji
export DBIJI_TEST_MARIADB=mariadb://root:dbiji@127.0.0.1:53307/dbiji
cargo test -p dbiji-app                      # editor, data editing, compare gate, backup ...
target/release/dbiji --bench all             # startup / memory benchmarks
```

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) and
[docs/PARITY.md](docs/PARITY.md).

## Bahasa Indonesia

DBiji adalah klien database SQL desktop yang ringan, ditulis 100% dengan
Rust, dengan target fitur setara DBeaver untuk database SQL. Antarmuka
tersedia dalam bahasa Inggris dan Indonesia (*Settings → General → Language*).
Semua password disimpan di Keychain macOS atau brankas terenkripsi; tidak
ada telemetri.

## License

MIT OR Apache-2.0.
