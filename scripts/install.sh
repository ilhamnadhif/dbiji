#!/usr/bin/env bash
# Installs (or updates) DBiji from its GitHub releases into /Applications.
#
#   curl -fsSL https://raw.githubusercontent.com/ilhamnadhif/dbiji/master/scripts/install.sh | bash
#   ... | bash -s -- v0.3.0          # a specific version
#
# The app is downloaded with curl, so macOS does not quarantine it and it opens without the
# "unidentified developer" prompt. Everything is fetched over HTTPS and checked (the SHA-256
# GitHub publishes, the code signature, the bundle id) before the installed copy is touched.
# The old copy is moved aside and put back if the new one can't be moved in. sudo is used only
# when /Applications or the installed app is not writable.
set -euo pipefail

REPO="${DBIJI_REPO:-ilhamnadhif/dbiji}"
TAG="${1:-latest}"
APP_DIR="${DBIJI_APP_DIR:-/Applications}"
APP="$APP_DIR/DBiji.app"
BUNDLE_ID="dev.dbiji.DBiji"

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
die() { printf 'Error: %s\n' "$*" >&2; exit 1; }
# HTTPS only, redirects included.
fetch() { curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fL "$@"; }
plist() { plutil -extract "$2" raw -o - "$1/Contents/Info.plist" 2>/dev/null || true; }
team() { codesign -dv "$1" 2>&1 | sed -n 's/^TeamIdentifier=//p' | grep -v '^not set$' || true; }

[ "$(uname -s)" = "Darwin" ] || die "DBiji is a macOS app."
case "$TAG" in
    latest | v[0-9]*) ;;
    [0-9]*) TAG="v$TAG" ;;
    *) die "not a version: $TAG" ;;
esac
[ -d "$APP_DIR" ] || die "$APP_DIR does not exist"

TMP="$(mktemp -d)"
MNT="$TMP/mnt"
SUDO=""
STAGE=""
OLD=""
cleanup() {
    # Interrupted between moving the old app aside and moving the new one in: put it back.
    if [ -n "$OLD" ] && [ -e "$OLD" ] && [ ! -e "$APP" ]; then
        $SUDO mv -h "$OLD" "$APP" || true
    fi
    [ -z "$STAGE" ] || $SUDO rm -rf "$STAGE" || true
    hdiutil detach -quiet "$MNT" 2>/dev/null || true
    rm -rf "$TMP"
}
trap cleanup EXIT

if [ "$TAG" = "latest" ]; then
    API="https://api.github.com/repos/$REPO/releases/latest"
else
    API="https://api.github.com/repos/$REPO/releases/tags/$TAG"
fi
say "Looking up the release ($TAG)"
fetch -sS -H "Accept: application/vnd.github+json" -o "$TMP/release.json" "$API" ||
    die "release $TAG of $REPO not found"

# plutil reads JSON. The app archive the in-app updater uses, or the disk image of releases
# that have none.
json() { plutil -extract "$1" raw -o - "$TMP/release.json" 2>/dev/null; }
VERSION="$(json tag_name)" || die "unexpected answer from GitHub"
VERSION="${VERSION#v}"
ZIP="" DMG=""
i=0
while name="$(json "assets.$i.name")"; do
    case "$name" in
        "DBiji-$VERSION.zip") ZIP=$i ;;
        "DBiji-$VERSION.dmg") DMG=$i ;;
    esac
    i=$((i + 1))
done
ASSET="${ZIP:-$DMG}"
[ -n "$ASSET" ] || die "no DBiji-$VERSION.zip or .dmg in release $TAG of $REPO"
NAME="$(json "assets.$ASSET.name")"
URL="$(json "assets.$ASSET.browser_download_url")"
DIGEST="$(json "assets.$ASSET.digest" || true)"
case "$URL" in https://*) ;; *) die "unexpected download address: $URL" ;; esac

