#!/usr/bin/env bash
#
# Publish a built Local Dictation release: upload the update archive and the
# appcast. This is the only step that touches the outside world, so it is gated
# three ways and refuses by default.
#
# Gates (all required for a real upload):
#   * a clean, committed tree
#   * a validated appcast with an HTTPS feed and a signed enclosure
#   * ALLOW_PUBLISH=1  — the explicit human publication decision
#   * RELEASE_UPLOAD_CMD — the sanctioned upload command for your host
#
# In --dry-run or without ALLOW_PUBLISH, nothing is uploaded; the plan is
# printed.
#
# Rollback semantics (--rollback): Sparkle 2 does not downgrade. Rolling back a
# bad feed means restoring the previous appcast so clients that have not yet
# updated see the prior version, and — for clients that already updated —
# shipping a NEW, higher build number containing the reverted code. This script
# does the first; it cannot do the second for you.
#
#   scripts/release/publish.sh --dry-run
#   scripts/release/publish.sh             # real, needs ALLOW_PUBLISH=1
#   scripts/release/publish.sh --rollback  # restore appcast.xml.prev

set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

APPCAST="${APPCAST:-$RELEASES_DIR/appcast.xml}"
ROLLBACK=0
for arg in "$@"; do
  case "$arg" in
    --dry-run)  export LD_RELEASE_DRY_RUN=1 ;;
    --rollback) ROLLBACK=1 ;;
    *) die "unknown flag: $arg" ;;
  esac
done

# ── credential/host handling ─────────────────────────────────────────────────
UPLOAD_MODE=""
if [ -n "${RELEASE_UPLOAD_CMD:-}" ]; then
  UPLOAD_MODE="RELEASE_UPLOAD_CMD"
elif [ -n "${RELEASE_UPLOAD_DIR:-}" ]; then
  UPLOAD_MODE="RELEASE_UPLOAD_DIR"
else
  die "no upload destination: set RELEASE_UPLOAD_CMD ('cmd <file>') or RELEASE_UPLOAD_DIR"
fi

# Never echo the upload command: it may embed a token. Log only its mode.
log "upload destination: $UPLOAD_MODE"

upload() {
  local file="$1"
  if is_dry_run; then
    log "would upload $(basename "$file") via $UPLOAD_MODE"
    return 0
  fi
  log "uploading $(basename "$file")"
  if [ "$UPLOAD_MODE" = "RELEASE_UPLOAD_CMD" ]; then
    # shellcheck disable=SC2086
    $RELEASE_UPLOAD_CMD "$file"
  else
    need_cmd rsync
    rsync -av -- "$file" "$RELEASE_UPLOAD_DIR/"
  fi
}

# ── rollback ─────────────────────────────────────────────────────────────────
if [ "$ROLLBACK" -eq 1 ]; then
  step "rollback: un-advertising the current release"
  [ -f "$APPCAST.prev" ] || die "no previous appcast at $APPCAST.prev to restore"
  python3 "$REPO_ROOT/scripts/release/appcast.py" check "$APPCAST.prev"
  cp -f "$APPCAST.prev" "$APPCAST"
  ok "restored the previous appcast"
  warn "already-updated clients will not downgrade; ship a higher build with the reverted code"
  upload "$APPCAST"
  ok "rollback published (or printed, in dry-run)"
  exit 0
fi

# ── publication gates ────────────────────────────────────────────────────────
step "checking publication gates"
# A dry run reports the plan and is allowed on a working tree; real publication
# must come from a clean commit.
if ! is_dry_run; then
  require_clean_tree
fi
[ -f "$APPCAST" ] || die "no appcast at $APPCAST (run build-release.sh first)"
python3 "$REPO_ROOT/scripts/release/appcast.py" check "$APPCAST" || die "the appcast is not publishable"

ARTIFACT="$(python3 - "$APPCAST" <<'PY'
import sys, xml.etree.ElementTree as ET
root = ET.parse(sys.argv[1]).getroot()
items = root.find("channel").findall("item")
# The archive named by the newest item.
url = items[-1].find("enclosure").get("url")
print(url)
PY
)"
ARCHIVE_NAME="$(basename "$ARTIFACT")"
ARCHIVE="$RELEASES_DIR/$ARCHIVE_NAME"
[ -f "$ARCHIVE" ] || die "the appcast points at $ARCHIVE_NAME but $ARCHIVE does not exist"

if is_dry_run; then
  step "dry run: printing the publication plan"
elif [ "${ALLOW_PUBLISH:-0}" != "1" ]; then
  die "refusing to publish: set ALLOW_PUBLISH=1 to make the release decision explicit"
fi

# ── upload ───────────────────────────────────────────────────────────────────
step "publishing $(basename "$ARCHIVE") and appcast.xml"
upload "$ARCHIVE"
upload "$APPCAST"
ok "published (or printed, in dry-run)"
