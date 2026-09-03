# Software-of-one prune audit

Audit target: one-user macOS dictation app with exactly this product path:

```text
push-to-talk → CoreAudio capture → Cohere Transcribe MLX → personal corrections
             → S1-mini local cleanup → AX/clipboard injection → history
```

The current checkout is `main` at `e2ab911` (`feat(ui): add native AppKit
Settings window`). `cargo metadata --no-deps` reports one package, one library,
one binary, nine examples, and one integration test. The repo is already a
usable shell; this is a pruning/migration plan, not a recommendation to
rewrite the app.

## Decision legend

- **KEEP** — part of the target product or directly protects latency/reliability.
- **REMOVE** — no longer justified after the single-stack migration.
- **DEFER** — preserve temporarily behind a clear boundary; do not add new
  product/UI surface until the Cohere → S1-mini path is benchmarked.

“Remove” below means remove only after the safe-order sequence at the end. In
particular, do not delete a module while an old config or feature branch still
references it.

## Source/module map

| Area / exact reference | Decision | Rationale and migration note |
|---|---|---|
| `src/audio.rs` (`AudioCaptureEngine`, `AlwaysOnCapture`, ring buffer, resampling) | **KEEP** | Core PTT capture and latency-critical buffering. Keep the callback allocation audit and 16 kHz mono contract. `AlwaysOnCapture`/pre-roll can remain as an opt-in latency experiment, but it should not be entangled with wake-word mode. |
| `src/daemon.rs` | **KEEP / simplify** | This is the main PTT state machine and pipeline owner. Keep event tap, cancellation, warm workers, focus capture, cleanup, injection, history, and UI state. Delete the `listen` branch and streaming-cleanup branch only after the normal PTT path is stable. |
| `src/transcriber.rs` (`AsrBackend`, `Backend`, `AsrConfig`) | **REWRITE, then KEEP** | Keep the worker interface and input normalization; replace `transcribe-rs` ONNX `CohereModel` with the Cohere MLX implementation. Collapse `AsrBackend` to Cohere and remove Parakeet/Moonshine/Whisper variants, accelerator fallbacks, and alternate model paths. Current ONNX/CoreML claims are stale: lines 267–299 explicitly describe external-data failure and CPU fallback. |
| `src/cleaner.rs` | **REWRITE, then KEEP** | Keep the persistent Metal worker, prefix KV-cache reuse, cancellation, runaway-output guard, and vocabulary-aware API. Rename model/path terminology from Gemma to S1-mini and lock the runtime to the S1-mini GGUF. Remove generic model discovery and “any GGUF” assumptions after compatibility is proven. |
| `src/corrections.rs` | **KEEP** | This is the actual personal dictionary: phrase matching, identity/casing entries, phonetic fixes, editor serialization, save/reload. It is more important than a large prompt. Preserve longest-match behavior and pre/post-cleanup application. |
| `src/refiner.rs` | **KEEP** | Central deterministic tail. Keep correction ordering and command classification if voice commands remain. It prevents the daemon and CLI from diverging. |
| `src/text_polish.rs` | **KEEP** | Cheap deterministic cleanup belongs after S1-mini. Keep filler/mechanics, casing, quote/artefact stripping, and number conversion; validate each pass against the S1-mini corpus. |
| `src/numbers.rs` | **KEEP** | It is used by `text_polish.rs` (line 230), so it is not dead code. Keep only if its current tests show no unwanted conversions. |
| `src/prompts.rs` + `prompts.example.json` | **KEEP / simplify** | Keep one short default cleanup prompt, one optional transform prompt if transforms survive, and vocabulary suffix support. Remove the four-way model-era format/preset complexity unless it is genuinely used. Preserve user overrides during migration. |
| `src/dictionary.rs` + `dictionary.example.json` | **DEFER** | This is a second, flat known-word list consumed by `cleaner.rs`; the menu edits `corrections.json`, not this file. Prefer one canonical personal-vocabulary store first. Keep the loader temporarily for backward compatibility, then either merge it into corrections or remove it with a migration notice. |
| `src/screen_context.rs` | **DEFER** | Focused-window proper-noun harvesting helps names but adds AX reads and complexity. Keep the API behind a setting/compile boundary while measuring it against deterministic corrections. It should never block audio capture or be required for correctness. |
| `src/injector.rs` + `src/clipboard_paste.rs` | **KEEP** | Core output quality and the likely largest non-model latency lever. Keep direct AX replacement, Electron/clipboard fallback, clipboard preservation, focus capture, `smart_pad`, and command keystrokes. Instrument “text visible” separately from “clipboard restored.” |
| `src/smart_pad.rs` | **KEEP** | Cheap caret-aware spacing/capitalization primitive used by injection. Do not replace it with an LLM instruction. |
| `src/cues.rs` | **KEEP** | User explicitly values start/stop/error feedback. Preserve the existing tiny `afplay` cues and quiet opt-out; consider bundled stable sound assets rather than network/runtime lookup. |
| `src/ui_channel.rs` | **KEEP** | Low-cost worker→HUD state and audio-level channel. Required for the waveform pill. |
| `src/menubar.rs` | **KEEP / polish** | Keep menu-bar agent, floating pill, hover expansion, waveform animation, settings/history/dictionary actions, and status/error states. Remove alternate model picker, Moonshine/Whisper/Cohere selector, wake-word controls, and streaming experiment toggles. The pill implementation and animation are already concentrated around lines 1931–2216. |
| `src/settings_window.rs` | **KEEP / simplify** | Keep native settings window, hotkey, cleanup/prompt controls, dictionary/history/log actions, and pre-roll only if retained. Remove backend/model/quantization/Whisper/Moonshine/wake-word/streaming controls. The current backend-specific controls are around lines 383–525 and 855–869. |
| `src/settings.rs` | **REWRITE, then KEEP** | Keep persistent settings, hotkey resolution, format/prompt selection if retained, and explicit Cohere language/model paths. Remove `gemma_model`, backend selector, Moonshine/Whisper fields, wake-word/listen, and streaming settings. Keep a one-release serde migration for old keys so an existing `settings.json` does not break startup. |
| `src/history.rs` | **KEEP** | User explicitly wants history. Keep SQLite persistence, recent/search/copy UI, and best-effort writes off the critical path. |
| `src/ipc.rs` | **KEEP (small)** | Keep local `start`/`stop`/`toggle`/`cancel` control because it is cheap and useful for shortcuts/automation, but do not grow it into an agent protocol/server. |
| `src/app_paths.rs` | **KEEP / simplify** | Keep bundle-aware config/model/history paths. Remove path resolvers for Parakeet, Moonshine, Whisper, and alternate cleanup models; add explicit Cohere MLX and S1-mini locations. |
| `src/audio_duck.rs` | **DEFER, likely REMOVE** | System-wide `osascript` mute is not part of the requested app and can add hot-path/process noise or surprise the user. Keep behind an opt-in until measured; default it off. Remove once cues and mic isolation are validated. |
| `src/spoken_command.rs` | **DEFER** | Transform/translate commands add prompt and UX surface. Keep only if the user actually uses transforms; otherwise remove with the `transform` CLI path. Do not let it block the single dictation loop. |
| `src/voice_commands.rs` | **DEFER / KEEP useful subset** | Cancel, undo, Return, Tab, and Escape are valuable one-user affordances, but they are not required to validate latency. Keep cancellation first; gate the rest behind tests and retain exact whole-utterance matching. |
| `src/vad.rs` | **DEFER** | Current production use is tied to experimental streaming cleanup and `listen`; regular PTT does not need it. Keep the pure segmentation primitive only if pre-release/background cleanup is adopted; otherwise remove with those modes. |
| `src/wake_word.rs` | **REMOVE** | Hands-free wake-word mode conflicts with the requested push-to-talk simplicity and continuously consumes the mic/ASR path. `listen` and `wake_word_lab` can go with it. |
| `src/model_fetch.rs` | **REMOVE or rewrite as explicit installer** | The desired product is local and single-user; runtime model download is not required. If first-run convenience is wanted, replace it with a one-shot explicit installer for Cohere MLX + S1-mini, not a generic default-model fetcher. Current comments and `main.rs` still fetch Parakeet + Qwen. |

