# Changelog

All notable changes to DBiji. The newest version comes first; DBiji shows the
notes of a new version once after it was updated (Help → What's new).

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
