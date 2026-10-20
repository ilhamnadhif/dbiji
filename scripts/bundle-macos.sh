#!/usr/bin/env bash
# Build DBiji.app and a DMG for macOS.
#
# Usage:
#   scripts/bundle-macos.sh [--universal] [--features "duckdb"]
#
# Optional environment variables:
#   DBIJI_SIGN_IDENTITY   "Developer ID Application: Name (TEAMID)" — sign with hardened runtime.
#                         Without it the bundle is ad-hoc signed (runs locally, not distributable).
#   DBIJI_NOTARY_PROFILE  notarytool keychain profile (xcrun notarytool store-credentials ...).
#                         When set (and a signing identity is given) the DMG is notarized and stapled.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

UNIVERSAL=0
FEATURES=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --universal) UNIVERSAL=1; shift ;;
    --features) FEATURES="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/version = "(.*)"/\1/')"
DIST="$ROOT/dist"
APP="$DIST/DBiji.app"
FEATURE_ARGS=()
if [[ -n "$FEATURES" ]]; then FEATURE_ARGS=(--features "$FEATURES"); fi

echo "==> Building DBiji $VERSION (release)"
if [[ $UNIVERSAL -eq 1 ]]; then
  rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
  cargo build --release -p dbiji-app --target aarch64-apple-darwin ${FEATURE_ARGS[@]+"${FEATURE_ARGS[@]}"}
  cargo build --release -p dbiji-app --target x86_64-apple-darwin ${FEATURE_ARGS[@]+"${FEATURE_ARGS[@]}"}
  mkdir -p "$DIST"
  lipo -create -output "$DIST/dbiji" \
    target/aarch64-apple-darwin/release/dbiji \
    target/x86_64-apple-darwin/release/dbiji
  BIN="$DIST/dbiji"
else
  cargo build --release -p dbiji-app ${FEATURE_ARGS[@]+"${FEATURE_ARGS[@]}"}
  BIN="$ROOT/target/release/dbiji"
fi

echo "==> Creating bundle"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/dbiji"

# Icon: render the SVG with AppKit (transparent corners; QuickLook thumbnails
# have a white background), then build an .icns with iconutil.
ICONSET="$DIST/DBiji.iconset"
rm -rf "$ICONSET" && mkdir -p "$ICONSET"
TMP_ICON="$(mktemp -d)"
SRC_PNG="$TMP_ICON/icon.png"
swift scripts/render-icon.swift crates/dbiji-app/assets/icon.svg "$SRC_PNG" 1024
for size in 16 32 64 128 256 512; do
  sips -z $size $size "$SRC_PNG" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z $double $double "$SRC_PNG" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/DBiji.icns"
rm -rf "$ICONSET" "$TMP_ICON"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>DBiji</string>
  <key>CFBundleDisplayName</key><string>DBiji</string>
  <key>CFBundleIdentifier</key><string>dev.dbiji.DBiji</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleExecutable</key><string>dbiji</string>
  <key>CFBundleIconFile</key><string>DBiji</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
  <key>NSRequiresAquaSystemAppearance</key><false/>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHumanReadableCopyright</key><string>© 2026 Tommy Lumos Empire. MIT OR Apache-2.0.</string>
</dict>
</plist>
PLIST

echo "==> Signing"
if [[ -n "${DBIJI_SIGN_IDENTITY:-}" ]]; then
  codesign --force --options runtime --timestamp \
    --entitlements packaging/macos/entitlements.plist \
    --sign "$DBIJI_SIGN_IDENTITY" "$APP"
else
  codesign --force --sign - "$APP"
fi
codesign --verify --verbose=2 "$APP"

echo "==> Creating the update archive"
# The in-app updater downloads this (the release asset DBiji-<version>.zip).
ZIP="$DIST/DBiji-${VERSION}.zip"
rm -f "$ZIP"
ditto -c -k --sequesterRsrc --keepParent "$APP" "$ZIP"

echo "==> Creating DMG"
DMG="$DIST/DBiji-${VERSION}.dmg"
rm -f "$DMG"
STAGE="$(mktemp -d)"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "DBiji ${VERSION}" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
rm -rf "$STAGE"

if [[ -n "${DBIJI_SIGN_IDENTITY:-}" ]]; then
  codesign --force --sign "$DBIJI_SIGN_IDENTITY" "$DMG"
  if [[ -n "${DBIJI_NOTARY_PROFILE:-}" ]]; then
    echo "==> Notarizing"
    xcrun notarytool submit "$DMG" --keychain-profile "$DBIJI_NOTARY_PROFILE" --wait
    xcrun stapler staple "$DMG"
  fi
fi

SIZE=$(du -h "$APP/Contents/MacOS/dbiji" | cut -f1)
echo "==> Done: $DMG (binary size $SIZE)"
shasum -a 256 "$DMG" "$ZIP"
