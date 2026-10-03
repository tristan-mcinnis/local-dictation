#!/usr/bin/env bash
#
# Verify a Local Dictation release DMG without launching the app.
#
# Usage: scripts/verify-dmg.sh path/to/Local-Dictation-<ver>-macos-arm64.dmg
#
# Mounts the image read-only and hidden, checks the bundle, and detaches. Prints
# one PASS/FAIL line per check and exits 1 on any FAIL. A spctl rejection is
# expected for an ad-hoc build and is recorded, not failed.

set -uo pipefail

DMG="${1:?usage: verify-dmg.sh <dmg>}"
[ -f "$DMG" ] || { echo "no such file: $DMG" >&2; exit 2; }

PASS=0; FAIL=0
pass() { echo "PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "FAIL: $1"; FAIL=$((FAIL + 1)); }
check() { local desc="$1"; shift; if "$@" >/dev/null 2>&1; then pass "$desc"; else fail "$desc"; fi; }

APP_NAME="Local Dictation"
EXEC_NAME="local-dictation"
MNT="$(mktemp -d -t ld-verify)"
ATTACHED=0
cleanup() {
  if [ "$ATTACHED" -eq 1 ]; then hdiutil detach "$MNT" -force >/dev/null 2>&1 || true; fi
  rmdir "$MNT" 2>/dev/null || true
}
trap cleanup EXIT

base="$(basename "$DMG")"
case "$base" in
  *" "*) fail "file name has no spaces ($base)" ;;
  Local-Dictation-*-macos-arm64.dmg) pass "file name shape ($base)" ;;
  *) fail "file name shape ($base)" ;;
esac
name_version="${base#Local-Dictation-}"; name_version="${name_version%-macos-arm64.dmg}"

check "hdiutil verify (image checksum)" hdiutil verify "$DMG"
fmt="$(hdiutil imageinfo "$DMG" 2>/dev/null | awk -F': ' '/^Format:/ {print $2; exit}')"
[ "$fmt" = "UDZO" ] && pass "image format is UDZO" || fail "image format is '$fmt', want UDZO"

if hdiutil attach -nobrowse -readonly -noautoopen -mountpoint "$MNT" "$DMG" >/dev/null 2>&1; then
  ATTACHED=1; pass "attached read-only, hidden"
else
  fail "could not attach the image"; echo "result: $PASS passed, $FAIL failed"; exit 1
fi

APP="$MNT/$APP_NAME.app"
CONTENTS="$APP/Contents"
RES="$CONTENTS/Resources"
[ -d "$APP" ] && pass "$APP_NAME.app is present" || { fail "$APP_NAME.app is missing"; echo "result: $PASS passed, $FAIL failed"; exit 1; }
[ -L "$MNT/Applications" ] && [ "$(readlink "$MNT/Applications")" = "/Applications" ] \
  && pass "/Applications symlink" || fail "/Applications symlink"

# Signature: the app, then the nested helper, then the whole seal, strictly.
check "codesign --verify --strict (app)" codesign --verify --strict --verbose=2 "$APP"
check "codesign --verify --deep --strict (app and nested code)" codesign --verify --deep --strict "$APP"
if [ -f "$RES/cohere-mlx-bridge" ]; then
  check "codesign --verify --strict (cohere-mlx-bridge)" codesign --verify --strict --verbose=2 "$RES/cohere-mlx-bridge"
else
  fail "cohere-mlx-bridge is missing"
fi
sig="$(codesign -dvv "$APP" 2>&1 || true)"
case "$sig" in *"Signature=adhoc"*) pass "signature is ad hoc (expected for the free build)" ;; *) fail "signature is not ad hoc" ;; esac

# Architecture of every Mach-O we ship.
for bin in "$CONTENTS/MacOS/$EXEC_NAME" "$RES/cohere-mlx-bridge"; do
  archs="$(lipo -archs "$bin" 2>/dev/null || echo none)"
  [ "$archs" = "arm64" ] && pass "lipo -archs $(basename "$bin") = arm64" || fail "lipo -archs $(basename "$bin") = $archs"
done

# Version in Info.plist matches the file name.
plist_version="$(plutil -extract CFBundleShortVersionString raw "$CONTENTS/Info.plist" 2>/dev/null || echo none)"
[ "$plist_version" = "$name_version" ] && pass "Info.plist version $plist_version matches the file name" \
  || fail "Info.plist version '$plist_version' does not match file name version '$name_version'"
min_macos="$(plutil -extract LSMinimumSystemVersion raw "$CONTENTS/Info.plist" 2>/dev/null || echo none)"
[ "$min_macos" = "14.0" ] && pass "LSMinimumSystemVersion is 14.0" || fail "LSMinimumSystemVersion is '$min_macos'"

# Notices and licences travel inside the bundle.
for f in THIRD_PARTY_NOTICES.md LICENSE-MIT LICENSE-APACHE download-models.sh; do
  [ -s "$RES/$f" ] && pass "Resources/$f is inside the app" || fail "Resources/$f is missing from the app"
done
[ -x "$RES/download-models.sh" ] && pass "download-models.sh is executable" || fail "download-models.sh is not executable"

# No Sparkle: a build with no EdDSA key must never check for updates.
for key in SUFeedURL SUPublicEDKey SUEnableAutomaticChecks; do
  if plutil -extract "$key" raw "$CONTENTS/Info.plist" >/dev/null 2>&1; then fail "Info.plist has $key (updates must be off)"; else pass "Info.plist has no $key"; fi
done
[ -e "$CONTENTS/Frameworks/Sparkle.framework" ] && fail "Sparkle.framework is embedded" || pass "no Sparkle.framework embedded"

# A slim build carries no models and no dev symlinks that dangle off this Mac.
[ -e "$RES/models" ] && fail "models are inside a slim build" || pass "no models inside the slim app"
dangling="$(find "$APP" -type l ! -exec test -e {} \; -print 2>/dev/null | head -3)"
[ -z "$dangling" ] && pass "no dangling symlinks in the app" || fail "dangling symlinks: $dangling"
leak="$(grep -rIl -E '/Users/[a-z]+/' "$CONTENTS/Info.plist" 2>/dev/null || true)"
[ -z "$leak" ] && pass "Info.plist carries no user path" || fail "Info.plist carries a user path"

# Gatekeeper. An ad-hoc build is rejected by design; record the exact words.
spctl_out="$(spctl --assess --type execute --verbose=4 "$APP" 2>&1 || true)"
echo "INFO: spctl --assess --type execute: $(printf '%s' "$spctl_out" | tr '\n' ' ')"

echo "result: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
