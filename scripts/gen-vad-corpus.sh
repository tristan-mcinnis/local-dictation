#!/usr/bin/env bash
# Generate round-3 VAD audio with KNOWN-TRUTH pause boundaries.
#
# Reads the round-2 corpus (prompts-lab/cleanup_stream.json) and, for each entry,
# synthesizes its GOLD sentences with `say`, inserting a fixed silence between
# them via `[[slnc MS]]`. Because we control the silences, we know exactly where
# the true sentence boundaries fall — so the lab can score a VAD's detected
# boundaries against ground truth, then run the streaming pipeline on real audio.
#
# Per entry it writes:
#   testdata/vad/<name>.wav         16 kHz mono PCM
#   testdata/vad/<name>.truth.json  { "pause_ms":N, "sentences":[...], "wav":... }
# (sentence boundary times are recovered in-lab from the silence gaps; the truth
#  file carries the gold sentences + the pause length used.)
#
# Usage: ./scripts/gen-vad-corpus.sh
set -euo pipefail
cd "$(dirname "$0")/.."
OUT=testdata/vad
mkdir -p "$OUT"
command -v say    >/dev/null || { echo "need macOS 'say'"; exit 1; }
command -v ffmpeg >/dev/null || { echo "need ffmpeg"; exit 1; }

VOICE=Samantha
RATE=180
PAUSE_MS=550   # inter-sentence silence ≈ a natural breath/period pause

python3 - "$OUT" "$VOICE" "$RATE" "$PAUSE_MS" <<'PY'
import json, subprocess, sys, os
out, voice, rate, pause = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
corpus = json.load(open("prompts-lab/cleanup_stream.json"))
for e in corpus["entries"]:
    name = e["name"]
    sents = [f["gold"] for f in e["fragments"]]
    # Join gold sentences with an explicit silence command between them.
    spoken = (f" [[slnc {pause}]] ").join(sents)
    aiff = f"{out}/{name}.aiff"
    wav  = f"{out}/{name}.wav"
    subprocess.run(["say","-v",voice,"-r",rate,"-o",aiff,spoken], check=True)
    subprocess.run(["ffmpeg","-y","-loglevel","error","-i",aiff,"-ar","16000",
                    "-ac","1","-sample_fmt","s16",wav], check=True)
    os.remove(aiff)
    dur = float(subprocess.check_output(["ffprobe","-v","error","-show_entries",
        "format=duration","-of","csv=p=0",wav]).decode().strip())
    json.dump({"wav":wav,"pause_ms":pause,"sentences":sents,"dur_s":dur,
               "tier":e["tier"],"names":e.get("names",[]),"vocab":e.get("vocab",[])},
              open(f"{out}/{name}.truth.json","w"), indent=2, ensure_ascii=False)
    print(f"  {name:<20} {dur:6.1f}s  {len(sents)} sentences")
print(f"corpus → {out}")
PY
