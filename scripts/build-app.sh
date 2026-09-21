#!/usr/bin/env bash
#
# Build "Local Dictation.app" — a double-clickable, menu-bar macOS app that
# wraps the release daemon binary.
#
# Usage:
#   ./scripts/build-app.sh                  # dev build: tiny app, models shared
#                                           #   from the repo via a symlink
#   ./scripts/build-app.sh --bundle-models  # ship build: copy the recommended
#                                           #   model stack into the app (~1.7 GB)
#   ./scripts/build-app.sh --slim           # ship build: NO models bundled; the
#                                           #   app never downloads anything, so
#                                           #   the user must supply the model
#                                           #   stack (it logs what is missing)
#   ./scripts/build-app.sh --install        # also copy to /Applications, add a
#                                           #   Login Item, and launch it
#   ./scripts/build-app.sh --dmg            # wrap the app in a distributable
#                                           #   dist/Local-Dictation-<ver>.dmg.
#                                           #   Defaults to --bundle-models (self-
#                                           #   contained); pair with --slim for a
#                                           #   small DMG with no models in it.
#   ./scripts/build-app.sh --release        # distribute build: hardened runtime,
#                                           #   Developer ID signing, entitlements.
#                                           #   Requires SIGN_IDENTITY, SU_FEED_URL,
#                                           #   SU_PUBLIC_ED_KEY and a clean tree.
#                                           #   Driven by scripts/release/; see
#                                           #   docs/release/direct-distribution.md.
#
# Flags combine, e.g.  ./scripts/build-app.sh --slim --dmg
#
# Optional release env:
#   SPARKLE_FRAMEWORK=/path/Sparkle.framework  embed + re-sign Sparkle 2
#   SU_FEED_URL=https://.../appcast.xml        Sparkle feed (Info.plist)
#   SU_PUBLIC_ED_KEY=<base64>                  Sparkle EdDSA public key
#
# Model resolution at runtime (see src/app_paths.rs):
#   DICTATE_MODELS_DIR env > <app>/Contents/Resources/models
#     > ~/Library/Application Support/Local Dictation/models > ./models
#
# Dev builds leave the models OUT of the app (instant rebuilds, no 1.7 GB copy)
# and instead symlink the App Support models dir back to the repo, so the app
# resolves the exact same files you develop against.

set -euo pipefail

# ── config ────────────────────────────────────────────────────────────────
APP_NAME="Local Dictation"
BUNDLE_ID="com.tristanmcinnis.local-dictation"
EXEC_NAME="local-dictation"        # Contents/MacOS/<EXEC_NAME>
# One macOS floor for the whole app. The Swift MLX helper requires 14.0
# (native/cohere-mlx/Package.swift `.macOS(.v14)`), so the bundle declares the
# same floor and the Rust binary is compiled for it. Change all three together.
MIN_MACOS="14.0"
export MACOSX_DEPLOYMENT_TARGET="$MIN_MACOS"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

DIST="$REPO_ROOT/dist"
APP="$DIST/$APP_NAME.app"
CONTENTS="$APP/Contents"
MACOS_DIR="$CONTENTS/MacOS"
RES_DIR="$CONTENTS/Resources"
BIN_SRC="$REPO_ROOT/target/release/fast-dictate-backend"

APP_SUPPORT="$HOME/Library/Application Support/$APP_NAME"

VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"

# WHICH COMMIT this bundle was built from, stamped into Info.plist below
# ALONGSIDE the version (never replacing it), so an installed binary is always
# traceable to a revision. A build always compiles the WORKING TREE, not HEAD,
# so a dirty tree is marked: such a build traces to no commit at all. Marking is
# not refusing — only --install refuses (see below).
COMMIT="$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
if [ -n "$(git -C "$REPO_ROOT" status --porcelain 2>/dev/null)" ]; then
  COMMIT="$COMMIT-dirty"
fi

# Fixed software-of-one model stack.
COHERE_REL="dictation/cohere-mlx-q4"
LLM_REL="llm/s1-mini"

BUNDLE_MODELS=0
SLIM=0
INSTALL=0
MAKE_DMG=0
RELEASE=0
for arg in "$@"; do
  case "$arg" in
    --bundle-models) BUNDLE_MODELS=1 ;;
    --slim)          SLIM=1 ;;
    --install)       INSTALL=1 ;;
    --dmg)           MAKE_DMG=1 ;;
    --release)       RELEASE=1 ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

