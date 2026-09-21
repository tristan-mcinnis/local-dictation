#!/usr/bin/env bash
#
# Offline, hermetic dry run of the whole Local Dictation release pipeline.
#
# It creates synthetic artifacts in a temp directory, exercises every tool
# (hardening, signing, Ed25519, appcast, publication gates, rollback, Homebrew
# cask), asserts the refusal paths, and changes nothing outside the temp dir.
# No network, no Apple credentials, no Keychain, no /Applications, no publish.
#
# Run: scripts/release/dry-run.sh

set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

WORK="$(mktemp -d -t ld-release-dryrun)"
trap 'rm -rf "$WORK"' EXIT

PASS=0
FAIL=0
pass()  { ok "PASS: $1"; PASS=$((PASS + 1)); }
fail()  { warn "FAIL: $1"; FAIL=$((FAIL + 1)); }

expect_ok() {
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then pass "$desc"; else fail "$desc"; fi
}
expect_fail() {
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then fail "$desc (should have failed)"; else pass "$desc"; fi
}

step "1/7 hardened-runtime entitlements"
plutil -lint "$REPO_ROOT/scripts/release/entitlements.plist" >/dev/null && pass "entitlements.plist is a valid plist"
# Read the parsed dictionary, not the file text, so a comment cannot satisfy a check.
ENT_KEYS="$(plutil -p "$REPO_ROOT/scripts/release/entitlements.plist")"
printf '%s' "$ENT_KEYS" | grep -q 'com.apple.security.device.audio-input' && pass "audio-input entitlement present"
printf '%s' "$ENT_KEYS" | grep -q 'app-sandbox' && fail "app-sandbox must not be present" || pass "no app-sandbox entitlement"
printf '%s' "$ENT_KEYS" | grep -q 'disable-library-validation' && fail "library validation must stay enabled" || pass "library validation left enabled"
printf '%s' "$ENT_KEYS" | grep -q 'get-task-allow' && fail "get-task-allow must not be present" || pass "no get-task-allow (debug-only) entitlement"

step "2/7 hardened-runtime signing of a synthetic bundle"
SB="$WORK/Test.app/Contents"
mkdir -p "$SB/MacOS" "$SB/Resources"
cp /usr/bin/true "$SB/MacOS/Test"
cat > "$WORK/Test.app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>com.example.test</string>
<key>CFBundleExecutable</key><string>Test</string>
<key>CFBundleShortVersionString</key><string>0.0.0</string>
</dict></plist>
PLIST
codesign --force --options runtime --entitlements "$REPO_ROOT/scripts/release/entitlements.plist" --sign - "$WORK/Test.app" >/dev/null 2>&1 \
  && pass "synthetic bundle signed with hardened runtime" || fail "could not sign the synthetic bundle"
codesign --verify --deep --strict "$WORK/Test.app" >/dev/null 2>&1 && pass "ad-hoc hardened-runtime signature verifies"
# Capture the verbose description first: piping into `grep -q` under pipefail
# can report a false failure when grep exits early and codesign gets SIGPIPE.
SIGOUT="$(codesign -dvvv "$WORK/Test.app" 2>&1 || true)"
case "$SIGOUT" in *"Runtime Version"*) pass "hardened runtime flag is present in the signature" ;; *) fail "hardened runtime flag missing" ;; esac
case "$(codesign -d --entitlements - "$WORK/Test.app" 2>/dev/null || true)" in *audio-input*) pass "signed build carries the audio-input entitlement" ;; *) fail "entitlement not embedded in the signature" ;; esac

step "3/7 Sparkle 2 Ed25519 and the appcast"
need_cmd swift
SIGNER="$REPO_ROOT/scripts/release/sparkle_sign.swift"
PUBKEY="$(swift "$SIGNER" keygen --out "$WORK/keys")"
[ -n "$PUBKEY" ] && pass "generated an Ed25519 keypair" || fail "keygen produced no public key"
[ "$(stat -f '%Lp' "$WORK/keys/ed25519_private.key")" = "600" ] && pass "private key file is 0600" || fail "private key file is not 0600"

# A synthetic update archive.
mkdir -p "$WORK/payload"
printf 'local dictation payload\n' > "$WORK/payload/README.txt"
ditto -c -k --keepParent "$WORK/payload" "$WORK/Local-Dictation-0.0.0-test.zip"
SIG="$(swift "$SIGNER" sign "$WORK/Local-Dictation-0.0.0-test.zip" --key "$WORK/keys/ed25519_private.key")"
expect_ok "signature verifies against its own public key" \
  swift "$SIGNER" verify "$WORK/Local-Dictation-0.0.0-test.zip" "$SIG" "$PUBKEY"

