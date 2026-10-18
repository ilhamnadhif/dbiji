# DBeaver parity matrix

Every row of the PRD §4 inventory, with where it is implemented and the
automated test that covers it, or the recorded reason for an exception
(PRD §13, phase 3 gate). Test names are `crate::file::test` (integration
tests in `crates/<crate>/tests/`). Tests marked *(env)* need the Docker
servers from `.github/workflows/ci.yml` (`DBIJI_TEST_PG`, `DBIJI_TEST_MYSQL`,
`DBIJI_TEST_MARIADB`, `DBIJI_TEST_CLICKHOUSE`, `DBIJI_TEST_MSSQL`).

Legend: ✅ implemented and tested · 🟡 implemented, partially tested ·
⛔ exception (reason given).

## Connections

| Feature | Status | Where | Tests |
| --- | --- | --- | --- |
| Connection wizard (all driver properties, URL paste, test) | ✅ | `dbiji-app/src/dialogs/connection.rs` | `dbiji-app::tabs_smoke`, `dbiji-config::import::url` unit tests |
| Connection types dev/test/prod, colours, confirmations, auto-commit | ✅ | `dbiji-config::connections`, settings → Connection types | `dbiji-app::editor_e2e::editor_executes_…` (confirmations) |
| SSH tunnel (password, keys, agent; host key policies) | ✅ | `dbiji-net::tunnel`, `ssh` | `dbiji-net::tunnel` (17 end-to-end tests against an in-process SSH server) |
| SSL with truststore / client certificates | ✅ | drivers (`ConnectParams.ssl`) | `dbiji-driver-postgres::integration` TLS modes *(env)*, `dbiji-driver-mysql::connect` *(env)* |
| Proxy (SOCKS5, HTTP CONNECT) | ✅ | `dbiji-net::socks`, `http_proxy` | `dbiji-net::tunnel` proxy → SSH chains |
| Network profiles | ✅ | `ConnectionStore::network_profiles`, settings → Network profiles, connection dialog selector | `dbiji-config::connections` unit tests |
| Init SQL and shell commands before/after connect | ✅ | `dbiji-app::conn::connect_task`, `dbiji-net::process` | `dbiji-net::process` unit tests |
| Auto-commit / manual mode, pending transaction, transaction log | ✅ | `SessionHandle::tx_state`, SQL editor toolbar | `dbiji-app::data_edit_e2e` (save inside transactions) *(env)* |
| Separate connection per editor | ✅ | `InitConfig::separate_editor_connection`, `SqlEditorTab::session` | `dbiji-app::editor_e2e` |
| Driver manager | ⛔ | Drivers are compiled in (no JDBC jars to manage); *Help → Database drivers* lists them with capabilities | — |
| Import connections (DBeaver, pgpass, URL) | ✅ | `dbiji-config::import`, `dialogs/import.rs` | `dbiji-config::tests::dbeaver_import` (no lost fields), url/pgpass unit tests |

## Authentication and security

| Feature | Status | Where | Tests |
| --- | --- | --- | --- |
| User / password | ✅ | all drivers | every integration suite *(env)* |
| pgpass | ✅ | `AuthMethod::Pgpass`, `import::pgpass::lookup` | `dbiji-config::import::pgpass` unit tests |
| Password from shell command | ✅ | `AuthMethod::Command`, `dbiji-net::password_from_command` | `dbiji-net::process` unit tests |
| Kerberos | ⛔ | No maintained pure-Rust GSSAPI implementation; would need the system Kerberos C libraries, which the PRD's "full Rust" rule excludes for network clients | — |
| MySQL 2FA (multi-factor) | ⛔ | Not supported by `mysql_async`; tracked for when the protocol support lands upstream | — |
| Secret storage with master password | ✅ | `dbiji-config::secrets` (Keychain, Argon2id + XChaCha20-Poly1305 vault) | `dbiji-config::secrets` unit tests (vault, wrong password, migration) |
| Read-only connections | ✅ | client-side classification (`dbiji-sql::classify`) | `dbiji-app::editor_e2e::read_only_connections_block_writes` |
| Change database password | ✅ | `tabs/dbtools.rs` (`ChangePasswordTab`) | see *Administration* |

## Navigator

