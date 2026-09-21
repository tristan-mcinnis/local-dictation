#!/usr/bin/env bash
#
# Notarize and staple a distribution artifact, then verify Gatekeeper accepts
# it. This wraps the four commands Apple actually requires; it creates no
# credentials and stores none.
#
# Inputs (env):
#   TARGET            a .app, .zip or .dmg to notarize (required)
#   NOTARY_PROFILE    a notarytool keychain profile (preferred), or all three of
#   NOTARY_APPLE_ID / NOTARY_TEAM_ID / NOTARY_PASSWORD
#   STAPLE=1          also staple (default 1 for .app/.dmg; a .zip is never
#                     stapled — staple the .app before zipping it)
#
# In dry-run mode nothing is submitted: the exact command is printed, the
# credential shape is validated, and the script exits 0. Publication happens
# only from a non-dry run with real credentials.

set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

TARGET="${TARGET:-}"
[ -n "$TARGET" ] || die "TARGET is not set (a .app, .zip or .dmg)"
[ -e "$TARGET" ] || die "TARGET does not exist: $TARGET"
STAPLE="${STAPLE:-1}"

need_cmd xcrun

# ── credential shape (never printed) ──────────────────────────────────────────
if [ -n "${NOTARY_PROFILE:-}" ]; then
  CREDENTIALS=(--keychain-profile "$NOTARY_PROFILE")
  CREDENTIAL_SHAPE="keychain profile '$NOTARY_PROFILE'"
else
  require_env NOTARY_APPLE_ID "the Apple ID used for notarization"
  require_env NOTARY_TEAM_ID "the Apple Developer team id"
  require_env NOTARY_PASSWORD "an app-specific password (or @keychain: reference)"
  CREDENTIALS=(--apple-id "$NOTARY_APPLE_ID" --team-id "$NOTARY_TEAM_ID" --password "$NOTARY_PASSWORD")
  CREDENTIAL_SHAPE="Apple ID + team id + app-specific password"
fi
log "notarization credentials: $CREDENTIAL_SHAPE"

SUBMIT=(xcrun notarytool submit "$TARGET" "${CREDENTIALS[@]}" --wait)

if is_dry_run; then
  step "dry run: not submitting to Apple"
  printf '  would run: xcrun notarytool submit %q %s --wait\n' "$TARGET" "$CREDENTIAL_SHAPE" >&2
  if [ "$STAPLE" = "1" ] && [ "${TARGET##*.}" != "zip" ]; then
    printf '  would run: xcrun stapler staple %q\n' "$TARGET" >&2
    printf '  would run: xcrun stapler validate %q\n' "$TARGET" >&2
  fi
  ok "notarization dry run complete (nothing submitted)"
  exit 0
fi

# ── real notarization ─────────────────────────────────────────────────────────
step "submitting $(basename "$TARGET") for notarization (this can take minutes)"
"${SUBMIT[@]}"

if [ "$STAPLE" = "1" ]; then
  case "$TARGET" in
    *.zip|*.pkg)
      log "a $TARGET cannot be stapled; staple the .app inside it before zipping"
      ;;
    *)
      step "stapling"
      xcrun stapler staple "$TARGET"
      xcrun stapler validate "$TARGET"
      ;;
  esac
fi

step "Gatekeeper assessment"
spctl --assess --type execute -vv "$TARGET" || spctl --assess -vv "$TARGET"
ok "notarization complete: $TARGET"
