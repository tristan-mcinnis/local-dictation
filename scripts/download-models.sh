#!/usr/bin/env bash
# Download the ONNX (Parakeet) and GGUF (Qwen 2.5 1.5B) models into ./models/.
# Idempotent: skips files that already exist with non-zero size.

set -euo pipefail
cd "$(dirname "$0")/.."

PARAKEET_DIR="models/dictation/parakeet-tdt-v3-int8"
# `LLM_DIR` is the recommended cleanup model; the var was historically GEMMA_DIR.
LLM_DIR="models/llm/qwen-2.5-1.5b-it"
mkdir -p "$PARAKEET_DIR" "$LLM_DIR"

PARAKEET_BASE="https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main"
LLM_BASE="https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct-GGUF/resolve/main"

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

echo
echo "==> Done. Total on disk:"
du -sh models/*/
