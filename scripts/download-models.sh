#!/usr/bin/env bash
# Download the on-device models into ./models/. Idempotent: skips files that
# already exist with non-zero size.
#
# Default (no args): the recommended stack — Parakeet TDT v3 ASR + Qwen 2.5 1.5B
# cleanup. This is everything the daemon needs out of the box.
#
# Opt-in extra ASR backend:
#   ./scripts/download-models.sh --cohere            # Cohere Transcribe int4 (default)
#   ./scripts/download-models.sh --cohere int8       # …or the int8 variant
#   ./scripts/download-models.sh --cohere-only int4  # ONLY fetch Cohere, skip the defaults
#
# Opt-in alternative ASR backend (English-only, ONNX, no ggml — the clean
# Whisper alternative; whisper.cpp/whisper-rs is blocked here by a ggml symbol
# clash with llama-cpp-2):
#   ./scripts/download-models.sh --moonshine         # Moonshine Base (default, ~9-10% WER)
#   ./scripts/download-models.sh --moonshine tiny    # …or the faster/rougher Tiny
#   ./scripts/download-models.sh --moonshine-only base # ONLY fetch Moonshine
#
# Opt-in Whisper backend (whisper.cpp + Metal; mutually exclusive with the
# `cleaner` feature at build time — both vendor ggml):
#   ./scripts/download-models.sh --whisper         # large-v3-turbo q5_0 (default)
#
# Cohere is a 2B encoder/decoder (~1–2.5 GB on disk) and is NOT downloaded by
# default — select it with the menu / `DICTATE_ASR_BACKEND=cohere` after fetching.
# Moonshine is a 27M/61M encoder/decoder (~60-120 MB int8) — select it with the
# menu / `DICTATE_ASR_BACKEND=moonshine` after fetching.
# Whisper is a single ggml .bin (~547 MB q5_0) — select it with the menu /
# `DICTATE_ASR_BACKEND=whisper` after fetching (build with --features whisper-stack).

set -euo pipefail
cd "$(dirname "$0")/.."

# --- parse args -------------------------------------------------------------
WANT_DEFAULT=1          # fetch Parakeet + Qwen
WANT_COHERE=0           # fetch Cohere
COHERE_QUANT="int4"     # int4 | int8
WANT_MOONSHINE=0        # fetch Moonshine
MOONSHINE_SIZE="base"   # tiny | base
WANT_WHISPER=0          # fetch Whisper
while [ $# -gt 0 ]; do
  case "$1" in
    --cohere)       WANT_COHERE=1; [ "${2:-}" = "int4" -o "${2:-}" = "int8" ] && { COHERE_QUANT="$2"; shift; } ;;
    --cohere-only)  WANT_COHERE=1; WANT_DEFAULT=0; [ "${2:-}" = "int4" -o "${2:-}" = "int8" ] && { COHERE_QUANT="$2"; shift; } ;;
    --moonshine)       WANT_MOONSHINE=1; [ "${2:-}" = "tiny" -o "${2:-}" = "base" ] && { MOONSHINE_SIZE="$2"; shift; } ;;
    --moonshine-only)  WANT_MOONSHINE=1; WANT_DEFAULT=0; [ "${2:-}" = "tiny" -o "${2:-}" = "base" ] && { MOONSHINE_SIZE="$2"; shift; } ;;
    --whisper)      WANT_WHISPER=1 ;;
    --whisper-only) WANT_WHISPER=1; WANT_DEFAULT=0 ;;
    int4|int8)      COHERE_QUANT="$1" ;;
    tiny|base)      MOONSHINE_SIZE="$1" ;;
    -h|--help)      sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *)              echo "unknown arg: $1" >&2; exit 1 ;;
  esac
  shift
done

PARAKEET_DIR="models/dictation/parakeet-tdt-v3-int8"
# `LLM_DIR` is the recommended cleanup model; the var was historically GEMMA_DIR.
LLM_DIR="models/llm/qwen-2.5-1.5b-it"