| Feature | Status | Where | Tests |
| --- | --- | --- | --- |
| Connection / object tree, lazy and cached | ✅ | `nav.rs`, `conn/meta.rs` | `dbiji-app::app_e2e::execute_fetch_count_and_metadata` *(env)* |
| Simple and advanced views | ✅ | `NavigatorView` per connection | `dbiji-app::tabs_smoke` |
| Object filters | ✅ | `ObjectFilters`, connection dialog → Filters | `dbiji-core` filter unit tests |
| Bookmarks | ✅ | `Project::add_bookmark`, Project panel | `dbiji-config::project` unit tests |
| Projects and project explorer (scripts, diagrams) | ✅ | `dbiji-config::project`, `project_view.rs` | `dbiji-config::project` unit tests |
| Metadata search | ✅ | `Session::search_metadata`, `tabs/search.rs` | driver suites *(env)* |
| Full-text / data search | ✅ | `tabs/search.rs` (data search over text columns) | `dbiji-app::tabs_smoke` |
| Properties editor | ✅ | object editor → Properties (+ virtual model) | `dbiji-app::tabs_smoke` |

## SQL editor

| Feature | Status | Where | Tests |
| --- | --- | --- | --- |
| Many scripts per connection saved as files, restored on start | ✅ | `SqlEditorTab::save`, `ProjectManager::{load,save}_session` | `dbiji-config::project` unit tests |
| Execute statement / script / selection, new result tab, cancel | ✅ | `sqlprep.rs`, `exec.rs` | `dbiji-app::editor_e2e`, `dbiji-app::app_e2e::errors_stop_the_script_and_cancel_works` *(env)* |
| Results in several tabs or one | ✅ | result tabs, *Open each query in a new tab* | `dbiji-app::editor_e2e` |
| Autocomplete and semantic analysis (tables, columns, joins, aliases, CTEs) | ✅ | `dbiji-sql::complete`, `sqllang.rs` | `dbiji-sql::complete` 42 scenarios on the standard schema |
| Templates | ✅ | `TemplateLibrary`, offered in autocomplete | `dbiji-app::sqllang::tests` |
| Formatter | ✅ | `dbiji-sql::format` | formatter corpus (PostgreSQL, MySQL, SQLite, T-SQL × 5 option sets, idempotent) |
| Variables / parameters panel | ✅ | parameter prompt + *Variables* result tab | `dbiji-app::sqlprep::tests`, `editor_e2e` |
| Object hyperlinks | ✅ | `EditorLanguage::link_at` | `dbiji-editor` headless tests |
| Outline | ✅ | `dbiji-sql::outline`, editor outline panel | `dbiji-sql::outline` tests |
| Error indicators | ✅ | server error position → editor diagnostic; unknown table/column diagnostics | `dbiji-app::editor_e2e` |
| Spell checker | ✅ | `dbiji-sql::spell` (English dictionary + user words) | `dbiji-sql::spell` tests |
| SQL generation (SELECT/INSERT/UPDATE/DELETE/UPSERT/DDL) | ✅ | `dbiji-sql::generate`, navigator → Generate SQL | `dbiji-sql::generate` tests |
| SQL terminal | ✅ | editor *Terminal mode*, SQL console tabs | `dbiji-app::tabs_smoke` |
| Client commands `@set`, `@include`, `@export`, `@echo` | ✅ | `dbiji-sql::commands`, `sqlprep.rs` | `dbiji-app::sqlprep::tests`, `editor_e2e` |
| Query manager (history) | ✅ | `dbiji-config::history`, `tabs/history.rs` | `dbiji-config::history` tests |
| Switch active connection / schema | ✅ | editor toolbar, ⌘9 / ⌘0 | `dbiji-app::keys::tests` |
| Execute with the native client | ✅ | `Driver::client_command` + `tools::run_command` | driver command tests |

## Query plan and builder

| Feature | Status | Where | Tests |
| --- | --- | --- | --- |
| Execution plan (text, tree, diagram; EXPLAIN ANALYZE) | ✅ | drivers' `explain`, `dbiji-visual::plan` | `editor_e2e` (explain), driver explain tests *(env)*, `dbiji-visual` plan tests |
| Visual query builder | ✅ | `dbiji-visual::builder`, `tabs/query_builder.rs` | `dbiji-visual` builder tests (SQL generation) |

## Data editor

| Feature | Status | Where | Tests |
| --- | --- | --- | --- |
| Grid, text, record, JSON, XML views; spatial | ✅ | `dbiji-grid`, `dbiji-visual::spatial` | `dbiji-grid` UI tests, spatial parser tests |
| Server- or client-side filter and sort | ✅ | `ResultPanel::requery`, grid client views | `dbiji-grid::view` tests |
| Inline editing, add / duplicate / delete rows, save / cancel | ✅ | `ResultModel`, `SessionHandle::apply_changes` | `dbiji-app::data_edit_e2e::edit_save_and_cancel` (PostgreSQL, MySQL, MariaDB, SQLite) |
| Value, references, metadata, grouping, calc panels | ✅ | `dbiji-grid::panels` | `dbiji-grid` UI tests |
| Advanced copy (TSV, CSV, Markdown, JSON, SQL, HTML, XML) | ✅ | `dbiji-grid::copy` | `dbiji-grid::copy` tests |
| Foreign key navigation with history | ✅ | `ResultPanel::navigate` | `dbiji-app::tabs_smoke` |
| Refresh and auto-refresh, fetch size, total count | ✅ | `ResultSource` | `dbiji-app::app_e2e::execute_fetch_count_and_metadata` |
| Virtual columns and virtual keys | ✅ | object editor → Virtual model, `ResultPanel` | `dbiji-app::data_edit_e2e::virtual_model_dictionary_and_virtual_columns` |
| Data dictionary | ✅ | dictionary description columns | same test |
| Data formats | ✅ | settings → Result sets → Value formats | `dbiji-core::value` tests |

