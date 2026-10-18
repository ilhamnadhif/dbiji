//! Running vendor command line tools (pg_dump, mysqldump, sqlcmd ...) for
//! backup, restore and "execute in native client" (FR-A2).

use dbiji_core::admin::CommandSpec;
use dbiji_core::{DbError, DbResult};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

/// Run `spec` to completion. Standard error (and standard output when it is
/// not redirected to a file) is passed line by line to `on_line`; secrets in
/// the environment are never logged. Cancelling kills the process.
pub async fn run_command(spec: CommandSpec, cancel: CancellationToken, mut on_line: impl FnMut(String) + Send) -> DbResult<()> {
    let mut cmd = tokio::process::Command::new(&spec.program);
    cmd.args(&spec.args).kill_on_drop(true).stderr(Stdio::piped());
    for (k, v) in &spec.env {
        cmd.env(k, v.expose());
    }
    match &spec.stdin_file {
        Some(p) => {
            let f = std::fs::File::open(p).map_err(|e| DbError::io(format!("{}: {e}", p.display())))?;
            cmd.stdin(Stdio::from(f));
        }
        None => {
            cmd.stdin(Stdio::null());
        }
    }
    let piped_stdout = match &spec.stdout_file {
        Some(p) => {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let f = std::fs::File::create(p).map_err(|e| DbError::io(format!("{}: {e}", p.display())))?;
            cmd.stdout(Stdio::from(f));
            false
        }
        None => {
            cmd.stdout(Stdio::piped());
            true
        }
    };
    let mut child = cmd.spawn().map_err(|e| DbError::io(format!("Cannot start {}: {e}. Install the client tools or set the tool path in the dialog.", spec.program)))?;
    let stderr = child.stderr.take();
    let stdout = if piped_stdout { child.stdout.take() } else { None };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    if let Some(e) = stderr {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(e).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                let _ = tx.send(l);
            }
        });
    }
    if let Some(o) = stdout {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(o).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                let _ = tx.send(l);
            }
        });
    }
    drop(tx);
    let mut last_lines: Vec<String> = Vec::new();
    let status = loop {
        tokio::select! {
            line = rx.recv() => match line {
                Some(l) => {
                    last_lines.push(l.clone());
                    if last_lines.len() > 20 {
                        last_lines.remove(0);
                    }
                    on_line(l);
                }
                None => break child.wait().await,
            },
            s = child.wait() => {
                while let Ok(l) = rx.try_recv() {
                    on_line(l);
                }
                break s;
            }
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                return Err(DbError::cancelled());
            }
        }
    };
    let status = status.map_err(DbError::from)?;
    if status.success() { Ok(()) } else { Err(DbError::io(format!("{} exited with {status}", spec.program)).with_detail(last_lines.join("\n"))) }
}