# Honour HF_ENDPOINT (same var huggingface_hub uses) so users where
# huggingface.co is blocked/slow — notably mainland China — can mirror, e.g.
#   HF_ENDPOINT=https://hf-mirror.com ./scripts/download-models.sh
HF_ENDPOINT="${HF_ENDPOINT:-https://huggingface.co}"
HF_ENDPOINT="${HF_ENDPOINT%/}"
PARAKEET_BASE="$HF_ENDPOINT/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main"
LLM_BASE="$HF_ENDPOINT/Qwen/Qwen2.5-1.5B-Instruct-GGUF/resolve/main"

fetch() {
  local url="$1" out="$2"
  if [ -s "$out" ]; then
    echo "  ✓ $out already present ($(du -h "$out" | cut -f1)), skipping"
    return
  fi
  echo "  ↓ $out"
  curl -L --fail --progress-bar --remote-time -o "$out.partial" "$url"
  mv "$out.partial" "$out"
}

# --- Cohere Transcribe (optional second ASR backend) ------------------------
# The repos transcribe-rs is built/tested against. The loader expects, in the
# model dir: cohere-encoder.<q>.onnx (+ .data), cohere-decoder.<q>.onnx (+ .data
# for int4), and tokens.txt — see app_paths::cohere_default_dir + transcriber.rs.
download_cohere() {
  local q="$1"
  local dir="models/dictation/cohere-$q"
  local repo base
  if [ "$q" = "int4" ]; then
    repo="cstr/cohere-transcribe-onnx-int4"
  else
    repo="tristanripke/cohere-transcribe-onnx-int8"
  fi
  base="$HF_ENDPOINT/$repo/resolve/main"
  mkdir -p "$dir"
  echo
  echo "==> Cohere Transcribe 03-2026 ONNX ($q) — from $repo"
  echo "    2B Conformer encoder/decoder, 14 languages. Higher accuracy than"
  echo "    Parakeet at a higher per-utterance latency (autoregressive decode)."
  echo "    Select it after download via the menu or DICTATE_ASR_BACKEND=cohere."
  fetch "$base/cohere-encoder.$q.onnx"      "$dir/cohere-encoder.$q.onnx"
  fetch "$base/cohere-encoder.$q.onnx.data" "$dir/cohere-encoder.$q.onnx.data"
  fetch "$base/cohere-decoder.$q.onnx"      "$dir/cohere-decoder.$q.onnx"
  # int4 ships an external-weights .data sidecar for the decoder; int8 bakes the
  # decoder weights in (no sidecar). Fetch the sidecar only when it exists.
  if curl -sfI "$base/cohere-decoder.$q.onnx.data" >/dev/null 2>&1; then
    fetch "$base/cohere-decoder.$q.onnx.data" "$dir/cohere-decoder.$q.onnx.data"
  fi
  fetch "$base/tokens.txt" "$dir/tokens.txt"
}

# --- Moonshine (optional third ASR backend, ONNX) ---------------------------
# UsefulSensors Moonshine v1 (English-only). The transcribe-rs loader expects,
# in the model dir: encoder_model.int8.onnx, decoder_model_merged.int8.onnx,
# and tokenizer.json — see app_paths::moonshine_default_dir + transcriber.rs.
# We fetch the int8 variants from onnx-community (smaller + faster on M-series;
# the HF files use an underscore, so we rename to the dot-separated form the
# loader's resolve_model_path looks for under Quantization::Int8).
download_moonshine() {
  local size="$1"
  local size_pretty="Base"
  [ "$size" = "tiny" ] && size_pretty="Tiny"
  local dir="models/dictation/moonshine-$size"
  local base="$HF_ENDPOINT/onnx-community/moonshine-${size}-ONNX/resolve/main"
  mkdir -p "$dir"
  echo
  echo "==> Moonshine v1 $size_pretty ONNX (int8) — from onnx-community/moonshine-${size}-ONNX"
  echo "    English-only encoder/decoder. Lower latency than Whisper (variable-length"
  echo "    input, no 30 s zero-padding) and no ggml clash with the cleanup LLM."
  echo "    Select it after download via the menu or DICTATE_ASR_BACKEND=moonshine."
  fetch "$base/onnx/encoder_model_int8.onnx"        "$dir/encoder_model.int8.onnx"
  fetch "$base/onnx/decoder_model_merged_int8.onnx" "$dir/decoder_model_merged.int8.onnx"
  fetch "$base/tokenizer.json"                      "$dir/tokenizer.json"
}