## Cargo features, dependencies, and build metadata

| Item | Decision | Exact evidence / action |
|---|---|---|
| `parakeet` feature + `parakeet-rs` dependency | **REMOVE** | `Cargo.toml:22–30`, `src/transcriber.rs:179,224–236,494–502`; delete feature, dependency, model path, tests, and Parakeet-only labs. |
| `moonshine` feature | **REMOVE** | `Cargo.toml:32–38`, `src/transcriber.rs:186,226,326–385`; no reason to ship an alternate ASR. |
| `whisper` + `whisper-stack` features | **REMOVE** | `Cargo.toml:40–51`, `build.rs:1–15`, `src/transcriber.rs:191,227,397–440,515`; remove the ggml collision workaround once Whisper is gone. |
| `cohere` feature | **KEEP, rename/rework** | Keep a single production feature if feature-gating remains useful, but its implementation must be MLX, not `transcribe-rs` ONNX. A simpler production build may make Cohere unconditional for the Mac app. |
| `cleaner` feature | **KEEP, rename if desired** | Keep Metal llama.cpp support for S1-mini. Remove generic Gemma/Qwen model selection after S1 is the only supported model. |
| `ax-inject` feature | **KEEP** | Required by AppKit pill, status menu, AX injection, clipboard fallback, history/settings windows. |
| `full` feature | **REWRITE** | It currently means `parakeet + cohere + moonshine + cleaner + ax-inject` (`Cargo.toml:69–71`). Make it exactly Cohere MLX + S1-mini + AX UI, or remove the convenience feature and use one app feature. |
| `transcribe-rs` | **REMOVE after MLX port** | Current `cohere`, Moonshine, and Whisper integrations all come from this multi-engine crate. Do not leave it as an accidental transitive dependency. |
| `parakeet-rs` | **REMOVE** | Only used by the old ASR path and Parakeet-specific labs. |
| `llama-cpp-2` + `encoding_rs` | **KEEP initially** | S1-mini cleanup still needs the Metal llama.cpp path; verify whether `encoding_rs` is actually used after cleanup simplification. |
| `hound` | **KEEP as dev/test-only or remove** | Used by WAV bench loaders behind ASR features. Keep while Cohere evals use WAV fixtures; remove from production dependencies if the MLX loader has its own test utility. |
| `ort` dev-dependency | **REMOVE** | It exists for Silero/VAD experiments (`Cargo.toml:138–142`). Remove with those labs, unless the new Cohere MLX test adapter explicitly needs it. |
| `rusqlite`, `cpal`, `ringbuf`, `tokio`, `serde`, `serde_json`, `eyre`, `libc` | **KEEP** | Core capture, async worker, config, history, and diagnostics. Re-run `cargo tree -e features` after pruning to catch newly unused crates. |
| `objc2*`, `accessibility-sys`, `core-*`, `arboard` | **KEEP** | Required for native Mac UI, AX injection, clipboard fallback, and AppKit pill/settings/history. Reduce AppKit feature list only after compiling the simplified UI. |
| package description/keywords (`Cargo.toml:6–10`) | **UPDATE** | They currently advertise “Parakeet TDT v3 + Qwen 2.5”; change to Cohere Transcribe + S1-mini. |

