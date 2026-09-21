#!/usr/bin/env bash
#
# Build, sign (hardened runtime + Developer ID), notarize, staple and package a
# Local Dictation release, then write the Sparkle appcast for the update archive.
#
# This is the one command that turns a clean commit into a publishable release.
# It never uploads: publication is a separate, explicitly gated step
# (publish.sh). In --dry-run it prints the full plan and changes nothing.
#
# Inputs (env):
#   SIGN_IDENTITY        Developer ID Application identity (required)
#   SU_FEED_URL          HTTPS appcast URL (required)
#   SU_PUBLIC_ED_KEY     base64 Ed25519 public key (required)
#   SU_PRIVATE_KEY / _FILE  the matching seed, for signing the archive
#   NOTARY_PROFILE       or NOTARY_APPLE_ID/NOTARY_TEAM_ID/NOTARY_PASSWORD
#   SPARKLE_FRAMEWORK    path to Sparkle.framework to embed (required for a
#                        real update feed; optional in --dry-run)
#   RELEASE_MODELS       bundled (default, self-contained) | slim
#   RELEASE_UPLOAD_BASE  the HTTPS base URL the archives will be served from
#   RELEASE_NOTES_URL    optional release-notes link
#
# Outputs, under dist/releases/:
#   Local-Dictation-<ver>.zip   the update archive the appcast points at
#   appcast.xml                 the feed (previous kept as appcast.xml.prev)
#   Local-Dictation-<ver>.dmg   optional manual-install image (RELEASE_DMG=1)

set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

DRY=0
SKIP_NOTARIZE=0
for arg in "$@"; do
  case "$arg" in
    --dry-run)       DRY=1 ;;
    --skip-notarize) SKIP_NOTARIZE=1 ;;
    *) die "unknown flag: $arg" ;;
  esac
done
[ "$DRY" -eq 1 ] && export LD_RELEASE_DRY_RUN=1

# A dry run reports the plan and is allowed on a working tree; a real release
# must come from a clean commit.
if [ "$DRY" -eq 0 ]; then
  require_clean_tree
fi
require_developer_id
require_env SU_FEED_URL "the HTTPS appcast URL"
require_env SU_PUBLIC_ED_KEY "the base64 Ed25519 public key"
case "$SU_FEED_URL" in https://*) ;; *) die "SU_FEED_URL must be HTTPS" ;; esac

VERSION="$(repo_version)"
COMMIT="$(repo_commit)"
MODELS="${RELEASE_MODELS:-bundled}"   # bundled = a stranger can run it
ARCHIVE="$RELEASES_DIR/Local-Dictation-$VERSION.zip"
APPCAST="$RELEASES_DIR/appcast.xml"
UPLOAD_BASE="${RELEASE_UPLOAD_BASE:-https://example.invalid/local-dictation/releases}"
DOWNLOAD_URL="$UPLOAD_BASE/Local-Dictation-$VERSION.zip"

step "release plan: v$VERSION ($COMMIT)"
log "models: $MODELS"
log "update archive: $ARCHIVE"
log "download url:   $DOWNLOAD_URL"
log "appcast:        $APPCAST"

if [ "$DRY" -eq 1 ]; then
  step "dry run: printing the steps that a real release would take"
  printf '  scripts/build-app.sh --release --%s\n' \
    "$([ "$MODELS" = slim ] && echo slim || echo bundle-models)" >&2
  printf '  ditto -c -k --keepParent %q %q\n' "$APP_BUNDLE" "$RELEASES_DIR/notarize.zip" >&2
  printf '  scripts/release/notarize.sh   (TARGET=%q)\n' "$RELEASES_DIR/notarize.zip" >&2
  printf '  xcrun stapler staple %q\n' "$APP_BUNDLE" >&2
  printf '  ditto -c -k --keepParent %q %q\n' "$APP_BUNDLE" "$ARCHIVE" >&2
  printf '  scripts/release/make-appcast.sh\n' >&2
  printf '  scripts/release/verify-release.sh\n' >&2
  ok "dry run complete: nothing was built, signed, notarized or uploaded"
  exit 0
fi

# ── 1. build, hardened runtime + Developer ID ────────────────────────────────
step "building and signing the app"
if [ "$MODELS" = "slim" ]; then
  "$REPO_ROOT/scripts/build-app.sh" --release --slim
else
  "$REPO_ROOT/scripts/build-app.sh" --release --bundle-models
fi

# ── 2. notarize the app inside a zip, then staple the .app ───────────────────
if [ "$SKIP_NOTARIZE" -eq 0 ]; then
  step "notarizing"
  NOTARIZE_ZIP="$RELEASES_DIR/notarize-$VERSION.zip"
  ditto -c -k --keepParent "$APP_BUNDLE" "$NOTARIZE_ZIP"
  TARGET="$NOTARIZE_ZIP" "$REPO_ROOT/scripts/release/notarize.sh"
  rm -f "$NOTARIZE_ZIP"
  xcrun stapler validate "$APP_BUNDLE"
else
  warn "skipping notarization (RELEASE without notarization is not fit to publish)"
fi

# ── 3. the update archive (Sparkle encloses this) ────────────────────────────
step "packaging the update archive"
mkdir -p "$RELEASES_DIR"
rm -f "$ARCHIVE"
ditto -c -k --sequesterRsrc --keepParent "$APP_BUNDLE" "$ARCHIVE"
log "archive: $(du -h "$ARCHIVE" | cut -f1)"

# ── 4. the appcast ───────────────────────────────────────────────────────────
step "writing the appcast"
ARTIFACT="$ARCHIVE" \
DOWNLOAD_URL="$DOWNLOAD_URL" \
FEED_URL="$SU_FEED_URL" \
RELEASE_NOTES_URL="${RELEASE_NOTES_URL:-}" \
OUT="$APPCAST" \
VERSION="$VERSION" SHORT_VERSION="$VERSION" \
  "$REPO_ROOT/scripts/release/make-appcast.sh"

# ── 5. optional DMG for manual installs ──────────────────────────────────────
# Built from the already-stapled app (never rebuilt), then notarized and
# stapled in its own right. The appcast still points at the ZIP: the DMG is a
# convenience for a human download, not the update artifact.
if [ "${RELEASE_DMG:-0}" = "1" ]; then
  step "building the DMG from the stapled app"
  DMG="$RELEASES_DIR/Local-Dictation-$VERSION.dmg"
  STAGE="$(mktemp -d -t ld-dmg)"
  cp -R "$APP_BUNDLE" "$STAGE/$APP_NAME.app"
  ln -s /Applications "$STAGE/Applications"
  rm -f "$DMG"
  hdiutil create -volname "$APP_NAME" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
  rm -rf "$STAGE"
  if [ "$SKIP_NOTARIZE" -eq 0 ]; then
    TARGET="$DMG" "$REPO_ROOT/scripts/release/notarize.sh"
  fi
  log "dmg: $DMG ($(du -h "$DMG" | cut -f1))"
fi

# ── 6. verify ────────────────────────────────────────────────────────────────
step "verifying the release"
TARGET="$APP_BUNDLE" APPCAST="$APPCAST" ARTIFACT="$ARCHIVE" \
  "$REPO_ROOT/scripts/release/verify-release.sh"

ok "release built: v$VERSION ($COMMIT)"
log "publish it with: ALLOW_PUBLISH=1 RELEASE_UPLOAD_CMD='…' scripts/release/publish.sh"
