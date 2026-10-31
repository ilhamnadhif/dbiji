#!/usr/bin/env bash
# Publishes the release of the pushed tag from CI (the publish job, after the
# tests passed and the bundle jobs built dist/) and points the Homebrew cask at
# its DMG.
# Installed copies are offered the release once it is published.
#
#   GITHUB_REF_NAME=v1.2.3 GH_TOKEN=... scripts/ci-publish.sh
#
# A release that scripts/release.sh already published is left as it is; the
# cask then gets the published DMG's checksum.
set -euo pipefail

cd "$(dirname "$0")/.."

TAG="${GITHUB_REF_NAME:?the tag to publish}"
VERSION="${TAG#v}"
BRANCH="master"
# macOS (the updater installs the .zip), Windows, and Linux for x86_64 and aarch64.
FILES=("dist/DBiji-$VERSION.dmg" "dist/DBiji-$VERSION.zip"
    "dist/DBiji-$VERSION-windows-x64-setup.exe" "dist/DBiji-$VERSION-windows-x64.zip")
for arch in x86_64 aarch64; do
    case "$arch" in x86_64) deb=amd64 ;; aarch64) deb=arm64 ;; esac
    FILES+=("dist/DBiji-$VERSION-linux-$arch.tar.gz" "dist/DBiji-$VERSION-linux-$arch.AppImage"
        "dist/DBiji-$VERSION-linux-$deb.deb" "dist/DBiji-$VERSION-linux-$arch.rpm"
        "dist/DBiji-$VERSION-linux-$arch.pkg.tar.zst")
done
CASK="packaging/homebrew/dbiji.rb"

fail() { echo "Error: $*" >&2; exit 1; }
grep -q "^version = \"$VERSION\"" Cargo.toml || fail "Cargo.toml is not at version $VERSION"
grep -q "^## $VERSION " CHANGELOG.md || fail "CHANGELOG.md has no section for $VERSION"
for f in "${FILES[@]}"; do [ -f "$f" ] || fail "missing $f"; done

STATE="$(gh release view "$TAG" --json isDraft --jq .isDraft 2>/dev/null || echo none)"
if [ "$STATE" = "false" ]; then
    echo "==> $TAG is already published"
else
    NOTES="$(mktemp)"
    awk -v v="$VERSION" '$0 ~ "^## " v " " {on=1; next} on && /^## / {exit} on' CHANGELOG.md >"$NOTES"
    echo "==> Draft release"
    if [ "$STATE" = "none" ]; then
        gh release create "$TAG" --draft --verify-tag --title "DBiji $VERSION" --notes-file "$NOTES" "${FILES[@]}"
    else
        gh release upload "$TAG" --clobber "${FILES[@]}"
    fi
    echo "==> Publish"
    gh release edit "$TAG" --draft=false --latest
fi

echo "==> Homebrew cask"
DOWNLOAD="$(mktemp -d)"
gh release download "$TAG" --pattern "DBiji-$VERSION.dmg" --dir "$DOWNLOAD"
SHA="$(shasum -a 256 "$DOWNLOAD/DBiji-$VERSION.dmg" | cut -d' ' -f1)"
git fetch --quiet origin "$BRANCH"
git checkout --quiet -B "$BRANCH" FETCH_HEAD
sed -i '' -e "s/^  version \".*\"/  version \"$VERSION\"/" -e "s/^  sha256 \".*\"/  sha256 \"$SHA\"/" "$CASK"
if git diff --quiet -- "$CASK"; then
    echo "The cask is at $VERSION already"
else
    git -c user.name="github-actions[bot]" -c user.email="41898282+github-actions[bot]@users.noreply.github.com" \
        commit --quiet -m "Homebrew cask: $VERSION" -- "$CASK"
    git push --quiet origin "$BRANCH"
fi
echo "==> Done: $(gh release view "$TAG" --json url --jq .url)"