# --- Whisper (optional fourth ASR backend, whisper.cpp + Metal) -------------
# OpenAI Whisper via whisper.cpp (ggml format). A single .bin file loaded by
# transcribe-rs' WhisperEngine. The default is large-v3-turbo q5_0 (~547 MB):
# best speed/accuracy in the Whisper family (4 decoder layers vs 32 in large-v3).
# NOTE: whisper.cpp vendors ggml, which clashes with llama-cpp-2 (the cleanup
# LLM) — build with --features whisper-stack (no cleaner) to use this backend.
download_whisper() {
  local dir="models/dictation/whisper"
  local file="ggml-large-v3-turbo-q5_0.bin"
  local base="$HF_ENDPOINT/ggerganov/whisper.cpp/resolve/main"
  mkdir -p "$dir"
  echo
  echo "==> Whisper large-v3-turbo (q5_0, ~547 MB) — from ggerganov/whisper.cpp"
  echo "    Multilingual, punctuated output. whisper.cpp + Metal on Apple Silicon."
  echo "    NOTE: mutually exclusive with the cleaner feature (both vendor ggml)."
  echo "    Build with --features whisper-stack; select via DICTATE_ASR_BACKEND=whisper."
  fetch "$base/$file" "$dir/$file"
}

if [ "$WANT_DEFAULT" -eq 0 ]; then
  [ "$WANT_COHERE" -eq 1 ]   && download_cohere "$COHERE_QUANT"
  [ "$WANT_MOONSHINE" -eq 1 ] && download_moonshine "$MOONSHINE_SIZE"
  [ "$WANT_WHISPER" -eq 1 ]  && download_whisper
  echo; echo "==> Done. Total on disk:"; du -sh models/*/ 2>/dev/null || true
  exit 0
fi

mkdir -p "$PARAKEET_DIR" "$LLM_DIR"

echo "==> Parakeet TDT v3 INT8 (~640 MB)"
fetch "$PARAKEET_BASE/encoder-model.int8.onnx"       "$PARAKEET_DIR/encoder-model.int8.onnx"
fetch "$PARAKEET_BASE/decoder_joint-model.int8.onnx" "$PARAKEET_DIR/decoder_joint-model.int8.onnx"
fetch "$PARAKEET_BASE/vocab.txt"                     "$PARAKEET_DIR/vocab.txt"

echo
echo "==> Qwen 2.5 1.5B-IT Q4_K_M GGUF (~1.0 GB)"
echo "    Best quality-per-ms for the dictation cleanup task — highest pass"
echo "    rate in the bake-off (examples/model_bakeoff.rs), beating models 3×"
echo "    its size, at ~140 ms hot cleanup latency on M-series:"
echo "      - Preserves domain casing (macOS, GitHub, iOS, TypeScript)."
echo "      - Handles reported-speech quoting and spoken numbers/decimals."
echo "      - Removes fillers, expands colloquial contractions cleanly."
echo "    Alternatives (drop into models/llm/<name>/ to pick from the menu):"
echo "      - qwen-2.5-0.5b-it  → fastest usable (~65 ms), slightly rougher."
echo "      - gemma-3-4b-it     → max polish on long sentences, ~2.4× slower."
fetch "$LLM_BASE/qwen2.5-1.5b-instruct-q4_k_m.gguf" "$LLM_DIR/qwen2.5-1.5b-instruct-q4_k_m.gguf"

# Optional second ASR backend, only when --cohere was passed.
if [ "$WANT_COHERE" -eq 1 ]; then
  download_cohere "$COHERE_QUANT"
fi

# Optional third ASR backend, only when --moonshine was passed.
if [ "$WANT_MOONSHINE" -eq 1 ]; then
  download_moonshine "$MOONSHINE_SIZE"
fi

# Optional fourth ASR backend, only when --whisper was passed.
if [ "$WANT_WHISPER" -eq 1 ]; then
  download_whisper
fi

echo
echo "==> Done. Total on disk:"
du -sh models/*/
