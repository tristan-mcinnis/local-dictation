# S1-mini evaluation

`../examples/s1_mini_eval.rs` runs the deterministic corpus in
`s1_mini_corpus.json` against one warmed `TextCleanupEngine`.

```sh
cargo run --release --features cleaner --example s1_mini_eval
cargo run --release --features cleaner --example s1_mini_eval -- --reps 3
cargo run --release --features cleaner --example s1_mini_eval -- --compact --json
cargo run --release --features cleaner --example s1_mini_eval -- --prompt-file /tmp/candidate.txt --json
```

The runner resolves the model the same way the app does
(`app_paths::llm_default_path`: `DICTATE_LLM_MODEL_PATH` / `S1_MODEL_PATH` /
`GEMMA_MODEL_PATH` env,
then the prepared model stack under the models base dir). Override it with
`S1_MODEL_PATH=/path/to/model.gguf` or `--model /path/to/model.gguf`.
`GEMMA_MODEL_PATH` remains accepted for compatibility with the older model
labs.

Each case scores content anchors, forbidden filler/answer text, exact
dictionary spellings, minimum output/input character-count ratio,
runaway-output safety, and (where requested) terminal punctuation. The runner
forces every case through S1 so prompts can be compared; production may instead
route clean speech through the deterministic `fix_speech_mechanics` path, which
has its own unit regressions. Latency excludes model loading and the initial
warm-up. Use `--prompt-file` for candidate prompt A/B runs and `--json` to save
machine-readable output from stdout.
