#!/usr/bin/env bash
#
# Sign one update archive and (re)write the Sparkle appcast for it.
#
# Inputs (env):
#   ARTIFACT            the .zip/.dmg to publish (required)
#   DOWNLOAD_URL        HTTPS URL the artifact will live at (required)
#   VERSION             sparkle:version / CFBundleVersion (default: Cargo.toml)
#   SHORT_VERSION       sparkle:shortVersionString (default: VERSION)
#   MINIMUM_SYSTEM      sparkle:minimumSystemVersion (default: 14.0.0; three-part)
#   FEED_URL            the appcast's own HTTPS URL (channel <link>)
#   RELEASE_NOTES_URL   optional
#   OUT                 where to write the appcast (default dist/releases/appcast.xml)
#   MERGE               existing appcast to keep older items from (default: OUT if it exists)
#   SIGN_UPDATE         path to Sparkle's official sign_update (preferred)
#   SU_PRIVATE_KEY      base64 Ed25519 seed (env)         } one of these two,
#   SU_PRIVATE_KEY_FILE path to a 0600 file with the seed } unless SIGN_UPDATE
#
# A previous appcast is preserved as <OUT>.prev for --rollback.

set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

ARTIFACT="${ARTIFACT:-}"
OUT="${OUT:-$RELEASES_DIR/appcast.xml}"
# Default to merging the appcast already at OUT, if there is one. Do this with
# an explicit branch: `$(...)` whose body short-circuits to a failing status
# would abort the script under `set -e`.
MERGE="${MERGE:-}"
if [ -z "$MERGE" ] && [ -f "$OUT" ]; then
  MERGE="$OUT"
fi

require_env ARTIFACT "the archive to sign and publish"
require_env DOWNLOAD_URL "the HTTPS URL the archive will be served from"
[ -f "$ARTIFACT" ] || die "artifact not found: $ARTIFACT"
VERSION="${VERSION:-$(repo_version)}"
SHORT_VERSION="${SHORT_VERSION:-$VERSION}"
MINIMUM_SYSTEM="${MINIMUM_SYSTEM:-14.0.0}"

case "$DOWNLOAD_URL" in
  https://*) ;;
  *) die "DOWNLOAD_URL must be HTTPS: $DOWNLOAD_URL" ;;
esac

mkdir -p "$(dirname "$OUT")"

# ── 1. signature ──────────────────────────────────────────────────────────────
step "signing $(basename "$ARTIFACT")"
SIGNATURE=""
if [ -n "${SIGN_UPDATE:-}" ]; then
  [ -x "$SIGN_UPDATE" ] || die "SIGN_UPDATE is not executable: $SIGN_UPDATE"
  log "using the official Sparkle sign_update"
  SIGN_OUTPUT="$("$SIGN_UPDATE" "$ARTIFACT")"
  SIGNATURE="$(printf '%s' "$SIGN_OUTPUT" | sed -nE 's/.*sparkle:edSignature="([^"]+)".*/\1/p')"
  if [ -z "$SIGNATURE" ]; then
    SIGNATURE="$(printf '%s' "$SIGN_OUTPUT" | grep -oE '[A-Za-z0-9+/]{80,}={0,2}' | head -1)"
  fi
  [ -n "$SIGNATURE" ] || die "could not parse a signature from sign_update output"
else
  log "using the built-in CryptoKit Ed25519 signer (Sparkle-compatible fallback)"
  need_cmd swift
  SIGNER="$REPO_ROOT/scripts/release/sparkle_sign.swift"
  if [ -n "${SU_PRIVATE_KEY_FILE:-}" ] && [ -z "${SU_PRIVATE_KEY:-}" ]; then
    SIGNATURE="$(swift "$SIGNER" sign "$ARTIFACT" --key "$SU_PRIVATE_KEY_FILE")"
  else
    KEY_MATERIAL="$(private_key_material)"
    SIGNATURE="$(printf '%s' "$KEY_MATERIAL" | swift "$SIGNER" sign "$ARTIFACT" --key -)"
  fi
fi

# The public key is public: it is only refused when it is absent.
PUBKEY="${SU_PUBLIC_ED_KEY:-}"
if [ -z "$PUBKEY" ] && [ -n "${SU_PRIVATE_KEY_FILE:-}" ] && [ -z "${SU_PRIVATE_KEY:-}" ]; then
  PUBKEY="$(swift "$REPO_ROOT/scripts/release/sparkle_sign.swift" pubkey --key "$SU_PRIVATE_KEY_FILE")"
fi
if [ -n "$PUBKEY" ]; then
  swift "$REPO_ROOT/scripts/release/sparkle_sign.swift" verify "$ARTIFACT" "$SIGNATURE" "$PUBKEY" >/dev/null \
    || die "the produced signature does not verify against SU_PUBLIC_ED_KEY (key/signature mismatch)"
  log "signature verifies against the configured public key"
fi

# ── 2. appcast ────────────────────────────────────────────────────────────────
step "writing the appcast"
if [ -f "$OUT" ] && [ -n "$MERGE" ] && [ "$MERGE" = "$OUT" ]; then
  cp -f "$OUT" "$OUT.prev"
  log "kept the previous appcast at $(basename "$OUT").prev"
fi

APPCAST_ARGS=(
  --artifact "$ARTIFACT"
  --url "$DOWNLOAD_URL"
  --signature "$SIGNATURE"
  --version "$VERSION"
  --short-version "$SHORT_VERSION"
  --minimum-system "$MINIMUM_SYSTEM"
  --out "$OUT"
)
[ -n "${FEED_URL:-}" ] && APPCAST_ARGS+=(--feed-url "$FEED_URL")
[ -n "${RELEASE_NOTES_URL:-}" ] && APPCAST_ARGS+=(--release-notes-url "$RELEASE_NOTES_URL")
[ -n "$MERGE" ] && APPCAST_ARGS+=(--merge "$MERGE")
python3 "$REPO_ROOT/scripts/release/appcast.py" build "${APPCAST_ARGS[@]}"

# ── 3. validate ───────────────────────────────────────────────────────────────
step "validating the appcast"
python3 "$REPO_ROOT/scripts/release/appcast.py" check "$OUT" --artifact "$ARTIFACT" --signature "$SIGNATURE" \
  || die "the generated appcast failed its contract check"
ok "appcast ready: $OUT"
