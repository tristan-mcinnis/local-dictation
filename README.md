# local-dictation

A local dictation app for Apple Silicon. Hold a hotkey, speak, release — cleaned text appears at your cursor.

**Optimized for lowest latency and highest accuracy**, with the obvious trade-offs: it's English-first, macOS-only, and the cleanup model spends ~300 ms thinking about your text. Everything runs on-device. No network at runtime.

## What you actually do

```bash
./target/release/fast-dictate-backend daemon
```

Then **hold Right Option**, speak, release. The cleaned text gets injected wherever your cursor is.

While you talk, a small dark pill floats at the bottom of your cursor's screen with a **live waveform** driven by your mic — bars rise instantly on each syllable and decay slowly so peaks are visible:

```
        ╭─────────────────────────╮
        │   ▎▍▆█▇▅▎▏ ▍▆▇▄▎       │   ← live mic, ~30 FPS
        ╰─────────────────────────╯
```

A small **menu-bar icon** mirrors the state: 🎤 idle · 🔴 recording · ⏳ processing. Subtle audio cues (Tink on start, Bottle on stop) confirm key transitions without being noisy.

## Verified performance

Measured on Apple Silicon, hot path after warm-up, end-to-end from key release to text injected:

| Speech length | Total latency |
| --- | --- |
| 2 s utterance | ~300–400 ms |
| 5 s utterance | ~500–700 ms |
| 10 s utterance | ~800 ms–1.1 s |

Breakdown of a typical 2 s utterance:

```
transcribe (Parakeet TDT v3 INT8, CoreML)    ~90 ms
cleanup    (Gemma 3 1B Q4_K_M, Metal)       ~150 ms
inject     (AX direct, native apps)           ~5 ms
                                            ────────
                                             ~245 ms
```

Inject is ~185 ms for Electron-class apps (VS Code, Slack, Discord, browsers, Notion, Obsidian, Zed, Figma — anything with a renderer process that swallows AX writes). For those we route through clipboard paste, which always works.

## Models

Both fully local. A helper script downloads them:

```bash
./scripts/download-models.sh
```

| Model | Size | Role |
| --- | --- | --- |
| Parakeet TDT v3 INT8 (ONNX) | 640 MB | ASR — speech → text |
| Gemma 3 1B-IT Q4_K_M (GGUF) | 770 MB | Cleanup — strip fillers, fix punctuation, preserve domain casing |

Override either via `PARAKEET_MODEL_DIR` / `GEMMA_MODEL_PATH`. Larger cleanup models like Gemma 3 4B give marginally better output but ~3× the latency.

## Quick start

```bash
# Toolchain (one-time)
rustup update stable
xcode-select --install
brew install cmake

# Get the code + models
git clone https://github.com/tristan-mcinnis/local-dictation
cd local-dictation
./scripts/download-models.sh

# Build (release, full feature set)
cargo build --features full --release

# Run
./target/release/fast-dictate-backend daemon
```

First daemon run prompts for **Microphone** and **Accessibility** permissions. Grant both in System Settings → Privacy & Security, then re-run.

## Features

- **Push-to-talk.** Hold Right Option (configurable via `DICTATE_HOTKEY_KEYCODE`).
- **Live waveform pill.** Floating dark pill at the bottom of your cursor's screen with 14 vertical bars driven by real-time mic RMS. Noise-gated so it stays still during ambient room sound; peak-hold + decay so loud syllables visibly linger before falling.
- **Menu-bar icon (SF Symbol).** Native macOS look that adopts your menu-bar tint: `mic` (idle) → `mic.fill` (recording) → `waveform` (processing). Click for an **Open Log** item (⌘L — opens `/tmp/dictate-daemon.log` in your default editor) and **Quit** (⌘Q).
- **Audio cues.** Tink on start, Bottle on stop, Basso on error. Mute with `DICTATE_QUIET=1`.
- **Smart spacing & capitalization.** Reads the focused element's caret context (or remembers the last-injected character when AX doesn't expose it) so consecutive dictations get the right spacing — no `wordswithoutspaces`, no `lowercase after a period`.
- **Cleanup that respects your voice.** Removes `uh / um / like / you know`, expands colloquial contractions (`wanna → want to`, `gonna → going to`, `kinda → kind of`), keeps standard contractions (`don't`, `it's`), and preserves domain casing (`macOS`, `Rust`, `ONNX`, `GitHub`).
- **Clipboard fallback for Electron.** VS Code, Slack, Discord, browsers, etc. silently accept AX writes without rendering them — for those, we use save-clipboard → Cmd+V → restore.
- **Personal corrections dictionary.** Drop a JSON file at `~/.config/local-dictation/corrections.json` mapping words you commonly mis-transcribe to their right form (`{"lings": "Lingzi", "github": "GitHub"}`). Applied after cleanup, before injection. Case-insensitive match at word boundaries; replacement is verbatim. See `corrections.example.json`.
- **Voice command: "press enter".** End a dictation with `press enter` / `press return` / `hit enter` / `hit return` and it injects the body then synthesizes a Return keystroke. Doesn't fire mid-sentence.
- **Structured logs.** Aligned per-utterance blocks at `/tmp/dictate-daemon.log` showing transcribe / cleanup / inject timings, the target app name, and the final injected text.

