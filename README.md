# fast-dictate-backend

Low-latency local dictation pipeline for Apple Silicon, written in Rust.

**Stack:** [Parakeet TDT v3 INT8](https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx) for ASR (via [`parakeet-rs`](https://crates.io/crates/parakeet-rs) on ONNX Runtime with the CoreML execution provider) → [Gemma 3 4B Q4_K_M](https://huggingface.co/ggml-org/gemma-3-4b-it-GGUF) for cleanup (via [`llama-cpp-2`](https://crates.io/crates/llama-cpp-2) with the Metal backend) → macOS Accessibility API for cursor-position text injection (with clipboard paste fallback).

Everything runs **on-device**. No network calls at runtime.

## Verified performance

Measured on an Apple Silicon Mac against an 8.35-second `say`-generated WAV. `release` build, models loaded once and timed both warm-up and hot calls:

| Stage | Wall time | Notes |
| --- | --- | --- |
| Load 8.35 s WAV (`hound`) | 0.9 ms | — |
| Parakeet TDT v3 INT8 model load | ~700 ms | One-time, CoreML EP |
| Gemma 3 4B Q4_K_M model load | ~850 ms | One-time, Metal |
| Transcribe (hot) | **~190 ms** | **~45× realtime** |
| Cleanup (hot) | ~860 ms | Gemma 3 4B Q4_K_M, Metal |
| **End-to-end hot (transcribe + cleanup)** | **~1.05 s** | for 8.35 s of speech |

Sample quality:

```
Raw  : "So uh yeah we should build the Rust loop and then like compile it on Mac OS,
        right? Um, the Parakeet ONNX runtime is the core dependency."
Clean: "We should build the Rust loop and then compile it on macOS, right?
        The Parakeet ONNX runtime is the core dependency."
```

Filler words stripped (`uh`, `like`, `Um`), domain casing preserved (`Mac OS → macOS`), punctuation tightened.

## Architecture

```
┌───────────────┐    cpal callback     ┌────────────────────┐   tokio drain    ┌────────────────────┐
│ MacBook mic   │ ───── 48 kHz f32 ──▶ │ resample → 16 kHz  │ ─── pop_slice ─▶ │ Parakeet TDT INT8  │
│ (CoreAudio)   │                      │ SPSC ring buffer   │                  │  CoreML execution  │
└───────────────┘                      │  (ringbuf 0.4)     │                  └─────────┬──────────┘
                                       └────────────────────┘                            │ raw transcript
                                                                                         ▼
┌───────────────┐   AX kAXSelectedText   ┌─────────────┐    text_polish    ┌────────────────────────┐
│ focused field │ ◀──── + smart_pad ──── │  smart_pad  │ ◀──── + cleanup ──│ Gemma 3 4B Q4_K_M GGUF │
│ in any app    │   (clipboard if AX     │  context    │   strip preamble  │  llama.cpp + Metal     │
└───────────────┘    refuses)            └─────────────┘                    └────────────────────────┘
```

## Models

Both fully local; place under `./models/`. A helper script downloads them into the right structure:

```bash
./scripts/download-models.sh
```

Layout after download:
```
models/
├── parakeet-tdt-v3-int8/
│   ├── encoder-model.int8.onnx       (~622 MB)
│   ├── decoder_joint-model.int8.onnx (~17 MB)
│   └── vocab.txt                      (~92 KB)
└── gemma-3-4b-it/
    └── gemma-3-4b-it-Q4_K_M.gguf      (~2.3 GB)
```

Total disk: ~3 GB. Override locations with `PARAKEET_MODEL_DIR` and `GEMMA_MODEL_PATH` env vars.

## Quick start

```bash
# 1. Toolchain: stable Rust + Xcode CLT + cmake.
rustup update stable
xcode-select --install   # if not already
brew install cmake

# 2. Get the code + models.
git clone https://github.com/<your-user>/fast-dictate-backend
cd fast-dictate-backend
./scripts/download-models.sh

# 3. Build (release; full feature set).
cargo build --features full --release

# 4. Run the offline bench against the bundled test WAV.
./target/release/fast-dictate-backend bench

# 5. Live mic capture (5-second window).
./target/release/fast-dictate-backend dictate 5000
```

## Subcommands

| Command | What it does |
| --- | --- |
| `mock-loop` | No models needed. Drives the ring buffer with a synthetic sine; useful for buffer-plumbing sanity. |
| `bench [path/to/wav]` | Loads Parakeet + Gemma, transcribes the WAV, cleans it, reports timings, then injects the cleaned text into the focused field (or `FOCUS_APP`). |
| `dictate <ms>` | Opens the default input device, records for `ms` ms, transcribes, cleans, injects. Plays `Tink` on start / `Pop` on stop. Ctrl+C cancels without injecting. |
| `inject-test [text]` | Smoke-test the AX injector with a fixed string (default: `"hello from fast-dictate-backend!"`). |
| `ax-check` | Probes Accessibility permission; surfaces the system prompt on first call. |

## Environment knobs

| Var | Effect |
| --- | --- |
| `PARAKEET_MODEL_DIR` | Overrides the parakeet model directory. Default: `models/parakeet-tdt-v3-int8`. |
| `GEMMA_MODEL_PATH` | Overrides the Gemma GGUF path. Default: `models/gemma-3-4b-it/gemma-3-4b-it-Q4_K_M.gguf`. |
| `FOCUS_APP` | If set (e.g. `FOCUS_APP=TextEdit`), activates the named app and injects into it by PID. Useful for scripted tests against a specific target. |
| `DICTATE_QUIET` | Mutes audio cues. |

## Cargo features

| Feature | Pulls in | Purpose |
| --- | --- | --- |
| (default) | ringbuf, cpal, eyre, tokio | Mock-mode + ring-buffer plumbing only — builds in seconds, tests in milliseconds. |
| `parakeet` | parakeet-rs (CoreML), hound | Real ASR over ONNX Runtime + WAV loader. |
| `cleaner` | llama-cpp-2 (Metal), encoding_rs | Gemma 3 cleanup engine (Metal GPU). |
| `ax-inject` | accessibility-sys, core-foundation, core-graphics, arboard | macOS AX text injection + clipboard fallback. |
| `full` | all of the above | What you want for actual dictation. |

`cargo build --features full --release` is the everyday build. First build compiles ONNX Runtime and llama.cpp from source — budget 5–15 min.

## Quality-of-life behaviors

These run automatically, no flags needed.

- **Smart spacing.** Before injecting, the injector reads `kAXValue` + `kAXSelectedTextRange` of the focused element, derives `char_before` / `char_after` / `last_non_ws_before`, and pads accordingly:
  - Start of field → capitalize, no leading space
  - After `. ! ?` → capitalize, leading space
  - Mid-text (no preceding space) → leading space added
  - After `(`, `"`, `'`, `[`, `{`, `—` → no leading space
  - Before `.`, `,`, `)`, `]`, `}` → no trailing space
- **Defensive cleanup polish.** Strips chat-template artefacts (`<end_of_turn>`, `</s>`), known preambles (`Here's the cleaned text:` etc.), wrapping quotes, markdown emphasis, and collapses internal whitespace.
- **Clipboard fallback.** If the AX path fails (Electron apps, some Java apps, sandboxed web views silently refuse `kAXValueAttribute` writes), the injector falls back to: save clipboard → write text → synthesize Cmd+V via `CGEvent` → 180 ms settle → restore. Plain-text clipboard contents are restored; rich types are not (a known limitation).
- **Skip-empty.** No-op when the final text is empty, so an accidentally-empty dictation never errors.
- **Audio cues.** `Tink` on record start, `Pop` on record stop, `Basso` on inject error. `DICTATE_QUIET=1` to silence.

## Permissions

macOS will prompt for two TCC permissions the first time you exercise the live paths:

1. **Microphone** — needed by cpal. Triggered by the first `dictate` run.
2. **Accessibility** — needed for AX injection and CGEvent-based clipboard paste. Triggered by the first `inject-test`, `dictate`, or `ax-check` that performs an inject.

Grant in **System Settings → Privacy & Security**. The binary you grant access to is the parent process (your terminal or `target/release/fast-dictate-backend` directly).

## Tests

```bash
cargo test                        # 17 unit + 2 integration, no models needed
cargo test --features full        # same set, against the heavier build
```

Coverage includes:
- `text_polish::polish` — preamble stripping, quote peeling, whitespace normalization, chat-token trimming.
- `smart_pad::smart_pad` — spacing/capitalization decisions for empty field, after `. `, mid-word, after opening punctuation, before closing punctuation.
- `audio::AudioCaptureEngine` — SPSC ring-buffer concurrency in mock mode.
- Inference termination — `tokio` drain loop respects the atomic stop flag.

## Project layout

```
src/
├── audio.rs            # cpal input + SPSC ring buffer + resample to 16 kHz mono
├── cleaner.rs          # Gemma 3 cleanup via llama-cpp-2 (Metal), then text_polish
├── clipboard_paste.rs  # save → set → Cmd+V → settle → restore
├── cues.rs             # afplay-based system sounds
├── injector.rs         # AX inject: read context → smart_pad → set value → fallback
├── lib.rs              # module exports
├── main.rs             # CLI: mock-loop / bench / dictate / inject-test / ax-check
├── smart_pad.rs        # spacing + capitalization rules (pure, unit-tested)
├── text_polish.rs      # defensive post-pass on cleaner output (pure, unit-tested)
└── transcriber.rs      # Parakeet TDT v3 wrapper + WAV loader
tests/verification.rs   # integration tests
testdata/               # bundled test WAV (~270 KB)
scripts/                # model download script
models/                 # downloaded models live here (gitignored)
```

## What's not built (yet)

Honest list of things a shipping dictation app would have but this backend doesn't:

- **Global hotkey daemon.** No `CGEventTap` push-to-talk listener. You drive the binary from the CLI today; a real product would run as a background `LaunchAgent` with a menu-bar status item and a hold-to-talk hotkey.
- **Voice activity detection.** Capture duration is hotkey-controlled (ms argument); no silence-based auto-stop.
- **Rich-clipboard round-trip.** The clipboard fallback restores plain text only; images/RTF/file URLs on the clipboard at fallback time are lost.
- **Code signing / notarization.** Not signed; AX prompts will identify the binary by path rather than developer name.
- **Streaming partials.** `dictate` is bounded-window, not streaming; partial transcripts aren't surfaced mid-utterance.

## Why these choices

- **Parakeet over Whisper for English dictation.** Per Vext's published benchmarks (and our own measurements), Parakeet TDT is ~20–50× faster than Whisper at equivalent accuracy on clean English. Whisper is the right call for multilingual.
- **INT8 ONNX over FP32.** Halves disk + memory, and CoreML can fuse the quantized graph onto the Neural Engine on most M-series chips.
- **Gemma 3 4B over Gemma 3 1B.** 4B is markedly better at preserving domain-specific casing (`macOS`, `ONNX`, `Rust`) and refusing to "creatively" rewrite. Trade-off: ~3× slower than 1B (~860 ms vs ~315 ms hot). Vext defaults to 4B-class models for the same reason.
- **`llama-cpp-2` over `llama_cpp`.** The crate named `llama_cpp` (0.3.x) ships a llama.cpp from April 2024 and silently fails to load Gemma 3. `llama-cpp-2` (despite its 0.1.x version number) tracks current llama.cpp.

## License

Dual-licensed under MIT OR Apache-2.0, at your option. See [LICENSE-MIT](./LICENSE-MIT) and [LICENSE-APACHE](./LICENSE-APACHE).

Models are not redistributed by this repo. Parakeet TDT v3 is © NVIDIA under their license; Gemma 3 is under Google's Gemma terms. Read each model's repo before commercial use.