## Charts and special data

| Feature | Status | Where | Tests |
| --- | --- | --- | --- |
| Charts from results (line, bar, area, scatter, pie) | ✅ | `dbiji-visual::chart`, result panel *Chart* | `dbiji-visual` chart tests |
| Dashboards (server health) | ✅ | `tabs/admin.rs` (`DashboardTab`), `dbiji-visual::dashboard` | `dbiji-visual` dashboard tests |
| Mock data generation | ✅ | `dbiji-transfer::mock`, `tabs/transfer` | `dbiji-transfer::mock` tests |
| GIS / spatial data | ✅ | `dbiji-visual::spatial` (WKT, WKB, EWKB, MySQL internal) | spatial tests |
| XML / JSON data | ✅ | grid JSON/XML views and value panel | `dbiji-grid` view tests |

## ER diagrams

| Feature | Status | Where | Tests |
| --- | --- | --- | --- |
| Automatic diagrams from a schema or objects | ✅ | `dbiji-visual::er`, `tabs/er.rs` | layout tests; 200-table benchmark (≤ 1.7 ms/frame) |
| Custom saved diagrams, image export | ✅ | project diagrams, SVG/PNG export | `dbiji-visual::er` export tests |
| Edit mode (change the schema from the diagram) | ✅ | edit events → reviewed DDL | `dbiji-visual` UI tests |

## Data transfer

| Feature | Status | Where | Tests |
| --- | --- | --- | --- |
| Export: CSV, DBUnit, JSON, Markdown, PHP, SQL, TXT, XML, XLSX, HTML (+ Parquet, JSON Lines, TSV) | ✅ | `dbiji-transfer::export`, `tabs/transfer` | `dbiji-transfer::formats`, `roundtrip` |
| Import: CSV, XLSX, XML (+ JSON) | ✅ | `dbiji-transfer::import` | `dbiji-transfer::import`, `roundtrip` |
| Import and replace | ✅ | `ImportMapping::mode` | `dbiji-transfer::import` |
| Migration between databases | ✅ | `dbiji-transfer::migration` | `dbiji-transfer::migration` |
| Save as task | ✅ | `TaskKind::{Export, Import, Migration}`, `transferexec.rs` | `dbiji-app::transfer_e2e` *(env)* |
| Round trip CSV / JSON / SQL produces identical data (PRD gate) | ✅ | — | `dbiji-transfer::roundtrip` |

## Comparison

| Feature | Status | Where | Tests |
| --- | --- | --- | --- |
| Schema compare → DDL script | ✅ | `dbiji-compare`, `tabs/compare.rs` | `dbiji-app::app_e2e::schema_compare_gate` (DDL makes schemas identical; PostgreSQL, MySQL, MariaDB) *(env)* |
| HTML report, diagram diff, Liquibase changelog | ✅ | `html_report`, *Diff diagram*, `liquibase_changelog` | `dbiji-compare` tests |
| Data compare with sync script | ✅ | `run_data_compare`, `tabs/data_compare.rs` | `dbiji-compare` data tests, `dbiji-app::tabs_smoke` |

## Tasks

| Feature | Status | Where | Tests |
| --- | --- | --- | --- |
| Task management, SQL script tasks, composite tasks | ✅ | `dbiji-tasks`, `tabs/tasks.rs`, `taskexec.rs` | `dbiji-tasks` tests, `dbiji-app::headless_task`, `dbiji-tasks` tests |
| Scheduler (in app, and launchd / cron / schtasks when closed) | ✅ | `Scheduler`, `dbiji_tasks::os`, `dbiji --run-task` | `dbiji-tasks` scheduler tests (DST, catch-up, cancellation) |

## Administration

