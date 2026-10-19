# DBeaver Community features not yet in DBiji (SQL only)

Source: DBeaver Community `a9c14c1` (2026-10-18), 160 plugins, ≈1 M lines of
Java. The inventory was taken from every plugin's `plugin.xml` (commands,
views, editors, preference pages, tools, tasks, dashboards, navigator object
folders) and the preference message bundles, then compared with DBiji's UI
strings and code. NoSQL plugins, databases without a pure-Rust driver
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

- ☐ Hide selected object(s) / show only selected / clear navigator filter per node
- ☐ Link with editor (select the active editor's object in the navigator)
- ☐ Connection order: move up / down / top / bottom
- ☐ Disconnect others
- ☐ New index / constraint from selected columns
- ☐ Open database object… (quick open dialog by name, all connections)
- ☐ Multiple projects (create, switch active project)

## 3. SQL editor

- ☑ Code folding (multi-line statements and block comments): gutter markers, toggle at the cursor, collapse / expand all; a fold opens when the cursor enters it — `dbiji-editor::headless::folding_and_occurrences`, `dbiji-editor::fold` unit test, `dbiji-app::ui_full::editor_commands_from_dbeaver`
- ☑ Mark occurrences of the word under the cursor (visible lines, whole words; Settings → SQL editor)
- ☑ Open declaration: Cmd+click / F3 on a table or view opens it
- ☑ Next / previous statement (⌘⌥↓ / ⌘⌥↑)
- ☑ Morph selection to a delimited list; trim trailing spaces
- ☑ Copy the current statement; count the rows of the current query
- ☐ Execute stored procedure (dialog for the arguments, generates CALL / SELECT)
- ☑ Evaluate expression (`SELECT <selection>`)
- ☐ Save / load an execution plan
- ☑ Search the selection on the web
- ☑ Toggle the results layout (below / right) and the results panel
  — `dbiji-app::ui_full::editor_commands_from_dbeaver`, `dbiji-app::textops` unit tests
- ☐ Formatter choices: compact formatter, external formatter command

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

- ☐ Properties panel for the selected navigator object
- ☐ Error log view (application log)
- ☐ Pending transactions dialog (connections with uncommitted changes)
- ☐ Tip of the day
- ☐ Reset settings
- ☐ Collect diagnostic info (logs and settings without secrets, zipped)

## 6. Database tools and dashboards

- ☐ PostgreSQL global backup (`pg_dumpall`)
- ☐ Enable / disable triggers tool (PostgreSQL, SQL Server)
- ☐ PostgreSQL foreign data wrapper configurator
- ☐ Database-specific dashboards: PostgreSQL (transactions per second, sessions, block IO), MySQL (queries, traffic, InnoDB data / memory, key efficiency)
