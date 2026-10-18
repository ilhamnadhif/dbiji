//! Running local shell commands: "shell command before connect", "after
//! disconnect" and "password from command" ([`dbiji_core::config::AuthMethod::Command`]).
//!
//! Commands run through `sh -c` on Unix and `cmd /C` on Windows, with stdin
//! closed. On timeout the whole process group is killed (Unix) so helper
//! processes spawned by the command do not linger.

use dbiji_core::{DbError, DbResult, ErrorKind, Secret};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use zeroize::Zeroizing;

/// Maximum number of bytes kept from stdout / stderr; the rest is drained and discarded.
const MAX_CAPTURE: usize = 1024 * 1024;
/// Maximum number of stderr characters quoted in error messages.
const MAX_STDERR_IN_ERROR: usize = 2000;

struct CommandOutput {
    status: ExitStatus,
    stdout: Zeroizing<Vec<u8>>,
    stderr: Vec<u8>,
}

/// Runs `command` in the platform shell and returns its standard output with
/// surrounding whitespace trimmed.
///
/// Errors:
/// * [`ErrorKind::Config`] for an empty command,
/// * [`ErrorKind::Io`] when the shell cannot be started or the command exits
///   with a non-zero status (the message includes the exit code and stderr),
/// * [`ErrorKind::Timeout`] when it runs longer than `timeout` (the process
///   and its process group are killed).
pub async fn run_shell_command(command: &str, timeout: Duration) -> DbResult<String> {
    let out = run_capture(command, timeout).await?;
    check_status(&out)?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Runs a "password command" and returns the first line of its standard
/// output as a [`Secret`] (trailing `\r`/`\n` removed, other whitespace kept).
///
/// The output is never logged or included in error messages; captured
/// buffers are wiped after use. An empty first line is an [`ErrorKind::Auth`] error.
pub async fn password_from_command(command: &str, timeout: Duration) -> DbResult<Secret> {
    let out = run_capture(command, timeout).await?;
    check_status(&out)?;
    let text = std::str::from_utf8(&out.stdout)
        .map_err(|_| DbError::auth("The password command printed non UTF-8 output"))?;
    let first = text
        .lines()
        .next()
        .unwrap_or("")
        .trim_end_matches(['\r', '\n']);
    if first.is_empty() {
        return Err(DbError::auth(
            "The password command printed an empty password",
        ));
    }
    Ok(Secret::new(first))
}

fn check_status(out: &CommandOutput) -> DbResult<()> {
    if out.status.success() {
        return Ok(());
    }
    let code = match out.status.code() {
        Some(c) => format!("exit code {c}"),
        None => "a signal".to_string(),
    };
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stderr = stderr.trim();
    let mut msg = format!("Shell command failed with {code}");
    if !stderr.is_empty() {
        let shown: String = stderr.chars().take(MAX_STDERR_IN_ERROR).collect();
        msg.push_str(": ");
        msg.push_str(&shown);
    }
    Err(DbError::io(msg))
}

fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C");
        // Pass the command line verbatim; cmd.exe does its own parsing.
        cmd.raw_arg(command);
        // CREATE_NO_WINDOW: do not flash a console window from a GUI app.
        cmd.creation_flags(0x0800_0000);
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    }
}

async fn read_capped<R: AsyncRead + Unpin>(reader: Option<R>, out: &mut Vec<u8>) {
    let Some(mut reader) = reader else { return };
    let mut buf = Zeroizing::new([0u8; 8192]);
    loop {
        match reader.read(&mut buf[..]).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = MAX_CAPTURE.saturating_sub(out.len());
                out.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
}

async fn run_capture(command: &str, timeout: Duration) -> DbResult<CommandOutput> {
    if command.trim().is_empty() {
        return Err(DbError::config("The shell command is empty"));
    }
    let mut cmd = shell_command(command);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|e| DbError::io(format!("Cannot start the shell command: {e}")))?;
    let pid = child.id();
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let mut stdout = Zeroizing::new(Vec::new());
    let mut stderr = Vec::new();

    let result = tokio::time::timeout(timeout, async {
        tokio::join!(
            read_capped(stdout_pipe, &mut stdout),
            read_capped(stderr_pipe, &mut stderr)
        );
        child.wait().await
    })
    .await;

    match result {
        Ok(Ok(status)) => Ok(CommandOutput {
            status,
            stdout,
            stderr,
        }),
        Ok(Err(e)) => Err(DbError::io(format!(
            "Waiting for the shell command failed: {e}"
        ))),
        Err(_) => {
            kill_process_group(pid);
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            Err(DbError::new(
                ErrorKind::Timeout,
                format!(
                    "The shell command did not finish within {}s and was killed",
                    timeout.as_secs_f32()
                ),
            ))
        }
    }
}

#[cfg(unix)]
fn kill_process_group(pid: Option<u32>) {
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) {
        // SAFETY: killpg only sends a signal; the process group was created
        // for this child with `process_group(0)` so its id equals the pid.
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: Option<u32>) {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn captures_trimmed_stdout() {
        let out = run_shell_command("printf '  hello world \\n\\n'", Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(out, "hello world");
    }

    #[tokio::test]
    async fn non_zero_exit_reports_stderr() {
        let err = run_shell_command("echo boom >&2; exit 3", Duration::from_secs(10))
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Io);
        assert!(err.message.contains("exit code 3"), "{}", err.message);
        assert!(err.message.contains("boom"), "{}", err.message);
    }

    #[tokio::test]
    async fn empty_command_is_config_error() {
        let err = run_shell_command("   ", Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Config);
    }

    #[tokio::test]
    async fn timeout_kills_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        // The background grandchild would create the marker after 2s if it was not killed.
        let cmd = format!("(sleep 2; touch '{}') & sleep 30", marker.display());
        let started = Instant::now();
        let err = run_shell_command(&cmd, Duration::from_millis(300))
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Timeout);
        assert!(started.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(!marker.exists(), "grandchild process was not killed");
    }

    #[tokio::test]
    async fn password_command_takes_first_line() {
        let s = password_from_command(
            "printf 's3cr:et \\nsecond line\\n'",
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(s.expose(), "s3cr:et ");
        let err = password_from_command("printf ''", Duration::from_secs(10))
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Auth);
        assert!(!format!("{err:?}").contains("s3cr"));
    }

    #[tokio::test]
    async fn password_never_in_error() {
        // Non-zero exit: stdout (which could be a password) must not appear in the error.
        let err = password_from_command("echo topsecret; exit 1", Duration::from_secs(10))
            .await
            .unwrap_err();
        assert!(!err.full_text().contains("topsecret"));
    }
}
