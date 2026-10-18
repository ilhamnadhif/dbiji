//! Online backup with SQLite's backup API (`sqlite3_backup_*`).
//!
//! The copy is consistent even while other connections write to the source.
//! SQL alternative: `VACUUM INTO 'file'` (see [`vacuum_into_sql`]) writes a
//! compacted copy; the `sqlite3` shell offers `.backup` / `.dump`
//! (see [`crate::SqliteDriver`]'s `backup_command`).

use crate::error::SqliteResultExt;
use crate::session::SqliteSession;
use dbiji_core::{DbError, DbResult};
use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// Pages copied per backup step.
const PAGES_PER_STEP: i32 = 1024;

/// What to back up.
pub enum BackupSource<'a> {
    /// A database (`"main"`, `"temp"` or an attached name) of an open session.
    /// The copy runs on the session's connection thread.
    Session {
        /// The session whose connection is copied.
        session: &'a mut SqliteSession,
        /// Database to copy: `"main"`, `"temp"` or an attached name.
        schema: &'a str,
    },
    /// A database file, opened read-only for the copy.
    File(&'a Path),
}

impl<'a> From<&'a mut SqliteSession> for BackupSource<'a> {
    fn from(session: &'a mut SqliteSession) -> Self {
        BackupSource::Session {
            session,
            schema: "main",
        }
    }
}

impl<'a> From<&'a Path> for BackupSource<'a> {
    fn from(path: &'a Path) -> Self {
        BackupSource::File(path)
    }
}

impl<'a> From<&'a PathBuf> for BackupSource<'a> {
    fn from(path: &'a PathBuf) -> Self {
        BackupSource::File(path.as_path())
    }
}

/// Progress of a running backup, reported after every step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackupProgress {
    /// Pages still to copy.
    pub remaining: i64,
    /// Pages of the source database.
    pub total: i64,
}

/// Result of a finished backup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupReport {
    /// Pages copied.
    pub pages: i64,
    /// Size of the copy in bytes (`pages * page_size`).
    pub bytes: i64,
    /// Duration of the copy.
    pub elapsed: Duration,
}

/// Progress callback.
pub type ProgressFn = Arc<dyn Fn(BackupProgress) + Send + Sync>;

/// `VACUUM <schema> INTO '<file>'`: SQL that writes a compacted copy of the
/// database (fails when the destination exists).
pub fn vacuum_into_sql(schema: Option<&str>, dest: &Path) -> String {
    let target = crate::dialect::string_literal(&dest.to_string_lossy());
    match schema {
        Some(s) => format!("VACUUM {} INTO {target}", crate::dialect::quote_ident(s)),
        None => format!("VACUUM INTO {target}"),
    }
}

/// Copy a database to `dest` (created or overwritten) with the online backup
/// API: `online_backup(&mut session, path)` or `online_backup(path, dest)`.
pub async fn online_backup<'a>(
    source: impl Into<BackupSource<'a>>,
    dest: &Path,
) -> DbResult<BackupReport> {
    online_backup_with_progress(source, dest, None).await
}

/// [`online_backup`] with a progress callback. A session backup can be
/// aborted with the session's cancel handle (the destination is then left
/// unchanged).
pub async fn online_backup_with_progress<'a>(
    source: impl Into<BackupSource<'a>>,
    dest: &Path,
    progress: Option<ProgressFn>,
) -> DbResult<BackupReport> {
    let dest = dest.to_path_buf();
    match source.into() {
        BackupSource::Session { session, schema } => {
            let schema = schema.to_string();
            session
                .call(move |w| {
                    let busy = w.busy_timeout;
                    run_backup(
                        &w.conn,
                        &schema,
                        &dest,
                        progress,
                        Some(&w.shared.cancel_requested),
                        busy,
                    )
                })
                .await
        }
        BackupSource::File(path) => {
            let src = path.to_path_buf();
            let (tx, rx) = oneshot::channel();
            std::thread::Builder::new()
                .name("dbiji-sqlite-backup".into())
                .spawn(move || {
                    let result = Connection::open_with_flags(
                        &src,
                        OpenFlags::SQLITE_OPEN_READ_ONLY
                            | OpenFlags::SQLITE_OPEN_URI
                            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                    )
                    .map_err(|e| {
                        DbError::io(format!(
                            "Cannot open '{}': {}",
                            src.display(),
                            crate::error::map_error(e).message
                        ))
                    })
                    .and_then(|conn| {
                        run_backup(&conn, "main", &dest, progress, None, Duration::from_secs(5))
                    });
                    let _ = tx.send(result);
                })
                .map_err(|e| DbError::internal(format!("Cannot start the backup thread: {e}")))?;
            rx.await
                .map_err(|_| DbError::internal("The backup thread failed"))?
        }
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn run_backup(
    src: &Connection,
    schema: &str,
    dest: &Path,
    progress: Option<ProgressFn>,
    cancel: Option<&AtomicBool>,
    busy_timeout: Duration,
) -> DbResult<BackupReport> {
    let started = Instant::now();
    if dest.is_dir() {
        return Err(DbError::io(format!("'{}' is a directory", dest.display())));
    }
    let source_file: Option<String> = src
        .query_row(
            "SELECT file FROM pragma_database_list WHERE name = ?1",
            [schema],
            |r| r.get(0),
        )
        .map_err(|_| DbError::query(format!("Unknown database '{schema}'")))?;
    if let Some(f) = source_file.filter(|f| !f.is_empty())
        && same_file(Path::new(&f), dest)
    {
        return Err(DbError::config(
            "The backup destination is the source database itself",
        ));
    }
    let page_size: i64 = src
        .query_row(
            &format!("PRAGMA {}.page_size", crate::dialect::quote_ident(schema)),
            [],
            |r| r.get(0),
        )
        .db()?;
    let mut dst = Connection::open(dest).map_err(|e| {
        DbError::io(format!(
            "Cannot open '{}': {}",
            dest.display(),
            crate::error::map_error(e).message
        ))
    })?;
    let backup = Backup::new_with_names(src, schema, &mut dst, "main").db()?;
    let mut waited = Duration::ZERO;
    loop {
        if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
            return Err(DbError::cancelled());
        }
        match backup.step(PAGES_PER_STEP).db()? {
            StepResult::Done => break,
            StepResult::More => {}
            StepResult::Busy | StepResult::Locked => {
                if waited >= busy_timeout {
                    return Err(DbError::query(format!(
                        "The database is locked; the backup gave up after {} ms",
                        busy_timeout.as_millis()
                    )));
                }
                std::thread::sleep(Duration::from_millis(25));
                waited += Duration::from_millis(25);
            }
            _ => {}
        }
        if let Some(cb) = &progress {
            let p = backup.progress();
            cb(BackupProgress {
                remaining: i64::from(p.remaining),
                total: i64::from(p.pagecount),
            });
        }
    }
    let p = backup.progress();
    let pages = i64::from(p.pagecount);
    if let Some(cb) = &progress {
        cb(BackupProgress {
            remaining: 0,
            total: pages,
        });
    }
    Ok(BackupReport {
        pages,
        bytes: pages * page_size,
        elapsed: started.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vacuum_into() {
        assert_eq!(
            vacuum_into_sql(None, Path::new("/tmp/x's.db")),
            "VACUUM INTO '/tmp/x''s.db'"
        );
        assert_eq!(
            vacuum_into_sql(Some("aux"), Path::new("b.db")),
            "VACUUM aux INTO 'b.db'"
        );
    }
}
