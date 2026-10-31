#!/bin/zsh
# Prepare the fixed local model stack. No model bytes are committed.
#
# Model BYTES live in ~/Models (override with LOCAL_MODELS_HOME); the repo's
# models/ entries are symlinks into it, so app_paths resolution (./models,
# Application Support, bundle) is unchanged.
#
# Speech model: Cohere Transcribe 03-2026 (Cohere Labs, Apache-2.0) as a public
# 4-bit MLX conversion by beshkenadze, pinned to one revision and verified by
# SHA-256. A local Superwhisper install, when present, is re-keyed instead and
# saves the 1.5 GB download. Cleanup model: "S1-mini" by "Superwhisper".
#
# Two modes:
#   Source tree (default): bytes go to ~/Models, the repo's models/ entries are
#     symlinks to them, and the native MLX helper is built.
#   App mode: the copy inside "Local Dictation.app/Contents/Resources" (also
#     `--app`). No repo, no Xcode: bytes go straight to the folder the app reads,
#     ~/Library/Application Support/Local Dictation/models (DICTATE_MODELS_DIR
#     overrides it). Use this after installing a downloaded build.
set -euo pipefail

script_dir="${0:A:h}"
app_mode=0
[[ "${1:-}" == "--app" ]] && app_mode=1
if [[ ! -f "$script_dir/../Cargo.toml" ]]; then app_mode=1; fi

if (( app_mode )); then
  repo_root="$script_dir"                       # LICENSE-APACHE sits beside this script
  app_models="${DICTATE_MODELS_DIR:-$HOME/Library/Application Support/Local Dictation/models}"
  models_home="$app_models"
else
  repo_root="${0:A:h:h}"
  cd "$repo_root"
  models_home="${LOCAL_MODELS_HOME:-$HOME/Models}"
fi
super_root="${SUPERWHISPER_SUPPORT_DIR:-$HOME/Library/Application Support/superwhisper}"
cohere_source="${SUPERWHISPER_COHERE_DIR:-$super_root/models/cohere-transcribe/cohere-transcribe-q4}"
s1_source="${S1_MODEL_PATH:-$super_root/s1-mini.gguf}"

# "S1-mini" by "Superwhisper", Apache-2.0 with a naming clause: open weights,
# published at huggingface.co/superwhisper/s1-mini-GGUF. A local Superwhisper
# install holds the same bytes, so it is used when present and downloaded
# otherwise. The digest is the published q4_k_m file; see THIRD_PARTY_NOTICES.md.
s1_rev="34add00a48a2e5d24e5a4ee5405a99620a3a240c"
s1_url="https://huggingface.co/superwhisper/s1-mini-GGUF/resolve/$s1_rev/s1-mini-q4_k_m.gguf"
s1_sha256="3b41ebe2502cbd03e811d5d16b022f5ab551eda58d62597d152f89535003c634"
s1_bytes=484219808

# Cohere Transcribe 4-bit MLX, already in the layout mlx-audio-swift loads.
# Pinned revision; every file is checked against its published byte count and
# SHA-256 before it is used.
cohere_repo="beshkenadze/cohere-transcribe-03-2026-mlx-4bit"
cohere_rev="104bc4391b5b1a12b040859793d7148525e1a08c"
cohere_files=(
  "model.safetensors 1505114042 5284ab5b678da720da092604323c7ce82cffe544e42b2da95064fbc85e281609"
  "config.json 4336 f5f4e46ea8a74e4e868d08504cb212afca7abfe55cd900d9b677bb2f9a1c210b"
  "preprocessor_config.json 420 9f297d330646ecc8ebb9dc5784f48b7c35b118c913e306a1ccd0192f2c976332"
  "special_tokens_map.json 4091 1814ce01458ff6a72b04a6618e75f18ce627be4dc17619cd3a7cd7f71e137f0f"
  "tokenizer.model 492827 6d21e6a83b2d0d3e1241a7817e4bef8eb63bcb7cfe4a2675af9a35ff3bbf0e14"
  "tokenizer_config.json 48141 0dfeb3eeba07bccaa1b4bf78f3135ad3059acf8d18f681675832b285ac0035b0"
)
file_ok() { [[ -f "$1" && $(stat -f %z "$1") == "$2" && $(shasum -a 256 "$1" | cut -d' ' -f1) == "$3" ]] }

if (( app_mode )); then
  cohere_dest="$app_models/dictation/cohere-mlx-q4"
else
  cohere_dest="$models_home/cohere-mlx-q4"
fi
# Complete means every file the helper loads is there. A run that died midway
# leaves some missing, and the next run fetches only those.
cohere_complete() {
  local row parts
  for row in "${cohere_files[@]}"; do
    parts=(${=row})
    [[ -s "$cohere_dest/$parts[1]" ]] || return 1
  done
}
if cohere_complete; then
  print "Cohere Transcribe Q4 already in place ($cohere_dest)."
