# Changelog

All notable changes to DBiji. The newest version comes first; DBiji shows the
notes of a new version once after it was updated (Help → What's new).

## 0.3.0 — 2026-10-19

### New
- Navigator: server-level objects like DBeaver's — roles, tablespaces, settings, foreign data wrappers, event triggers, extensions (PostgreSQL), users, status and variables, engines, charsets, plugins (MySQL / MariaDB), logins and configuration (SQL Server), and more — grouped under Security, Storage, Administer and System Info, each with properties, DDL and sub-objects.
- Navigator: hide an object, link with the editor, move connections up / down, disconnect the other connections, create an index or a unique constraint from a column, execute a stored procedure, Open Database Object (⌘⇧D), several projects.
- Properties panel for the selected navigator object (View → Properties panel).
- SQL editor: code folding, mark occurrences, open declaration (⌘ click / F3), next / previous statement, copy the statement, count its rows, evaluate an expression, morph to a delimited list, trim trailing spaces, search on the web, save / load execution plans, formatter choices (compact, external command, current statement only).
- Data editor: add a row before the current one, copy a value from the row above / below, generate UUIDs, fit columns, hide empty columns, move columns, zoom, highlight rows by value, save the filter and order as the table default.
- Database tools: PostgreSQL backup of all databases or only roles and tablespaces (pg_dumpall), enable / disable triggers, a foreign data wrapper configurator, InnoDB memory and key cache charts in the MySQL dashboard.
- Pending transactions (Database menu): every connection with uncommitted changes, commit or roll back one or all; quitting with open transactions asks first.
- Help → Error log, Tip of the day, Collect diagnostic info (a zip without passwords, host names or other connection details); Settings → Reset all settings.
- Clearer text and calmer colours: the Inter typeface, near-black (light) / near-white (dark) text instead of thin grey text, new light and dark themes.
- Column menu (▾ in every column header): order ascending / descending, disable the order, filter by the current cell's value, write a custom condition, save or reset the default filter, remove all filters and orderings.
- The result toolbar is grouped (refresh · editing · export · view · panels) with the row count and fetch buttons on the right; messages such as "The result has no geometry column" appear there instead of over the buttons.
- Interface polish: a new start page (start actions with their shortcuts, recent connections and scripts); text fields with a visible border in dialogs; grouped main toolbar with green run and red stop buttons; the result filter bar on one line with a Server / Client switch; readable property tables; shorter database file paths in the navigator; notifications no longer cover the row count.
- Tab with several rows selected shows them side by side (one column per row, like DBeaver); ⌘C copies the picked value.
- The app icon has transparent corners (no white square in the Dock and Finder).
- Record view redesigned: column names with key / link icons and types, values styled by kind (NULL, true / false, JSON), the selected field highlighted, a copy button per value, a row number to jump to.
- Tab in the result grid shows the selected row as a record (column → value, long values wrapped) like DBeaver; Tab goes back, ← / → show the previous / next row.
- Fixed: the Create button of New database… could not be clicked (the dialog grew every frame); New database… is also on database nodes, and the new database appears in the navigator ("Show all databases" is turned on when only the connection's database was listed).
- New database… and New schema… (connection menu, database menu, Database menu, command palette): name, owner / encoding / template (PostgreSQL) or character set / collation (MySQL, MariaDB, SQL Server), the statement shown before it runs.
- Main toolbar buttons that cannot work in the current tab are greyed out and say why (e.g. Run needs a SQL editor, Commit needs manual commit).
- Exported files are named after the database, schema and table (or partition) as the database names them, then the date and time, e.g. `shop_public_orders_20261019_101500.csv` (MySQL: `shop_orders_…`, SQLite: `file_orders_…`).

## 0.2.0 — 2026-10-18

### New
- Switch between the light and the dark theme any time: View → Theme, the sun / moon button in the status bar, or "Toggle Light / Dark Theme" in the command palette (a shortcut can be set in Settings → Keyboard shortcuts).
- "What's new" after an update, and an optional daily check for new releases with Download, Skip this version and Remind me later (Help → Check for updates…).

### Fixed
- PostgreSQL with manual commit (production connections): changing the ORDER BY or filter of a large table no longer fails with "current transaction is aborted". Each statement now runs in its own savepoint, so a failed or cancelled statement no longer aborts the whole transaction.
- Enter now runs the query in the parameter dialog, and confirms the connection password and task folder prompts.
- The command palette always opens empty and closes when you click elsewhere.
- MySQL: closing a large result stops the query on the server instead of reading every row; query timeouts are applied while rows stream.
- MariaDB backup and restore work with the MySQL client tools when the MariaDB ones are not installed.
- SQLite backup offers only what SQLite supports (SQL dump or database file copy); "Change password" is hidden for databases without passwords.
- The connection dialog fits on small screens; clicking anywhere on a column header sorts it.

## 0.1.0 — 2026-10-17

First release: a native SQL client for PostgreSQL, CockroachDB, MySQL, MariaDB, SQLite, SQL Server, ClickHouse and DuckDB with a SQL editor, data grid, navigator, data transfer, compare, tasks, backup / restore, administration tools and an optional AI assistant.
