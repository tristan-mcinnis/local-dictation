# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Workflow

Work directly on `main` — no worktrees, no feature branches. After every change, commit it and
push to `origin/main` in the same step (don't wait to be asked). Keep the working tree clean.

## What this is

A local-first, push-to-talk dictation daemon for Apple Silicon macOS. Hold a hotkey, speak,
release — audio is transcribed (Parakeet TDT v3, CoreML), cleaned up (Gemma 3 1B, llama.cpp +
Metal), and injected at the cursor (macOS Accessibility, with a clipboard fallback for Electron
apps). Everything runs on-device; nothing hits the network at runtime. The crate is
`fast-dictate-backend` (lib `fast_dictate_backend` + bin `fast-dictate-backend`).

## Build, run, test

```bash
./scripts/download-models.sh                 # one-time: fetches Parakeet + Gemma 3 1B into ./models/
cargo build --features full --release        # the real build — see feature flags below
./target/release/fast-dictate-backend daemon # run the push-to-talk daemon

cargo test                                   # 64 unit + 2 integration, NO models or features needed
cargo test --features full                   # adds menubar/history/injector/cleaner/hotkey suites (75 total)
cargo test --features full <name>            # single test by name substring
```

Subcommands of the binary: `daemon [--no-cleanup]`, `logs`, `bench [wav]`, `dictate <ms>`,
`inject-test [text]`, `transform "<instruction>" "<text>"`, `ax-check`, `context-probe`,
`mock-loop`. (`context-probe` waits 4 s then prints the proper-noun terms harvested from the
focused window — the live check for screen-context vocabulary.) Dispatch is a
plain `match` in `src/main.rs`. The default subcommand is `mock-loop` from the CLI, but **`daemon`
when the binary is running inside a `.app` bundle** (a double-clicked app gets no args) — see
`app_paths::running_in_bundle`.

## Packaging as a Mac app (`Local Dictation.app`)

End users run **`./install.sh`** (repo root) — a one-shot installer that checks
prerequisites (Rust/CLT/cmake), downloads models if missing, then calls
`build-app.sh --install`. It's idempotent. The app itself is built by
`scripts/build-app.sh`:

```bash
./scripts/build-app.sh                  # dev: tiny app; models shared from ./models via a symlink
./scripts/build-app.sh --bundle-models  # ship: copies ONLY the recommended stack (~1.4 GB) into the app
./scripts/build-app.sh --install        # also copy to /Applications, add Login Item, launch it
```

The bundle is `dist/Local Dictation.app` — `LSUIElement` menu-bar agent (no Dock icon), ad-hoc
signed, `Info.plist` carries `NSMicrophoneUsageDescription` + a stable bundle id
(`com.tristanmcinnis.local-dictation`). Icon is rendered from an SF Symbol by
`scripts/make-icon.swift`. Because it's a stable signed bundle, macOS attributes Microphone +
Accessibility to the app itself (granted once), instead of to whatever terminal launched it.
Ad-hoc signing means a *rebuild* changes the signature hash and macOS may re-ask — a real
Developer ID would make grants stick. When bundled, the daemon redirects stdout/stderr to
`/tmp/dictate-daemon.log` (no terminal to print to).

**Model resolution** (`src/app_paths.rs`), in order: `DICTATE_MODELS_DIR` env >
`<app>/Contents/Resources/models` (shipped, self-contained) > `~/Library/Application
Support/Local Dictation/models` (dev symlink → repo) > `./models` (repo-relative, for `cargo run`).
Per-model env overrides (`PARAKEET_MODEL_DIR`, `GEMMA_MODEL_PATH`) and `settings.json` still win.
TODO (someday): let end users drop in / switch alternate cleanup models.

## Iterating fast (which loop am I in?)

The model + all config are read **once at daemon boot**, so changes take effect on relaunch — but
there are two very different loops, and only one needs a rebuild:

1. **Tuning prompts / formats / corrections (no code change).** Edit
   `~/.config/local-dictation/prompts.json` (or `settings.json` / `corrections.json`), then
   **`./scripts/reload-daemon.sh`** — it kills the running daemon and relaunches the *same*
   installed bundle, so the signature is unchanged and macOS keeps the Mic + Accessibility grants.
   Even faster: don't touch the live app at all — A/B candidate prompts with the offline harness
   **`cargo run --release --features cleaner --example prompt_lab`** (loads Gemma once, scores a
   corpus; see `prompts-lab/`). This is the right loop for prompt work.

2. **Changing code or built-in defaults** (`src/**`, the `DEFAULT_*` consts in `prompts.rs`).
   Rebuild + reinstall the bundle: **`./scripts/build-app.sh --install`**. Because the bundle is
   *ad-hoc* signed, a rebuild changes the signature hash and **macOS may revoke + re-prompt for
   Accessibility/Microphone** — the daemon will exit at boot (`daemon.rs` permission check) until
   you re-grant it in System Settings → Privacy & Security. An agent cannot grant this; it requires
   the user. (A real Developer ID signature would make the grant stick across rebuilds.)

Gotcha: `build-app.sh --install` runs `open`, which only *activates* an already-running instance
rather than replacing it — so after an `--install` you still need a real relaunch
(`reload-daemon.sh`, or quit from the menu bar) to run the new binary.

## Feature flags are the architecture

The crate is split by Cargo features so the lib and tests build without native/model deps. This
is the single most important thing to understand before editing — most modules are behind
`#[cfg(feature = ...)]` and a default `cargo build` compiles almost none of the real app.

- **`parakeet`** — ASR via `parakeet-rs` + `ort` (downloads onnxruntime at build time) + CoreML.
- **`cleaner`** — Gemma cleanup via `llama-cpp-2` (NOT `llama_cpp` 0.3.x, which vendors an old
  llama.cpp that can't load Gemma 3) + Metal.
- **`ax-inject`** — real macOS Accessibility text injection, clipboard fallback, menu-bar status
  item, floating waveform pill (objc2 / core-graphics / arboard stack).
- **`full`** = all three. Use this for any real build or when running the feature-gated tests.

When adding code, gate it behind the right feature and keep the default (featureless) build green
so `cargo test` stays fast and dependency-free.

## Pipeline data flow (the daemon path)

`daemon.rs` owns a `CGEventTap` watching the hotkey modifier + a worker thread. On key release it
drains audio and runs the pipeline:

```
audio.rs (cpal + SPSC ring buffer)
  → transcriber.rs (Parakeet → raw text)
  → cleaner.rs (Gemma cleanup; skipped if --no-cleanup / cleanup disabled)
  → text_polish.rs (strip LLM preamble/quotes/artefacts)
  → refiner.rs (corrections dict + voice-command parse) — shared by daemon & CLI
  → smart_pad.rs (spacing/capitalization from caret context)
  → injector.rs (AX direct, or clipboard paste for Electron) + voice_commands keystroke
```

`ui_channel.rs` carries worker→UI state (recording/processing, last dictation, audio levels) to
`menubar.rs` (status item, waveform pill, history window). `history.rs` writes every injected
dictation to SQLite at `~/.config/local-dictation/history.db`. The model is loaded once at boot,
so config changes (model/hotkey/cleanup) **relaunch** the daemon rather than hot-swapping.

`cleaner.rs` runs Gemma on a **dedicated worker thread that owns one persistent
`LlamaContext`** for the daemon's lifetime (a context borrows its model, so it can't live in
a struct). The worker tracks the resident token sequence and **reuses the shared KV-cache
prefix** with each new prompt (`clear_kv_cache_seq` wipes only the divergent tail), so the
~400-token constant cleanup instructions are decoded once at boot and reused — ~85 ms off
every utterance. Reuse is generic (token-sequence comparison, no hardcoded template) and
verified bit-identical to a from-scratch decode by the prefix-cache parity test in
`examples/latency_lab.rs`. `eval_cleanup_uncached` forces a full prefill for that test.

## Config & precedence

User config lives in `~/.config/local-dictation/` — the directory is resolved in
**one** place (`app_paths::config_dir` / `config_file`), so every consumer
(`settings.rs`, `prompts.rs`, `corrections.rs`, `history.rs`, the menu bar) shares
the same location rather than re-joining the path:
- `settings.json` (`settings.rs`) — written by the menu bar: `gemma_model`, `hotkey_keycode`, `cleanup_enabled`, `active_format`.
- `prompts.json` (`prompts.rs`) — hand-edited transform + cleanup system prompts, plus a `formats` map of named output-shape presets. Four presets ship built-in (`numbered`/`bullets`/`email`/`code`, in `DEFAULT_FORMATS`); a user's `prompts.json` `formats` map overlays them (same key overrides, new key adds). The active preset (`active_format`/`DICTATE_FORMAT`) replaces the cleanup prompt for normal dictation, and list presets keep their line breaks (the cleanup path preserves newlines when a preset is active); unknown/blank falls back to default. The default cleanup/transform prompts are tuned against Gemma 3 1B in `prompts-lab/` (a data-driven eval harness, `examples/prompt_lab.rs`); a deterministic `text_polish::fix_speech_mechanics` pass handles leftover filler + lone "i" so the prompt doesn't have to.
- `corrections.json` (`corrections.rs`) — word→replacement map (hard from→to fixes). Applied verbatim after cleanup **and** fed (target spellings only) into the cleanup prompt as a known-vocabulary hint via `prompts::vocabulary_suffix`.
- `dictionary.json` (`dictionary.rs`) — a flat JSON array of known words/phrases already spelled how you want (names, products, domain terms). A **soft** hint only: fed into the same cleanup vocabulary block so the model preserves them, but never rewrites text on its own (unlike corrections). `_`-prefixed strings are comments. `DICTATE_DICTIONARY_PATH` overrides the path. See `dictionary.example.json`. The cleanup *instructions* are the cached/warmed prefix; the vocabulary block is appended **per-utterance** (so the prefix cache reuses the instructions every time, and the vocab too when unchanged). Alongside the curated corrections terms, `screen_context.rs` harvests proper nouns from the focused window (field value + title, read in the parallel focus capture so it's off the critical path) — `extract_terms` ranks mixed-case/identifier/proper-noun tokens, caps at `SCREEN_VOCAB_CAP=16`, and the daemon passes them to `cleaner::process_transcript_with_vocab`. This stops Gemma from substituting similar-sounding names (measured: "Saoirse"→"Seamus" without it, correct with it). Verify coverage live with `context-probe`.

Precedence is always **env var > JSON file > built-in default**. Key env knobs:
`PARAKEET_MODEL_DIR`, `GEMMA_MODEL_PATH`, `DICTATE_HOTKEY_KEYCODE` (default `0x3D` Right Option),
`DICTATE_FORMAT` (active output-format preset), `DICTATE_QUIET`, `DICTATE_PROMPTS_PATH`,
`DICTATE_TRANSFORM_PROMPT`, `DICTATE_CLEANUP_PROMPT`,
`DICTATE_CORRECTIONS_PATH`, `FOCUS_APP`/`INJECT_DIAG` (scripted-test helpers). Models default to
`models/dictation/parakeet-tdt-v3-int8` and `models/llm/gemma-3-1b-it/...` inside the repo tree.

## Debugging

Structured per-utterance logs (transcribe/cleanup/inject timings, target app, injected text) go to
`/tmp/dictate-daemon.log` — `tail -f` it, or `fast-dictate-backend logs` to open it. First daemon
run needs Microphone + Accessibility permissions granted in System Settings; macOS ties
Accessibility permission to the launching terminal, so the daemon must be started from a real
terminal (an agent can't grant it). `ax-check` surfaces the permission prompt.
