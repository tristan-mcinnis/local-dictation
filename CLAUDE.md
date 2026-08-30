# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Workflow

Work directly on `main` — no worktrees, no feature branches. After every change, commit it and
push to `origin/main` in the same step (don't wait to be asked). Keep the working tree clean.

## Design principle (read before adding features)

**Primitives, first-principles, extremely low latency — and tested before they go in.** This is a
real-time tool fronting two small on-device models (Cohere Transcribe Q4 MLX for ASR, S1-mini for
cleanup). Every feature must earn its place against that:

- **Lowest-latency method wins.** Reason from first principles about where the cost lands (is it on
  the critical path, or hidden behind work already happening?). Prefer the cheapest primitive that
  works — a native AX read over a screenshot, a deterministic string pass over an LLM call.
- **Guard the small model's signal-to-noise.** The cleanup model has a narrow job (clean *this*
  utterance). Do NOT feed it broad/irrelevant context — favour the tightest, most relevant slice
  (e.g. text around the caret / on-screen proper nouns, not the whole window). Cluttering the prompt
  degrades quality, not just speed.
- **Measure, then decide.** Validate latency *and* accuracy with the harnesses
  (`examples/latency_lab`, `examples/preroll_lab`, `examples/prompt_lab`, `examples/s1_mini_eval`,
  `prompts-lab/`, `context-probe`) before committing. If it doesn't fit the stack or doesn't pull its
  weight on the numbers, don't add it. "It works" is not "it's worth it."
- **Don't add heavyweight machinery** (OCR, vision LLMs, whole-window scraping, a second ASR engine)
  when a primitive gets most of the value at a fraction of the cost/permissions. Park it explicitly
  with the reason.

## What this is

A local-first, push-to-talk dictation daemon for Apple Silicon macOS. Hold a hotkey, speak,
release — audio is transcribed (**Cohere Transcribe Q4 through a persistent native MLX Swift
helper**), cleaned up (**S1-mini GGUF via llama-cpp-2 + Metal**), and injected at the cursor
(macOS Accessibility, with a clipboard fallback for Electron apps). Everything runs on-device;
nothing hits the network at runtime. The crate is `fast-dictate-backend` (lib
`fast_dictate_backend` + bin `fast-dictate-backend`).

The ASR is **fixed**: it is always Cohere MLX. The `AsrBackend`/`CohereQuant`/`MoonshineSize`
enums in `transcriber.rs` are legacy settings-migration shims kept only so an old
`settings.json`/menu state decodes without breaking — they never change the runtime (see
`Settings::resolve_asr`, which hard-pins `backend = AsrBackend::Cohere`).

## The native Cohere MLX bridge

Rust does not decode audio. It spawns a **native Swift child-process helper**
(`native/cohere-mlx/`, binary `cohere-mlx-bridge`) that keeps one `CohereTranscribeModel`
resident for the daemon's lifetime. The Rust side (`src/cohere_mlx.rs::CohereMlxWorker`) talks to
it over **newline-delimited JSON on stdin/stdout**:

- Request: `{"op":"transcribe","request_id":"1","sample_rate":16000,"channels":1,"language":"en","max_tokens":256,"audio_f32_base64":"<base64 little-endian f32 PCM>"}`.
  Rust owns resampling/downmixing to mono 16 kHz (see `audio.rs`); the helper expects mono,
  finite, little-endian f32 PCM at exactly 16,000 Hz.
- Startup: the helper emits one `{"ok":true,"event":"ready",…}` line, which the worker waits for
  before admitting dictation work.
- Response: one line per request, matched on `request_id`. `timings_ms.total` is helper-side
  latency; the Rust worker records its own encode/write/read/paste timings separately.
- Requests are serialized — the model object is single-flight and kept memory-stable.
- `Drop` kills/waits the helper process.

The helper is built by `native/cohere-mlx/build.sh` (an `xcodebuild` release build into
`native/cohere-mlx/.build/xcode/…`; `.build/` is gitignored). Full Xcode is required because MLX's
Metal kernels (`mlx-swift_Cmlx.bundle/…/default.metallib`) are emitted only by Xcode's package
build — a plain `swift build` compiles but fails at runtime. Protocol reference:
`native/cohere-mlx/README.md`.

## Build, run, test