## What the log looks like

`/tmp/dictate-daemon.log` after a couple of utterances:

```
[boot] parakeet    loaded in  757 ms
[boot] cleaner     loaded in  360 ms · gemma-3-1b-it-Q4_K_M.gguf
[boot] warm-up     done   in  374 ms
[boot] ready · hold Right Option (0x3d) to dictate · ⌘Q quits

▶ recording
⏹ stopped · held 2.16s
  xcr   142 ms · cln  305 ms · inj  183 ms
  app  Visual Studio Code (pid 82458)
  ✓    "How do these things are they showing up better?"

▶ recording
⏹ stopped · held 0.89s
  skip · empty transcript
```

`xcr` = transcribe (Parakeet), `cln` = cleanup (Gemma), `inj` = inject (AX or clipboard). `app` is the resolved name of whatever process owned the focused UI element. Tail it live with `tail -f /tmp/dictate-daemon.log`.

## Subcommands

| Command | What it does |
| --- | --- |
| `daemon` | Push-to-talk daemon — the way to use this for real |
| `logs` | Open `/tmp/dictate-daemon.log` in your default editor |
| `bench [wav]` | Transcribe + clean a WAV, report timings |
| `dictate <ms>` | Fixed-duration capture (no hotkey) |
| `inject-test [text]` | AX-only smoke test |
| `ax-check` | Surface the Accessibility permission prompt |

## Environment knobs

| Var | Effect |
| --- | --- |
| `PARAKEET_MODEL_DIR` | Default: `models/parakeet-tdt-v3-int8` |
| `GEMMA_MODEL_PATH` | Default: `models/gemma-3-1b-it/gemma-3-1b-it-Q4_K_M.gguf` |
| `DICTATE_HOTKEY_KEYCODE` | Default: `0x3D` (Right Option). Other useful values: `0x36` Right ⌘, `0x3E` Right Control |
| `DICTATE_QUIET` | Set to anything to mute audio cues |
| `FOCUS_APP` | Activate a specific app and inject by PID (for scripted tests) |
| `INJECT_DIAG` | Log focused element role + PID before every inject |
| `DICTATE_CORRECTIONS_PATH` | Override the corrections JSON path (default: `~/.config/local-dictation/corrections.json`) |

## Trade-offs (the honest list)

- **English-first.** Parakeet TDT v3 supports multilingual but the cleanup prompt + model are tuned for English.
- **~300 ms minimum cleanup latency.** Skip it with the `--no-cleanup` flag if you want the raw ~150 ms transcribe-only path.
- **Electron apps cost ~185 ms** (clipboard paste settle time). Native Cocoa apps inject in ~5 ms.
- **No voice activity detection.** Recording is bounded by key hold time, not silence.
- **Plain-text-only clipboard restore.** RTF / images / file URLs on your clipboard at inject time are lost (only matters when the Electron fallback fires).
- **macOS-only.** cpal + AX + Metal + CoreML are all macOS-specific. Linux/Windows ports would need different runtime + injection layers.

## Project layout

```
src/
├── audio.rs            cpal input + SPSC ring buffer + RMS levels
├── cleaner.rs          Gemma cleanup (llama-cpp-2 + Metal)
├── clipboard_paste.rs  save → set → Cmd+V → restore (+ Return key synth)
├── cues.rs             afplay system sounds
├── daemon.rs           push-to-talk loop, CGEventTap, worker thread
├── injector.rs         AX direct + smart-spacing + Electron clipboard route
├── menubar.rs          NSStatusItem + floating waveform pill
├── smart_pad.rs        spacing & capitalization rules
├── text_polish.rs      strip LLM preamble / quotes / artefacts
├── transcriber.rs      Parakeet wrapper + WAV loader
└── voice_commands.rs   trailing "press enter" detection
tests/verification.rs   ring buffer + drain integration tests
```

## Tests

```bash
cargo test                # 17 unit + 2 integration, no models needed
cargo test --features full  # adds the menubar/injector/cleaner suites — 24 total
```

## License

Dual-licensed under MIT OR Apache-2.0, at your option. See [LICENSE-MIT](./LICENSE-MIT) and [LICENSE-APACHE](./LICENSE-APACHE).

Models are not redistributed by this repo. Parakeet TDT v3 is © NVIDIA under their license; Gemma 3 is under Google's Gemma terms. Check each model's repo before commercial use.