## CLI, scripts, examples, and tests

| Item | Decision | Action |
|---|---|---|
| `daemon` | **KEEP** | The only primary runtime command; make it always use the fixed stack. Keep `--no-cleanup` only as a diagnostic if needed, not as a product setting. |
| `toggle`, `start`, `stop`, `cancel` | **KEEP** | Thin IPC controls in `main.rs:49–55`; useful and cheap. |
| `logs`, `ax-check`, `inject-test`, `bench`, `dictate` | **KEEP** | Diagnostics and latency evaluation. Update output/model names and ensure they use Cohere/S1. |
| `transform` | **DEFER** | Keep only if `spoken_command.rs` and custom prompts survive; otherwise remove from `main.rs:56` and the usage text at line 63. |
| `mock-loop` | **KEEP as unit/dev probe** | It supports featureless tests, but should not be presented as a product mode. |
| `listen` | **REMOVE** | Depends on wake-word + VAD + always-on ASR; not in the PTT product. |
| `fetch-models` | **REMOVE or replace** | Current `main.rs:162–179` fetches the old default models via `model_fetch`. Replace only with an explicit local model setup command if desired. |
| `context-probe` | **DEFER** | Keep while screen-context vocabulary is being measured; remove if that feature is removed. |
| `examples/latency_lab.rs` | **KEEP / rewrite** | Make this the authoritative end-to-end S1 cleanup and prefix-cache eval; current comments and model names are Gemma-specific. |
| `examples/prompt_lab.rs` + `prompts-lab/*` | **KEEP / rewrite** | Keep the corpus/eval harness, replace Gemma/Qwen assumptions with S1-mini, and add correction/name-preservation and no-hallucination gates. |
| `examples/model_bakeoff.rs` | **REMOVE** | A one-model product has no bakeoff. Retain its best fixed cases in the S1 prompt eval instead. |
| `examples/cleanup_stream_lab.rs`, `streaming_lab.rs`, `vad_stream_lab.rs` | **DEFER** | These are explicitly experiments tied to Parakeet/Gemma and simulated/chunked streaming. Do not carry them into the product until true Cohere streaming is proven. |
| `examples/preroll_lab.rs` | **KEEP / optional** | Useful for testing the one latency feature that may materially improve press-to-speech capture. |
| `examples/silero_seg.rs` | **REMOVE** | Dev-only ONNX/VAD experiment; no PTT requirement. |
| `examples/wake_word_lab.rs` | **REMOVE** | Only tests the removed wake-word mode. |
| `tests/verification.rs` | **KEEP / update** | Preserve integration checks for config, capture/injection, corrections, and the fixed model pipeline; remove alternate backend cases. |
| `scripts/build-app.sh` | **KEEP / rewrite** | Keep packaging, icon, signing, install, and login item. Replace `BUILD_FEATURES=full` and old model bundle constants with Cohere MLX + S1-mini. |
| `scripts/reload-daemon.sh` | **KEEP** | Fast config/prompt/corrections iteration; update comments if settings become hot-reloadable or fixed-stack. |
| `scripts/make-icon.swift` | **REMOVED 2026-09-03** | Replaced by `resources/AppIcon.icns` from the design-system repo. |
| `scripts/download-models.sh` | **REWRITE** | Current default is Parakeet + Qwen (`lines 5–11`). Make it an explicit Cohere/S1 setup script or remove it if models are already installed locally. |
| `scripts/gen-streaming-corpus.sh`, `scripts/gen-vad-corpus.sh` | **DEFER / remove after experiments** | Keep only while the corresponding latency/segmentation decision is open. They are not product dependencies. |
| `install.sh` | **KEEP / update** | Keep one-shot installation, but remove stale default model download assumptions. |
| `docs/feature-architecture.html` | **UPDATE or remove** | It currently visualizes Parakeet/Cleaner/AX and does not describe the new Cohere MLX runtime. A stale architecture graphic is worse than no graphic. |
| `docs/wake-word-and-preroll.md` | **DEFER / split** | Retain pre-roll findings if pre-roll remains; archive wake-word guidance after removing `listen`. |
| `docs/pill.png`, `docs/demo.gif` | **KEEP as references** | Existing visual assets document the desired pill and can guide polish; do not make them runtime dependencies. |