```bash
./scripts/download-models.sh                  # one-time: prepare the fixed Cohere Q4 + S1-mini stack
cargo build --features full --release         # the real build — see the feature flags below
./target/release/fast-dictate-backend daemon  # run the push-to-talk daemon

cargo test                                    # lib + unit + integration, NO models or features needed
cargo test --features full                    # adds the cleaner/injector/menubar/history suites
cargo test --features full <name>             # single test by name substring

native/cohere-mlx/test.sh                     # protocol tests for the Swift bridge (no models)
cargo run --release --features cleaner --example s1_mini_eval   # offline S1-mini eval
cargo run --release --features full -- bench testdata/dictation_sample.wav  # current-machine timings
```

Subcommands of the binary (a plain `match` in `src/main.rs`): `daemon [--no-cleanup]`,
`listen [--no-cleanup]`, `toggle`, `start`, `stop`, `cancel`, `logs`, `bench [wav]`,
`dictate <ms>`, `inject-test [text]`, `transform "<instruction>" "<text>"`, `fetch-models [dir]`,
`ax-check`, `context-probe`, `mock-loop`, `synth-press [ms]`.

- `daemon` is the push-to-talk daemon (the real way to use it). `listen` is the EXPERIMENTAL
  hands-free wake-word mode — continuous mic + VAD + `wake_word::detect` → inject.
- `toggle`/`start`/`stop`/`cancel` drive a **running** daemon over its local control socket
  (`src/ipc.rs`), so external automation (macOS Shortcuts, Raycast, a Stream Deck button, a foot
  pedal) can trigger dictation without the hotkey or a second copy of the models. `toggle` reads
  live recording state to fire start-or-stop; `cancel` is socket-only — it stops capture and
  discards without transcribing/injecting.
- `bench`/`dictate` transcribe a WAV or capture for N ms and report per-stage timings.
  `transform "<i>" "<t>"` runs the warm transform path on raw text (no audio/AX) — the fast loop
  for eyeballing transform prompts.
- `fetch-models [dir]` verifies the fixed local model stack is present (errors if incomplete).
  `context-probe` waits 4 s, captures the focused element like the daemon does, and prints the
  proper-noun terms that would feed cleanup — the live check for screen-context vocabulary.
- `inject-test [text]` injects literal text via the production injector. `mock-loop` spins up a
  fake pipeline (no models). `synth-press [ms]` synthesizes a Right-Option hold via CGEvent.
- The default subcommand is `mock-loop` from the CLI, but **`daemon` when the binary is running
  inside a `.app` bundle** (a double-clicked app gets no args) — see `app_paths::running_in_bundle`.

## Packaging as a Mac app (`Local Dictation.app`)

End users run **`./install.sh`** (repo root) — a one-shot, idempotent installer that checks
prerequisites (Xcode Command Line Tools + full Xcode for `xcodebuild`, Rust, cmake), prepares the
Cohere Q4 + S1-mini model stack, then calls `build-app.sh --install`, adds a Login Item, and
launches. The app itself is built by `scripts/build-app.sh`:

```bash
./scripts/build-app.sh                  # dev: tiny app; models shared from ./models via a symlink
./scripts/build-app.sh --bundle-models  # ship: copy ONLY the fixed stack (cohere-mlx-q4 + s1-mini, ~1.7 GB) into the app
./scripts/build-app.sh --slim           # ship: NO models bundled; app verifies/errors on the fixed stack at first run
./scripts/build-app.sh --install        # also copy to /Applications, add a Login Item, launch it
./scripts/build-app.sh --dmg            # wrap in a distributable dist/Local-Dictation-<ver>.dmg (defaults to --bundle-models; --slim --dmg = fetch-on-first-run DMG)
```

`build-app.sh` builds the native MLX helper first (`native/cohere-mlx/build.sh`), then the Rust
release binary, then assembles the bundle: helper copied to `Contents/Resources/cohere-mlx-bridge`
with `Contents/Resources/mlx.metallib` beside it (MLX looks for `mlx.metallib` next to its
executable before bundle lookup).

