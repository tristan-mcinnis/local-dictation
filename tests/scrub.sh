#!/bin/sh
# Publish scrub: no user-specific paths, private-stack references, personal
# emails or key material in tracked files.
#
# Generic patterns live here. Private tokens (client, colleague and personal
# names) belong in .scrub-private (gitignored, one ERE per line) so the
# scanner never publishes what it scans for. This file is the one tracked file
# excluded from the scan; review it by hand.
#
# Run: sh tests/scrub.sh   (cargo test runs it too, via tests/scrub_gate.rs)
set -e
cd "$(dirname "$0")/.."

GENERIC='/Users/[a-z]+/|tristan@innerchapter|~/vault|~/memory|vault-vps|BEGIN (RSA|OPENSSH|EC) PRIVATE KEY|sk-[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16}|xox[baprs]-|hf_[A-Za-z0-9]{30,}|ghp_[A-Za-z0-9]{30,}'
EMAIL='[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}'
OKMAIL='@(example\.(com|org|net|invalid)|anthropic\.com)$|^(git|noreply)@github\.com$'
PATTERN="$GENERIC"
if [ -f .scrub-private ]; then
  PRIVATE=$(grep -v '^[[:space:]]*$' .scrub-private | grep -v '^#' | paste -sd'|' -)
  [ -n "$PRIVATE" ] && PATTERN="$GENERIC|$PRIVATE"
fi

# Positive control: prove the pipeline detects a planted hit.
control=$(mktemp)
printf '/Users/nobody/leak\n' > "$control"
if ! grep -qE "$PATTERN" "$control"; then
  echo "SCRUB_SELFTEST_FAILED"; rm -f "$control"; exit 1
fi
printf 'jane.doe@gmail.com\n' > "$control"
if ! grep -ohE "$EMAIL" "$control" | grep -viE "$OKMAIL" | grep -q .; then
  echo "SCRUB_SELFTEST_FAILED"; rm -f "$control"; exit 1
fi
rm -f "$control"

hits=$(git ls-files -z | grep -zv '^tests/scrub\.sh$' | xargs -0 grep -lIE "$PATTERN" 2>/dev/null || true)
mail=$(git ls-files -z | grep -zv '^tests/scrub\.sh$' | xargs -0 grep -HoIE "$EMAIL" 2>/dev/null | grep -viE "$OKMAIL" || true)
if [ -n "$hits" ] || [ -n "$mail" ]; then
  echo "PRIVATE CONTENT FOUND:"
  [ -n "$hits" ] && echo "$hits"
  [ -n "$mail" ] && echo "$mail"
  exit 1
fi
echo "SCRUB_CLEAN"
