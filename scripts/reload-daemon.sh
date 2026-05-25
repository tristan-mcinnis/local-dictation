#!/usr/bin/env bash
# Restart the running Local Dictation daemon so it reloads on-disk config
# (prompts.json / settings.json / corrections.json) WITHOUT a rebuild.
#
# Use this for the fast loop — tweaking prompts or formats in
# ~/.config/local-dictation/. The daemon reads all config once at boot, so a
# relaunch is all it takes to pick up edits.
#
#   • Config-only change (prompts/settings/corrections) → this script.
#   • Code or built-in-default change (src/**, DEFAULT_* in prompts.rs)
#       → scripts/build-app.sh --install  (rebuilds the bundle, then relaunches).
#
# NOTE: this reuses the already-installed bundle, so the code signature is
# unchanged and macOS keeps the existing Microphone + Accessibility grants.
# (A build-app.sh rebuild re-signs ad-hoc, which can make macOS re-prompt.)
set -euo pipefail

APP="/Applications/Local Dictation.app"
if [ ! -d "$APP" ]; then
    APP="$(cd "$(dirname "$0")/.." && pwd)/dist/Local Dictation.app"
fi
if [ ! -d "$APP" ]; then
    echo "✗ no installed app found. Build one first: scripts/build-app.sh --install" >&2
    exit 1
fi

if pkill -f "Local Dictation.app/Contents/MacOS/local-dictation" 2>/dev/null; then
    echo "• stopped running daemon"
    sleep 1
else
    echo "• no daemon was running"
fi

open "$APP"
echo "✓ relaunched ${APP##*/} — config reloaded. Logs: tail -f /tmp/dictate-daemon.log"
