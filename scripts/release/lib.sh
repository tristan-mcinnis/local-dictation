#!/usr/bin/env bash
#
# Shared helpers for the Local Dictation direct-distribution release pipeline.
# Sourced by the release scripts; never executed on its own.
#
# Everything here is deliberately small and secret-agnostic: no helper prints a
# credential value, and every credential is read from the environment or a
# user-owned 0600 file, never from the repository.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DIST_DIR="$REPO_ROOT/dist"
RELEASES_DIR="${LD_RELEASES_DIR:-$DIST_DIR/releases}"
APP_NAME="Local Dictation"
APP_BUNDLE="$DIST_DIR/$APP_NAME.app"
BUNDLE_ID="com.tristanmcinnis.local-dictation"

log()  { printf '• %s\n' "$*" >&2; }
step() { printf '\n▶ %s\n' "$*" >&2; }
warn() { printf '⚠ %s\n' "$*" >&2; }
ok()   { printf '✓ %s\n' "$*" >&2; }
die()  { printf '✗ %s\n' "$*" >&2; exit 1; }

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

# True when --dry-run was requested (the orchestrator exports LD_RELEASE_DRY_RUN=1).
is_dry_run() { [ "${LD_RELEASE_DRY_RUN:-0}" = "1" ]; }

# Refuse to run outside a dry run. Use on every step that mutates the outside
# world: submitting for notarization, uploading, touching a Keychain item.
forbid_outside_dry_run() {
  is_dry_run || die "$1 is not permitted outside --dry-run"
}

# require_env NAME [hint] — fail with a non-secret message when unset.
require_env() {
  local name="$1" hint="${2:-}"
  if [ -z "${!name:-}" ]; then
    die "$name is not set${hint:+ ($hint)}"
  fi
}

# The repo version, read from the single source: Cargo.toml.
repo_version() {
  grep -m1 '^version' "$REPO_ROOT/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/'
}

# The exact commit a build traces to, or <sha>-dirty.
repo_commit() {
  local sha
  sha="$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  if [ -n "$(git -C "$REPO_ROOT" status --porcelain 2>/dev/null)" ]; then
    sha="$sha-dirty"
  fi
  printf '%s' "$sha"
}

# A publication must come from a clean, committed tree.
require_clean_tree() {
  if [ -n "$(git -C "$REPO_ROOT" status --porcelain 2>/dev/null)" ]; then
    git -C "$REPO_ROOT" status --short >&2
    die "the working tree is dirty; commit or stash before releasing"
  fi
}

# Read a base64 Ed25519 private key without ever echoing it.
# Order: SU_PRIVATE_KEY (env) > SU_PRIVATE_KEY_FILE (0600 file).
# Prints the key to stdout for a caller that pipes it straight into a signer;
# never assign it to a variable that gets logged.
private_key_material() {
  if [ -n "${SU_PRIVATE_KEY:-}" ]; then
    printf '%s' "$SU_PRIVATE_KEY"
    return 0
  fi
  if [ -n "${SU_PRIVATE_KEY_FILE:-}" ]; then
    [ -f "$SU_PRIVATE_KEY_FILE" ] || die "SU_PRIVATE_KEY_FILE does not exist"
    local mode
    mode="$(stat -f '%Lp' "$SU_PRIVATE_KEY_FILE" 2>/dev/null || echo unknown)"
    case "$mode" in
      600|400) ;;
      *) warn "SU_PRIVATE_KEY_FILE mode is $mode; tighten it to 0600" ;;
    esac
    cat "$SU_PRIVATE_KEY_FILE"
    return 0
  fi
  die "no Sparkle signing key: set SU_PRIVATE_KEY or SU_PRIVATE_KEY_FILE"
}

# A Developer ID identity is the only identity that can be notarized.
require_developer_id() {
  require_env SIGN_IDENTITY "the Developer ID Application certificate name"
  case "$SIGN_IDENTITY" in
    "Developer ID Application:"*) ;;
    *) die "SIGN_IDENTITY is not a Developer ID Application identity: refusing release signing" ;;
  esac
}
