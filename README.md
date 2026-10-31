# DBiji

**A lightweight SQL database client, written entirely in Rust.**

DBiji is a native alternative to DBeaver for SQL databases: one small binary, no JVM, no Electron.
It opens in about 0.1 s, idles at 0 % CPU and uses around 70 MB of memory — and result sets of
millions of rows never have to fit in memory.

[Bahasa Indonesia](README.id.md)

![DBiji: navigator, SQL editor and result grid](docs/screenshots/editor.png)

## Highlights

- **Many databases, one interface**: PostgreSQL, CockroachDB, MySQL, MariaDB, SQLite, SQL Server,
  ClickHouse, MongoDB, Redis and (optional) DuckDB.
- **SQL editor** with schema-aware autocomplete (tables, columns, joins, aliases, CTEs), templates,
  `:name` / `${name}` parameters, `@set` / `@include` / `@export`, formatter, outline, error markers,
  explain plans (text, tree, diagram) and a searchable query history.
- **Data grid** that scrolls 1 M rows at under 1 ms per frame: inline editing saved in one
  transaction, foreign key navigation, server- or client-side filter and sort, record / JSON / XML
  views, value panels, charts and a map for geometry columns.
- **Safe by default**: development / test / production connection types, confirmations for
  `DELETE`/`UPDATE` without `WHERE` and `DROP`, read-only connections, a separate session per editor.
- **Connections** through SSH tunnels, SOCKS5 / HTTP proxies and SSL. Passwords live in the **macOS
  Keychain** or an encrypted vault with a master password — never in a file.
- **Import** connections from DBeaver, `~/.pgpass` and URLs.
- **Everything else DBeaver users expect**: export / import (CSV, JSON, SQL, XLSX, XML, Parquet …),
  migration between databases, mock data, ER diagrams, visual query builder, schema and data compare
  (with Liquibase output), table designer, tasks and scheduler, backup / restore, session and lock
  manager, server dashboard, PostgreSQL debugger and an optional AI assistant with your own API key.
- English and Indonesian UI, light and dark themes (View → Theme or the sun / moon button in the status
  bar), customisable shortcuts and toolbar, no telemetry.
- **Automatic updates** from GitHub releases (optional), with one click to install, and release notes after every update.

## Install

Requires macOS 12 Monterey or later, on Apple Silicon or Intel.

### One-line installer (recommended)

```sh
curl -fsSL https://raw.githubusercontent.com/ilhamnadhif/dbiji/master/scripts/install.sh | bash
```

The script downloads the latest release, checks it (the SHA-256 GitHub publishes, the code
signature, the bundle id and version), installs `DBiji.app` into `/Applications`, and opens it. Run
it again to update. To install a specific version, pass it as an argument: `… | bash -s -- v0.3.0`.

### Disk image

Download **`DBiji-<version>.dmg`** from the
[latest release](https://github.com/ilhamnadhif/dbiji/releases/latest), open it and drag **DBiji**
onto **Applications**.

> **“DBiji” Not Opened / “Apple could not verify…”?** Release builds are not notarized by Apple yet,
> so macOS blocks apps downloaded with a browser. Do one of these:
>
> - **Use the one-line installer** above. Nothing downloaded with `curl` is blocked.
> - **Allow it once in Settings**: click **Done** in the warning, open **System Settings → Privacy &
>   Security**, and click **Open Anyway** next to “DBiji was blocked…”.
> - **In Terminal**: `xattr -dr com.apple.quarantine /Applications/DBiji.app`

### Homebrew

```sh
brew install --cask ./packaging/homebrew/dbiji.rb
```

### From source

```sh
cargo build --release -p dbiji-app                     # target/release/dbiji
cargo build --release -p dbiji-app --features duckdb   # with the embedded DuckDB engine
scripts/bundle-macos.sh                                # dist/DBiji.app, DBiji-<version>.dmg and .zip
```

## Getting started

1. **New connection** (or **Import connections from DBeaver**) on the welcome page — or click
   **Create sample database** to try everything on a small SQLite shop database.
2. Open a SQL editor with `⌘]`, type a query and press `⌘↩`.
3. Double-click a table in the navigator to edit its data; click a column header to sort it
   (ascending → descending → off); `⌘S` saves the changes.

## Updates

If you allow it, DBiji looks for a new release when it starts and every 6 hours. A new version
opens a dialog with the current and new version and what's new (found later, it lights up an
**Update** button in the toolbar instead of interrupting you). **Install update** then:

1. downloads the new version and verifies it (size, the SHA-256 GitHub publishes, bundle id,
   version and code signature);
2. after **Restart now**, quits DBiji (asking about unsaved scripts first), swaps in the new app and
   opens it again; the previous copy is put back if anything goes wrong.

macOS asks for an administrator password only when your user cannot replace the app. Help → Check
for updates… checks right away; **Skip this version** and **Remind me later** are there too. After an
update DBiji shows what changed (also in Help → What's new; all notes are in
[CHANGELOG.md](CHANGELOG.md)). The one-line installer above also updates, and Homebrew users update
with `brew upgrade --cask dbiji`. The update check is one anonymous request to GitHub; nothing about
you or your databases is sent.

