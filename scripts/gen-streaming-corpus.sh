#!/usr/bin/env bash
# Generate a reproducible synthetic dictation corpus for the streaming lab.
#
# macOS `say` reads each ground-truth text aloud, then ffmpeg downsamples to
# 16 kHz mono PCM WAV (what Parakeet expects). Because we know the exact words
# we fed `say`, the matching .txt is the ground truth for WER. Clips are graded
# in length (~3s → ~180s) so the lab can plot latency/accuracy vs duration.
#
# Usage: ./scripts/gen-streaming-corpus.sh   (idempotent; writes testdata/streaming/)
set -euo pipefail
cd "$(dirname "$0")/.."
OUT=testdata/streaming
mkdir -p "$OUT"

command -v say   >/dev/null || { echo "need macOS 'say'"; exit 1; }
command -v ffmpeg >/dev/null || { echo "need ffmpeg (brew install ffmpeg)"; exit 1; }

# Voice + rate fixed for reproducibility. ~170 wpm at rate 180.
VOICE=Samantha
RATE=180

# Sentence pool: natural prose with proper nouns (stresses cleanup + vocab),
# no end punctuation (Parakeet emits raw lowercase-ish words; the .txt is the
# word-level ground truth, normalized before WER).
read -r -d '' POOL <<'EOF' || true
i think we should ship the parakeet refactor today before the standup
lingzi and tristan reviewed the cleanup prompt on macos this morning
the latency lab shows the gemma model warms up once at boot
we pushed the coreml build to github and it passed every test
the daemon drains the ring buffer when you release the hotkey
screen context harvests proper nouns from the focused window
remember to measure accuracy and latency before merging into main
the injector falls back to a clipboard paste inside electron apps
EOF

# Build a text of roughly N sentences by cycling the pool.
make_text () {
  local n=$1 i=0
  local -a lines
  while IFS= read -r line; do [ -n "$line" ] && lines+=("$line"); done <<< "$POOL"
  local count=${#lines[@]}
  local out=""
  while [ "$i" -lt "$n" ]; do
    out+="${lines[$((i % count))]} "
    i=$((i+1))
  done
  echo "$out"
}

# name  sentences  (tuned so durations land near 3/30/60/180s)
declare -a CLIPS=( "tiny 1" "short 7" "medium 14" "long 42" )

for spec in "${CLIPS[@]}"; do
  name=${spec%% *}; n=${spec##* }
  txt=$(make_text "$n")
  echo -n "$txt" > "$OUT/$name.txt"
  say -v "$VOICE" -r "$RATE" -o "$OUT/$name.aiff" "$txt"
  ffmpeg -y -loglevel error -i "$OUT/$name.aiff" -ar 16000 -ac 1 -sample_fmt s16 "$OUT/$name.wav"
  rm -f "$OUT/$name.aiff"
  dur=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$OUT/$name.wav")
  printf "  %-8s %6.1fs  %3d words\n" "$name" "$dur" "$(wc -w <<< "$txt")"
done
echo "corpus → $OUT"
