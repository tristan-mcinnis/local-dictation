#!/bin/zsh
# Prepare this user's fixed local model stack. No model bytes are committed.
#
# Model BYTES live in ~/Models (the canonical local-models home, registered in
# ~/Models/models.json); the repo's models/ entries are symlinks into it, so
# app_paths resolution (./models, Application Support, bundle) is unchanged.
set -euo pipefail

repo_root="${0:A:h:h}"
cd "$repo_root"

models_home="${LOCAL_MODELS_HOME:-$HOME/Models}"
super_root="${SUPERWHISPER_SUPPORT_DIR:-$HOME/Library/Application Support/superwhisper}"
cohere_source="${SUPERWHISPER_COHERE_DIR:-$super_root/models/cohere-transcribe/cohere-transcribe-q4}"
s1_source="${S1_MODEL_PATH:-$super_root/s1-mini.gguf}"

# "S1-mini" by "Superwhisper", Apache-2.0 with a naming clause: open weights,
# published at huggingface.co/superwhisper/s1-mini-GGUF. A local Superwhisper
# install holds the same bytes, so it is used when present and downloaded
# otherwise. The digest is the published q4_k_m file; see THIRD-PARTY-NOTICES.md.
s1_url="https://huggingface.co/superwhisper/s1-mini-GGUF/resolve/main/s1-mini-q4_k_m.gguf"
s1_sha256="3b41ebe2502cbd03e811d5d16b022f5ab551eda58d62597d152f89535003c634"
s1_bytes=484219808

test -f "$cohere_source/model.safetensors" || {
  print -u2 "Missing Cohere Q4 checkpoint at $cohere_source"
  print -u2 "The Cohere Transcribe weights are Apache-2.0 (CohereLabs/cohere-transcribe-03-2026),"
  print -u2 "but this script still expects the local 4-bit MLX conversion. Set SUPERWHISPER_COHERE_DIR"
  print -u2 "to a directory holding model.safetensors."
  exit 2
}

# Cohere: convert into ~/Models/cohere-mlx-q4, symlink the repo entry to it.
cohere_dest="$models_home/cohere-mlx-q4"
python3 scripts/models/convert_superwhisper_cohere.py "$cohere_source" "$cohere_dest"
mkdir -p "$repo_root/models/dictation"
ln -sfn "$cohere_dest" "$repo_root/models/dictation/cohere-mlx-q4"

# S1-mini: local copy if there is one, else download; verify the digest either way.
s1_dest="$models_home/gguf/s1-mini.gguf"
mkdir -p "$models_home/gguf" "$repo_root/models/llm/s1-mini"
s1_ok() { [[ -f "$1" && $(stat -f %z "$1") == "$s1_bytes" && $(shasum -a 256 "$1" | cut -d' ' -f1) == "$s1_sha256" ]] }

if s1_ok "$s1_dest"; then
  print "S1-mini already in place ($s1_dest)."
elif [[ -f "$s1_source" ]]; then
  cp -c "$s1_source" "$s1_dest" 2>/dev/null || cp "$s1_source" "$s1_dest"
  s1_ok "$s1_dest" || { print -u2 "Copied S1-mini does not match the published digest."; exit 2 }
  print "Copied S1-mini from $s1_source."
else
  print "Downloading S1-mini (462 MB) from Hugging Face…"
  curl -fL --retry 3 -C - -o "$s1_dest.part" "$s1_url"
  mv "$s1_dest.part" "$s1_dest"
  s1_ok "$s1_dest" || { print -u2 "Downloaded S1-mini does not match the published digest."; exit 2 }
  # The model's licence requires its LICENSE and NOTICE to travel with it.
  curl -fsL -o "$models_home/gguf/s1-mini-LICENSE.txt" \
    "https://huggingface.co/superwhisper/s1-mini-GGUF/raw/main/LICENSE"
  curl -fsL -o "$models_home/gguf/s1-mini-NOTICE.txt" \
    "https://huggingface.co/superwhisper/s1-mini-GGUF/raw/main/NOTICE"
  print "Downloaded \"S1-mini\" by \"Superwhisper\" with its LICENSE and NOTICE."
fi
ln -sfn "$s1_dest" "$repo_root/models/llm/s1-mini/s1-mini.gguf"

native/cohere-mlx/build.sh >/dev/null
print "Fixed Cohere MLX + S1-mini stack is ready (bytes in ~/Models, repo symlinked)."