elif (( ! app_mode )) && [[ -f "$cohere_source/model.safetensors" ]]; then
  # A local Superwhisper install: re-key its header names for mlx-audio-swift.
  python3 scripts/models/convert_superwhisper_cohere.py "$cohere_source" "$cohere_dest"
  print "Re-keyed Cohere Transcribe Q4 from $cohere_source."
else
  print "Downloading Cohere Transcribe Q4 MLX (1.5 GB) from Hugging Face…"
  mkdir -p "$cohere_dest"
  for row in "${cohere_files[@]}"; do
    parts=(${=row}); name=$parts[1]; bytes=$parts[2]; sha=$parts[3]
    target="$cohere_dest/$name"
    if ! file_ok "$target" "$bytes" "$sha"; then
      curl -fL --retry 3 -C - -o "$target.part" \
        "https://huggingface.co/$cohere_repo/resolve/$cohere_rev/$name"
      file_ok "$target.part" "$bytes" "$sha" || {
        print -u2 "Downloaded $name does not match the pinned digest; deleting it."
        rm -f "$target.part"; exit 2
      }
      mv "$target.part" "$target"
    fi
  done
  print "Downloaded Cohere Transcribe Q4."
fi
# Apache-2.0 asks for the licence to travel with the weights. Neither the
# conversion repo nor a re-keyed local copy ships one, so save ours (same text).
[[ -s "$cohere_dest/LICENSE" ]] || cp "$repo_root/LICENSE-APACHE" "$cohere_dest/LICENSE"
[[ -s "$cohere_dest/NOTICE" ]] ||
  print "Cohere Transcribe 03-2026 by Cohere Labs (Apache-2.0), 4-bit MLX conversion by beshkenadze." \
    > "$cohere_dest/NOTICE"
if (( ! app_mode )); then
  mkdir -p "$repo_root/models/dictation"
  ln -sfn "$cohere_dest" "$repo_root/models/dictation/cohere-mlx-q4"
fi

# S1-mini: local copy if there is one, else download; verify the digest either way.
if (( app_mode )); then
  s1_dir="$app_models/llm/s1-mini"
  s1_dest="$s1_dir/s1-mini.gguf"
  s1_license="$s1_dir/LICENSE.txt"; s1_notice="$s1_dir/NOTICE.txt"
else
  s1_dir="$models_home/gguf"
  s1_dest="$s1_dir/s1-mini.gguf"
  s1_license="$s1_dir/s1-mini-LICENSE.txt"; s1_notice="$s1_dir/s1-mini-NOTICE.txt"
  mkdir -p "$repo_root/models/llm/s1-mini"
fi
mkdir -p "$s1_dir"
s1_ok() { [[ -f "$1" && $(stat -f %z "$1") == "$s1_bytes" && $(shasum -a 256 "$1" | cut -d' ' -f1) == "$s1_sha256" ]] }

if s1_ok "$s1_dest"; then
  print "S1-mini already in place ($s1_dest)."
elif [[ -f "$s1_source" ]]; then
  # Verify the copy before it takes the name the app loads.
  rm -f "$s1_dest.part"
  cp -c "$s1_source" "$s1_dest.part" 2>/dev/null || cp "$s1_source" "$s1_dest.part"
  s1_ok "$s1_dest.part" || {
    print -u2 "Copied S1-mini does not match the published digest; deleting it."
    rm -f "$s1_dest.part"; exit 2
  }
  mv "$s1_dest.part" "$s1_dest"
  print "Copied S1-mini from $s1_source."
else
  print "Downloading S1-mini (462 MB) from Hugging Face…"
  curl -fL --retry 3 -C - -o "$s1_dest.part" "$s1_url"
  s1_ok "$s1_dest.part" || {
    print -u2 "Downloaded S1-mini does not match the published digest; deleting it."
    rm -f "$s1_dest.part"; exit 2
  }
  mv "$s1_dest.part" "$s1_dest"
  print "Downloaded \"S1-mini\" by \"Superwhisper\"."
fi
# The model's licence requires its LICENSE and NOTICE to travel with it,
# whichever way the weights arrived.
for pair in "LICENSE $s1_license" "NOTICE $s1_notice"; do
  parts=(${=pair})
  [[ -s "$parts[2]" ]] && continue
  curl -fsL -o "$parts[2].part" \
    "https://huggingface.co/superwhisper/s1-mini-GGUF/raw/$s1_rev/$parts[1]"
  mv "$parts[2].part" "$parts[2]"
done
if (( app_mode )); then
  print "Fixed Cohere MLX + S1-mini stack is ready in $app_models."
  print "Quit and reopen Local Dictation (menu bar icon, Quit) to load the models."
else
  ln -sfn "$s1_dest" "$repo_root/models/llm/s1-mini/s1-mini.gguf"
  ln -sfn "$s1_license" "$repo_root/models/llm/s1-mini/LICENSE.txt"
  ln -sfn "$s1_notice" "$repo_root/models/llm/s1-mini/NOTICE.txt"

  native/cohere-mlx/build.sh >/dev/null
  print "Fixed Cohere MLX + S1-mini stack is ready (bytes in $models_home, repo symlinked)."
fi
