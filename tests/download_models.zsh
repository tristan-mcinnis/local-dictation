#!/bin/zsh
# Exercises scripts/download-models.sh offline: a fake curl on PATH, invented
# model bytes with matching digests patched into a copy of the script, and a
# temporary HOME and model store. Prints nothing and exits 0 on pass.
set -u
SRC=${SRC:-${0:A:h:h}/scripts/download-models.sh}
T=$(mktemp -d) || exit 1
trap 'rm -rf $T' EXIT
mkdir -p $T/bin
fail() { print "download_models.zsh: $*"; exit 1 }
names=(model.safetensors config.json preprocessor_config.json special_tokens_map.json tokenizer.model tokenizer_config.json)
sha() { print -n "good-$1" | shasum -a 256 | cut -d' ' -f1 }
sz() { print -n "good-$1" | wc -c | tr -d ' ' }

mk_repo() {
  rm -rf $T/repo; mkdir -p $T/repo/scripts/models $T/repo/native/cohere-mlx
  : > $T/repo/Cargo.toml; print apache > $T/repo/LICENSE-APACHE
  print '#!/bin/sh' > $T/repo/native/cohere-mlx/build.sh; chmod +x $T/repo/native/cohere-mlx/build.sh
  local s n; s=$(<$SRC)
  s=${s//3b41ebe2502cbd03e811d5d16b022f5ab551eda58d62597d152f89535003c634/$(sha s1-mini-q4_k_m.gguf)}
  s=${s//484219808/$(sz s1-mini-q4_k_m.gguf)}
  for n in $names; do
    s=$(print -r -- "$s" | perl -pe "s/^  \"\Q$n\E \\d+ [0-9a-f]+\"/  \"$n $(sz $n) $(sha $n)\"/")
  done
  print -r -- "$s" > $T/repo/scripts/download-models.sh
  cat > $T/repo/scripts/models/convert_superwhisper_cohere.py <<'P'
import sys, pathlib
d = pathlib.Path(sys.argv[2]); d.mkdir(parents=True, exist_ok=True)
for n in ["model.safetensors", "config.json", "preprocessor_config.json",
          "special_tokens_map.json", "tokenizer.model", "tokenizer_config.json"]:
    (d / n).write_text("rekeyed")
P
}

# Writes "good-<basename>"; BAD names a file to corrupt, FAILON one to fail.
cat > $T/bin/curl <<'C'
#!/bin/zsh
out=""; url=""
while (( $# )); do case $1 in -o) out=$2; shift 2;; -*) shift;; *) url=$1; shift;; esac; done
name=${url:t}
print -r -- "$url" >> $CURL_LOG
[[ -n "$FAILON" && $name == $FAILON ]] && exit 22
if [[ -n "$BAD" && $name == $BAD ]]; then print -n evil > $out; else print -n "good-$name" > $out; fi
C
chmod +x $T/bin/curl || exit 1

run() {
  [[ -x $T/bin/curl ]] || fail "no curl shim"
  env -i PATH=$T/bin:/usr/bin:/bin HOME=$T/home LOCAL_MODELS_HOME=$T/models \
    SUPERWHISPER_SUPPORT_DIR=$T/sw CURL_LOG=$T/curl.log BAD=${BAD:-} FAILON=${FAILON:-} \
    /bin/zsh $T/repo/scripts/download-models.sh >$T/out 2>&1
}
cohere=$T/models/cohere-mlx-q4
s1=$T/models/gguf/s1-mini.gguf
mk_repo

BAD=s1-mini-q4_k_m.gguf run; rc=$?
(( rc == 2 )) || fail "bad S1 download exited $rc"
[[ ! -e $s1 && ! -e $s1.part ]] || fail "bad S1 bytes left on disk"
grep -Eq '/resolve/[0-9a-f]{40}/s1-mini-q4_k_m.gguf$' $T/curl.log || fail "S1 URL not pinned to a revision"

rm -rf $T/models; mkdir -p $T/sw; print -n evil > $T/sw/s1-mini.gguf
run; rc=$?
(( rc == 2 )) || fail "bad S1 local copy exited $rc"
[[ ! -e $s1 && ! -e $s1.part ]] || fail "bad S1 local copy left on disk"
rm -rf $T/sw

rm -rf $T/models; : > $T/curl.log
FAILON=tokenizer.model run && fail "interrupted Cohere download exited 0"
[[ ! -e $cohere/tokenizer.model ]] || fail "failed file present"
: > $T/curl.log
run || fail "resumed run failed: $(<$T/out)"
grep -q "Cohere Transcribe Q4 already in place" $T/out && fail "half-downloaded Cohere dir reported complete"
grep -q tokenizer_config.json $T/curl.log || fail "resumed run skipped tokenizer_config.json"
grep -q model.safetensors $T/curl.log && fail "resumed run refetched a verified file"
for n in $names LICENSE NOTICE; do [[ -s $cohere/$n ]] || fail "Cohere dir missing $n"; done

rm -rf $T/models
BAD=config.json run; rc=$?
(( rc == 2 )) || fail "bad Cohere file exited $rc"
[[ ! -e $cohere/config.json && ! -e $cohere/config.json.part ]] || fail "bad Cohere file left on disk"

rm -rf $T/models; run || fail "clean run failed: $(<$T/out)"
for n in s1-mini.gguf LICENSE.txt NOTICE.txt; do
  [[ -s $T/repo/models/llm/s1-mini/$n ]] || fail "models/llm/s1-mini/$n missing (bundled builds copy this dir)"
done

rm -rf $T/models $T/repo/models
mkdir -p $T/sw/models/cohere-transcribe/cohere-transcribe-q4
print -n x > $T/sw/models/cohere-transcribe/cohere-transcribe-q4/model.safetensors
print -n good-s1-mini-q4_k_m.gguf > $T/sw/s1-mini.gguf
: > $T/curl.log
run || fail "Superwhisper run failed: $(<$T/out)"
[[ -s $cohere/LICENSE && -s $cohere/NOTICE ]] || fail "re-keyed Cohere dir has no LICENSE or NOTICE"
[[ -s $T/repo/models/llm/s1-mini/NOTICE.txt ]] || fail "copied S1 has no NOTICE"
grep -q gguf $T/curl.log && fail "local S1 copy also downloaded the weights"
run || fail "rerun failed"
grep -q "Cohere Transcribe Q4 already in place" $T/out || fail "re-keyed Cohere dir not seen as complete"
exit 0