say "Downloading $NAME"
fetch --progress-bar -o "$TMP/$NAME" "$URL"
if [ -n "$DIGEST" ]; then
    expected="$(printf '%s' "${DIGEST#sha256:}" | tr 'A-F' 'a-f')"
    actual="$(shasum -a 256 "$TMP/$NAME" | awk '{print $1}')"
    [ "$actual" = "$expected" ] || die "the download does not match the SHA-256 GitHub published"
    say "Checksum verified"
else
    say "GitHub published no checksum for this file; relying on the code signature"
fi

if [ -n "$ZIP" ]; then
    ditto -x -k "$TMP/$NAME" "$TMP/unpacked"
    SRC="$TMP/unpacked/DBiji.app"
else
    mkdir -p "$MNT"
    hdiutil attach -quiet -nobrowse -readonly -mountpoint "$MNT" "$TMP/$NAME"
    SRC="$MNT/DBiji.app"
fi
[ -d "$SRC" ] || die "the download does not contain DBiji.app"

# Checks an app before it is installed. A copy signed by a team only accepts the same team.
check_app() {
    $SUDO codesign --verify --deep --strict "$1" 2>/dev/null || die "the downloaded app's signature is not valid"
    [ "$(plist "$1" CFBundleIdentifier)" = "$BUNDLE_ID" ] || die "the download is not DBiji"
    [ "$(plist "$1" CFBundleShortVersionString)" = "$VERSION" ] || die "the downloaded version does not match the release"
    if [ -n "$INSTALLED_TEAM" ] && [ "$(team "$1")" != "$INSTALLED_TEAM" ]; then
        die "the download is signed by someone else than the installed DBiji; remove $APP first to install it anyway"
    fi
}
INSTALLED_TEAM=""
[ ! -e "$APP" ] || INSTALLED_TEAM="$(team "$APP")"
check_app "$SRC"

if [ ! -w "$APP_DIR" ] || { [ -e "$APP" ] && { [ ! -w "$APP" ] || [ ! -w "$APP/Contents" ]; }; }; then
    SUDO="sudo"
    say "$APP_DIR or the installed DBiji.app is not writable by $(id -un)."
    say "sudo will ask for your password to replace it."
    sudo -v || die "DBiji was not installed: administrator rights are needed"
fi

# Only the copy being replaced is quit (it asks about unsaved scripts first).
running() { pgrep -qf "^$APP/Contents/MacOS/dbiji"; }
if running; then
    say "Quitting the running DBiji"
    osascript -e "tell application \"$APP\" to quit" >/dev/null 2>&1 || true
    for _ in $(seq 1 60); do running || break; sleep 0.5; done
    running && die "DBiji is still running; quit it and run this again."
fi

# The copy that gets installed is made into a new folder only this script (or root) can write,
# and checked again there. /private/tmp is on the same volume as /Applications, so moving it in
# is a rename. As root it gets the owner of the copy it replaces.
STAGE="$($SUDO mktemp -d /private/tmp/dbiji-install.XXXXXXXX)"
NEW="$STAGE/DBiji.app"
$SUDO ditto --norsrc --noextattr --noacl "$SRC" "$NEW"
if [ -n "$SUDO" ]; then
    owner="root:wheel"
    [ ! -e "$APP" ] || owner="$(stat -f %u:%g "$APP")"
    $SUDO chown -R "$owner" "$NEW"
fi
$SUDO chmod -R u+rwX,go+rX,go-w,a-st "$NEW"
check_app "$NEW"

if [ -e "$APP" ]; then
    say "Replacing DBiji $(plist "$APP" CFBundleShortVersionString) in $APP_DIR with $VERSION"
    OLD="$(mktemp -u "$APP_DIR/.DBiji.app.old-XXXXXXXX")"
    # -h: never move into a directory a link points to.
    $SUDO mv -h "$APP" "$OLD" || die "the installed app could not be moved aside; nothing was changed"
else
    say "Installing DBiji $VERSION into $APP_DIR"
fi
if ! $SUDO mv -h "$NEW" "$APP"; then
    if [ -n "$OLD" ]; then
        $SUDO rm -rf "$APP"
        $SUDO mv -h "$OLD" "$APP" && OLD=""
    fi
    die "the new version could not be moved into place; the previous one was kept"
fi
OLD=""
# The old copy, and any an interrupted update left behind (the new app is in place and checked).
for leftover in "$APP_DIR"/.DBiji.app.old-*; do
    if [ -e "$leftover" ] || [ -L "$leftover" ]; then
        $SUDO rm -rf "$leftover" 2>/dev/null || say "Could not remove $leftover; delete it yourself."
    fi
done

say "Done. DBiji $(plist "$APP" CFBundleShortVersionString) is in $APP_DIR."
open "$APP" || true
