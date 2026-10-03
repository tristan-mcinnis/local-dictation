#!/usr/bin/env bash
#
# Package Local Dictation as a free GitHub-release DMG. No Apple Developer ID.
#
# Builds the slim app with ad-hoc signing, wraps it in a compressed DMG, checks
# the result, and writes the files a release needs. It never uploads anything.
#
# Usage:
#   scripts/make-dmg.sh                 # slim build (no models), arm64
#   RELEASE_OUT=/some/dir scripts/make-dmg.sh
#
# Env:
#   RELEASE_OUT      output folder (default dist/github-release)
#   RELEASE_MODELS   slim (default) | bundled (adds ~1.9 GB; GitHub caps an
#                    asset at 2 GiB, so slim is the supported choice)
#   ALLOW_DIRTY=1    allow an uncommitted tree (the build is stamped -dirty)
#
# Outputs in RELEASE_OUT:
#   Local-Dictation-<ver>-macos-arm64.dmg   the download (no spaces in the name)
#   SHA256SUMS                              checked with shasum -a 256 -c
#   RELEASE_NOTES.md                        from docs/release/*.md templates
#   VERIFY.txt                              scripts/verify-dmg.sh output
#
# This is the free path. The Developer ID, notarized, Sparkle path is in
# scripts/release/ and stays untouched. See docs/release/github-release.md.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

APP_NAME="Local Dictation"
GH_REPO="tristan-mcinnis/local-dictation"
DIST="$REPO_ROOT/dist"
APP="$DIST/$APP_NAME.app"
OUT="${RELEASE_OUT:-$DIST/github-release}"
MODELS="${RELEASE_MODELS:-slim}"

log() { printf '• %s\n' "$*" >&2; }
die() { printf '✗ %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = "Darwin" ] || die "this packages a macOS app; run it on a Mac"
[ "$(uname -m)" = "arm64" ] || die "run on an Apple Silicon Mac so the build is arm64"
case "$MODELS" in slim) MODEL_FLAG="--slim" ;; bundled) MODEL_FLAG="--bundle-models" ;; *) die "RELEASE_MODELS must be slim or bundled" ;; esac

if [ -n "$(git status --porcelain 2>/dev/null)" ] && [ "${ALLOW_DIRTY:-0}" != "1" ]; then
  git status --short >&2
  die "the working tree is dirty; commit first, or set ALLOW_DIRTY=1 (the build is then stamped -dirty)"
fi

# A free build is ad hoc and never carries Sparkle. Clear anything the shell
# might hand to build-app.sh that would turn updates on or sign differently.
unset SU_FEED_URL SU_PUBLIC_ED_KEY SPARKLE_FRAMEWORK
export SIGN_IDENTITY="-"

# 1. Build and sign (build-app.sh signs inside-out, ad hoc, no --deep).
log "building $APP_NAME ($MODEL_FLAG, ad-hoc signed)"
"$REPO_ROOT/scripts/build-app.sh" "$MODEL_FLAG"
[ -d "$APP" ] || die "build produced no app at $APP"

# 2. Name the file from the app's own Info.plist, and check it against Cargo.
VERSION="$(plutil -extract CFBundleShortVersionString raw "$APP/Contents/Info.plist")"
CARGO_VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
[ "$VERSION" = "$CARGO_VERSION" ] || die "Info.plist version $VERSION differs from Cargo.toml $CARGO_VERSION"
COMMIT="$(plutil -extract LocalDictationBuiltFromCommit raw "$APP/Contents/Info.plist")"
DMG_NAME="Local-Dictation-$VERSION-macos-arm64.dmg"
DMG="$OUT/$DMG_NAME"

# 3. Stage: the app plus an /Applications symlink. ditto keeps the signature
#    and the seal intact (cp -R is not guaranteed to).
mkdir -p "$OUT"
STAGE="$(mktemp -d -t ld-dmg)"
trap 'rm -rf "$STAGE"' EXIT
ditto "$APP" "$STAGE/$APP_NAME.app"
ln -s /Applications "$STAGE/Applications"

# 4. Compressed, read-only image.
rm -f "$DMG"
log "creating $DMG_NAME"
hdiutil create -volname "$APP_NAME" -srcfolder "$STAGE" -ov -format UDZO -fs HFS+ \
  -imagekey zlib-level=9 "$DMG" >/dev/null

# 5. Checksums, read back to prove they match.
( cd "$OUT" && shasum -a 256 "$DMG_NAME" > SHA256SUMS && shasum -a 256 -c SHA256SUMS >/dev/null )
SHA="$(awk '{print $1}' "$OUT/SHA256SUMS")"
SIZE="$(du -h "$DMG" | awk '{print $1}')"

# 6. Release notes from the committed templates.
python3 - "$OUT/RELEASE_NOTES.md" "$VERSION" "$DMG_NAME" "$SHA" "$SIZE" "$COMMIT" <<'PY'
import pathlib, sys
out, version, dmg, sha, size, commit = sys.argv[1:7]
root = pathlib.Path("docs/release")
text = (root / "RELEASE_NOTES.template.md").read_text()
subs = {
    "@WHATS_NEW@": (root / "WHATS_NEW.md").read_text().strip(),
    "@FIRST_OPEN@": (root / "first-open.md").read_text().strip(),
    "@VERSION@": version, "@DMG@": dmg, "@SHA256@": f"{sha}  {dmg}",
    "@SIZE@": size, "@COMMIT@": commit,
}
for key, value in subs.items():
    text = text.replace(key, value)
left = [t for t in subs if t in text]
if left:
    sys.exit(f"unfilled placeholders: {left}")
pathlib.Path(out).write_text(text)
PY

# 7. Verify the artifact without launching the app.
log "verifying the DMG"
"$REPO_ROOT/scripts/verify-dmg.sh" "$DMG" | tee "$OUT/VERIFY.txt" >&2

echo >&2
log "dmg:    $DMG ($SIZE)"
log "sha256: $SHA"
log "models: $MODELS"
echo >&2
echo "Draft the release (run it yourself; this script never pushes):" >&2
echo "  gh release create v$VERSION --repo $GH_REPO --draft --title \"$APP_NAME $VERSION\" --notes-file \"$OUT/RELEASE_NOTES.md\" \"$DMG\" \"$OUT/SHA256SUMS\"" >&2
