# Local Dictation — repo contract

`AGENTS.md` is a symlink to this file. Both runtimes read one contract.
Deep architecture (pipeline, bridge protocol, config precedence, every env
var) lives in `docs/architecture.md`; keep it in sync when behaviour moves.

## What this is

A local-first push-to-talk dictation daemon for Apple Silicon macOS. Hold a
key, speak, release: audio goes to Cohere Transcribe Q4 (native MLX Swift
helper, `native/cohere-mlx/`), then guarded S1-mini cleanup (llama.cpp +
Metal), then injection at the cursor (Accessibility, clipboard fallback for
Electron/terminals). Crate `fast-dictate-backend`; product name
**Local Dictation**. Everything runs on-device; nothing hits the network at
runtime.

## Build, run, test

```bash
./scripts/download-models.sh                   # one-time model stack
cargo build --features full --release          # the real build
./target/release/fast-dictate-backend daemon   # run the daemon
cargo test                                     # no models, no native deps
cargo test --features full                     # adds cleaner/injector/menubar/history suites
native/cohere-mlx/test.sh                      # Swift bridge protocol tests
./scripts/build-app.sh --install               # bundle + /Applications + login item
./scripts/reload-daemon.sh                     # relaunch to pick up config edits
```

Run `cargo test` after every source change. A rebuild re-signs ad-hoc, so
macOS may re-ask for Microphone + Accessibility; only the user can grant it.

## Boundaries (do not widen without a design)

- The ASR is fixed: Cohere MLX Q4. The only ASR knob is the decoder
  language. No model picker for ASR, no cloud providers, no accounts, no
  telemetry, no network at runtime.
- Features are the architecture: `cohere-mlx`, `cleaner`, `ax-inject`,
  `full`. Gate new code behind the right feature; keep featureless
  `cargo test` green and dependency-free.
- Lowest-latency primitive wins. Measure with the `examples/*_lab` harnesses
  before adding anything on the critical path. No OCR, vision LLMs, or
  whole-window scraping.
- The cleanup model gets the tightest relevant context only.

## One way of doing things

- Paths: every per-user location resolves through `src/app_paths.rs`
  (`home_dir`, `config_dir`, `config_file`, `cache_dir`, `app_support_dir`,
  `models_base_dir`, `DAEMON_LOG_PATH`). Never re-join `$HOME` elsewhere.
- Precedence is always env var > JSON file > built-in default, resolved in
  `settings.rs` `resolve_*` methods. Read config once at boot; relaunch to
  apply.
- Constants: default hotkey is `hotkeys::DEFAULT_KEYCODE`; keycode names come
  from `hotkeys::modifier_keycode_name`. No scattered `0x3D`.
- Process lookups by PID go through `injector::process_command_path`.
- Logging is `eprintln!` with a `[tag]` prefix to the daemon log. No logging
  crate.
- Child processes use `std::process::Command` with argument arrays. No
  shell interpolation.
- User data on disk (`~/.config/local-dictation/*.json`, `history.db`) keeps
  its format. Unknown keys are ignored by serde; never break an old file.
  Renaming a key means a migration: read the old key when the new one is
  missing, write the new key, keep the old read for one release. Current
  one: `gemma_model` → `llm_model` (env `GEMMA_MODEL_PATH` →
  `DICTATE_LLM_MODEL_PATH`, `S1_MODEL_PATH` stays an alias), 2026-09.
- Names are model-neutral: the cleanup LLM is `llm_*` in code, keys, and
  env vars, never the name of whichever model is loaded today.

## Repo hygiene

- `.memory/` is agent session notes (project context only, no secrets) and
  is tracked in git on purpose; the owner can untrack it at any time.

## Privacy

Never commit model bytes, API keys, personal dictionary or corrections
content, dictation history, logs, or `~/.config/local-dictation` data.
Example files (`*.example.json`) are the only config in the tree.

## Workflow

Work on `main`. Commit after each change; push to `origin/main` when asked
or when the session contract says so. Keep the tree clean.