# A release build is the only build that may ship the hardened runtime and a
# Developer ID signature, and the only one that must carry a Sparkle feed. The
# gates live here as well as in scripts/release/ so a hand-run cannot skip them.
ENTITLEMENTS_PLIST="$REPO_ROOT/scripts/release/entitlements.plist"
if [ "$RELEASE" -eq 1 ]; then
  case "${SIGN_IDENTITY:-}" in
    "Developer ID Application:"*) ;;
    "") echo "--release needs SIGN_IDENTITY (a Developer ID Application identity)" >&2; exit 1 ;;
    *)  echo "--release refuses SIGN_IDENTITY '$SIGN_IDENTITY' (not a Developer ID Application identity)" >&2; exit 1 ;;
  esac
  [ -f "$ENTITLEMENTS_PLIST" ] || { echo "missing $ENTITLEMENTS_PLIST" >&2; exit 1; }
  if [ -n "$(git -C "$REPO_ROOT" status --porcelain 2>/dev/null)" ]; then
    echo "--release refuses a dirty working tree" >&2
    git -C "$REPO_ROOT" status --short >&2
    exit 1
  fi
  : "${SU_FEED_URL:?--release needs SU_FEED_URL}"
  : "${SU_PUBLIC_ED_KEY:?--release needs SU_PUBLIC_ED_KEY}"
fi

if [ "$SLIM" -eq 1 ] && [ "$BUNDLE_MODELS" -eq 1 ]; then
  echo "--slim and --bundle-models are mutually exclusive" >&2; exit 2
fi

# Installing from a dirty checkout puts a binary in /Applications that traces to
# no commit at all (bitten 2026-09-12 in a sibling house repo: a build installed
# mid-edit ran for an hour answering in a reply format that existed only in
# uncommitted edits, which is the only reason the mismatch was ever noticed).
# Refuse --install by default; override deliberately with
# DICTATION_ALLOW_DIRTY=1, and the stamp above then records <sha>-dirty. Only
# the install path is guarded: a plain build, a --dmg, and every dev loop still
# build a dirty tree without complaint, they just get a marked stamp. The check
# is here, before the multi-minute compile, so a refusal costs nothing.
if [ "$INSTALL" -eq 1 ] && [ -n "$(git -C "$REPO_ROOT" status --porcelain 2>/dev/null)" ]; then
  if [ "${DICTATION_ALLOW_DIRTY:-0}" = "1" ]; then
    echo "⚠ dirty tree: this build traces to no commit; stamping it $COMMIT" >&2
  else
    echo "refusing to install from a dirty working tree." >&2
    git -C "$REPO_ROOT" status --short >&2
    echo "commit or stash first, or re-run with DICTATION_ALLOW_DIRTY=1 to override." >&2
    exit 1
  fi
fi

# A DMG is meant to be moved/shared, so a dev symlink into the repo would dangle
# the moment the .app leaves this machine. Either carry the models (default) or
# go --slim (the app fetches them on first run) — but never ship the symlink.
if [ "$MAKE_DMG" -eq 1 ] && [ "$BUNDLE_MODELS" -eq 0 ] && [ "$SLIM" -eq 0 ]; then
  echo "• --dmg defaults to --bundle-models (a portable DMG must be self-contained; use --slim to fetch on first run)"
  BUNDLE_MODELS=1
fi

echo "▶ Local Dictation.app  (v$VERSION)"

# ── 1. compile native MLX helper + Rust app ──────────────────────────────────
echo "• building native Cohere MLX helper…"
COHERE_HELPER="$($REPO_ROOT/native/cohere-mlx/build.sh | tail -1)"
COHERE_PRODUCTS="$(dirname "$COHERE_HELPER")"

BUILD_FEATURES="${BUILD_FEATURES:-full}"
echo "• building release binary (cargo build --features $BUILD_FEATURES --release)…"
cargo build --features "$BUILD_FEATURES" --release
[ -x "$BIN_SRC" ] || { echo "build produced no binary at $BIN_SRC" >&2; exit 1; }

# ── 2. lay out the bundle skeleton ───────────────────────────────────────────
echo "• assembling bundle…"
rm -rf "$APP"
mkdir -p "$MACOS_DIR" "$RES_DIR"
cp "$BIN_SRC" "$MACOS_DIR/$EXEC_NAME"
chmod +x "$MACOS_DIR/$EXEC_NAME"
cp "$COHERE_HELPER" "$RES_DIR/cohere-mlx-bridge"
chmod +x "$RES_DIR/cohere-mlx-bridge"
# MLX searches for mlx.metallib beside its executable before bundle lookup.
cp "$COHERE_PRODUCTS/mlx-swift_Cmlx.bundle/Contents/Resources/default.metallib" \
   "$RES_DIR/mlx.metallib"