## Stale terminology and contradictions to clean up

1. `CLAUDE.md` describes the product as “Parakeet TDT v3 + Qwen 2.5 1.5B” and
   calls Parakeet the default (`lines 26–31`, `159–177`). Rewrite the pipeline
   and feature matrix after the MLX port, not before.
2. `src/settings.rs` calls the S1/Qwen path `gemma_model` and uses
   `DEFAULT_GEMMA_REL` (`lines 18–38`). Keep a compatibility read for one
   release, but expose `s1_model` or a fixed S1 path thereafter.
3. `Cargo.toml`, `README.md`, `scripts/download-models.sh`, and
   `scripts/build-app.sh` all describe the old Parakeet/Qwen default. They must
   be changed together so install instructions cannot produce a stack the app
   no longer loads.
4. `src/transcriber.rs:267–299` says Cohere CoreML is attempted and falls back
   to CPU because ONNX external-data sidecars fail. This is precisely the
   latency failure the MLX implementation is meant to remove; delete the
   fallback comments and `DICTATE_COHERE_ACCEL` knob after migration.
5. `src/settings_window.rs` and `src/menubar.rs` offer four ASR engines and
   backend-specific fields (`settings_window.rs:383–455`, `menubar.rs:692–775`).
   A software-of-one UI should show no engine picker.
6. `src/main.rs:162–179` and `src/model_fetch.rs` still describe downloading
   “default models”; the current default is not the requested stack.
7. `examples/*`, `prompts-lab/README.md`, and `CLAUDE.md` repeatedly say Gemma,
   Qwen, or Parakeet. Search after migration with:

   ```bash
   rg -n -i 'parakeet|qwen|gemma|moonshine|whisper|wake.?word|listen|onnx|ort|GEMMA_' .
   ```

   Remaining matches should be historical notes, compatibility migration code,
   or explicitly archived experiments—not active product paths.
