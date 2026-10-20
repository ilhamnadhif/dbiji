#!/usr/bin/env bash
# Publishes the version in Cargo.toml as a GitHub release. Installed copies
# that check for updates offer it at their next start (or within a few hours)
# and install it in place from DBiji-<version>.zip.
#
#   scripts/release.sh            # notes from this version's CHANGELOG.md section
#   scripts/release.sh notes.md   # notes from a file
#
# Set DBIJI_SIGN_IDENTITY and DBIJI_NOTARY_PROFILE (see bundle-macos.sh) for
# Developer ID signed, notarized builds; DBIJI_SKIP_TESTS=1 leaves the tests
# to CI.
#
# Steps: checks → tests → build (.app, .dmg, .zip) → tag → push → draft
# release with the files → publish. Users see nothing before the last step;
# if a step fails, run it again.
set -euo pipefail

cd "$(dirname "$0")/.."
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

NOTES_FILE="${1:-}"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
TAG="v$VERSION"
BRANCH="master"
FILES=("dist/DBiji-$VERSION.dmg" "dist/DBiji-$VERSION.zip")

fail() { echo "Error: $*" >&2; exit 1; }
command -v gh >/dev/null || fail "the GitHub CLI (gh) is needed: brew install gh"
gh auth status >/dev/null 2>&1 || fail "log in first: gh auth login"
[ "$(git symbolic-ref --quiet --short HEAD || true)" = "$BRANCH" ] || fail "releases are made from $BRANCH"
# Untracked files (notes, crash reports …) are not part of the release.
[ -z "$(git status --porcelain --untracked-files=no)" ] || fail "commit or stash your changes first"
[ -z "$NOTES_FILE" ] || [ -f "$NOTES_FILE" ] || fail "no such notes file: $NOTES_FILE"
grep -q "^## $VERSION " CHANGELOG.md || fail "CHANGELOG.md has no section for $VERSION"
git fetch --quiet origin "$BRANCH"
git merge-base --is-ancestor "origin/$BRANCH" HEAD || fail "$BRANCH is behind origin/$BRANCH: pull first"

STATE="$(gh release view "$TAG" --json isDraft --jq .isDraft 2>/dev/null || echo none)"
[ "$STATE" != "false" ] || fail "$TAG is already published"

NOTES="$(mktemp)"
trap 'rm -f "$NOTES"' EXIT
if [ -n "$NOTES_FILE" ]; then
    cp "$NOTES_FILE" "$NOTES"
else
    awk -v v="$VERSION" '$0 ~ "^## " v " " {on=1; next} on && /^## / {exit} on' CHANGELOG.md >"$NOTES"
fi

if [ -z "${DBIJI_SKIP_TESTS:-}" ]; then
    echo "==> Tests"
    cargo test --workspace --quiet
fi

echo "==> Build"
scripts/bundle-macos.sh --universal
for f in "${FILES[@]}"; do [ -f "$f" ] || fail "missing $f"; done

echo "==> Tag $TAG"
git rev-parse -q --verify "refs/tags/$TAG" >/dev/null || git tag -a "$TAG" -m "DBiji $VERSION"
git push --quiet origin "$BRANCH"
# A tag alone shows users nothing: they are offered the published release.
git push --quiet origin "$TAG"

echo "==> Draft release"
if [ "$STATE" = "none" ]; then
    gh release create "$TAG" --draft --title "DBiji $VERSION" --notes-file "$NOTES" "${FILES[@]}"
else
    gh release upload "$TAG" --clobber "${FILES[@]}"
fi

echo "==> Publish"
gh release edit "$TAG" --draft=false --latest
echo "==> Done: $(gh release view "$TAG" --json url --jq .url)"
