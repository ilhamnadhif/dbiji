# Changelog

All notable changes to DBiji. The newest version comes first; DBiji shows the
notes of a new version once after it was updated (Help → What's new).

## 0.3.6 — 2026-10-20

### Changed
- Record view of one row: the same table as rows side by side — column names down the left (the type on hover, key columns pinned), the value column with a draggable border (double-click fits it), a click picks a value (⌘C copies it), a double-click edits it, booleans have a check box; row navigation and Hide empty stay above it.
- Rows side by side: the "Differences only" option and the difference marks were removed.

## 0.3.5 — 2026-10-20

### New
- Filter bar: the WHERE and ORDER BY boxes suggest the result's column names (and AND, OR, LIKE, IS NULL … / ASC, DESC) for the word being typed; ↑ ↓ to pick, Tab or Enter to take one, Escape to close. Column names are quoted when the database needs it.

### Fixed
- SQL editor: an editor opened on a database in the navigator (PostgreSQL) suggested only keywords after FROM — its schema snapshot had no schema; it now uses the database's current schema (partitioned tables included, partitions not), reads another database of the server through its own session, and no longer mixes in tables of other databases. The schema button shows the schema in use.
- macOS: a crash on MacBooks with a Touch Bar (an AppKit exception in the Touch Bar's responder tracking); DBiji now keeps the Touch Bar machinery away (it has no Touch Bar items).

## 0.3.4 — 2026-10-20

### Changed
- Record view: no type column any more (more room for the values); the type shows when the pointer is on a column name.
- Rows side by side: the columns whose values differ between the selected rows are marked (an amber bar and background, the name in full colour) and counted; **Differences only** shows just those.
- Record view: **Hide empty** hides the NULL and empty columns; key columns stay at the top while the others scroll; both choices (and Differences only) are remembered.
- Command palette: full-width rows with coloured icons and each command's category and shortcut on the right; no more duplicate commands; a word typed in one piece ranks first ("set" → Settings).
- Wizards in pop-ups (export, import, backup …): the title bar names the wizard and the connection (no second heading), and the main action (Run export, Start backup, Generate …) is a blue button; so is "Save and connect" in the connection dialog.
- Navigator: long names and connection details are cut with "…" at the panel's edge.
- Connection dialog: the database types in their colours; Settings: coloured section icons, and the update check text says "at start and every few hours".
- ER diagram: one toolbar (the layout, edit mode and export buttons were shown twice).
- Primary key columns in the properties' column list have a gold key; the column lines of property tables end at the last row.
- Navigator menus show the keys: Open ↩, Copy name ⌘C, Refresh F5, Drop… ⌘⌫ (new: ⌘C copies the selected object's name, ⌘⌫ / Delete opens the drop dialog), New SQL editor ⌘].

### New
- Connection folders: New folder (navigator toolbar and folder menu), Rename folder…, Delete folder… (its connections and subfolders move up; no connection is deleted), and Move to folder in a connection's menu.

### Fixed
- SQL editor: the completion popup no longer stays open after running a statement.
- PostgreSQL: the data of a table in another database of the server (shown with "Show all databases") failed with "relation … does not exist"; the data tab now connects to that database.

## 0.3.3 — 2026-10-20

### Changed
- Coloured icons, DBeaver-style, so things can be told apart at a glance: in the navigator solid orange folders, blue tables, teal views, gold keys and indexes, magenta functions, and connections in their database's colour (PostgreSQL blue, MySQL teal, MariaDB brown …) with a green / orange / red dot for connected / connecting / failed; in menus, buttons, toolbars and tab titles green for new and run, red for delete and stop, gold for edit, blue for refresh, teal for export and import. One palette readable in the light and the dark theme.
- Primary buttons (New connection, Install update, Restart now) are blue in the dark theme too.

## 0.3.2 — 2026-10-20

### Changed
- Export, import, data transfer, mock data, backup / restore, maintenance, change password and the foreign data wrapper configurator open in a pop-up window over the tabs instead of a new tab (movable and resizable; the close button cancels a running job as before).

## 0.3.1 — 2026-10-20

### New
- macOS application menu like other Mac apps: About DBiji, **Check for Updates…**, Settings… (⌘,), Services, Hide DBiji / Hide Others / Show All, Quit DBiji (⌘Q, asks about unsaved scripts first).
- One-line installer: `curl -fsSL https://raw.githubusercontent.com/ilhamnadhif/dbiji/master/scripts/install.sh | bash` installs or updates DBiji in /Applications without the Gatekeeper prompt.

## 0.3.0 — 2026-10-20

### New
- Navigator: server-level objects like DBeaver's — roles, tablespaces, settings, foreign data wrappers, event triggers, extensions (PostgreSQL), users, status and variables, engines, charsets, plugins (MySQL / MariaDB), logins and configuration (SQL Server), and more — grouped under Security, Storage, Administer and System Info, each with properties, DDL and sub-objects.
- Navigator: hide an object, link with the editor, move connections up / down, disconnect the other connections, create an index or a unique constraint from a column, execute a stored procedure, Open Database Object (⌘⇧D), several projects.
- Properties panel for the selected navigator object (View → Properties panel).
- SQL editor: code folding, mark occurrences, open declaration (⌘ click / F3), next / previous statement, copy the statement, count its rows, evaluate an expression, morph to a delimited list, trim trailing spaces, search on the web, save / load execution plans, formatter choices (compact, external command, current statement only).
- Data editor: add a row before the current one, copy a value from the row above / below, generate UUIDs, fit columns, hide empty columns, move columns, zoom, highlight rows by value, save the filter and order as the table default.
- Database tools: PostgreSQL backup of all databases or only roles and tablespaces (pg_dumpall), enable / disable triggers, a foreign data wrapper configurator, InnoDB memory and key cache charts in the MySQL dashboard.
- Software update (macOS): with automatic checks on, DBiji looks for a new release at every start and every 6 hours; a new version shows its notes and installs in place (download, signature check, quit, swap, reopen — an administrator password only when needed), or lights up an Update button in the toolbar. Help → Check for updates… does the same on demand.
- Pending transactions (Database menu): every connection with uncommitted changes, commit or roll back one or all; quitting with open transactions asks first.
- Help → Error log, Tip of the day, Collect diagnostic info (a zip without passwords, host names or other connection details); Settings → Reset all settings.
- Clearer text and calmer colours: the Inter typeface, near-black (light) / near-white (dark) text instead of thin grey text, new light and dark themes.
- Column menu (▾ in every column header): order ascending / descending, disable the order, filter by the current cell's value, write a custom condition, save or reset the default filter, remove all filters and orderings.
- The result toolbar is grouped (refresh · editing · export · view · panels) with the row count and fetch buttons on the right; messages such as "The result has no geometry column" appear there instead of over the buttons.
- Interface polish: a new start page (start actions with their shortcuts, recent connections and scripts); text fields with a visible border in dialogs; a main toolbar with global actions only (new connection and editor, query history, pending transactions, tasks, command palette; running, transactions and the active connection stay in the SQL editor's toolbar, green run and red stop); the result filter bar on one line with a Server / Client switch; readable property tables; shorter database file paths in the navigator; notifications no longer cover the row count.
- Record view: booleans are check boxes (a click toggles them), never a text field.
- Rows side by side: drag a column border in the header to resize it (also the column-name column), double-click the border to fit the longest value.
- Tab with several rows selected shows them side by side (one column per row, like DBeaver); ⌘C copies the picked value.
- The app icon has transparent corners (no white square in the Dock and Finder).
- Record view redesigned: column names with key / link icons and types, values styled by kind (NULL, true / false, JSON), the selected field highlighted, a copy button per value, a row number to jump to.
- Tab in the result grid shows the selected row as a record (column → value, long values wrapped) like DBeaver; Tab goes back, ← / → show the previous / next row.
- New schema… is on database and schema nodes (a schema belongs to a database), not on the connection of servers with several databases; the start page lists recent connections on two lines (name, then the address).
- Production connections ask before statements that change data or schema only; SELECT, EXPLAIN and sorting or filtering a table run without a question (dangerous statements still always ask).
- macOS Keychain: all DBiji passwords are kept in one Keychain entry, read once per launch, so the Keychain asks at most once (not once per password and connection); existing entries move into it automatically.
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
