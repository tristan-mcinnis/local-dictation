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
#   ./scripts/build-app.sh --install        # also copy to /Applications, add a
#                                           #   Login Item, and launch it
#   ./scripts/build-app.sh --dmg            # wrap the app in a distributable
#                                           #   dist/Local-Dictation-<ver>.dmg
#                                           #   (implies --bundle-models so the
#                                           #   DMG is self-contained/portable)
#
# Flags combine, e.g.  ./scripts/build-app.sh --bundle-models --install
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
MIN_MACOS="13.0"

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

# Recommended (shipped) model stack — only these get bundled with --bundle-models.
PARAKEET_REL="dictation/parakeet-tdt-v3-int8"
LLM_REL="llm/qwen-2.5-1.5b-it"

BUNDLE_MODELS=0
INSTALL=0
MAKE_DMG=0
for arg in "$@"; do
  case "$arg" in
    --bundle-models) BUNDLE_MODELS=1 ;;
    --install)       INSTALL=1 ;;
    --dmg)           MAKE_DMG=1 ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

# A DMG is meant to be moved/shared, so it must carry its own models — a dev
# symlink into the repo would dangle the moment the .app leaves this machine.
if [ "$MAKE_DMG" -eq 1 ] && [ "$BUNDLE_MODELS" -eq 0 ]; then
  echo "• --dmg implies --bundle-models (a portable DMG must be self-contained)"
  BUNDLE_MODELS=1
fi

echo "▶ Local Dictation.app  (v$VERSION)"

# ── 1. compile the release binary ───────────────────────────────────────────
echo "• building release binary (cargo build --features full --release)…"
cargo build --features full --release
[ -x "$BIN_SRC" ] || { echo "build produced no binary at $BIN_SRC" >&2; exit 1; }

# ── 2. lay out the bundle skeleton ───────────────────────────────────────────
echo "• assembling bundle…"
rm -rf "$APP"
mkdir -p "$MACOS_DIR" "$RES_DIR"
cp "$BIN_SRC" "$MACOS_DIR/$EXEC_NAME"
chmod +x "$MACOS_DIR/$EXEC_NAME"

# ── 3. Info.plist ────────────────────────────────────────────────────────────
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
    <key>LSMinimumSystemVersion</key>      <string>$MIN_MACOS</string>
    <key>NSHighResolutionCapable</key>     <true/>
    <!-- Menu-bar agent: no Dock icon, no app-switcher entry. -->
    <key>LSUIElement</key>                 <true/>
    <key>NSMicrophoneUsageDescription</key>
    <string>Local Dictation transcribes your speech on-device when you hold the push-to-talk key.</string>
</dict>
</plist>
PLIST

# ── 4. icon ──────────────────────────────────────────────────────────────────
echo "• rendering icon…"
ICON_PNG="$(mktemp -t ld-icon).png"
swift "$REPO_ROOT/scripts/make-icon.swift" "$ICON_PNG" >/dev/null
ICONSET="$(mktemp -d -t ld-iconset).iconset"
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size"               "$ICON_PNG" --out "$ICONSET/icon_${size}x${size}.png"      >/dev/null
  sips -z "$((size*2))" "$((size*2))"   "$ICON_PNG" --out "$ICONSET/icon_${size}x${size}@2x.png"   >/dev/null
done
iconutil -c icns "$ICONSET" -o "$RES_DIR/Icon.icns"
rm -rf "$ICONSET" "$ICON_PNG"

# ── 5. models ────────────────────────────────────────────────────────────────
if [ "$BUNDLE_MODELS" -eq 1 ]; then
  echo "• bundling model stack into the app (recommended stack only)…"
  mkdir -p "$RES_DIR/models/dictation" "$RES_DIR/models/llm"
  cp -R "$REPO_ROOT/models/$PARAKEET_REL" "$RES_DIR/models/$PARAKEET_REL"
  cp -R "$REPO_ROOT/models/$LLM_REL"      "$RES_DIR/models/$LLM_REL"
  echo "  bundled: $(du -sh "$RES_DIR/models" | cut -f1)"
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

# ── 6. ad-hoc code signature ─────────────────────────────────────────────────
# Ad-hoc ('-') is enough to run on Apple Silicon. Note: rebuilding changes the
# signature hash, so macOS may re-ask for Accessibility/Microphone after a
# rebuild. A real Developer ID would make those grants stick across rebuilds.
echo "• ad-hoc code-signing…"
codesign --force --deep --sign - "$APP" >/dev/null 2>&1 || \
  codesign --force --deep --sign - "$APP"
codesign --verify --deep --strict "$APP" && echo "  signature OK"

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