8. `settings.rs:259–261` contains a duplicated “daemon additionally requires”
   comment. Remove it when listen mode is removed.

## Evaluation gates for this migration

Run these on the same Mac, with warmed models, before deleting a path. Record
raw numbers in a checked-in result or the daemon log; do not use model-load time
as per-utterance latency.

### Build and safety gates

- `cargo fmt --check`
- `cargo test`
- the production feature build (`cargo build --release` with the new app
  feature set)
- `cargo tree -e features` contains no Parakeet, Moonshine, Whisper,
  `transcribe-rs`, or `ort` unless an explicitly retained test needs it
- no audio callback performs allocation, model work, logging, or blocking I/O

### Pipeline latency gates

Create a fixed set of short, medium, and long WAV fixtures and report p50/p95
for: key release → audio finalized, Cohere raw text, S1 cleanup, corrections /
polish, text visible, and clipboard restoration. Acceptance criteria:

- no CPU fallback in the Cohere path; the MLX backend must report its device;
- both models load once and remain resident;
- release-to-visible p50 and p95 are lower than the current local-dictation
  baseline on the same fixtures (record the baseline before the port);
- cleanup cancellation leaves the raw/corrected transcript usable; no stuck
  `processing` HUD state;
- direct AX injection remains fast, and clipboard fallback is measured
  separately rather than hidden in a single “inject” number.

### Quality gates

The fixed S1 eval should include at least:

- all-spoken-content preservation (no summary, answer, translation, or
  invented text);
- punctuation/filler cleanup;
- names, brands, code terms, casing, numbers, and multi-word corrections;
- correction-before-cleanup and correction-after-cleanup backstop;
- empty audio, silence, cancellation, and runaway-output fallback;
- repeated runs with temperature zero or equivalent deterministic settings.

The personal dictionary gate should require 100% exact target spelling on the
curated correction cases, while allowing a failed unknown-name case to be
reported separately when screen-context harvesting is disabled.

### UI/interaction gates

- hold/release PTT works without focus theft;
- pill appears on recording, expands on hover, shows live levels, and exits
  cleanly on success, cancellation, and error;
- cue sounds are ordered start/stop/error and respect the quiet setting;
- settings, dictionary, history, logs, and relaunch actions remain reachable;
- Accessibility and microphone permission failures are actionable;
- no wake-word/listen or model-choice controls remain visible.

## Safe removal order

1. **Snapshot behavior.** Save current `settings.json`, `prompts.json`,
   `corrections.json`, model paths, and baseline latency/quality results.
2. **Add the fixed-stack evals.** Make Cohere MLX and S1-mini testable behind
   the existing `LocalInferenceWorker` / `TextCleanupEngine` interfaces before
   touching unrelated UI.
3. **Port the models.** Replace the ONNX Cohere loader and generic cleanup path;
   verify warm inference, cancellation, correction ordering, and logs.
4. **Collapse configuration.** Add one-release migration for old `gemma_model`,
   `asr_backend`, and alternate model fields; then simplify settings/menu
   controls to hotkey, prompt/format, dictionary, history, and optional
   pre-roll.
5. **Rewrite packaging/model setup.** Update Cargo metadata, app feature flags,
   model paths, build/install/download scripts, and README/CLAUDE instructions
   together.
6. **Remove alternate backends.** Delete Parakeet/Moonshine/Whisper branches,
   `transcribe-rs`, `parakeet-rs`, Whisper build constraint, and their model
   resolvers only after the fixed-stack build passes.
7. **Remove experiments.** Delete or archive wake-word, streaming, Silero/VAD,
   and model-bakeoff code after their decision is recorded. Keep the latency and
   S1 prompt evals.
8. **Decide optional niceties.** Measure screen-context, audio ducking,
   transforms, voice commands, and pre-roll. Remove any that do not improve
   the one-user loop.
9. **Final consistency sweep.** Run the repository-wide stale-term search,
   `cargo fmt --check`, unit/integration tests, release build, and manual HUD /
   cue / injection smoke tests.

The key principle is to preserve the proven shell while reducing choices. The
first production milestone is not “all old modules deleted”; it is a warmed,
measured Cohere MLX → corrections → S1-mini → paste loop whose latency and
quality are demonstrably better for this one user.