| Feature | Status | Where | Tests |
| --- | --- | --- | --- |
| Backup and restore with the vendor tools | ✅ | `Driver::backup_command`/`restore_command`, `tools.rs`, `tabs/dbtools.rs` | `dbiji-app::backup_e2e` (PostgreSQL custom + plain; MySQL / MariaDB when the client tools are installed) *(env)* |
| Session manager, lock manager | ✅ | `tabs/admin.rs` | driver admin tests *(env)* |
| Maintenance tools | ✅ | `Dialect::maintenance_ops`, `tabs/dbtools.rs` | `dbiji-app::tabs_smoke` |
| PostgreSQL debugger | ✅ | `PgSession::debug_routine` (pldebugger), `DebuggerTab` | `dbiji-driver-postgres` debugger test *(env, pldebugger)* |
| Server health dashboard | ✅ | `DashboardTab` | `dbiji-app::tabs_smoke` |

## Schema and DDL

| Feature | Status | Where | Tests |
| --- | --- | --- | --- |
| Create tables, columns, indexes, constraints, foreign keys, triggers | ✅ | `tabs/table_designer.rs`, `DdlGenerator` | `dbiji-app::tabs_smoke`, driver DDL round trips |
| Partitions | 🟡 | shown in the object editor; created via SQL (the designer does not edit partition schemes) | driver metadata tests |
| DDL generation | ✅ | `Session::object_ddl`, `DdlGenerator` | driver DDL round-trip tests |
| Sample database | ✅ | *Help → Create sample database* (`sample.rs`) | `dbiji-app::sample::tests` |

## Interface

| Feature | Status | Where | Tests |
| --- | --- | --- | --- |
| Themes (light, dark, system), high contrast | ✅ | `theme.rs` | `dbiji-app::tabs_smoke` |
| Interface language (English, Indonesian) | ✅ | `dbiji-core::i18n`, `locales/id.tsv` | `dbiji-app::i18n_coverage`, `i18n_labels` |
| Keyboard shortcuts (customisable, DBiji / DBeaver presets) | ✅ | `dbiji-config::keymap`, `keys.rs` | `dbiji-config::keymap` tests, `dbiji-app::keys::tests` |
| Toolbar customisation | ✅ | settings → Toolbar | `dbiji-config::settings` tests |
| Accessibility | 🟡 | AccessKit tree (egui), editor text runs, keyboard navigation everywhere; screen reader support per grid cell is not implemented (one node per grid) | `dbiji-editor` AccessKit test |

## AI assistant

| Feature | Status | Where | Tests |
| --- | --- | --- | --- |
| Chat, query suggestions, explanations, error fixes, object descriptions | ✅ | `dbiji-ai`, `tabs/ai.rs` | `dbiji-ai` tests (mock HTTP server), `dbiji-app::tabs_smoke` (prompt assembly) |
| Several model providers | ✅ | Anthropic, OpenAI-compatible servers | `dbiji-ai` tests |

## Databases (PRD §7)

| Database | Status | Tests |
| --- | --- | --- |
| PostgreSQL | ✅ | `dbiji-driver-postgres` (66 unit, 23 integration) *(env)* |
| MySQL, MariaDB | ✅ | `dbiji-driver-mysql` (67 unit, 34 integration × 2 servers) *(env)* |
| SQLite | ✅ | `dbiji-driver-sqlite` (87) |
| SQL Server | ✅ | `dbiji-driver-mssql` (39 unit, 6 integration) *(env)* |
| ClickHouse | ✅ | `dbiji-driver-clickhouse` (54 unit, 5 integration) *(env)* |
| DuckDB | ✅ | `dbiji-driver-duckdb` (43), optional `duckdb` feature |
| CockroachDB | ✅ | PostgreSQL driver with CockroachDB catalog adjustments; `dbiji-app` end-to-end suites (execution, cancel, schema compare gate, data editing, editor) run against CockroachDB *(env `DBIJI_TEST_COCKROACH`)* |
| Oracle | ⛔ | Only driver needs Oracle's native client library (OCI), excluded by the full-Rust rule (PRD §7) |
| Db2, Firebird, H2, Trino | ⛔ | No usable pure-Rust network driver yet (PRD §7: "only if a viable Rust driver exists") |

## Performance targets (PRD §2)

Measured with `dbiji --bench all` on the release build (macOS, Apple M1 Pro):

| Target | Result |
| --- | --- |
| Binary < 40 MB | 37.0 MB (without DuckDB) |
| Cold start < 1 s | 105 ms to the first frame |
| Idle memory < 120 MB | 69 MB |
| 3 connections + 5 result tabs < 250 MB | 83 MB (20 000 rows fetched per tab) |
| 1 M rows scroll, < 16 ms/frame | p95 0.5 ms (grid benchmark), 1 M × 100 columns: avg < 3 ms, worst 11.7 ms |
| Editor typing at 50 000 lines | p95 0.4 ms |
