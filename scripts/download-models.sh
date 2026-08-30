#!/bin/zsh
# Prepare this user's fixed local model stack. No model bytes are committed.
set -euo pipefail

repo_root="${0:A:h:h}"
cd "$repo_root"

super_root="${SUPERWHISPER_SUPPORT_DIR:-$HOME/Library/Application Support/superwhisper}"
cohere_source="${SUPERWHISPER_COHERE_DIR:-$super_root/models/cohere-transcribe/cohere-transcribe-q4}"
s1_source="${S1_MODEL_PATH:-$super_root/s1-mini.gguf}"

test -f "$cohere_source/model.safetensors" || {
  print -u2 "Missing Cohere Q4 checkpoint at $cohere_source"
  exit 2
}
test -f "$s1_source" || {
  print -u2 "Missing S1-mini GGUF at $s1_source"
  exit 2
}

python3 scripts/models/convert_superwhisper_cohere.py \
  "$cohere_source" \
  "$repo_root/models/dictation/cohere-mlx-q4"

mkdir -p "$repo_root/models/llm/s1-mini"
ln -sfn "$s1_source" "$repo_root/models/llm/s1-mini/s1-mini.gguf"

native/cohere-mlx/build.sh >/dev/null
print "Fixed Cohere MLX + S1-mini stack is ready."
