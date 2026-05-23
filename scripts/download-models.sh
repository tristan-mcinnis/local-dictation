#!/usr/bin/env bash
# Download the ONNX (Parakeet) and GGUF (Gemma 3 1B) models into ./models/.
# Idempotent: skips files that already exist with non-zero size.

set -euo pipefail
cd "$(dirname "$0")/.."

PARAKEET_DIR="models/parakeet-tdt-v3-int8"
GEMMA_DIR="models/gemma-3-1b-it"
mkdir -p "$PARAKEET_DIR" "$GEMMA_DIR"

PARAKEET_BASE="https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main"
GEMMA_BASE="https://huggingface.co/ggml-org/gemma-3-1b-it-GGUF/resolve/main"

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

echo "==> Parakeet TDT v3 INT8 (~640 MB)"
fetch "$PARAKEET_BASE/encoder-model.int8.onnx"       "$PARAKEET_DIR/encoder-model.int8.onnx"
fetch "$PARAKEET_BASE/decoder_joint-model.int8.onnx" "$PARAKEET_DIR/decoder_joint-model.int8.onnx"
fetch "$PARAKEET_BASE/vocab.txt"                     "$PARAKEET_DIR/vocab.txt"

echo
echo "==> Gemma 3 1B-IT Q4_K_M GGUF (~770 MB)"
echo "    Sweet spot for the dictation cleanup task:"
echo "      - Reliably follows the system instruction (filler removal,"
echo "        colloquial expansion, casing preservation)."
echo "      - ~310 ms hot cleanup latency on M-series."
echo "      - Same cleanup output as the 4B model on conversational English."
echo "    Alternatives: GEMMA_MODEL_PATH=models/gemma-3-4b-it/... for max"
echo "    polish; models/qwen-2.5-0.5b-it/... for raw speed (drops some"
echo "    quality, struggles with formal-spelling rules)."
fetch "$GEMMA_BASE/gemma-3-1b-it-Q4_K_M.gguf" "$GEMMA_DIR/gemma-3-1b-it-Q4_K_M.gguf"

echo
echo "==> Done. Total on disk:"
du -sh models/*/
