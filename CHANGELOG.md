# Changelog

All notable changes to DBiji. The newest version comes first; DBiji shows the
notes of a new version once after it was updated (Help → What's new).

## 0.3.13 — 2026-10-21

### Fixed
- The connection's dot turns red when the connection is lost: also when the server closed it while DBiji was idle (idle timeout, restart), and when a data tab or SQL editor found its own session closed (the connection is checked then). Double-click a red connection to reconnect.
- A data tab or SQL editor result whose session was lost saves and loads through a new session instead of failing with "connection closed" until the tab is opened again; a session of another database is opened again too.
- New rows show the values the database gave them right after **Save**: a generated id (serial, identity, auto increment) and column defaults (they stayed NULL until a refresh). PostgreSQL, SQLite, DuckDB and MariaDB return the row (`RETURNING`); MySQL and SQL Server read it back by its key.
- Security: certificate checks of TLS connections no longer accept distrusted intermediate certificates (rustls-webpki 0.103.16, webpki-roots 1.0.10).

## 0.3.12 — 2026-10-21

### Changed
- An in-memory database (`:memory:`, SQLite and DuckDB) is one per connection: a table made in the SQL editor is seen by the navigator and the data tabs (each session had an empty database of its own).
- Disconnect also closes the sessions of the connection's SQL editors and data tabs (they kept running with the old settings, transactions and locks); they open a new session for their next statement.
- Generated DDL follows the connected server: the MySQL / MariaDB version, CockroachDB, and SQLite's foreign key setting (a table rebuild no longer switches `foreign_keys` on).
- PostgreSQL: filter values and generated SQL no longer cast to enum, domain or extension types (`'happy'::mood` failed when the type's schema is not on the search path); built-in types keep their cast.
- MySQL: BIT and geometry cells are saved as `b'…'` and `ST_GeomFromText(…)` (the server refused the edits); a time with a time zone is written in the session's time zone instead of as UTC.
- DuckDB: changing a table with indexes drops and re-creates the indexes around the change (DuckDB refused it); a table that other tables reference with foreign keys cannot be changed in DuckDB, and the designer says so instead of failing half way.
- Import: new tables get roomier types (BIGINT, twice the longest text of the sample, 4 more digits for decimals); **Detect** reads binary values as base64 first in JSON and XML files (as the export writes them); a one-column CSV reads blank lines as NULL (as the export writes them); an accented character after the first 64 KB still selects windows-1252.
- Data compare matches a text key with a number or UUID key on the other side (`'1'` and `1`).
- A task that continues after errors is recorded as failed (and notifies) when a statement failed; the statements that worked stay applied.
- The table designer does not run DDL while its session has uncommitted changes of another tab (they would be committed with it).
- Settings: query history, the task scheduler and **Reduce motion** apply at once, not at the next start.
- Collect diagnostic info replaces connection names, hosts, users, databases and file names in the logs.

### Fixed
- **Data safety**: Drop… of an object of another database of the server (PostgreSQL, SQL Server with "Show all databases") dropped the object of the same name in the connection's database. Fetching more rows of `UPDATE … RETURNING`, `CALL` or `EXEC` ran the statement again. A failed Commit or Rollback in the SQL editor was not reported. Saving grid edits in manual commit mode left half of a failed batch applied (saving again repeated it). **Save** in the "Unsaved changes" prompt discarded the result edits.
- **Read-only connections**: T-SQL statements following another one without `;` (`SELECT 1 DELETE FROM t`), `SET … READ WRITE`, `default_transaction_read_only = off` and MySQL `/*! … */` comments (`INTO OUTFILE`) could write.
- **SQL editor**: choosing a schema after a database lost the database; a schema that cannot be used is reported; the error mark of a statement that is in the script twice; MySQL procedures with `IF (…) THEN`, `IF EXISTS` or `IF NOT` were split; `@b INT` after a blank line of a T-SQL parameter list ended the statement; `RESET ALL` or `DEFAULT VALUES` before a blank line swallowed the next statement; `'%'||?||'%'` placeholders (SQLite); completion of quoted names, `${var}`, `:name` and `@var`; formatting changed the case of ClickHouse columns such as `database` or `rows`; crashes with some non-Latin text (spell check, generated routine calls, debugger arguments).
- **Grid**: the Text, JSON and XML views showed old rows after Refresh or Fetch all; a client sort or filter was lost on refresh; scrolling no longer loaded more rows after a refresh; opening and closing a NULL cell in the record view stored ''; empty input in array, interval, inet, enum … cells is NULL; typing in the value panel was overwritten while rows arrived; "contains" filters found nothing on SQLite and SQL Server; a copied cell with line breaks pasted into several rows, and copied NULLs pasted as ''; a row inserted above the first row moved after sorting; a group filter replaced the current filter; Generate SQL → INSERT wrote NULL for DEFAULT cells; an open cell editor could write into another record when a background sort finished; a table opened through a foreign key with the same column names could save into the previous table; sorting mixed numbers and text could crash.
- **Transactions**: PostgreSQL `SET TRANSACTION ISOLATION LEVEL` in manual commit mode; a failing EXPLAIN aborted the transaction; the chosen schema was lost after ROLLBACK; untyped `$1` parameters inside a transaction. MySQL: the statement timeout cancelled a result being scrolled; a failed `ROLLBACK TO SAVEPOINT` or `DROP TEMPORARY TABLE` showed the transaction as ended.
- **Other databases of a server**: Generate SQL, Execute routine, foreign key navigation, References and dictionaries read the connection's database; a database that cannot be opened showed "Loading…" forever (it shows its error now, Refresh tries again).
- **Data transfer**: migrations into SQL Server identity columns failed and PostgreSQL sequences stayed behind the copied ids (SQL Server identity columns work for imports and data compare scripts too); JSON Lines whose first record holds an array imported nothing; two tables migrated into one target table are refused; a parallel export reported "cancelled" instead of the error that stopped it; rows written in replace mode were overcounted; XLSX: blank rows above the data, and a warning for cells right of the columns found.
- **Schema compare**: foreign keys of a dropped table on a key that changes are dropped first; Apply re-ticked the excluded objects.
- **Drivers**: PostgreSQL collations like `"en_US.utf8"`, `REINDEX SCHEMA` in manual commit mode, the `sslmode` property for the client tools. MySQL: a new primary key on a column added in the same change, `RANGE COLUMNS … MAXVALUE`, metadata search under `NO_BACKSLASH_ESCAPES`, table statistics and internal queries with a non-UTF-8 connection character set, client tools through an SSH tunnel with Verify full. SQL Server: Count rows and paging of `WITH` queries, key changes of a table other tables reference, error positions after `GO`. ClickHouse: `USE` after a comment containing "use", the engine of a materialized view without `TO`, sorting key changes that ClickHouse cannot make fail instead of doing nothing. DuckDB: the sequence of a renamed table or column, `TIME '24:00:00'`, Checkpoint of a schema, TIME_NS results (an error instead of a closed connection). SQLite: Cancel of a statement waiting to start. A SQL Server or ClickHouse session could hang when an old result was dropped late.
- **Other**: Reset all settings did nothing; Terminate session could hit another session after an auto-refresh; a backup or maintenance closed while running stayed "Running"; Show rows of a data search used the connection chosen afterwards; Save as task saved twice on a double-click; closing a SQL editor on another database closed the session the navigator and other tabs share there. Tasks: SQL tasks with a database on PostgreSQL always failed, an import with a repeated id, Windows task ids, runs of a live `--run-task` marked interrupted. A password saved by another DBiji process could be lost; the connection dialog deleted a saved password it could not read (locked vault); double-quoted passwords stayed in the history; SQL Server JDBC URLs with `{…}` values; a before-connect command that starts a background helper timed out; external formatter output over 64 KB; the last lines of a client tool's error; `~` for paths next to the home folder; renaming a script by case only (macOS).

## 0.3.11 — 2026-10-20

### Changed
- Errors of a result open a pop-up, in full (selectable, scrolls when long) with **Copy**: a refused edit, a failed save or paste, a failed query (once; the ⚠ in the status bar shows it again). A table without a key explains why it is read-only and offers **Define virtual key…**.

### Fixed
- Updates always offer the newest release: Check for Updates… and the Update button ask GitHub again instead of offering a version found earlier (0.3.8 offered 0.3.9 although 0.3.10 was out). An update being installed is replaced by a newer one.
- A table of another database of the server (PostgreSQL, SQL Server) was read-only ("no primary or unique key") although it has a key: its keys are now read in its own database.

## 0.3.10 — 2026-10-20

### New
- The main toolbar shows where the focused tab works, as in DBeaver: its connection (logo, name, the connection type's colour) and `schema@database` (e.g. `public@zain_iq`; one name for MySQL or SQLite). A click shows it in the navigator. The tab's tooltip names them too, and the status bar now shows the tab's own database and schema (a table of another database included) instead of the connection's current ones.

## 0.3.9 — 2026-10-20

### Changed
- Light theme: the UI text is a little heavier (Inter Medium instead of Regular) and the secondary text (column names, details) is darker, so it reads more easily; the dark theme keeps Inter Regular. The font follows the theme, also when the theme follows the system.
- Grid messages (e.g. why a cell cannot be edited) appear once, in the status bar under the grid, instead of also over the data.

### Fixed
- A table without a primary key or unique constraint but with a unique index (`CREATE UNIQUE INDEX`) is editable: the index identifies the rows, as in DBeaver (NOT NULL columns preferred; partial and expression indexes are not used).

## 0.3.8 — 2026-10-20

### Changed
- Scroll bars are easy to see: the data grid's thumb stands out from its track in both themes (stronger while hovered or dragged), and the other scroll bars stay visible instead of appearing only on hover.
- Booleans are check boxes (☑ / ☐) in the grid and the record view, without the words; Settings → Result sets still offers true / false or 1 / 0. Copying a boolean still gives true / false.

## 0.3.7 — 2026-10-20

### Changed
- Connections show their database's logo — PostgreSQL's elephant, MySQL's dolphin, MariaDB's seal, SQLite's feather, CockroachDB, ClickHouse, DuckDB (from Simple Icons) — in its colour, in the navigator, the connection dialog and the start page; SQL Server keeps the database icon. A connection type's colour (e.g. production) now tints the connection's row, DBeaver-like.
- New screenshots in the README.

## 0.3.6 — 2026-10-20

### Changed
- Record view of one row: the same table as rows side by side — column names down the left (the type on hover, key columns pinned), the value column with a draggable border (double-click fits it), a click picks a value (⌘C copies it), a double-click edits it, booleans have a check box; row navigation and Hide empty stay above it.
- Rows side by side: the "Differences only" option and the difference marks were removed.
- Pop-ups close by themselves once their job succeeded, and a notification tells the result: export (with the file's name and **Show in Finder**), import, data transfer, mock data, backup (Show in Finder) / restore, maintenance without output to read, change password. A failure, a partial failure or warnings keep the pop-up open.

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
