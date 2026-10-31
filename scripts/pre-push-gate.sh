#!/bin/sh
# Pre-push gate. The global pre-push hook runs this as the repo's test
# command and passes git's pushed-ref lines on stdin:
#   <local ref> <local sha> <remote ref> <remote sha>
#
# The publish scrub always runs. `cargo test` is skipped only when every file
# changed by the push is documentation (*.md or docs/). With no ref lines on
# stdin (a manual run), or when the range cannot be worked out, the tests run.
#
# Run by hand: sh scripts/pre-push-gate.sh </dev/null
set -eu
cd "$(dirname "$0")/.."

ZERO=0000000000000000000000000000000000000000

sh tests/scrub.sh >/dev/null || { sh tests/scrub.sh; exit 1; }

# Prints the files a pushed ref changes, or fails when the range is unknown.
changed_files() {
  local_sha=$1 remote_sha=$2
  if [ "$remote_sha" = "$ZERO" ]; then
    base=$(git merge-base origin/main "$local_sha" 2>/dev/null) || return 1
  else
    git cat-file -e "$remote_sha^{commit}" 2>/dev/null || return 1
    base=$remote_sha
  fi
  git diff --name-only "$base" "$local_sha"
}

docs_only=0
files=$(mktemp)
trap 'rm -f "$files"' EXIT
if [ ! -t 0 ]; then
  while read -r _lref local_sha _rref remote_sha; do
    [ -n "${local_sha:-}" ] || continue
    [ "$local_sha" = "$ZERO" ] && continue   # branch delete: nothing to test
    changed_files "$local_sha" "$remote_sha" >>"$files" || { docs_only=0; break; }
    docs_only=1
  done
fi

if [ "$docs_only" = 1 ] && [ -s "$files" ] && ! grep -v -E '\.md$|^docs/' "$files" >/dev/null; then
  echo "pre-push-gate: docs-only push ($(sort -u "$files" | wc -l | tr -d ' ') files), scrub clean, cargo test skipped"
  exit 0
fi

exec cargo test -q
