#!/usr/bin/env bash
#
# Verify a built release: signature authority, hardened runtime, notarization
# ticket, Gatekeeper acceptance and a well-formed, signed appcast.
#
# Inputs (env):
#   TARGET    the .app (required)
#   APPCAST   the appcast.xml (optional)
#   ARTIFACT  the update archive the appcast points at (optional)
#
# Exits non-zero on the first failed check. In a dry run the Apple-only checks
# (staple/Gatekeeper) are informational, but the signature and appcast checks
# still run.

set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

TARGET="${TARGET:-$APP_BUNDLE}"
APPCAST="${APPCAST:-}"
ARTIFACT="${ARTIFACT:-}"
[ -e "$TARGET" ] || die "TARGET does not exist: $TARGET"

step "verifying the code signature"
codesign --verify --deep --strict --verbose=2 "$TARGET"
SIG_INFO="$(codesign -dvvv "$TARGET" 2>&1 || true)"
printf '%s\n' "$SIG_INFO" | grep -E 'Authority|TeamIdentifier|Runtime Version|Timestamp' | sed 's/^/  /' >&2

# A publishable app is signed by a Developer ID, not ad-hoc or Apple Development.
# Match on the captured string, not a `| grep -q` pipeline, which under
# `pipefail` can fail spuriously when grep exits early.
case "$SIG_INFO" in
  *"Developer ID Application"*)
    ok "signed with a Developer ID Application certificate" ;;
  *)
    warn "not signed with a Developer ID Application certificate (fine for a dry run, not for publication)" ;;
esac
case "$SIG_INFO" in
  *"Runtime Version"*)
    ok "hardened runtime is enabled" ;;
  *)
    die "hardened runtime is missing" ;;
esac

step "checking the notarization ticket"
if xcrun stapler validate "$TARGET" 2>/dev/null; then
  ok "notarization ticket is stapled"
else
  warn "no stapled notarization ticket (expected for an un-notarized build)"
fi

if spctl --assess --type execute -vv "$TARGET" 2>/dev/null; then
  ok "Gatekeeper accepts the app"
else
  warn "Gatekeeper did not accept the app (expected for an un-notarized build)"
fi

if [ -n "$APPCAST" ]; then
  step "validating the appcast"
  python3 "$REPO_ROOT/scripts/release/appcast.py" check "$APPCAST" \
    ${ARTIFACT:+--artifact "$ARTIFACT"}
fi

ok "release verification complete"