The bundle is `dist/Local Dictation.app` — `LSUIElement` menu-bar agent (no Dock icon), ad-hoc
signed, `Info.plist` carries `NSMicrophoneUsageDescription` + a stable bundle id
(`com.tristanmcinnis.local-dictation`). Icon is rendered from an SF Symbol by
`scripts/make-icon.swift`. Because it's a stable signed bundle, macOS attributes Microphone +
Accessibility to the app itself (granted once), instead of to whatever terminal launched it.
Ad-hoc signing means a *rebuild* changes the signature hash and macOS may re-ask — a real
Developer ID would make grants stick. When bundled, the daemon redirects stdout/stderr to
`/tmp/dictate-daemon.log` (no terminal to print to).

**Model resolution** (`src/app_paths.rs`). The *base* directory, in order:
`DICTATE_MODELS_DIR` env > `<app>/Contents/Resources/models` (shipped, self-contained) >
`~/Library/Application Support/Local Dictation/models` (dev symlink → repo) > `./models`
(repo-relative, for `cargo run`). Under that base:
- **Cohere Q4 checkpoint** (`cohere_mlx_default_dir`): `DICTATE_COHERE_MLX_MODEL_DIR` env >
  `<base>/dictation/cohere-mlx-q4`. Converted by `scripts/models/convert_superwhisper_cohere.py`
  from the locally installed Superwhisper checkpoint.
- **S1-mini GGUF** (`gemma_default_path`, name is a retained compatibility alias): `S1_MODEL_PATH`
  env > `GEMMA_MODEL_PATH` env > `settings.json` `gemma_model` > `<base>/llm/s1-mini/s1-mini.gguf`.
- **Native helper** (`cohere_mlx_helper_path`): `DICTATE_COHERE_MLX_BRIDGE` env >
  `<bundle>/Resources/cohere-mlx-bridge` > `native/cohere-mlx/.build/xcode/Build/Products/Release/cohere-mlx-bridge`.

## Iterating fast (which loop am I in?)

The model + all config are read **once at daemon boot**, so changes take effect on relaunch — but
there are two very different loops, and only one needs a rebuild:

1. **Tuning prompts / formats / corrections (no code change).** Edit
   `~/.config/local-dictation/prompts.json` (or `settings.json` / `corrections.json`), then
   **`./scripts/reload-daemon.sh`** — it kills the running daemon and relaunches the *same*
   installed bundle, so the signature is unchanged and macOS keeps the Mic + Accessibility grants.
   Even faster: don't touch the live app at all — A/B candidate prompts offline with
   **`cargo run --release --features cleaner --example prompt_lab`** (loads S1-mini once, scores a
   corpus; see `prompts-lab/`), or `--example s1_mini_eval`. This is the right loop for prompt work.

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
`#[cfg(feature = ...)]` and a default `cargo build` compiles most of the lib but almost none of
the real app. **Only three features exist** (plus `full`):

- **`cohere-mlx`** (`base64`, `hound`) — the NDJSON client for the native Cohere MLX bridge
  (`src/cohere_mlx.rs`). This IS the ASR; `bench`/`dictate`/`daemon`/`listen` require it
  (`daemon`/`listen` also need `ax-inject`). Without it, `initialize` returns a "not compiled"
  error.
- **`cleaner`** (`llama-cpp-2` with Metal, `encoding_rs`) — S1-mini cleanup via `src/cleaner.rs`.
  The `transform` subcommand and Shift+PTT transform mode also live here.
- **`ax-inject`** (`accessibility-sys`, `core-foundation`, `core-graphics`, `arboard`, `objc2*`) —
  real macOS Accessibility text injection, clipboard fallback, menu-bar status item, floating
  wave pill, settings window. Gates `injector.rs`, `clipboard_paste.rs`, `menubar.rs`,
  `settings_window.rs`, and the daemon.
- **`full`** = `cohere-mlx` + `cleaner` + `ax-inject` — the one-user production stack. It is always
  what the shipped app builds with (`BUILD_FEATURES=${BUILD_FEATURES:-full}` in `build-app.sh`).
  Use it for any real build or when running the feature-gated tests.

When adding code, gate it behind the right feature and keep the default (featureless) build green so
`cargo test` stays fast and dependency-free.

## Pipeline data flow (the daemon path)

`daemon.rs` owns a `CGEventTap` watching the hotkey modifier + a worker thread. On key release it
drains audio and runs the pipeline:

```
audio.rs (cpal capture, SPSC ring, downmix→16 kHz mono)        — with optional PreRoll/AlwaysOnCapture
  → transcriber.rs / cohere_mlx.rs (spawn bridge, NDJSON transcribe, normalize_for_asr → raw text)
  → cleaner.rs (S1-mini; skipped when the deterministic gate says the speech is clean
    and no format preset is active — clear dictation goes straight through)
  → text_polish.rs (polish, fix_speech_mechanics, flatten or preserve line breaks)
  → refiner.rs (corrections dict + trailing voice-command parse) — shared by daemon & CLI
  → smart_pad.rs (spacing/capitalization from caret context)
  → injector.rs (AX direct, or clipboard paste for Electron) + trailing-action keystroke
```

`cleaner.rs` runs the cleanup LLM on a **dedicated worker thread that owns one persistent
`LlamaContext`** for the daemon's lifetime (a context borrows its model, so it can't live in a
struct). The worker tracks the resident token sequence and **reuses the shared KV-cache prefix**
with each new prompt (`common_prefix_len` + `clear_kv_cache_seq` wipes only the divergent tail),
so the constant cleanup instruction block is decoded once at boot and reused on every utterance.
Reuse is generic (token-sequence comparison, no hardcoded template) and verified bit-identical to
a from-scratch decode by the parity test (`eval_cleanup_uncached` in `examples/latency_lab.rs`).
S1-mini's merged Qwen3 template emits EOG immediately when given a separate system role, so its
instruction and transcript are deliberately combined into **one user turn** (matching
Superwhisper). Sampling is low-temperature fixed-seed (temp 0.2, seed 1234) so cleanup is
deterministic. The context is capped at `MAX_CTX = 8192`.

**Guards around cleanup** (so the small model can't wreck text):
- A deterministic **gate** (`text_polish::needs_llm_cleanup`): clean dictation (no contractions,
  stutters, filler, or long run-ons) skips generation entirely, taking the deterministic
  `fix_speech_mechanics` pass instead.
- A **runaway guard** (`cleaner::cleanup_ran_away`): if the model empties non-empty input, balloons
  output (>4× + heading), repeats ≥4 tokens, or leaks a chat-template marker, the raw transcript is
  used instead.
- A **diff-guard** (`text_polish::cleanup_dropped_content`): if the model rewrote away real content
  (dropped a clause), the faithful deterministic result wins.

`ui_channel.rs` carries worker→UI state (recording/processing, last dictation, audio levels) to
`menubar.rs` (status item, waveform pill, history window, settings window). `history.rs` writes
every injected dictation to SQLite at `~/.config/local-dictation/history.db`. `screen_context.rs`
harvests proper nouns from the focused window and feeds them to
`cleaner::process_transcript_with_vocab` so the model preserves on-screen spellings.
`audio_duck.rs` mutes other audio during capture; `cues.rs` plays start/stop cues. `numbers.rs`
converts spoken numbers to numerals deterministically; `spoken_command.rs` handles *prefix*
commands (e.g. "translate to Chinese …") via the warm transform engine; `voice_commands.rs` parses
*trailing* commands ("press enter", "new line", "scratch that", "undo that"). The model is loaded
once at boot, so config changes (model/hotkey/cleanup) **relaunch** the daemon rather than
hot-swapping.

## Config & precedence

User config lives in `~/.config/local-dictation/` — the directory is resolved in **one** place
(`app_paths::config_dir` / `config_file`), so every consumer (`settings.rs`, `prompts.rs`,
`corrections.rs`, `history.rs`, `dictionary.rs`, the menu bar) shares the same location rather than
re-joining the path:
- `settings.json` (`settings.rs`) — written by the menu bar. Keys: `gemma_model`, `hotkey_keycode`,
  `cleanup_enabled`, `active_format`, `streaming_asr`, `preroll_ms`, `listen_mode`, `wake_word`.
  The `asr_backend` / `cohere_quant` / `cohere_language` / `moonshine_variant` keys still exist but
  are **ignored** — kept only so an old file doesn't fail to parse. (`gemma_model` holds whatever
  cleanup model is selected; the name is historical.)
- `prompts.json` (`prompts.rs`) — hand-edited `cleanup` + `transform` system prompts, plus a
  `formats` map of named output-shape presets. Four presets ship built-in
  (`numbered`/`bullets`/`email`/`code` in `DEFAULT_FORMATS`); a user's `formats` overlays them (same
  key overrides, new key adds). The active preset (`active_format` / `DICTATE_FORMAT`) replaces the
  cleanup prompt for normal dictation, and list presets keep their line breaks (cleanup preserves
  newlines when a preset is active); unknown/blank falls back to default. A deterministic
  `text_polish::fix_speech_mechanics` pass handles leftover filler + lone "i" so the prompt doesn't
  have to.