python3 "$REPO_ROOT/scripts/release/appcast.py" build \
  --artifact "$WORK/Local-Dictation-0.0.0-test.zip" \
  --url "https://example.com/Local-Dictation-0.0.0-test.zip" \
  --signature "$SIG" --version 0.0.0 --short-version 0.0.0-test \
  --out "$WORK/appcast.xml" >/dev/null 2>&1 && pass "appcast built" || fail "appcast build failed"
python3 "$REPO_ROOT/scripts/release/appcast.py" check "$WORK/appcast.xml" \
  --artifact "$WORK/Local-Dictation-0.0.0-test.zip" --signature "$SIG" >/dev/null 2>&1 \
  && pass "appcast passes its contract check" || fail "appcast contract check failed"
grep -q '<sparkle:version>0.0.0</sparkle:version>' "$WORK/appcast.xml" && pass "version is a top-level item element" || fail "version element missing"
case "$(cat "$WORK/appcast.xml")" in *'sparkle:minimumSystemVersion>14.0.0<'*) pass "minimumSystemVersion is three-part" ;; *) fail "minimumSystemVersion is not three-part" ;; esac

# The gates that must refuse.
expect_fail "appcast.py build refuses a non-HTTPS download URL" \
  python3 "$REPO_ROOT/scripts/release/appcast.py" build \
  --artifact "$WORK/Local-Dictation-0.0.0-test.zip" --url "http://example.com/x.zip" \
  --signature "$SIG" --version 0.0.0 --out "$WORK/bad-http.xml"
expect_fail "appcast.py check refuses an enclosure without a matching signature" \
  python3 "$REPO_ROOT/scripts/release/appcast.py" check "$WORK/appcast.xml" --artifact "$WORK/Local-Dictation-0.0.0-test.zip"
expect_fail "make-appcast refuses a non-HTTPS download URL" \
  env ARTIFACT="$WORK/Local-Dictation-0.0.0-test.zip" DOWNLOAD_URL="http://example.com/x.zip" \
      SU_PRIVATE_KEY_FILE="$WORK/keys/ed25519_private.key" OUT="$WORK/bad-appcast.xml" \
      bash "$REPO_ROOT/scripts/release/make-appcast.sh"

step "4/7 make-appcast end to end (throwing-key fallback)"
cp "$WORK/appcast.xml" "$WORK/appcast.xml.prev"   # preserve for rollback later
expect_ok "make-appcast produces a signed, validated feed" \
  env ARTIFACT="$WORK/Local-Dictation-0.0.0-test.zip" \
      DOWNLOAD_URL="https://example.com/Local-Dictation-0.0.0-test.zip" \
      FEED_URL="https://example.com/appcast.xml" \
      SU_PRIVATE_KEY_FILE="$WORK/keys/ed25519_private.key" \
      SU_PUBLIC_ED_KEY="$PUBKEY" VERSION=0.0.0 SHORT_VERSION=0.0.0-test \
      OUT="$WORK/feed/appcast.xml" bash "$REPO_ROOT/scripts/release/make-appcast.sh"
python3 "$REPO_ROOT/scripts/release/appcast.py" check "$WORK/feed/appcast.xml" >/dev/null 2>&1 \
  && pass "the generated feed validates" || fail "the generated feed does not validate"

step "5/7 publication gates and rollback"
# A publication needs the archive the appcast names to be present locally, so
# stage a copy in a releases dir and point the pipeline at it.
mkdir -p "$WORK/pub"
cp "$WORK/feed/appcast.xml" "$WORK/pub/appcast.xml"
cp "$WORK/feed/appcast.xml" "$WORK/pub/appcast.xml.prev"
cp "$WORK/Local-Dictation-0.0.0-test.zip" "$WORK/pub/"
PUBCAST="$WORK/pub/appcast.xml"
expect_fail "publish refuses without ALLOW_PUBLISH" \
  env LD_RELEASES_DIR="$WORK/pub" RELEASE_UPLOAD_CMD="true" APPCAST="$PUBCAST" bash "$REPO_ROOT/scripts/release/publish.sh"
expect_fail "publish refuses without an upload destination" \
  env LD_RELEASES_DIR="$WORK/pub" ALLOW_PUBLISH=1 APPCAST="$PUBCAST" bash "$REPO_ROOT/scripts/release/publish.sh"
expect_ok "publish --dry-run prints the plan and uploads nothing" \
  env LD_RELEASES_DIR="$WORK/pub" LD_RELEASE_DRY_RUN=1 RELEASE_UPLOAD_CMD="false" APPCAST="$PUBCAST" \
      bash "$REPO_ROOT/scripts/release/publish.sh"
