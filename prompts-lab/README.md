# prompts-lab — testing & tuning the Gemma prompts

This directory is a small evaluation harness for the two system prompts that
shape what Gemma 3 1B does to your dictation (`cleanup`, `transform`) plus the
output-format presets (`numbered`, `bullets`, `email`, `code`). It exists so
prompt changes are **measured, not vibed** — and so the trade-offs of driving a
~1B open-weights model are visible instead of guessed at.

## Run it

```bash
# needs the cleaner stack + a Gemma GGUF on disk (./scripts/download-models.sh)
cargo run --release --features cleaner --example prompt_lab

# compare a bigger model on the same corpus:
GEMMA_MODEL_PATH=models/llm/gemma-3-4b-it/gemma-3-4b-it-Q4_K_M.gguf \
  cargo run --release --features cleaner --example prompt_lab
```

It loads the model once, runs every case in [`lab.json`](lab.json), scores each
output against per-case heuristic checks, and writes a Markdown report + a raw
JSONL of every generation to `results/` (gitignored — they're build artifacts).

Once you've found prompts you like, apply them to the live app without a
rebuild: copy them into `~/.config/local-dictation/prompts.json` and run
`../scripts/reload-daemon.sh` (the daemon reads config at boot, so a relaunch is
all it needs). Only changing the *built-in defaults* (`DEFAULT_*` in
`src/prompts.rs`) needs `../scripts/build-app.sh --install`. See the "Iterating
fast" section of the repo `CLAUDE.md`.

The runner is [`../examples/prompt_lab.rs`](../examples/prompt_lab.rs); it calls
`TextCleanupEngine::eval_cleanup` / `eval_transform`, which mirror the exact
framing and token budget the production daemon uses, so the lab measures what
the app would actually inject (including the deterministic
`text_polish::fix_speech_mechanics` post-pass).

## How to think about testing prompts for a small local model

First principles, in priority order:

1. **Determinism is mostly an illusion you can lean on, then can't.** Production
   samples at `temp 0.2` with a *fixed* seed, so for a given input the output is
   stable *within a process*. But across separate process launches, Metal
   floating-point differences flip the occasional borderline token — so a prompt
   that "passes once" can regress. The harness runs each case `reps` times and
   flags drift; the real robustness probe, though, is **corpus breadth**, not
   reruns of one input.

2. **Don't ask the model to do what a regex does perfectly.** A 1B model is
   unreliable at the boring-but-mechanical (stripping a *leading* "um", lone
   lowercase "i"), yet a 5-line deterministic pass nails them every time. We
   moved those into `fix_speech_mechanics` and kept the prompt focused on the
   judgement calls only a model can make (is this "main" the branch or the US
   state? recase `macos` → `macOS`). Cheap, exact, and it can't hallucinate.

3. **Spend the model's limited instruction-following budget on the few rules
   that matter.** Long prompts dilute attention. Each cleanup bullet earns its
   place by fixing an observed failure; ceremony that didn't move a check came
   out.

4. **Score what you can, eyeball the rest.** "Did the proper noun survive?" and
   "is this a numbered list?" are automatable (see the `checks` block per case).
   "Does it read naturally?" is not — the report prints every raw output so the
   qualitative read is one glance.

5. **Name the trade-offs you can't fix.** Some failures are the model, not the
   prompt. Gemma 1B will not reliably expand "2 bullets → exactly 4" no matter
   how the instruction is phrased; it now at least returns a well-formed list
   instead of a prose wall. We keep the count nudge (it helps clearer cases and
   bigger models) and accept the miss rather than contort the prompt around it.

## What the tuning round changed (vs. the original prompts)

Measured on `gemma-3-1b-it-Q4_K_M`, all-checks-pass rate rose from **15/24 → 38/43**
configurations (the corpus also grew). Net wins on the shipped prompts:

- **Cleanup** now strips throat-clearing fillers even at the start, recases
  `macos`/`onnx`/`github` → `macOS`/`ONNX`/`GitHub`, stops dropping command
  tokens like `cd`, and (via the deterministic pass) always fixes lone "i" and
  leftover "um"/"uh".
- **Transform** now honors implied list structure ("turn these bullets into…"
  returns bullets, not a paragraph) and respects explicit counts where the model
  can.
- **Formats** are tuned + made built-in: `numbered` (new), `bullets` (no longer
  duplicates), `email` (no invented `Subject:`/`[Your Name]`), `code` (no ```
  fences). List presets now keep their line breaks through the cleanup path.
- **`text_polish`** gained defensive peels for ``` code fences and leaked
  list-intro lines ("Here's a bulleted list:") that small models still emit.

The only shipping-config miss is the exact-count expand case above — a model
limitation, documented and accepted.

## Editing the corpus / candidates

`lab.json` is pure data — add cases or candidate prompts and re-run, no
recompile needed (unless you change `text_polish`/`cleaner`, which the example
links against). Each case's `checks` support: `must_contain`, `must_not_contain`,
`must_keep` (case-sensitive), `expand` (`[from,to]`), `no_preamble`,
`single_line`, `min_lines`, `line_kind` (`numbered`/`bullet`), `max_len_ratio`.
