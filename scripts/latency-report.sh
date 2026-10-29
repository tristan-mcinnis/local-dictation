#!/usr/bin/env bash
# Percentiles of the per-dictation [latency] lines in the daemon log.
# Usage: scripts/latency-report.sh [LOG]   (default: $HOME/Library/Logs/Local Dictation/daemon.log)
# Prints numbers only, never dictated text. Nearest-rank percentiles.
log="${1:-$HOME/Library/Logs/Local Dictation/daemon.log}"
[ -f "$log" ] || { echo "latency-report: no such file: $log" >&2; exit 1; }

lines="$(grep -a '^\[latency\]' "$log")"
n="$(printf '%s' "$lines" | grep -c .)"
[ "$n" -gt 0 ] || { echo "latency-report: no [latency] lines in $log" >&2; exit 1; }
echo "n=$n"

for key in release_ms asr_ms cleanup_ms; do
  printf '%s\n' "$lines" \
    | sed -n "s/.* $key=\([0-9][0-9]*\).*/\1/p" \
    | sort -n \
    | awk -v key="$key" '
        { v[NR] = $1 }
        function at(p,  k) { k = int((p * NR + 99) / 100); if (k < 1) k = 1; return v[k] }
        END { printf "%s p50=%s p90=%s p99=%s\n", key, at(50), at(90), at(99) }'
done

printf '%s\n' "$lines" | awk '
  /inserted=false/ { bad++ }
  END { printf "inserted_false=%.1f%%\n", 100 * bad / NR }'
