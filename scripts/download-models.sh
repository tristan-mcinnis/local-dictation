#!/usr/bin/env bash
# Download the ONNX (Parakeet) and GGUF (Qwen 2.5 0.5B) models into ./models/.
# Idempotent: skips files that already exist with non-zero size.

set -euo pipefail
cd "$(dirname "$0")/.."

PARAKEET_DIR="models/parakeet-tdt-v3-int8"
QWEN_DIR="models/qwen-2.5-0.5b-it"
mkdir -p "$PARAKEET_DIR" "$QWEN_DIR"

PARAKEET_BASE="https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main"
QWEN_BASE="https://huggingface.co/bartowski/Qwen2.5-0.5B-Instruct-GGUF/resolve/main"

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
echo "==> Qwen 2.5 0.5B-Instruct Q4_K_M GGUF (~380 MB)"
echo "    (Qwen 0.5B is the snappy default — ~150 ms hot cleanup on M-series."
echo "     If you want stronger domain-casing fidelity (macOS, ONNX, etc.) at"
echo "     the cost of ~150 ms more per utterance, point GEMMA_MODEL_PATH at"
echo "     models/gemma-3-1b-it/gemma-3-1b-it-Q4_K_M.gguf or the 4B variant.)"
fetch "$QWEN_BASE/Qwen2.5-0.5B-Instruct-Q4_K_M.gguf" "$QWEN_DIR/Qwen2.5-0.5B-Instruct-Q4_K_M.gguf"

echo
echo "==> Done. Total on disk:"
du -sh models/*/
