# local-dictation

Fast, private, on-device dictation for Apple Silicon Macs. Hold a key, talk, release, and the text lands at your cursor in about 150 ms. Everything runs locally. Nothing leaves your Mac, no account, no subscription, no cloud.

Built for how people actually dictate now: into an AI chat, a Slack thread, a message box. The text drops in at the cursor, so you talk, press Enter, and it sends. The whole point is speed, so the cleanup model only runs when your speech is actually messy. Clear speech goes straight through.

A free, open-source alternative to Superwhisper, Wispr Flow, MacWhisper, and built-in Apple Dictation.

![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)
![Platform: macOS · Apple Silicon](https://img.shields.io/badge/platform-macOS%20%C2%B7%20Apple%20Silicon-black)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust-dea584)
![On-device](https://img.shields.io/badge/network%20at%20runtime-none-success)
![Latency ~150 ms](https://img.shields.io/badge/latency-~150ms%20clear%20speech-brightgreen)

<p align="center">
  <img src="docs/demo.gif" alt="local-dictation demo: hold Right Option, speak, release, cleaned text lands at the cursor" width="420">
</p>

## Features, in order of impact

1. **Lowest latency.** About 150 ms from key release to text on screen for clear speech (transcribe plus inject, no cloud round-trip). Parakeet TDT v3 runs on the Apple Neural Engine via CoreML. This is the reason the project exists.
2. **Fully private and offline.** Audio and text never leave your Mac. No account, no telemetry, no network at runtime.
3. **Lands at your cursor, in any app.** Dictate into any text field or chat box and press Enter to send. Works in native Cocoa apps directly, and falls back to clipboard paste for Electron apps (VS Code, Slack, browsers, Notion).
4. **Cleanup only when needed.** Clear speech is passed through untouched and fast. The on-device LLM runs only when the text is genuinely messy (filler, stutters, run-ons). When it does run it removes filler and fixes punctuation without rewriting, dropping, or substituting your words.
5. **Personal dictionary.** Your names, brands, and tools (Todoist, Pencil, macOS) are kept as you mean them instead of being "corrected" to common words. Edit it from the menu bar.
6. **Hands-free mode.** Hold the key and tap Space to latch, then talk with nothing held down. Tap the key once to stop. Good for long thoughts.
7. **Voice commands.** End an utterance with "press enter", "new line", or "press tab" to emit the keystroke. Say "scratch that" to cancel, or "undo that" to revert the last dictation.
8. **Transform selected text by voice.** Select a passage, hold Shift plus the key, speak an instruction ("make this concise", "translate to Spanish"), and it rewrites in place.
9. **Output formats.** Numbered, bullets, email, and code presets reshape a dictation when you want structure. Off by default, picked from the menu bar.

Also included: always-on pre-roll mic so the first word is never clipped, a live waveform pill at the cursor, a menu-bar app with searchable history, a control socket so Shortcuts / Raycast / a foot pedal can trigger it, and editable system prompts in plain JSON.

## Install

**Download the app.** Grab the `.dmg` from the [latest release](https://github.com/tristan-mcinnis/local-dictation/releases/latest) (about 12 MB), open it, and drag **Local Dictation.app** into `/Applications`. It is small because it downloads the two models (Parakeet plus Qwen 2.5 1.5B, about 1.7 GB) on first launch, so the first run needs internet.

Because the app is ad-hoc signed, clear the Gatekeeper quarantine once:

```bash
xattr -dr com.apple.quarantine "/Applications/Local Dictation.app"
```

Launch it. You will see a "Downloading models" notification on the first run, then a prompt for Microphone and Accessibility in System Settings, Privacy and Security. The menu-bar mic icon means it is ready.

**If huggingface.co is blocked or slow (for example in mainland China):** use a VPN for the first launch, or pre-download through a mirror with no VPN:

```bash
HF_ENDPOINT=https://hf-mirror.com "/Applications/Local Dictation.app/Contents/MacOS/local-dictation" fetch-models
```

`HF_ENDPOINT` also works with `./scripts/download-models.sh` and `./install.sh`.

**Build from source.**

```bash
git clone https://github.com/tristan-mcinnis/local-dictation
cd local-dictation
./install.sh
```

`install.sh` checks prerequisites (offering to install Rust and cmake), downloads the models, builds the app, installs it to `/Applications`, sets it to launch at login, and starts it. It is safe to re-run. You still grant Microphone and Accessibility yourself on first launch.

The app is ad-hoc signed, so after a rebuild macOS may re-ask for those two permissions. A real Developer ID would make the grants stick.

<details>
<summary><b>Run from a terminal, or build the app yourself</b></summary>

```bash
# Toolchain (one-time)
rustup update stable
xcode-select --install
brew install cmake

# Code plus models
git clone https://github.com/tristan-mcinnis/local-dictation
cd local-dictation
./scripts/download-models.sh

# Build and run
cargo build --features full --release
./target/release/fast-dictate-backend daemon

# Or build the .app bundle
./scripts/build-app.sh --install      # build, copy to /Applications, login item, launch
./scripts/build-app.sh --slim --dmg   # tiny DMG, models fetched on first run (the shipped release)
```

</details>

<details>
<summary><b>Hand it to an AI agent</b> (paste into Claude Code, Codex, or Cursor)</summary>

```text
Set up the "local-dictation" app on this Mac (Apple Silicon) from
https://github.com/tristan-mcinnis/local-dictation:
1. Clone the repo (or use it if we are already inside it).
2. Run ./install.sh. It checks prerequisites, downloads the models, builds
   "Local Dictation.app", installs it to /Applications, and sets it to launch
   at login. Answer "yes" to prompts about installing Rust or cmake.
Then stop and tell me the one thing you cannot do for me: on first launch macOS
asks for Microphone and Accessibility permission, and I have to grant both in
System Settings, Privacy and Security myself.
Hotkeys: hold Right Option to dictate. For hands-free, hold Right Option and tap
Space, let go of both, keep talking, then tap Right Option once to stop.
If huggingface.co is blocked here (for example mainland China), run the download
with HF_ENDPOINT=https://hf-mirror.com so it uses a mirror instead of a VPN.
```

</details>

## Using it

Hold **Right Option**, speak, release. The text lands at your cursor.

For hands-free, hold Right Option and tap **Space** (a short pop confirms), let go of both keys, and keep talking. Tap Right Option once to stop.

To edit existing text, select it, hold **Shift plus Right Option**, speak an instruction, and release.

The menu-bar app holds the model, hotkey, output-format, and cleanup pickers, the personal dictionary, dictation history, and prompt and log tools.

## Performance

Measured on Apple Silicon, hot path after warm-up, end to end from key release to injected text.

| Path | Latency |
| --- | --- |
| Clear speech (cleanup skipped) | about 150 ms |
| Messy speech (cleanup model runs) | about 300 to 700 ms, scales with length |

A 2 second clear utterance: transcribe about 90 ms (Parakeet TDT v3 INT8, CoreML), inject about 5 ms native or about 185 ms for Electron apps. When the cleanup model runs it adds roughly 140 to 250 ms (Qwen 2.5 1.5B Q4_K_M, Metal). The cleanup prompt is decoded once at boot and its KV-cache prefix is reused, so it does not re-prefill each time.

## Models

Both run fully on-device. `./scripts/download-models.sh` fetches the two defaults.

| Model | Size | Role |
| --- | --- | --- |
| Parakeet TDT v3 INT8 (ONNX) | 640 MB | Speech to text |
| Qwen 2.5 1.5B-IT Q4_K_M (GGUF) | 1.0 GB | Cleanup |

Qwen 2.5 1.5B won a bake-off of eight local models on 11 hard dictation cases ([`examples/model_bakeoff.rs`](./examples/model_bakeoff.rs)): 50 of 55 passes at 137 ms mean cleanup, beating Gemma 3 4B at a third of the size. Swap models from the menu bar or with `GEMMA_MODEL_PATH`.

## Compared to the alternatives

| | local-dictation | Apple Dictation | Cloud (Wispr Flow) | Local apps (Superwhisper, MacWhisper) |
| --- | --- | --- | --- | --- |
| Runs fully on-device | Yes | Yes | No | Yes |
| Latency, clear speech | about 150 ms | varies | network-bound | varies |
| Cleanup that keeps your words | Yes | No | Yes | Some |
| Edit selected text by voice | Yes | No | Partial | Rare |
| Editable prompts | Yes (JSON) | No | No | No |
| Price | Free | Free | Subscription | Paid or freemium |
| Account or signup | None | None | Usually | Varies |
| One-click install | No (build or ad-hoc app) | Yes | Yes | Yes |

If you want zero setup and a maintained app, the others win. If you want the lowest latency and full control over the model's behavior in a text file, this is for you.

## Configuration

User config lives in `~/.config/local-dictation/`. The menu bar writes `settings.json` (model, hotkey, cleanup, output format). Hand-edit `prompts.json` for the cleanup and transform prompts, and `corrections.json` for the dictionary. Env vars win over files. Common ones:

| Var | Effect |
| --- | --- |
| `DICTATE_HOTKEY_KEYCODE` | Push-to-talk key. Default `0x3D` (Right Option) |
| `GEMMA_MODEL_PATH` | Cleanup model path (name is historical) |
| `PARAKEET_MODEL_DIR` | ASR model directory |
| `DICTATE_FORMAT` | Active output-format preset |
| `HF_ENDPOINT` | Model download mirror (for example `https://hf-mirror.com`) |
| `DICTATE_QUIET` | Mute audio cues |
| `DICTATE_NO_MUTE` | Do not mute other audio during capture |

## Subcommands

| Command | What it does |
| --- | --- |
| `daemon` | The push-to-talk daemon, the real way to use this |
| `toggle`, `start`, `stop`, `cancel` | Drive a running daemon over its control socket (Shortcuts, Raycast, a foot pedal) |
| `fetch-models [dir]` | Download the default models (respects `HF_ENDPOINT`) |
| `bench [wav]` | Transcribe and clean a WAV, report timings |
| `logs` | Open the daemon log |
| `ax-check` | Surface the Accessibility permission prompt |
| `context-probe` | Print the proper nouns harvested from the focused window |

## Trade-offs

* English-first. Parakeet is multilingual but the cleanup is tuned for English.
* Electron apps add about 185 ms (clipboard paste settle time). Native apps inject in about 5 ms.
* Recording is bounded by key hold, not silence detection.
* macOS and Apple Silicon only.

## Tests

```bash
cargo test                 # core unit and integration tests, no models needed
cargo test --features full # adds the menubar, history, injector, and cleaner suites
```

## License

Dual-licensed under MIT OR Apache-2.0, at your option. See [LICENSE-MIT](./LICENSE-MIT) and [LICENSE-APACHE](./LICENSE-APACHE).

Models are not redistributed here. Parakeet TDT v3 is NVIDIA's under their license. Qwen 2.5 is Apache-2.0. Check each model's terms before commercial use.

## Acknowledgements

Standing on [NVIDIA Parakeet TDT v3](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3), [Qwen 2.5](https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct), [parakeet-rs](https://github.com/cdpierse/parakeet-rs), [ONNX Runtime](https://github.com/microsoft/onnxruntime) via [`ort`](https://github.com/pykeio/ort), [llama.cpp](https://github.com/ggml-org/llama.cpp) via [`llama-cpp-2`](https://github.com/utilityai/llama-cpp-rs), [cpal](https://github.com/RustAudio/cpal), [objc2](https://github.com/madsmtm/objc2), and [arboard](https://github.com/1Password/arboard).

---

Keywords: macOS dictation, speech to text, voice to text, voice typing, on-device, offline, local, private, push to talk, Parakeet, Qwen, local LLM, Apple Silicon, Superwhisper alternative, Wispr Flow alternative, MacWhisper alternative, Apple Dictation alternative, dictation for ChatGPT and Claude, low latency speech recognition, Rust.
