#!/usr/bin/env bash
#
# Generate the Homebrew cask for a release from a signed, notarized DMG.
#
# It writes one file and never touches a real tap unless you point TAP_DIR at a
# checkout you own. It does not create a tap, push anything, or run `brew`
# against a network cask.
#
# Inputs (env):
#   VERSION     release version (default: Cargo.toml)
#   ARTIFACT    path to the DMG (computes SHA256) OR
#   SHA256      the DMG's sha256 (64 hex) OR
#   URL         the HTTPS DMG download URL (required if ARTIFACT name != canonical)
#   OUT         where to write the cask (default dist/releases/homebrew/local-dictation.rb)
#   TAP_DIR     optional: a local tap checkout; writes Casks/local-dictation.rb into it
#   SOURCE_URL  override the template URL entirely
#
# The URL defaults to the canonical GitHub release asset for VERSION:
#   https://github.com/tristan-mcinnis/local-dictation/releases/download/v<VERSION>/Local-Dictation-<VERSION>.dmg

set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

TEMPLATE="$REPO_ROOT/scripts/release/homebrew/local-dictation.rb.tmpl"
VERSION="${VERSION:-$(repo_version)}"
OUT="${OUT:-$RELEASES_DIR/homebrew/local-dictation.rb}"
ARTIFACT="${ARTIFACT:-}"

# ── sha256 ───────────────────────────────────────────────────────────────────
if [ -n "$ARTIFACT" ]; then
  [ -f "$ARTIFACT" ] || die "ARTIFACT not found: $ARTIFACT"
  SHA256="$(shasum -a 256 "$ARTIFACT" | awk '{print $1}')"
fi
require_env SHA256 "the DMG sha256 (or set ARTIFACT)"
case "$SHA256" in
  *[!0-9a-fA-F]*|"") die "SHA256 must be 64 hex characters" ;;
esac
[ "${#SHA256}" -eq 64 ] || die "SHA256 must be 64 hex characters (got ${#SHA256})"

# ── url ──────────────────────────────────────────────────────────────────────
URL="${SOURCE_URL:-${URL:-https://github.com/tristan-mcinnis/local-dictation/releases/download/v$VERSION/Local-Dictation-$VERSION.dmg}}"
case "$URL" in
  https://*) ;;
  *) die "URL must be HTTPS: $URL" ;;
esac

# ── render ───────────────────────────────────────────────────────────────────
step "rendering the Homebrew cask"
RENDERED="$(sed -e "s|@VERSION@|$VERSION|g" -e "s|@SHA256@|$SHA256|g" -e "s|@URL@|$URL|g" "$TEMPLATE")"
mkdir -p "$(dirname "$OUT")"
printf '%s\n' "$RENDERED" > "$OUT"

# Always syntax-check the Ruby.
need_cmd ruby
ruby -c "$OUT" >/dev/null || die "generated cask is not valid Ruby"
ok "wrote $OUT"

if [ -n "${TAP_DIR:-}" ]; then
  case "$TAP_DIR" in
    */Casks) ;;
    *) TAP_DIR="$TAP_DIR/Casks" ;;
  esac
  mkdir -p "$TAP_DIR"
  cp -f "$OUT" "$TAP_DIR/local-dictation.rb"
  ok "installed into $TAP_DIR/local-dictation.rb"
  log "this is a local checkout only; pushing the tap is a separate human gate"
fi

# ── optional local lint (best effort, never fatal without Homebrew) ───────────
if [ -z "${LD_RELEASE_DRY_RUN:-}" ] && command -v brew >/dev/null 2>&1; then
  # `brew style` on a loose file also applies Sorbet sigil / frozen-string-literal
  # cops that a real tap disables for casks, so only the Cask cops are meaningful
  # here.
  STYLE_OUT="$(brew style "$OUT" 2>&1 || true)"
  case "$STYLE_OUT" in
    *"Cask/"*) warn "brew style reported Cask stanza issues (run 'brew style $OUT' to see them)" ;;
    *) ok "brew style has no Cask stanza issues" ;;
  esac
fi