expect_ok "publish --rollback restores the previous appcast (dry run)" \
  env LD_RELEASES_DIR="$WORK/pub" LD_RELEASE_DRY_RUN=1 RELEASE_UPLOAD_CMD="false" APPCAST="$PUBCAST" \
      bash "$REPO_ROOT/scripts/release/publish.sh" --rollback

step "6/7 notarization credential gating"
expect_ok "notarize --dry-run plans without submitting" \
  env LD_RELEASE_DRY_RUN=1 TARGET="$WORK/Local-Dictation-0.0.0-test.zip" NOTARY_PROFILE="test-profile" \
      bash "$REPO_ROOT/scripts/release/notarize.sh"
expect_fail "notarize refuses without any credentials" \
  env LD_RELEASE_DRY_RUN=1 TARGET="$WORK/Local-Dictation-0.0.0-test.zip" \
      bash "$REPO_ROOT/scripts/release/notarize.sh"
expect_fail "build-release --dry-run requires a Developer ID identity" \
  env SIGN_IDENTITY="Apple Development: nope" SU_FEED_URL="https://example.com/appcast.xml" \
      SU_PUBLIC_ED_KEY="$PUBKEY" bash "$REPO_ROOT/scripts/release/build-release.sh" --dry-run
expect_ok "build-release --dry-run prints the plan with valid inputs" \
  env SIGN_IDENTITY="Developer ID Application: Dry Run (TEST)" \
      SU_FEED_URL="https://example.com/appcast.xml" SU_PUBLIC_ED_KEY="$PUBKEY" \
      SU_PRIVATE_KEY_FILE="$WORK/keys/ed25519_private.key" \
      bash "$REPO_ROOT/scripts/release/build-release.sh" --dry-run
expect_fail "build-app --release refuses a non-Developer-ID identity" \
  env SIGN_IDENTITY="Apple Development: nope" bash "$REPO_ROOT/scripts/build-app.sh" --release

step "7/7 Homebrew cask generation"
DMG="$WORK/Local-Dictation-0.0.0.dmg"
printf 'not a real dmg, but a real file\n' > "$DMG"
CASK_OUT="$WORK/homebrew/local-dictation.rb"
expect_ok "update-cask renders a cask from a DMG" \
  env VERSION=0.0.0 ARTIFACT="$DMG" SOURCE_URL="https://example.com/Local-Dictation-0.0.0.dmg" \
      OUT="$CASK_OUT" bash "$REPO_ROOT/scripts/release/update-cask.sh"
[ -f "$CASK_OUT" ] && ruby -c "$CASK_OUT" >/dev/null 2>&1 && pass "generated cask is valid Ruby"
DMG_SHA="$(shasum -a 256 "$DMG" | awk '{print $1}')"
grep -q "sha256 \"$DMG_SHA\"" "$CASK_OUT" && pass "cask sha256 matches the artifact" || fail "cask sha256 does not match"
for field in 'cask "local-dictation"' 'name "Local Dictation"' 'app "Local Dictation.app"' 'auto_updates true' 'depends_on arch: :arm64' 'depends_on macos: :sonoma'; do
  grep -qF "$field" "$CASK_OUT" && pass "cask has: $field" || fail "cask is missing: $field"
done
expect_fail "update-cask refuses a malformed sha256" \
  env VERSION=0.0.0 SHA256="nothex" SOURCE_URL="https://example.com/x.dmg" OUT="$WORK/bad.rb" \
      bash "$REPO_ROOT/scripts/release/update-cask.sh"
expect_fail "update-cask refuses a non-HTTPS URL" \
  env VERSION=0.0.0 SHA256="$DMG_SHA" SOURCE_URL="http://example.com/x.dmg" OUT="$WORK/bad2.rb" \
      bash "$REPO_ROOT/scripts/release/update-cask.sh"
if command -v brew >/dev/null 2>&1; then
  # Loose-file `brew style` also raises Sorbet/frozen-string cops that a tap
  # disables for casks; only Cask stanza offenses are meaningful here.
  STYLE_OUT="$(brew style "$CASK_OUT" 2>&1 || true)"
  case "$STYLE_OUT" in
    *"Cask/"*) fail "brew style reported Cask stanza issues" ;;
    *) pass "brew style has no Cask stanza issues" ;;
  esac
else
  warn "Homebrew is not installed; skipping brew style (non-fatal)"
fi

echo
if [ "$FAIL" -eq 0 ]; then
  ok "dry run passed: $PASS checks, 0 failures"
else
  die "dry run failed: $PASS passed, $FAIL failed"
fi
