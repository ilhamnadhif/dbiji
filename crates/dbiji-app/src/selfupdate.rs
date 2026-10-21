//! Installing an update (macOS), after the release check of [`crate::updates`]
//! found a newer version (same approach as Peler's updater).
//!
//! The release's `DBiji-<version>.zip` is downloaded and checked (size,
//! SHA-256 published by GitHub, bundle id, version, code signature, and the
//! same signing team when this build has one). Once DBiji has quit, a helper
//! replaces the installed app and opens the new version. When the app is not
//! writable by the user, macOS asks for an administrator password. The
//! outcome is left in the data folder for the next launch to report.
//!
//! macOS's own tools do the work (`curl`, `ditto`, `codesign`, `plutil`).

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub use crate::updates::{Asset, archive_name};
use dbiji_core::tr;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

const BUNDLE_ID: &str = "dev.dbiji.DBiji";
const APP_NAME: &str = "DBiji.app";
/// Name prefix of the old app while the new one is moved in (next to it, so a
/// crash in between never loses it).
const ASIDE_PREFIX: &str = ".DBiji.app.old-";

/// Allowed URL schemes: HTTPS only (debug builds also read local files, for tests).
fn protocols() -> &'static str {
    if cfg!(debug_assertions) { "=https,file" } else { "=https" }
}

/// The installed `DBiji.app` this process runs from, when it can be updated
/// in place: not a development build, not a disk image, not a translocated
/// copy macOS made read-only.
pub fn installed_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let bundle = exe.parent()?.parent()?.parent()?.to_path_buf();
    let is_app = bundle.extension().is_some_and(|e| e == "app") && exe.parent()?.ends_with("Contents/MacOS");
    let path = bundle.to_string_lossy();
    let movable = !path.contains("/AppTranslocation/") && !path.starts_with("/Volumes/");
    (is_app && movable).then_some(bundle)
}

/// Progress of a download, for the UI.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Stage {
    #[default]
    Starting,
    Downloading {
        done: u64,
        total: u64,
    },
    Verifying,
}

/// A downloaded and checked update, ready to replace the installed app.
#[derive(Debug)]
pub struct Prepared {
    pub version: String,
    pub dir: PathBuf,
    pub app: PathBuf,
}

