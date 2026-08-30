# S1-mini evaluation

`../examples/s1_mini_eval.rs` runs the deterministic corpus in
`s1_mini_corpus.json` against one warmed `TextCleanupEngine`.

```sh
cargo run --release --features cleaner --example s1_mini_eval
cargo run --release --features cleaner --example s1_mini_eval -- --reps 3
cargo run --release --features cleaner --example s1_mini_eval -- --compact --json
```

The runner uses `/Users/user/Library/Application Support/superwhisper/s1-mini.gguf`
by default. Override it with `S1_MODEL_PATH=/path/to/model.gguf` or
`--model /path/to/model.gguf`. `GEMMA_MODEL_PATH` remains accepted for
compatibility with the older model labs.

Each case scores content anchors, forbidden filler/answer text, exact
dictionary spellings, runaway-output safety, and (where requested) terminal
punctuation. Latency excludes model loading and the initial warm-up. Use
`--json` to save machine-readable output from stdout.