# ── 3. Info.plist ────────────────────────────────────────────────────────────
# Sparkle 2 keys are written only when a feed is configured, so a dev/self
# build stays a plain, non-updating app. Release mode above requires both.
SPARKLE_PLIST=""
if [ -n "${SU_FEED_URL:-}" ] && [ -n "${SU_PUBLIC_ED_KEY:-}" ]; then
  case "$SU_FEED_URL" in
    https://*) ;;
    *) echo "SU_FEED_URL must be HTTPS: $SU_FEED_URL" >&2; exit 1 ;;
  esac
  SPARKLE_PLIST="    <key>SUFeedURL</key>                <string>$SU_FEED_URL</string>
    <key>SUPublicEDKey</key>              <string>$SU_PUBLIC_ED_KEY</string>
    <key>SUEnableAutomaticChecks</key>    <true/>
    <key>SUScheduledCheckInterval</key>   <integer>86400</integer>"
fi

cat > "$CONTENTS/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>                <string>$APP_NAME</string>
    <key>CFBundleDisplayName</key>         <string>$APP_NAME</string>
    <key>CFBundleIdentifier</key>          <string>$BUNDLE_ID</string>
    <key>CFBundleExecutable</key>          <string>$EXEC_NAME</string>
    <key>CFBundleIconFile</key>            <string>Icon</string>
    <key>CFBundlePackageType</key>         <string>APPL</string>
    <key>CFBundleShortVersionString</key>  <string>$VERSION</string>
    <key>CFBundleVersion</key>             <string>$VERSION</string>
    <key>LocalDictationBuiltFromCommit</key> <string>$COMMIT</string>
    <key>LSMinimumSystemVersion</key>      <string>$MIN_MACOS</string>
    <key>NSHighResolutionCapable</key>     <true/>
    <!-- Menu-bar agent: no Dock icon, no app-switcher entry. -->
    <key>LSUIElement</key>                 <true/>
    <key>NSMicrophoneUsageDescription</key>
    <string>Local Dictation transcribes your speech on-device when you hold the push-to-talk key.</string>
$SPARKLE_PLIST
</dict>
</plist>
PLIST

# ── 4. icon ──────────────────────────────────────────────────────────────────
# Rendered by design-system/icons/make-icon.py --glyph waveform (house icon rule).
cp "$REPO_ROOT/resources/AppIcon.icns" "$RES_DIR/Icon.icns"

# ── 5. models ────────────────────────────────────────────────────────────────
if [ "$BUNDLE_MODELS" -eq 1 ]; then
  echo "• bundling model stack into the app (recommended stack only)…"
  mkdir -p "$RES_DIR/models/dictation" "$RES_DIR/models/llm"
  cp -R "$REPO_ROOT/models/$COHERE_REL" "$RES_DIR/models/$COHERE_REL"
  cp -RL "$REPO_ROOT/models/$LLM_REL"   "$RES_DIR/models/$LLM_REL"
  echo "  bundled: $(du -sh "$RES_DIR/models" | cut -f1)"
elif [ "$SLIM" -eq 1 ]; then
  # Ship NO models and NO dev symlink: on a clean machine the app resolves no
  # model base, so the daemon fetches the defaults into Application Support on
  # first launch (src/model_fetch.rs). Keeps the app/DMG to a few MB.
  echo "• slim ship build: no models bundled — the app downloads them on first run."
else
  echo "• dev build: linking shared models via Application Support…"
  mkdir -p "$APP_SUPPORT"
  # Point the app's model base at the repo's models/ (no 1.4 GB copy).
  if [ ! -e "$APP_SUPPORT/models" ] || [ -L "$APP_SUPPORT/models" ]; then
    ln -sfn "$REPO_ROOT/models" "$APP_SUPPORT/models"
    echo "  linked: $APP_SUPPORT/models → $REPO_ROOT/models"
  else
    echo "  NOTE: $APP_SUPPORT/models exists and is not a symlink — leaving it as-is"
  fi
fi

# ── 5b. embed Sparkle 2 (optional) ───────────────────────────────────────────
# The framework is never committed: it is supplied at build time from the
# Sparkle release. Re-signed below with the app's own identity so hardened
# runtime library validation passes without weakening it.
if [ -n "${SPARKLE_FRAMEWORK:-}" ]; then
  [ -d "$SPARKLE_FRAMEWORK" ] || { echo "SPARKLE_FRAMEWORK is not a directory: $SPARKLE_FRAMEWORK" >&2; exit 1; }
  echo "• embedding Sparkle.framework…"
  mkdir -p "$CONTENTS/Frameworks"
  rm -rf "$CONTENTS/Frameworks/Sparkle.framework"
  cp -R "$SPARKLE_FRAMEWORK" "$CONTENTS/Frameworks/Sparkle.framework"
  # The Rust binary loads Sparkle at runtime from @rpath; add the bundle rpath
  # once, so a repeat build does not stack duplicate LC_RPATH entries.
  if ! otool -l "$MACOS_DIR/$EXEC_NAME" | grep -q "@executable_path/../Frameworks"; then
    install_name_tool -add_rpath "@executable_path/../Frameworks" "$MACOS_DIR/$EXEC_NAME"
  fi