Installing in place needs DBiji in `/Applications` (or another folder it can write to); version
0.3.0 is the first with the updater.

## Command line

```text
dbiji [FILE.sql | DATABASE.db ...]   open scripts, SQLite / DuckDB files
dbiji --execute FILE.db FILE.sql     open and run the script on the database
dbiji --run-task <id|name>           run a saved task without a window (exit code 0/1/2/3)
dbiji --portable <DIR>               keep all settings and data in DIR
dbiji --bench all                    startup, memory, grid and editor benchmarks
```

## Keyboard shortcuts

| Action | Shortcut |
| --- | --- |
| Execute statement / selection | `⌘↩` |
| Execute script | `⌥X` |
| Execute in a new result tab | `⇧⌘↩` |
| Explain plan | `⇧⌘E` |
| Cancel query | `⌘.` |
| New SQL editor | `⌘]` |
| Active connection / schema | `⌘9` / `⌘0` |
| Format SQL | `⇧⌘F` |
| Autocomplete | `⌃Space` |
| Command palette | `⇧⌘P` |
| Save script / data | `⌘S` |

Every shortcut can be changed in **Settings → Keyboard shortcuts** (DBiji or DBeaver preset).

## Performance

Measured on the release build (Apple M1 Pro) with `dbiji --bench all`:

| | Target | Measured |
| --- | --- | --- |
| Binary size | < 40 MB | 37 MB |
| Cold start to first frame | < 1 s | ≈ 0.1 s |
| Idle memory | < 120 MB | ≈ 69 MB |
| 3 connections + 5 result tabs | < 250 MB | ≈ 83 MB |
| 1 M rows scroll | < 16 ms / frame | p95 0.5 ms |
| Typing in a 50 000-line script | < 16 ms / frame | p95 0.4 ms |

## Security and data

- No telemetry. DBiji only connects to the servers you configure and, if you enable them, to your AI
  provider and to GitHub for the update check (one anonymous request a day for the latest release;
  asked once, changeable in Settings → General).
- Passwords and API keys are stored in the macOS Keychain or in `vault.bin` (Argon2id +
  XChaCha20-Poly1305); configuration files are written with mode 0600.
- Settings, connections, scripts, history and tasks live in
  `~/Library/Application Support/dev.dbiji.DBiji/` (or `DBIJI_HOME` / `--portable`).

## Building and testing

```sh
cargo test --workspace --exclude dbiji-driver-duckdb
# integration tests against real servers (Docker):
export DBIJI_TEST_PG=postgres://dbiji:dbiji@127.0.0.1:55432/dbiji
export DBIJI_TEST_MYSQL=mysql://root:dbiji@127.0.0.1:53306/dbiji
export DBIJI_TEST_MARIADB=mariadb://root:dbiji@127.0.0.1:53307/dbiji
export DBIJI_TEST_COCKROACH='cockroachdb://root@127.0.0.1:26257/defaultdb?sslmode=disable'
export DBIJI_TEST_MONGO=127.0.0.1:27018   # mongod on that port
export DBIJI_TEST_REDIS=127.0.0.1:6390   # redis-server on that port
cargo test -p dbiji-app
```

Signing and notarization: set `DBIJI_SIGN_IDENTITY` (Developer ID) and `DBIJI_NOTARY_PROFILE`
before running `scripts/bundle-macos.sh`.

## Project layout

| Crate | Role |
| --- | --- |
| `dbiji-app` | The application: window, tabs, navigator, execution engine, CLI |
| `dbiji-core` | Shared contract: values, driver/session traits, metadata, DDL, dialects, row store, i18n |
| `dbiji-sql` | Tokenizer, splitter, formatter, autocomplete, analysis, parameters |
| `dbiji-driver-*` | PostgreSQL, MySQL, SQLite, SQL Server, ClickHouse, DuckDB, MongoDB, Redis drivers |
| `dbiji-editor` / `dbiji-grid` / `dbiji-visual` | Editor widget, data grid, diagrams and charts |
| `dbiji-config` / `dbiji-net` | Settings, secrets, importers / SSH tunnels and proxies |
| `dbiji-transfer` / `dbiji-compare` / `dbiji-tasks` / `dbiji-ai` | Data transfer, compare, tasks, AI |

More in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) and the DBeaver parity matrix in
[docs/PARITY.md](docs/PARITY.md).

## Known limitations

- Oracle, Db2, Firebird, H2 and Trino are not supported (no pure-Rust network driver yet).
- MongoDB and Redis have no SQL: their editors take the shell language (`db.users.find({…})`) and
  Redis commands (`HGETALL session:9`), one statement per line or `;`. Cell editing, SQL tools
  (query builder, ER diagram, table designer, data search) and transactions do not apply to them.
- Kerberos and MySQL multi-factor authentication are not supported.
- Release builds are not notarized yet.

## License

MIT OR Apache-2.0.

The bundled Inter typeface (`crates/dbiji-app/assets/fonts`) is licensed under the SIL Open Font License 1.1 (`Inter-LICENSE.txt`).
