# DBeaver Community features not yet in DBiji (SQL only)

Source: DBeaver Community `a9c14c1` (2026-10-18), 160 plugins, ≈1 M lines of
Java. The inventory was taken from every plugin's `plugin.xml` (commands,
views, editors, preference pages, tools, tasks, dashboards, navigator object
folders) and the preference message bundles, then compared with DBiji's UI
strings and code. NoSQL plugins other than the MongoDB and Redis drivers
DBiji ships (see PARITY.md), databases without a pure-Rust driver
(Oracle, Db2, …, see PARITY.md) and Eclipse/Java specifics (JDBC driver
manager, Maven/classpath, workspace resources, PRO upsell, usage statistics)
are out of scope.

Status: ☐ open · ☑ done (with the test that covers it).

## 1. Navigator: server-level objects

| Database | Missing folders |
| --- | --- |
| PostgreSQL | Roles (members, grants), Tablespaces, Settings (`pg_settings`), Foreign data wrappers / servers / user mappings, Event triggers, Languages, Collations, Encodings, Access methods, Operator classes / families, Available extensions; per table: Policies (RLS), Rules |
| MySQL / MariaDB | Users (grants), System info (session / global status and variables), Engines, Charsets, Collations, Plugins |
| SQL Server | Logins, Database triggers, External tables, Extended properties |

- ☑ Generic catalog folders (`dbiji_core::catalog`: list query, DDL queries, sub-folders) for PostgreSQL, CockroachDB, MySQL, MariaDB, ClickHouse, SQLite, DuckDB and SQL Server — `dbiji-app::catalog_e2e::every_catalog_folder_runs` runs every query, DDL and sub-folder against each server (SQL Server: no test server yet)
- ☑ Navigator groups Security / Storage / Administer / System Info, database and table level folders, "View as table", refresh — `dbiji-app::ui_full::catalog_folders_in_the_navigator`
- ☑ Object tab with properties, DDL (copy / open in editor) and sub-objects (e.g. role members) — same test

## 2. Navigator behaviour

- ☑ Hide an object in the navigator (connection filter) and clear the filter of an object type
- ☑ Link with editor (the active tab's table or view is selected in the navigator)
- ☑ Connection order: move to the top / up / down / to the bottom (within its folder)
- ☑ Disconnect others
- ☑ Column › Create index… / Add unique constraint… (DDL in an editor)
- ☑ Open Database Object (⇧⌘D, the command palette lists tables, views and routines)
- ☑ Multiple projects: create, switch the active project (Project panel); scripts, diagrams and bookmarks follow
  — `dbiji-app::ui_full::navigator_behaviour_from_dbeaver`, `dbiji-config::project::tests::active_project`

## 3. SQL editor

- ☑ Code folding (multi-line statements and block comments): gutter markers, toggle at the cursor, collapse / expand all; a fold opens when the cursor enters it — `dbiji-editor::headless::folding_and_occurrences`, `dbiji-editor::fold` unit test, `dbiji-app::ui_full::editor_commands_from_dbeaver`
- ☑ Mark occurrences of the word under the cursor (visible lines, whole words; Settings → SQL editor)
- ☑ Open declaration: Cmd+click / F3 on a table or view opens it
- ☑ Next / previous statement (⌘⌥↓ / ⌘⌥↑)
- ☑ Morph selection to a delimited list; trim trailing spaces
- ☑ Copy the current statement; count the rows of the current query
- ☑ Execute stored procedure: navigator › routine › Execute… opens a CALL / SELECT / EXEC script with `:name` parameters (OUT arguments declared) — `dbiji-sql::generate::routine_tests`, `dbiji-app::ui_full::execute_routine_script`
- ☑ Evaluate expression (`SELECT <selection>`)
- ☑ Save / load an execution plan (JSON)
- ☑ Search the selection on the web
- ☑ Toggle the results layout (below / right) and the results panel
  — `dbiji-app::ui_full::editor_commands_from_dbeaver`, `dbiji-app::textops` unit tests
- ☑ Formatter choices: default, compact, external command; format only the statement under the cursor — `dbiji-app::textops::tests::external_formatter`, `dbiji-app::ui_full::editor_commands_from_dbeaver`

## 4. Data editor (result grid)

- ☑ Add row before the current one (Shift+Insert); copy value from the row above / below (⌘⌥↑ / ⌘⌥↓)
- ☑ Generate UUID into the selected cells
- ☑ Column widths: fit to screen / fit to values; hide columns that have no data
- ☑ Move column left / right (⌘⌥← / ⌘⌥→)
- ☑ Zoom the grid in / out (saved with the layout)
- ☑ Row highlighting by value (six colours, saved with the layout)
  — `dbiji-grid::grid::tests::insert_before_copy_adjacent_uuid_and_columns`, `…::fit_hide_empty_highlight_and_zoom`, menu sweep in `dbiji-app::ui_full`
- ☑ Save the current filter / order as the table's default and reset it (project virtual model)
  — `dbiji-app::data_edit_e2e::default_filter_per_table`
- ☑ Toggle the sort order of a column: click the header (ascending → descending → off)

## 5. Workspace and layout

- ☑ Properties panel for the selected navigator object (View › Properties panel or the navigator toolbar): connection, database, schema, object (type, comment, estimated rows, columns) and catalog object properties
- ☑ Error log view (Help › Error log): errors, warnings and info of the session, level filter, search, copy; also written to `logs/dbiji.log` (rotated at 2 MB, mode 0600) — `dbiji-app::applog::tests::buffer_and_file`
- ☑ Pending transactions (Database › Pending transactions…): every session with an open transaction, commit / roll back one or all; quitting with open transactions asks first
- ☑ Tip of the day (on start of an existing installation, can be turned off; Help › Tip of the day)
- ☑ Reset settings (Settings › Reset all settings…; the language and the update choices are kept)
- ☑ Collect diagnostic info (Help): zip with the log, settings and system info in ~/Downloads; no connection names, hosts, users, databases, file paths or properties, URL credentials masked, home directory as `~` — `dbiji-app::diagnostics::tests::masks_and_collects_without_identifying_data`
  — `dbiji-app::ui_full::workspace_from_dbeaver`

## 6. Database tools and dashboards

- ☑ PostgreSQL global backup (`pg_dumpall`): all databases, or only roles and tablespaces (Backup › Scope) — `dbiji-driver-postgres::tools::tests::dumpall`, `dbiji-driver-postgres::integration::backup_and_restore_with_local_tools`, `dbiji-app::tabs::dbtools::backup::tests::forms_roundtrip`
- ☑ Enable / disable triggers (PostgreSQL: user or all triggers of a table, one trigger; SQL Server) from Maintenance — `dbiji-driver-postgres::dialect::tests::trigger_maintenance`, `dbiji-driver-mssql::dialect` tests
- ☑ Foreign data wrapper configurator: Tools › Foreign data wrapper… generates CREATE EXTENSION / SERVER / USER MAPPING / IMPORT FOREIGN SCHEMA from another connection (postgres_fdw, mysql_fdw, tds_fdw, sqlite_fdw, clickhouse_fdw, duckdb_fdw); the password is the `:password` parameter, never written to the script — `dbiji-app::tabs::dbtools::fdw::tests::scripts`, `dbiji-app::fdw_e2e::postgres_fdw_script_imports_a_schema`
- ☑ Database-specific dashboards: PostgreSQL (transactions per second, sessions, block IO — already present), MySQL InnoDB memory and key efficiency (cache hit ratio) added — `dbiji-driver-mysql::admin::tests::metric_mapping`