fn plist_value(app: &Path, key: &str) -> Option<String> {
    let out = Command::new("/usr/bin/plutil").args(["-extract", key, "raw", "-o", "-"]).arg(app.join("Contents/Info.plist")).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The signing team of an app (`None` for ad-hoc or unsigned builds).
fn team_id(app: &Path) -> Option<String> {
    let out = Command::new("/usr/bin/codesign").args(["-dv"]).arg(app).output().ok()?;
    let text = String::from_utf8_lossy(&out.stderr);
    text.lines().find_map(|l| l.strip_prefix("TeamIdentifier=")).map(str::trim).filter(|t| *t != "not set").map(str::to_string)
}

fn signature_valid(app: &Path) -> bool {
    Command::new("/usr/bin/codesign").args(["--verify", "--deep", "--strict"]).arg(app).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// A new folder for one download, private to this user and never shared with
/// another download or another running DBiji.
fn new_download_dir() -> Result<PathBuf, String> {
    use std::os::unix::fs::DirBuilderExt;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    loop {
        let dir = std::env::temp_dir().join(format!("dbiji-update-{}-{nanos}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
        // `create` (not `create_all`) fails when anything, a link included, is already there.
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Downloads and checks the archive of `version`.
pub fn download(version: &str, asset: &Asset, stage: &Arc<Mutex<Stage>>) -> Result<Prepared, String> {
    let dir = new_download_dir()?;
    let result = download_into(version, asset, &dir, stage);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    result
}

fn download_into(version: &str, asset: &Asset, dir: &Path, stage: &Arc<Mutex<Stage>>) -> Result<Prepared, String> {
    let archive = dir.join(&asset.name);
    *stage.lock() = Stage::Downloading { done: 0, total: asset.size };
    let mut child = Command::new("/usr/bin/curl")
        .args(["-fsSL", "--proto", protocols(), "--proto-redir", protocols()])
        .args(["--max-time", "1800"])
        .args(["-H", &format!("User-Agent: DBiji/{}", crate::updates::current_version())])
        .arg("-o")
        .arg(&archive)
        .arg(&asset.url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        let done = std::fs::metadata(&archive).map(|m| m.len()).unwrap_or(0);
        *stage.lock() = Stage::Downloading { done, total: asset.size };
        std::thread::sleep(Duration::from_millis(150));
    };
    if !status.success() {
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut err);
        }
        return Err(format!("download failed: {}", err.trim()));
    }
    *stage.lock() = Stage::Verifying;
    let size = std::fs::metadata(&archive).map(|m| m.len()).unwrap_or(0);
    if size != asset.size {
        return Err(format!("download incomplete ({size} of {} bytes)", asset.size));
    }
    if let Some(expected) = &asset.sha256 {
        let actual = sha256_file(&archive).map_err(|e| e.to_string())?;
        if &actual != expected {
            return Err("the download does not match its published checksum".into());
        }
    }
    let unpacked = dir.join("app");
    let ok = Command::new("/usr/bin/ditto").args(["-x", "-k"]).arg(&archive).arg(&unpacked).status().map_err(|e| e.to_string())?.success();
    if !ok {
        return Err("the archive could not be unpacked".into());
    }
    let app = unpacked.join(APP_NAME);
    if plist_value(&app, "CFBundleIdentifier").as_deref() != Some(BUNDLE_ID) {
        return Err("the download is not DBiji".into());
    }
    if plist_value(&app, "CFBundleShortVersionString").as_deref() != Some(version) {
        return Err("the downloaded version does not match the release".into());
    }
    if !signature_valid(&app) {
        return Err("the downloaded app's signature is not valid".into());
    }
    // A build signed by a team only accepts updates signed by the same team.
    if let Some(bundle) = installed_bundle()
        && let Some(team) = team_id(&bundle)
        && team_id(&app).as_deref() != Some(&team)
    {
        return Err("the downloaded app is signed by someone else".into());
    }
    let _ = std::fs::remove_file(&archive);
    Ok(Prepared { version: version.to_string(), dir: dir.to_path_buf(), app })
}

/// Whether the current user can replace `bundle` without an administrator password.
fn user_can_replace(bundle: &Path) -> bool {
    let probe = |dir: &Path| {
        let p = dir.join(format!(".dbiji-write-test-{}", std::process::id()));
        let ok = std::fs::File::create(&p).is_ok();
        let _ = std::fs::remove_file(&p);
        ok
    };
    // Renaming a folder needs write access to the folder itself, not only to its parent.
    bundle.parent().is_some_and(probe) && probe(bundle) && probe(&bundle.join("Contents"))
}

/// Replaces the installed app (`$2`) with the downloaded one (`$1`), as the
/// user or as root. The download is first copied into a new folder only this
/// process can write, and everything is checked on that copy: signature,
/// bundle id (`$5`), version (`$3`) and, when set, signing team (`$4`). As
/// root the copy gets the owner of the app it replaces. The old app is moved
/// aside, and put back when the new one can't be moved in. Only the last line
/// of a failure is meant for the user.
const SWAP_SCRIPT: &str = r#"
fail() { echo "$1" >&2; exit 1; }
src=$1 app=$2 version=$3 team=$4 id=$5
umask 022
[ -d "$app" ] || fail "the installed app was not found"
stage=$(/usr/bin/mktemp -d /private/tmp/dbiji-update.XXXXXXXX) || fail "no folder for the update could be created"
trap '/bin/rm -rf "$stage"' EXIT
new=$stage/DBiji.app
/usr/bin/ditto --norsrc --noextattr --noacl "$src" "$new" 2>/dev/null || fail "the update could not be copied"
if [ "$(/usr/bin/id -u)" = 0 ]; then
    owner=$(/usr/bin/stat -f %u:%g "$app") && /usr/sbin/chown -R "$owner" "$new" ||
        fail "the update could not be given the installed app's owner"
fi
# No set-id bits, nothing writable by others.
/bin/chmod -R u+rwX,go+rX,go-w,a-st "$new" || fail "the update's permissions could not be set"
/usr/bin/codesign --verify --deep --strict "$new" 2>/dev/null || fail "the downloaded app's signature is not valid"
plist() { /usr/bin/plutil -extract "$1" raw -o - "$new/Contents/Info.plist" 2>/dev/null; }
[ "$(plist CFBundleIdentifier)" = "$id" ] || fail "the download is not DBiji"
[ "$(plist CFBundleShortVersionString)" = "$version" ] || fail "the downloaded version does not match the release"
if [ -n "$team" ]; then
    signer=$(/usr/bin/codesign -dv "$new" 2>&1 | /usr/bin/sed -n 's/^TeamIdentifier=//p')
    [ "$signer" = "$team" ] || fail "the downloaded app is signed by someone else"
fi
dir=$(/usr/bin/dirname "$app")
old=$(/usr/bin/mktemp -u "$dir/.DBiji.app.old-XXXXXXXX") || fail "the installed app could not be moved aside"
# -h: never move into a directory a link points to.
/bin/mv -h "$app" "$old" 2>/dev/null || fail "the installed app could not be moved aside"
if ! /bin/mv -h "$new" "$app" 2>/dev/null; then
    /bin/rm -rf "$app"
    /bin/mv -h "$old" "$app" 2>/dev/null || fail "the new version could not be moved into place; the old one is in $old"
    fail "the new version could not be moved into place"
fi
/bin/rm -rf "$old"
# Old copies an interrupted update left behind; the app itself is in place and checked.
for leftover in "$dir"/.DBiji.app.old-*; do
    if [ -e "$leftover" ] || [ -L "$leftover" ]; then /bin/rm -rf "$leftover"; fi
done
exit 0
"#;

/// Waits for DBiji (`$1`) to quit, runs the command after the first five
/// arguments (the swap, directly or behind the administrator password
/// prompt), records its outcome in `$2` for the next launch, opens the app
/// (`$5`) and deletes the download (`$3`). Nothing is spliced into the
/// script: every value is an argument.
const HELPER_SCRIPT: &str = r#"
pid=$1 status=$2 download=$3 version=$4 app=$5
shift 5
while /bin/kill -0 "$pid" 2>/dev/null; do /bin/sleep 0.2; done
out=$("$@" 2>&1)
rc=$?
printf '%s\n%s\n%s\n' "$version" "$rc" "$out" > "$status.tmp" && /bin/mv -f "$status.tmp" "$status"
/usr/bin/open "$app"
/bin/rm -rf "$download"
"#;

/// Runs the swap script (item 2) as root with the remaining items as its
/// arguments, after macOS asked for an administrator password with the
/// prompt in item 1; nothing changes if it is declined. AppleScript quotes
/// every argument, and no file the user can write is run.
const ADMIN_APPLESCRIPT: &str = r#"on run argv
    set cmd to "/bin/sh -c " & quoted form of (item 2 of argv) & " dbiji-update"
    repeat with arg in rest of rest of argv
        set cmd to cmd & " " & quoted form of (contents of arg)
    end repeat
    do shell script cmd with prompt (item 1 of argv) with administrator privileges
end run"#;

/// Where the installer helper leaves the outcome of an update for the next launch.
fn outcome_file(data_dir: &Path) -> PathBuf {
    data_dir.join("update-outcome")
}

/// Starts a helper that waits for this process to exit, swaps in the new app
/// (asking for an administrator password when needed), and opens it. The
/// caller then quits the app.
/// The update can replace `bundle` (another copy may have been installed meanwhile).
pub fn check_installable(prepared: &Prepared, bundle: &Path) -> Result<(), String> {
    if let Some(installed) = plist_value(bundle, "CFBundleShortVersionString")
        && !crate::updates::is_newer(&prepared.version, &installed)
    {
        return Err(format!("DBiji {installed} is already installed; quit DBiji and open it again"));
    }
    Ok(())
}

pub fn launch_installer(prepared: &Prepared, bundle: &Path, data_dir: &Path) -> Result<(), String> {
    check_installable(prepared, bundle)?;
    let status = outcome_file(data_dir);
    dbiji_config::paths::ensure_private_dir(data_dir).map_err(|e| e.to_string())?;
    let mut swap: Vec<OsString> = if user_can_replace(bundle) {
        ["/bin/sh", "-c", SWAP_SCRIPT, "dbiji-update"].map(OsString::from).into()
    } else {
        ["/usr/bin/osascript", "-e", ADMIN_APPLESCRIPT, tr("DBiji wants to install an update."), SWAP_SCRIPT].map(OsString::from).into()
    };
    swap.extend([
        prepared.app.clone().into_os_string(),
        bundle.as_os_str().to_owned(),
        prepared.version.clone().into(),
        team_id(bundle).unwrap_or_default().into(),
        BUNDLE_ID.into(),
    ]);
    Command::new("/bin/sh")
        .args(["-c", HELPER_SCRIPT, "dbiji-update"])
        .arg(std::process::id().to_string())
        .arg(&status)
        .arg(&prepared.dir)
        .arg(&prepared.version)
        .arg(bundle)
        .args(&swap)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// How the update started by the previous run went.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// This version was installed.
    Installed(String),
    /// The administrator password prompt was dismissed.
    Cancelled,
    /// Why it was not installed.
    Failed(String),
}

/// The outcome the installer helper recorded, if any. It is reported once:
/// the file is deleted.
pub fn take_outcome(data_dir: &Path) -> Option<Outcome> {
    let path = outcome_file(data_dir);
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    parse_outcome(&text)
}

/// The helper's record: version, exit status, then what the swap printed.
fn parse_outcome(text: &str) -> Option<Outcome> {
    let mut lines = text.splitn(3, '\n');
    let version = lines.next()?.trim();
    let code: i32 = lines.next()?.trim().parse().ok()?;
    if code == 0 {
        return Some(Outcome::Installed(version.to_string()));
    }
    let output = lines.next().unwrap_or("");
    // osascript reports "<range>: execution error: <message> (<number>)"; -128 is "User canceled".
    let mut message = output.rsplit_once("execution error: ").map_or(output, |(_, m)| m).trim();
    if let Some((rest, number)) = message.rsplit_once(" (")
        && let Some(number) = number.strip_suffix(')')
        && let Ok(number) = number.parse::<i64>()
    {
        if number == -128 {
            return Some(Outcome::Cancelled);
        }
        message = rest;
    }
    // The swap's own message comes last (osascript joins lines with \r).
    let reason = message.split(['\r', '\n']).map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("unknown error");
    Some(Outcome::Failed(reason.chars().take(300).collect()))
}

/// Deletes old copies an interrupted update left next to the app, but only
/// while the app itself is in place and intact, so the only copy is never
/// removed.
pub fn remove_leftovers() {
    if let Some(bundle) = installed_bundle() {
        remove_leftovers_beside(&bundle, BUNDLE_ID);
    }
}

fn remove_leftovers_beside(bundle: &Path, id: &str) {
    let Some(Ok(entries)) = bundle.parent().map(std::fs::read_dir) else { return };
    let leftovers: Vec<PathBuf> = entries.flatten().filter(|e| e.file_name().to_string_lossy().starts_with(ASIDE_PREFIX)).map(|e| e.path()).collect();
    if leftovers.is_empty() || plist_value(bundle, "CFBundleIdentifier").as_deref() != Some(id) || !signature_valid(bundle) {
        return;
    }
    for path in leftovers {
        // A link is removed itself, never followed.
        let _ = match std::fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(&path),
            _ => std::fs::remove_file(&path),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes() {
        assert_eq!(parse_outcome("1.2.0\n0\n\n"), Some(Outcome::Installed("1.2.0".into())));
        assert_eq!(parse_outcome("1.2.0\n1\n0:301: execution error: User canceled. (-128)\n"), Some(Outcome::Cancelled));
        // osascript joins the swap's lines with \r; the last one is the reason.
        assert_eq!(parse_outcome("1.2.0\n1\n7:158: execution error: noise\rthe download is not DBiji (1)\n"), Some(Outcome::Failed("the download is not DBiji".into())));
        assert_eq!(parse_outcome("1.2.0\n1\nditto: noise\nthe update could not be copied\n"), Some(Outcome::Failed("the update could not be copied".into())));
        assert_eq!(parse_outcome("1.2.0\n2\n"), Some(Outcome::Failed("unknown error".into())));
        assert_eq!(parse_outcome(""), None);
        assert_eq!(parse_outcome("garbage"), None);
    }

    #[test]
    fn download_dirs_are_unique_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let a = new_download_dir().unwrap();
        let b = new_download_dir().unwrap();
        assert_ne!(a, b);
        assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o700);
        std::fs::remove_dir(&a).unwrap();
        std::fs::remove_dir(&b).unwrap();
    }

    /// A small ad-hoc signed bundle (its executable is a copy of `true`).
    fn fake_app(parent: &Path, version: &str, id: &str) -> PathBuf {
        let app = parent.join(APP_NAME);
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::create_dir_all(app.join("Contents/Resources")).unwrap();
        std::fs::copy("/usr/bin/true", app.join("Contents/MacOS/dbiji")).unwrap();
        std::fs::write(app.join("Contents/Resources/notes.txt"), version).unwrap();
        std::fs::write(
            app.join("Contents/Info.plist"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>dbiji</string>
<key>CFBundleIdentifier</key><string>{id}</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
</dict></plist>"#
            ),
        )
        .unwrap();
        let signed = Command::new("/usr/bin/codesign").args(["--force", "--sign", "-"]).arg(&app).stderr(Stdio::null()).status().unwrap();
        assert!(signed.success());
        app
    }

    /// Runs `script` the way the helper does for a user-writable app; returns its stderr.
    fn run_swap(script: &str, new: &Path, app: &Path, version: &str, team: &str) -> String {
        let out = Command::new("/bin/sh").args(["-c", script, "dbiji-update"]).arg(new).arg(app).args([version, team, "dev.dbiji.test"]).output().unwrap();
        assert_eq!(out.status.success(), out.stderr.is_empty());
        String::from_utf8_lossy(&out.stderr).trim().to_string()
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with(ASIDE_PREFIX)).collect()
    }

    #[test]
    fn swap_installs_a_checked_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let (apps, download) = (tmp.path().join("Applications"), tmp.path().join("dl"));
        let app = fake_app(&apps, "1.0.0", "dev.dbiji.test");
        let new = fake_app(&download, "2.0.0", "dev.dbiji.test");
        // From an update that was interrupted earlier.
        std::fs::create_dir_all(apps.join(".DBiji.app.old-1234/Contents")).unwrap();
        assert_eq!(run_swap(SWAP_SCRIPT, &new, &app, "2.0.0", ""), "");
        assert_eq!(plist_value(&app, "CFBundleShortVersionString").as_deref(), Some("2.0.0"));
        assert!(signature_valid(&app));
        assert!(leftovers(&apps).is_empty());
        assert!(new.exists(), "the download is copied, not moved");
    }

    #[test]
    fn swap_refuses_a_bad_update_and_keeps_the_app() {
        let tmp = tempfile::tempdir().unwrap();
        let (apps, download) = (tmp.path().join("Applications"), tmp.path().join("dl"));
        let app = fake_app(&apps, "1.0.0", "dev.dbiji.test");
        let new = fake_app(&download, "2.0.0", "dev.dbiji.test");
        let kept = |app: &Path| {
            assert_eq!(plist_value(app, "CFBundleShortVersionString").as_deref(), Some("1.0.0"));
            assert!(leftovers(&apps).is_empty());
        };
        assert_eq!(run_swap(SWAP_SCRIPT, &new, &app, "3.0.0", ""), "the downloaded version does not match the release");
        kept(&app);
        assert_eq!(run_swap(SWAP_SCRIPT, &new, &app, "2.0.0", "ABCDE12345"), "the downloaded app is signed by someone else");
        kept(&app);
        // Changed after it was checked.
        std::fs::write(new.join("Contents/Resources/notes.txt"), "evil").unwrap();
        assert_eq!(run_swap(SWAP_SCRIPT, &new, &app, "2.0.0", ""), "the downloaded app's signature is not valid");
        kept(&app);
    }

    #[test]
    fn swap_puts_the_old_app_back_when_the_new_one_cannot_move_in() {
        let tmp = tempfile::tempdir().unwrap();
        let (apps, download) = (tmp.path().join("Applications"), tmp.path().join("dl"));
        let app = fake_app(&apps, "1.0.0", "dev.dbiji.test");
        let new = fake_app(&download, "2.0.0", "dev.dbiji.test");
        let move_in = r#"/bin/mv -h "$new" "$app" 2>/dev/null"#;
        assert!(SWAP_SCRIPT.contains(move_in));
        let failing = SWAP_SCRIPT.replace(move_in, "false");
        assert_eq!(run_swap(&failing, &new, &app, "2.0.0", ""), "the new version could not be moved into place");
        assert_eq!(plist_value(&app, "CFBundleShortVersionString").as_deref(), Some("1.0.0"));
        assert!(signature_valid(&app));
        assert!(leftovers(&apps).is_empty());
    }

    #[test]
    fn leftovers_go_only_while_the_app_is_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let apps = tmp.path().join("Applications");
        let app = fake_app(&apps, "1.0.0", "dev.dbiji.test");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), "x").unwrap();
        std::fs::create_dir_all(apps.join(".DBiji.app.old-a/Contents")).unwrap();
        std::os::unix::fs::symlink(&outside, apps.join(".DBiji.app.old-link")).unwrap();
        std::fs::write(app.join("Contents/Resources/notes.txt"), "damaged").unwrap();
        remove_leftovers_beside(&app, "dev.dbiji.test");
        assert_eq!(leftovers(&apps).len(), 2, "kept: the app is not intact");
        std::fs::remove_dir_all(&app).unwrap();
        let app = fake_app(&apps, "1.0.0", "dev.dbiji.test");
        remove_leftovers_beside(&app, "dev.dbiji.DBiji");
        assert_eq!(leftovers(&apps).len(), 2, "kept: not the expected app");
        remove_leftovers_beside(&app, "dev.dbiji.test");
        assert!(leftovers(&apps).is_empty());
        assert!(outside.join("keep.txt").exists(), "links are not followed");
        assert!(app.exists());
    }

    /// The download of a release archive (a local file in debug builds):
    /// checksum, bundle id and version are verified.
    #[test]
    fn download_checks_the_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fake_app(&src, "9.9.9", BUNDLE_ID);
        let zip = tmp.path().join(archive_name("9.9.9"));
        let ok = Command::new("/usr/bin/ditto").args(["-c", "-k", "--keepParent"]).arg(src.join(APP_NAME)).arg(&zip).status().unwrap();
        assert!(ok.success());
        let size = std::fs::metadata(&zip).unwrap().len();
        let sha = sha256_file(&zip).unwrap();
        let asset = Asset { name: archive_name("9.9.9"), url: format!("file://{}", zip.display()), size, sha256: Some(sha) };
        let stage = Arc::new(Mutex::new(Stage::Starting));
        let prepared = download("9.9.9", &asset, &stage).expect("downloaded and checked");
        assert_eq!(plist_value(&prepared.app, "CFBundleShortVersionString").as_deref(), Some("9.9.9"));
        assert_eq!(*stage.lock(), Stage::Verifying);
        std::fs::remove_dir_all(&prepared.dir).unwrap();
        // A wrong checksum, a wrong version.
        let bad = Asset { sha256: Some("00".repeat(32)), ..asset.clone() };
        assert_eq!(download("9.9.9", &bad, &stage).unwrap_err(), "the download does not match its published checksum");
        assert_eq!(download("9.9.8", &asset, &stage).unwrap_err(), "the downloaded version does not match the release");
    }

    /// The administrator variant can't run in a test; the same AppleScript
    /// without the password prompt shows every argument arrives unchanged.
    #[test]
    fn applescript_passes_arguments_verbatim() {
        let privileged = " with prompt (item 1 of argv) with administrator privileges";
        assert!(ADMIN_APPLESCRIPT.contains(privileged));
        let plain = ADMIN_APPLESCRIPT.replace(privileged, "");
        let args = ["it's \"quoted\"", "$HOME `id` \\ ; *", "", "two  spaces"];
        let out = Command::new("/usr/bin/osascript").args(["-e", &plain, "prompt", r#"printf '<%s>' "$@""#]).args(args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let expected: String = args.iter().map(|a| format!("<{a}>")).collect();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim_end(), expected);
    }
}
