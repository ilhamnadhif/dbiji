# Changelog

All notable changes to DBiji. The newest version comes first; DBiji shows the
notes of a new version once after it was updated (Help → What's new).

## 0.3.22 — 2026-10-22

### Fixed
- PostgreSQL 11: tables opened read-only ("Read-only" in the result toolbar) and column properties failed, because the column definitions were read with a catalog column of PostgreSQL 12; the Collations folder failed the same way. PostgreSQL 11 is supported again and tested.

### Changed
- Saving a connection that is connected reconnects it when a setting its sessions use changed (host, user, password, SSL, SSH, proxy, read-only, auto-commit, initialization SQL, driver properties) instead of only asking for a reconnect; uncommitted transactions are confirmed first. A new name, colour, connection type or navigator setting applies at once, without a reconnect.
- The start page is no longer a tab: it fills the editor area whenever no tab is open.
- The record view is as compact as the grid (Settings → Results → Row height) and shows each column's type icon, as in DBeaver.

## 0.3.21 — 2026-10-22

### Added
- Record view: several values can be selected and copied together. Click a value, ⇧-click for a block, ⌘-click to add or remove one, click a column name to select it in every row shown, ⌘A for all; ⌘C copies them laid out as shown, and the context menu copies them with the column names.

### Changed
- Result grids show more rows: rows are more compact by default (Settings → Results → Row height: Compact or Normal).
- Columns fit their header and their first values when a result opens, as in DBeaver (timestamps and e-mail addresses are no longer cut, short columns take less room); widths you set yourself are kept.
- Column headers show a small icon of the column's type (number, text, date, JSON …); hovering a header shows its database type.
- Result toolbar: Refresh has a label, Save and Cancel form a group of their own next to it with their labels, and the status shows when the rows were fetched; after a refresh it shows the new run's time.
- The window title names the active tab ("DBiji - orders"), so DBiji windows can be told apart in ⌘-Tab and Mission Control.

## 0.3.20 — 2026-10-22

### Fixed
- A date or time format that does not fit the value (`%H` in the date format, `%z` in the timestamp format) made DBiji crash every time a table was opened; such formats now fall back to the standard ones.
- An open cell editor wrote into another row when the filter was cleared or the data refreshed while typing; it now stays with its row, and auto-refresh waits while a cell is being edited.
- `WITH … (…)` or `INSERT INTO t (a, b)` followed by a blank line was split into two statements, so the wrong statement could run.
- SQL Server: `UPDATE` or `DELETE` without WHERE ran without the confirmation when another statement followed on the next line.
- Closing an editor with an uncommitted transaction asks first ("Roll back and close"); quitting warns about exports, imports and migrations still running, and closes the connections cleanly.
- Refresh and auto-refresh no longer run a statement that changes data again (INSERT / UPDATE … RETURNING, CALL, EXEC) without asking.
- A connected connection cannot be deleted; switching an editor's connection or database is refused while a statement runs, a result has unsaved changes or its transaction is open.
- Copying a value and a NULL and pasting them lost the NULL row; a block pasted onto a selected range starts at its top left; copies with gaps (⌘-click) are refused instead of pasting empty values.
- A header sort clicked while a filter was still being computed dropped the filter; the Record, Text, JSON and XML views now show filtered rows too.
- Grouping merged different integers above 2^53; Calc no longer reports "only the first cells were analysed" for overlapping selections.
- Scheduled tasks also registered with the system scheduler ran twice while DBiji was open.
- Migrations: MySQL TIMESTAMP values into PostgreSQL timestamptz were shifted by the time zone difference; text primary keys into MySQL or SQL Server failed (they get varchar(255) / nvarchar(450)); MySQL BIT and geometry values were written as strings; ids into a PostgreSQL GENERATED ALWAYS identity were refused.
- ClickHouse: imports, migrations and mock data failed at the start ("does not support transactions"); binary String values (`unhex`, digests) are kept as bytes instead of being garbled.
- SQL Server: NULL into varbinary or image columns was refused; rows with a `datetime` key were often "not found" when saved.
- The navigator no longer shows a horizontal scrollbar all the time; Run in the query builder runs the query; restored scripts keep the connection they had.

### Known limits
- SQL Server `money` values above about 900 billion lose their last decimal digits (the client library reads them as floating point).

## 0.3.19 — 2026-10-21

### Fixed
- Shift+click in the Calc table could crash DBiji after the selected cells changed.
- Saving grid edits failed for no reason on SQL Server with SET NOCOUNT ON or a trigger ("the row was not found", "would change 2 rows"), and on MySQL when a value was saved unchanged: the row count check now applies only where the count can be trusted.
- One-column CSV files lost a NULL in their last row (DBiji's own export writes NULL as an empty line).
- A JSON file holding one object with an array of objects (`{"id": 1, "orders": [...]}`) was read as a wrapper of those objects; only keys such as items, data, results or records are taken as a wrapper now.
- MySQL before 8.0.19 and MariaDB: times with a time zone before 1970 or after 2038 were not converted to the session's zone.
- Migrations to DuckDB no longer draw the sequence for hours when ids are huge; above a million draws a message says how to set it.
- `SELECT name, * FROM t` is editable again; only a real self-join (the table twice in FROM / JOIN) is read-only.
- ClickHouse: adding key columns together with other new columns placed after them works.
- A failed fetch shows its bar again after a retry fails the same way; rows no longer appear twice after "Fetch all" and Count; the query builder of another database lists that database's tables.
- A SQL editor leaves "connection lost" by itself once the connection is back; closing it while a save preview waits in another result shows that result; a ROLLBACK typed in the editor reloads results saved inside the transaction.
- Cancelling the quit after "Restart now" no longer installs the update at a later quit.
- Fewer background tasks: one idle-cursor watcher per session instead of one per statement; CockroachDB results are not asked about generated columns.

## 0.3.18 — 2026-10-21

### Fixed
- A table's data turned read-only ("the result columns cannot be traced back to a table") after a refresh, a server-side filter or sort, or a while idle: the query ran again without looking up the columns' tables (since 0.3.13). Reopen a tab that shows it, or update.

## 0.3.17 — 2026-10-21

### Changed
- macOS: File, Database, Tools, View and Help are in the system's menu bar, as in DBeaver, with a Window menu (Minimize, Zoom, Full Screen) and the menu search of Help; the menu row inside the window is gone. New database / New schema are on only when the selected connection can create one, and the check marks follow the properties panel and the theme. Windows and Linux keep the in-window menu.
- Copying from the Calc panel: a copy button at the end of the hovered row ("Copied" confirms it), a click selects a row (Shift / ⌘ more) and ⌘C copies the selected values, a double click copies too, and the right-click menu copies a value, the name and value, or the whole table (tab separated). Values are copied plain: 1234, not "1,234"; the mode without its count.

## 0.3.16 — 2026-10-21

### Changed
- The Calc panel is a plain table (function, value) like DBeaver's, drawn like the result grid, with Mode (the most frequent value and how often it occurs) added; a click on a row copies its value.
- Panel tabs never break into letters: when they do not fit, the other tabs show their icon only (the name on hover). The panels are no longer framed; the "Panels" button of the result toolbar is gone (the strip at the right edge opens and closes them).
- Value panel: Apply, Revert and Set NULL sit right below the text, not at the bottom of the panel.
- Notifications appear centred above the status bar instead of over the side panels' buttons at the right.
- With pending changes the result toolbar shows "Save (n)" in the selection colour and "Discard" (Save was glaring white in the dark theme).
- A new column is wide enough for its whole name ("category_id" was cut to "category…"); widths you set are kept.
- The navigator's toolbar has plain icons for the frequent actions; New folder, the properties panel and Link with editor are in "⋯".
- A failed fetch shows its error in a bar above the result (selectable, Copy, ×) instead of a pop-up that stops the work.
- Server / Client in the filter bar each say what they do on hover.
- A running SQL editor shows, above its results, which statement runs, for how long, and a Cancel button.
- Tabs with the same title tell what separates them: "product · prod" and "product · dev", or schema@database within one connection.

## 0.3.15 — 2026-10-21

### Changed
- The side panels of a result (Value, Metadata, Grouping, Calc, References) have a strip of buttons at the right edge, as in DBeaver: one coloured button per panel opens it, or closes it while it is shown, and the top one shows or hides the panels. A dot marks a panel that is open behind another one.
- Each panel's tab has a × to close it, and » hides the whole panel area (the open panels come back with it). The panels sit in a framed area.
- The Calc panel shows Count, Distinct and NULLs, and Sum, Average and Median, as tiles with large numbers, and the range (min, max, lengths) as a card; every value copies on a click.

## 0.3.14 — 2026-10-21

### Changed
- A smaller app: egui's Ubuntu font, which DBiji does not use, is no longer bundled (about 360 KB). The few symbols the code font lacks are drawn with Inter, like the rest of the UI.

## 0.3.13 — 2026-10-21

### Fixed
- The connection's dot turns red when the connection is lost: also when the server closed it while DBiji was idle (idle timeout, restart), and when a data tab or SQL editor found its own session closed (the connection is checked then). Double-click a red connection to reconnect.
- A data tab or SQL editor result whose session was lost saves and loads through a new session instead of failing with "connection closed" until the tab is opened again; a session of another database is opened again too.
- New rows show the values the database gave them right after **Save**: a generated id (serial, identity, auto increment) and column defaults (they stayed NULL until a refresh). PostgreSQL, SQLite, DuckDB and MariaDB return the row (`RETURNING`); MySQL and SQL Server read it back by its key.
- Security: certificate checks of TLS connections no longer accept distrusted intermediate certificates (rustls-webpki 0.103.16, webpki-roots 1.0.10).
- Uncommitted transactions are no longer lost without asking: Disconnect, Reconnect and Disconnect all ask first (or open the pending transactions), closing a SQL editor whose own session has uncommitted changes asks too, and Drop… refuses while a session of the connection has uncommitted writes (it commits its DDL in manual commit mode).
- A SQL editor whose session was lost tells when uncommitted changes were rolled back, gets its schema and transaction mode back on the new session, and shows "reconnect" instead of retrying for ever when the connection itself was lost. A failed COMMIT when switching back to auto-commit is reported (PostgreSQL also stopped showing the transaction as open).
- Closing a tab with unsaved changes asks first (data tabs, object editors); reloading, filtering, sorting or navigating a result with pending changes asks before discarding them. Saving twice, editing or pasting while a save runs is no longer possible.
- Saving grid changes: an UPDATE / DELETE that matches no row or several rows fails instead of reporting success; in auto-commit a failed save is rolled back whole. A self-join result (`users a JOIN users b`) is read-only instead of editing the wrong row; a result whose table definition cannot be read says so instead of "no unique key".
- Scrolling a result after saving no longer skips rows; a result whose columns changed (ALTER, another statement) is editable again with its new columns; cancelled fetches leave the operations list; a server-side filter or sort of a query ending in a `-- comment` works.
- An open cursor not read for 5 minutes is closed (it held a snapshot and table locks: an ALTER of the table waited).
- Database nodes: New SQL editor, New schema, Backup / Restore and Query builder use that database instead of the default one; Refresh of a database reloads only it; the navigator shows tables created or dropped by a script in any schema; revealing an object picks the one of the right database.
- Connections that failed to connect stay red; two connection dialogs no longer share one window; applying a URL keeps SSH, colour, filters and the other settings; the update installer starts only once DBiji really quits.
- MySQL: dropping a partition runs `ALTER TABLE … DROP PARTITION` (it ran `DROP TABLE` with the partition's name) and its data opens as `t PARTITION (p)`; moving the primary key to a new column while dropping the old one works; imports split very wide VARCHAR tables (row size limit) by turning the longest columns into TEXT.
- SQL Server: an ALTER that would silently lose another table's foreign key (key removed or retyped) is refused before anything runs; queries with `OPTION (…)` / `FOR XML` are no longer wrapped into invalid SQL.
- ClickHouse: extending the sorting key adds its new columns in the same ALTER (ClickHouse requires it), and an unsupported key change fails before the other changes are applied.
- SQLite: in-memory databases shared by the sessions of a connection use the memdb VFS (no more "table is locked" between an editor and a data tab).
- SQL: `MERGE JOIN` hints and unrelated `*read_only*` settings no longer count as writes on read-only connections; turning a read-only setting on is no write; MySQL routines and triggers whose body is a single `IF … END IF` split correctly.
- Imports: JSON with metadata before the array (`{"total": 2, "items": [...]}`), blank lines at the end of one-column CSV files, and a failing SQL Server identity insert that left the session in manual commit.
- Shell commands before connecting: a helper left running in the background (`ssh -fN`, VPN) is no longer killed by a closed output pipe. Connection URLs with `#` in a password or in `{…}` values are read whole.
- Diagnostics replace connection names, hosts and users as whole words only.
- F5 with the navigator in use refreshes the navigator, not the active tab; ⌘W in a pop-up (Export, Backup …) closes the pop-up instead of the tab behind it, and no shortcut reaches the tabs while a question is open; Ctrl+Tab moves on in split layouts.
- An object editor shows the table as it is after the designer applied changes or a Refresh. ER "Edit table" opens the table designer.
- Rolling back a transaction reloads the results whose edits were saved inside it (they showed the rolled-back values). Typed text in the Value panel no longer goes to another row after a refresh, and Set NULL shows the NULL.
- "Save password" on an "Ask every time" connection stops asking. "Hide in navigator" no longer revives the old patterns of a filter that was switched off.
- PostgreSQL: a saved new row shows timestamptz in the session time zone, money with its symbol and tsvector / point / vector as text (not UTC or bytes); generated (`STORED`) columns are filled by the database when duplicating a row; casts no longer treat user types named like `region` or `price_range` as built-in; a collation typed as `myschema.mycoll` works.
- MySQL before 8.0.19 and MariaDB: times with a time zone are converted by the server with its daylight saving rules (they could be one hour off), in one query per statement.
- SQL Server: queries with `OPTION (MAXRECURSION n)` can be counted and paged.
- Migrations to DuckDB advance the auto-increment sequences past the copied ids. Imports detect a windows-1252 file whose first accent is cut by a read. A task run left "Running" before a reboot is closed even when its process number was reused.

### Changed
- The dark theme is a softer grey instead of near black, easier on the eyes over long sessions (panels, grid and editor backgrounds, borders and text adjusted together).
- The navigator rows are more compact: more of the tree fits in the sidebar.

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