fi

# ── 6. code signature ────────────────────────────────────────────────────────
# A stable signing identity keeps the Accessibility/Microphone grants across
# rebuilds (TCC keys them to the signature). Default: the first "Apple
# Development" identity in the login keychain; SIGN_IDENTITY=- forces ad-hoc.
if [ -z "${SIGN_IDENTITY:-}" ]; then
  SIGN_IDENTITY=$(security find-identity -v -p codesigning 2>/dev/null \
    | grep -o '"Apple Development: [^"]*"' | head -1 | tr -d '"')
  SIGN_IDENTITY=${SIGN_IDENTITY:--}
fi
echo "• code-signing with: $SIGN_IDENTITY"

CODESIGN_FLAGS=(--force --sign "$SIGN_IDENTITY")
if [ "$RELEASE" -eq 1 ]; then
  CODESIGN_FLAGS+=(--options runtime --timestamp --entitlements "$ENTITLEMENTS_PLIST")
fi

if [ "$RELEASE" -eq 1 ]; then
  # Sign inside-out, never with --deep (Sparkle's own guidance: --deep is a
  # common source of sandbox/notarization failures). Sparkle 2 lays its helpers
  # out under Versions/B/, so discover the version dir rather than hardcoding it.
  [ -f "$RES_DIR/cohere-mlx-bridge" ] && codesign "${CODESIGN_FLAGS[@]}" "$RES_DIR/cohere-mlx-bridge"
  if [ -d "$CONTENTS/Frameworks/Sparkle.framework" ]; then
    FW="$CONTENTS/Frameworks/Sparkle.framework"
    VER_DIR="$(ls -d "$FW"/Versions/[A-Z] 2>/dev/null | head -1 || true)"
    [ -n "$VER_DIR" ] || { echo "Sparkle.framework has no Versions/<X> directory" >&2; exit 1; }
    for part in \
      "$VER_DIR/XPCServices/Installer.xpc" \
      "$VER_DIR/XPCServices/Downloader.xpc" \
      "$VER_DIR/Autoupdate" \
      "$VER_DIR/Updater.app" \
      "$VER_DIR/Sparkle"; do
      [ -e "$part" ] && codesign "${CODESIGN_FLAGS[@]}" "$part"
    done
    codesign "${CODESIGN_FLAGS[@]}" "$FW"
  fi
  codesign "${CODESIGN_FLAGS[@]}" "$APP"
else
  codesign "${CODESIGN_FLAGS[@]}" --deep "$APP" >/dev/null 2>&1 || \
    codesign "${CODESIGN_FLAGS[@]}" --deep "$APP"
fi
codesign --verify --deep --strict "$APP" && echo "  signature OK"
if [ "$RELEASE" -eq 1 ]; then
  codesign -dvv "$APP" 2>&1 | grep -E 'Authority|Runtime Version|Timestamp' | sed 's/^/  /' >&2 || true
fi

echo "✓ built: $APP"

# ── 7. optional install + login item ─────────────────────────────────────────
if [ "$INSTALL" -eq 1 ]; then
  DEST="/Applications/$APP_NAME.app"
  echo "• installing to ${DEST}…"
  rm -rf "$DEST"
  cp -R "$APP" "$DEST"

  echo "• registering as a Login Item…"
  osascript >/dev/null <<OSA
tell application "System Events"
    if exists login item "$APP_NAME" then delete login item "$APP_NAME"
    make login item at end with properties {name:"$APP_NAME", path:"$DEST", hidden:false}
end tell
OSA

  echo "• launching…"
  open "$DEST"
  echo "✓ installed, set to launch at login, and started."
  echo "  First launch: grant Microphone + Accessibility when macOS asks"
  echo "  (System Settings → Privacy & Security)."
fi

# ── 8. optional distributable DMG ─────────────────────────────────────────────
if [ "$MAKE_DMG" -eq 1 ]; then
  DMG="$DIST/Local-Dictation-${VERSION}.dmg"
  echo "• building DMG…"
  STAGE="$(mktemp -d -t ld-dmg)"
  cp -R "$APP" "$STAGE/$APP_NAME.app"
  ln -s /Applications "$STAGE/Applications"   # drag-to-install affordance
  rm -f "$DMG"
  hdiutil create -volname "$APP_NAME" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
  rm -rf "$STAGE"
  echo "✓ DMG: $DMG ($(du -sh "$DMG" | cut -f1))"
fi