- `corrections.json` (`corrections.rs`) — the user's single personal vocabulary, a word→replacement
  map. Keys may be **multi-word phrases**, matched as a unit (longest phrase wins; words join only
  across spaces/hyphens, never a comma/period). Applied to the **raw transcript before cleanup**
  (`refiner.apply_corrections`, so the model sees the intended spelling), **again after** cleanup as
  a backstop, **and** fed (target spellings only) into the cleanup prompt as a known-vocabulary hint
  via `prompts::vocabulary_suffix`. Edited via the menu bar's "Dictionary…" window; a bare word saved
  becomes an identity entry. Save rewrites the file and relaunches the daemon (read once at boot).
- `dictionary.json` (`dictionary.rs`) — an *optional, advanced* flat JSON array of extra known words
  merged into the same vocabulary block (`DICTATE_DICTIONARY_PATH` overrides the path;
  `dictionary.example.json` shows the format). It is a plain-list input for power users; the menu bar
  manages `corrections.json`, not this file.

Precedence is always **env var > JSON file > built-in default**. The active env knobs on the fixed
stack:

| Var | Effect |
| --- | --- |
| `DICTATE_MODELS_DIR` | Override the models base directory |
| `DICTATE_COHERE_MLX_MODEL_DIR` | Cohere Q4 MLX checkpoint directory |
| `DICTATE_COHERE_MLX_BRIDGE` | Native MLX helper binary path |
| `DICTATE_COHERE_LANG` | Decoder language hint (default `en`) |
| `S1_MODEL_PATH` | S1-mini GGUF path (also `GEMMA_MODEL_PATH`, a compat alias) |
| `DICTATE_HOTKEY_KEYCODE` | Push-to-talk key (default `0x3D` Right Option; decimal or `0x` hex) |
| `DICTATE_FORMAT` | Active output-format preset |
| `DICTATE_STREAMING_ASR` | Incremental streaming ASR: transcribe each VAD-paused sentence during the hold for a live preview (default on; `0`/`false`/`off` disables). Whole-utterance cleanup still runs at release. Off while pre-roll is active |
| `DICTATE_PREROLL_MS` | Always-on mic lookback ms (default 0 = off; e.g. 400) |
| `DICTATE_LISTEN_MODE=1` + `DICTATE_WAKE_WORD` | Hands-free wake-word listen mode (default wake word `computer`) |
| `DICTATE_QUIET` | Mute audio cues |
| `DICTATE_NO_MUTE` | Do not duck other audio during capture |
| `DICTATE_NO_NORMALIZE` | Skip ASR amplitude normalization |
| `DICTATE_PROMPTS_PATH` / `DICTATE_CLEANUP_PROMPT` / `DICTATE_TRANSFORM_PROMPT` | Override the prompts file / cleanup prompt / transform prompt |
| `DICTATE_CONTROL_SOCK` | Override the daemon control-socket path |
| `DICTATE_CORRECTIONS_PATH` / `DICTATE_DICTIONARY_PATH` | Override the corrections / dictionary file paths |

The old multi-backend selectors (`DICTATE_ASR_BACKEND`, `DICTATE_COHERE_QUANT`,
`DICTATE_MOONSHINE_VARIANT`, `PARAKEET_MODEL_DIR`, `DICTATE_COHERE_MODEL_DIR`,
`DICTATE_MOONSHINE_MODEL_DIR`, `DICTATE_WHISPER_MODEL_PATH`) are no longer honored; they remain only
so an existing config parses. `FOCUS_APP` / `INJECT_DIAG` / `INJECT_PROFILE` are scripted-test
helpers.

## Debugging

Structured per-utterance logs (asr/cleanup/inject timings, target app, injected text, screen-vocab)
go to `/tmp/dictate-daemon.log` — `tail -f` it, or `fast-dictate-backend logs` to open it. First
daemon run needs Microphone + Accessibility permissions granted in System Settings; macOS ties
Accessibility permission to the launching terminal, so the daemon must be started from a real
terminal (an agent can't grant it). `ax-check` surfaces the permission prompt.
