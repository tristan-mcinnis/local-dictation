# Local Dictation — architecture reference

The long-form companion to `CLAUDE.md` (the short repo contract). This file describes how the daemon is put together; the contract says what to run and what not to change. Keep both in sync when behaviour moves.

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
- **Reads are bounded and the helper is restartable.** `stdout` is drained on its own thread into
  a channel, so every read is a `recv_timeout` with a budget of three times real time, floored at
  15 s (`cohere_mlx::read_budget`); the ready line gets 120 s for the model load. A helper that
  has gone (EOF / broken pipe) is killed, waited for, respawned with backoff
  (`cohere_mlx::restart_backoff`) and the request retried **once**; a helper that is merely hung
  is replaced but the request is **not** retried, so the user waits one budget rather than two
  and the utterance's audio stays recoverable. Three failed restarts in a row raise
  `health::Fault::HelperFailed`.

The helper is built by `native/cohere-mlx/build.sh` (an `xcodebuild` release build into
`native/cohere-mlx/.build/xcode/…`; `.build/` is gitignored). Full Xcode is required because MLX's
Metal kernels (`mlx-swift_Cmlx.bundle/…/default.metallib`) are emitted only by Xcode's package
build — a plain `swift build` compiles but fails at runtime. Protocol reference:
`native/cohere-mlx/README.md`.

## Build, run, test

```bash
./scripts/download-models.sh                  # one-time: prepare the fixed Cohere Q4 + S1-mini stack
# A built app carries a copy of that script in Contents/Resources; run outside a source tree it
# writes the models to ~/Library/Application Support/Local Dictation/models (no repo, no Xcode).
# scripts/make-dmg.sh packages the free GitHub release DMG (docs/release/github-release.md).
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
  live recording state to fire start-or-stop; `cancel` stops capture and
  discards without transcribing/injecting. Escape (while an utterance is in flight) is the
  hotkey gesture for the same thing — both route through the out-of-band cancel gate.
- `bench`/`dictate` transcribe a WAV or capture for N ms and report per-stage timings.
  `transform "<i>" "<t>"` runs the warm transform path on raw text (no audio/AX) — the fast loop
  for eyeballing transform prompts.
- `fetch-models [dir]` reports the model stack and fetches whatever the compiled-in manifest can
  legitimately fetch (see "Models: the manifest and the one download"). A model with no published
  artefact is reported with what to do about it; no URL is ever invented.
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
./scripts/build-app.sh --slim           # ship: NO models bundled; the app never downloads, it logs what is missing at first run
./scripts/build-app.sh --install        # also copy to /Applications, add a Login Item, launch it
./scripts/build-app.sh --dmg            # wrap in a distributable dist/Local-Dictation-<ver>.dmg (defaults to --bundle-models; --slim --dmg = a small DMG with no models in it)
```

`build-app.sh` builds the native MLX helper first (`native/cohere-mlx/build.sh`), then the Rust
release binary, then assembles the bundle: helper copied to `Contents/Resources/cohere-mlx-bridge`
with `Contents/Resources/mlx.metallib` beside it (MLX looks for `mlx.metallib` next to its
executable before bundle lookup).

The bundle is `dist/Local Dictation.app` — `LSUIElement` menu-bar agent (no Dock icon), signed
with the first Apple Development identity in the login keychain (ad-hoc only when there is none,
or with `SIGN_IDENTITY=-`), `LSMinimumSystemVersion` 14.0, `Info.plist` carries `NSMicrophoneUsageDescription` + a stable bundle id
(`com.tristanmcinnis.local-dictation`). Icon is rendered from an SF Symbol by
`resources/AppIcon.icns` (rendered by design-system/icons/make-icon.py --glyph waveform). Because it's a stable signed bundle, macOS attributes Microphone +
Accessibility to the app itself (granted once), instead of to whatever terminal launched it.
With a development identity the grants survive rebuilds. An *ad-hoc* signature changes on every
rebuild, so macOS re-asks. A development build is not notarized, so a copy sent to another Mac is
blocked by Gatekeeper. A published release is different: `scripts/build-app.sh --release` signs with
a Developer ID Application certificate under the hardened runtime, embeds and re-signs
`Sparkle.framework`, and the notarize/staple/appcast steps run from `scripts/release/`. The app
checks an HTTPS appcast through Sparkle 2 (`src/updater.rs`, loaded at runtime) and updates itself in
place. The pipeline and its human gates are documented in `docs/release/direct-distribution.md`.
When bundled, the daemon redirects stdout/stderr to
`~/Library/Logs/Local Dictation/daemon.log` (no terminal to print to; see Debugging).

**Model resolution** (`src/app_paths.rs`). The *base* directory, in order:
`DICTATE_MODELS_DIR` env > `<app>/Contents/Resources/models` (shipped, self-contained) >
`~/Library/Application Support/Local Dictation/models` (dev symlink → repo) > `./models`
(repo-relative, for `cargo run`). Under that base:
- **Cohere Q4 checkpoint** (`cohere_mlx_default_dir`): `DICTATE_COHERE_MLX_MODEL_DIR` env >
  `<base>/dictation/cohere-mlx-q4`. `scripts/download-models.sh` downloads the public 4-bit MLX
  conversion (`beshkenadze/cohere-transcribe-03-2026-mlx-4bit`, pinned revision, per-file
  SHA-256). When a local Superwhisper install is present it re-keys that checkpoint instead with
  `scripts/models/convert_superwhisper_cohere.py`; tensor bytes are unchanged either way.
- **S1-mini GGUF** (`llm_default_path`, resolved by `Settings::resolve_llm_model`):
  `DICTATE_LLM_MODEL_PATH` env > `S1_MODEL_PATH` env (alias) > `GEMMA_MODEL_PATH` env (deprecated,
  warns once) > `settings.json` `llm_model` > `<base>/llm/s1-mini/s1-mini.gguf`.
- **Native helper** (`cohere_mlx_helper_path`): `DICTATE_COHERE_MLX_BRIDGE` env >
  `<bundle>/Resources/cohere-mlx-bridge` > `native/cohere-mlx/.build/xcode/Build/Products/Release/cohere-mlx-bridge`.

## Iterating fast (which loop am I in?)

The model + all config are read **once at daemon boot**, so changes take effect on relaunch — but
there are two very different loops, and only one needs a rebuild:

1. **Tuning prompts / formats / corrections (no code change).** Use Settings → Dictionary for
   personal terms and heard→correct replacements, or edit `~/.config/local-dictation/prompts.json`
   (`settings.json` / `corrections.json` remain available to power users), then
   **`./scripts/reload-daemon.sh`** — it kills the running daemon and relaunches the *same*
   installed bundle, so the signature is unchanged and macOS keeps the Mic + Accessibility grants.
   Even faster: don't touch the live app at all — A/B candidate prompts offline with
   **`cargo run --release --features cleaner --example prompt_lab`** (loads S1-mini once, scores a
   corpus; see `prompts-lab/`), or `--example s1_mini_eval`. This is the right loop for prompt work.

2. **Changing code or built-in defaults** (`src/**`, the `DEFAULT_*` consts in `prompts.rs`).
   Rebuild + reinstall the bundle: **`./scripts/build-app.sh --install`**. When the bundle is
   *ad-hoc* signed (no Apple Development identity, or `SIGN_IDENTITY=-`), a rebuild changes the
   signature hash and **macOS may revoke + re-prompt for Accessibility/Microphone**. The daemon
   does **not** exit over that any more (see "First run, permissions and health"): the menu bar
   comes up, the Setup window says what is missing, and the event tap installs itself the moment
   you re-grant it in System Settings → Privacy & Security. An agent cannot grant this; it requires
   the user. (A stable signing identity makes the grant stick across rebuilds.)

Gotcha: `build-app.sh --install` runs `open`, which only *activates* an already-running instance
rather than replacing it — so after an `--install` you still need a real relaunch
(`reload-daemon.sh`, or quit from the menu bar) to run the new binary.

## First run, permissions and health

`daemon::run` never exits over a missing grant. The order is: panic hook, architecture check,
single-instance lock, one prompting Accessibility check, the microphone request, then the worker
and the menu bar.

- **Single instance** (`src/single_instance.rs`). An `flock` on
  `~/Library/Application Support/Local Dictation/daemon.lock`. Two daemons would mean two event
  taps (every dictation pasted twice), two model loads and a stolen control socket. The guard
  *waits* up to `HANDOFF_WAIT` (12 s) rather than exiting, because the menu's "Apply and restart"
  spawns the replacement **before** the parent terminates. The parent hands the lock over from
  `applicationWillTerminate:` with `single_instance::release_held`, **after**
  `shutdown::release_models` has returned — so the handoff takes a moment rather than the full
  wait, and the replacement (which loads the whole model stack the instant it gets the lock) never
  overlaps the parent's Metal buffers with its own. Only a lock still held at the end of the wait
  makes the new process exit 0, leaving the working daemon alone.
- **Accessibility** (`src/permissions.rs`). `AccessibilityInjector::check_permission` is called
  once — the prompting `AXIsProcessTrustedWithOptions`, which is what lists the app in System
  Settings. After that everything polls the silent `AXIsProcessTrusted`. Only the event tap needs
  the grant, so the models load regardless; the whole tap installation is a closure handed to
  `run_when_accessibility_granted`, and the menu bar's once-a-second tick calls
  `poll_pending_accessibility`, which installs it the moment the grant lands. No relaunch.
- **Microphone** (`src/permissions.rs`). `AVCaptureDevice.authorizationStatusForMediaType:` and
  `requestAccessForMediaType:completionHandler:` through a hand-written AVFoundation binding
  (`objc2` + `block2`, both already `ax-inject` dependencies; a whole framework crate for two
  selectors was not worth the build surface). Asked for at boot, not as a side effect of the first
  `cpal` stream open.
- **Health** (`src/health.rs`). The channel beside `ui_channel`: `UiState` is a `Copy` `repr(u8)`
  enum describing one utterance and cannot carry a reason string. Faults are ordered by what the
  user must fix first (Accessibility, Microphone, Models, Engine, Helper, Audio, Cleanup,
  Booting); `health::current()` returns the one to act on. `AudioLost` and `CleanupUnavailable` do
  not block dictation. `install_panic_hook` turns a panic on any thread into a logged, visible
  `EngineFailed`, and the worker thread holds a `ReleaseOnExit` drop guard so a panic still
  releases the models instead of making Quit wait out its timeout.
- **Where health shows.** The status item takes the `danger` glyph and states the reason in its
  tooltip; the status menu's first row carries the fault and opens the Setup window; a hotkey
  press that cannot work shows `UiState::Unavailable` with the reason and the next step, instead
  of sending into a channel with no reader.
- **Setup window** (`src/menubar/setup.rs`). The registered Slate "setup checklist": three rows
  (Microphone, Accessibility, Models) with live state and one action each. It opens by itself on
  a launch that is not ready, never on one that is, and is always reachable from the menu as
  "Setup & permissions…". Render proofs `setup-fresh` / `setup-ready` in both appearances, from
  invented rows that never read the user's permissions or model directory.
- **Audio resilience** (`src/audio.rs`). cpal's error callback sets a flag and raises `AudioLost`,
  so a parked warm stream that died is rebuilt on the next press rather than resumed into
  silence; `AudioCaptureEngine::should_rebuild` also throws away a stream the Mac slept through,
  detected as the **gap between the wall clock and the monotonic clock** (`audio::slept_for`),
  because `Instant` does not advance while the Mac sleeps and `SystemTime` does. Comparing the
  wall-clock age against the warm window instead called every stream that had merely been open
  longer than the window a sleep. Multi-channel input picks one sticky channel per stream
  (`ChannelPicker`: the loudest channel of the first 200 ms window that carries sound, above
  `PICK_RMS_FLOOR`) instead of averaging every channel, which cost a one-channel mic on an 8-in
  interface about 18 dB. The window has to carry sound: a stream opens before anyone speaks, so
  deciding from silence would tie-break onto channel 0 and stick there for the stream's life —
  on that same interface, a silent recording. The mean is used until a window decides.

## Models: the manifest and the one download

`src/model_fetch.rs` carries a compiled-in manifest of every model the app may fetch: id, role,
path under the models base, URL, byte count, SHA-256, licence and the credit the licence requires.

- **Dictation still never touches the network.** The one exception is a fetch the user starts from
  the Setup window's Models row (or `fetch-models` on the command line). Nothing here runs unasked.
- **The cleanup model** has a published artefact, so the app can get it: `/usr/bin/curl` with an
  argument array, `-fL --retry 3 -C -` into a `.part` file, a free-disk preflight, progress and
  cancellation, then a SHA-256 check (`src/sha256.rs`, in-tree, checked against the FIPS 180-4
  vectors) before one atomic rename. Bytes that do not match the manifest are deleted, never
  loaded. The model's own LICENSE and NOTICE are saved beside the weights.
- **The ASR model has no URL in the app yet.** Its public 4-bit MLX conversion
  (`beshkenadze/cohere-transcribe-03-2026-mlx-4bit`) is several files and `Source` describes one,
  so the manifest entry carries none. `scripts/download-models.sh` fetches it from a pinned
  revision with per-file SHA-256 checks. In the app the Models row says what is missing, where to
  put it, and offers a folder picker for a checkpoint the user already has.
- **Readiness follows the resolved paths**, not the manifest defaults. `model_fetch::resolved_status`
  is the single place that resolution lives: the ASR checkpoint through
  `app_paths::cohere_mlx_default_dir` (which honours `DICTATE_COHERE_MLX_MODEL_DIR`) and the cleanup
  model through `settings::resolve_llm_model`, exactly as the daemon loads them. The boot check, the
  Setup window and the Models action all read it, so they cannot disagree — checking the manifest
  default instead reported a missing speech model on a Mac whose daemon loads one perfectly well,
  and `ModelsMissing` refuses every hotkey press.
- **The two models are not the same kind of problem.** A missing speech model is
  `ModelsMissing`, which blocks a press; a missing cleanup model is `CleanupUnavailable`, which
  does not — the same split the worker makes (CP-11). The cleanup row is only required when
  cleanup is on, so a missing LLM alone never dead-ends the app, and a cleanup model that fails to
  load is a warning, not a dead worker.

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
  wave pill, settings window. Gates `injector.rs`, `clipboard_paste.rs`, `menubar/`,
  `settings_window.rs`, and the daemon.
- **`full`** = `cohere-mlx` + `cleaner` + `ax-inject` — the one-user production stack. It is always
  what the shipped app builds with (`BUILD_FEATURES=${BUILD_FEATURES:-full}` in `build-app.sh`).
  Use it for any real build or when running the feature-gated tests.

When adding code, gate it behind the right feature and keep the default (featureless) build green so
`cargo test` stays fast and dependency-free.

## The house design (Slate)

Every surface the app draws — the pill, the history panel, the dictionary editor, the eight-pane
settings shell (including the Dictionary form and its term list, and the History pane and its
day-grouped dictation list) — follows the shared house rule kept in the `design-system` repo (`DESIGN.md` +
`tokens.json`). The generated token file is vendored at `src/menubar/house_theme.rs`; it is
generated, never hand-edited, and `make check` in `design-system` flags a stale copy. Nothing in
the UI types a colour, a radius, a font size or a control height: `src/menubar/slate.rs` is the
one drawing layer, and it reads only tokens. It gives the rest of the code a single painted
`NSView` subclass (`SlateView`, which paints a panel, card, well, tile, chip, key cap, row,
divider or status dot from its `Paint`), the ink waveform, the ink toggle (`NSSwitch` paints
itself in the system accent and cannot be tinted, and the rule forbids chrome colour), and the
composite builders — key caps, chips, icon tiles, section labels. `menubar::theme` exposes each
token as a *dynamic* `NSColor` for the parts AppKit paints itself (text fields, popup titles),
while `slate`'s `drawRect:` code resolves the palette from
`NSAppearance::currentDrawingAppearance`. Between them, one view tree renders correctly in both
appearances with no rebuild.

Dark is the default and light is first-class. The old `apply_light_chrome`, which pinned the
history and settings windows to `NSAppearanceNameAqua`, is gone; every window now follows the
application appearance, which `menubar::apply_app_appearance` sets at boot from the new
`appearance` setting (`system` / `light` / `dark`, default `dark`, env `DICTATE_APPEARANCE`) and
again the moment the Settings popup changes — the one setting that applies live rather than on
restart, because a look you cannot see until a restart is not a choice. The menu-bar glyph is the
waveform in all three states, matching the app icon: template ink when idle, the same symbol
tinted `danger` while recording, and the template at 40 % while the pipeline works.

`fast-dictate-backend render-ui <dir>` snapshots the whole design offscreen. It builds the pill in
each recording, processing, completion, warning, and recovery layout, the history panel with sample rows, the dictionary editor and the
settings shell (its Dictation, Dictionary and History panes, the last two seeded with the same
sample rows so the proof never depends on what this Mac dictated today), draws each straight into a bitmap with `cacheDisplay(in:to:)` under both
`NSAppearanceNameDarkAqua` and `NSAppearanceNameAqua`, writes PNGs and exits. No window is ever
ordered on screen and the activation policy is `Prohibited`, so it never steals focus. A blur
material renders nothing useful in a window that was never ordered in, so while the snapshot runs
`slate::set_opaque_materials(true)` swaps the `NSVisualEffectView` for a plain container and
paints `panel_tint` at full alpha instead; everything else draws exactly as it does on screen.

## Pipeline data flow (the daemon path)

`daemon/` owns a `CGEventTap` watching the hotkey modifier + a worker thread (`mod.rs` boot + event tap, `ptt.rs` the pure hold/latch state machine, `worker.rs` the capture → ASR → cleanup → inject thread, `listen.rs` the wake-word mode). On key release it
drains audio and runs the pipeline:

```
audio.rs (cpal capture, SPSC ring, downmix→16 kHz mono)        — with optional PreRoll/AlwaysOnCapture,
  release tail-capture grace, and warm-window mic reuse        (see below)
  → transcriber.rs / cohere_mlx.rs (spawn bridge, NDJSON transcribe, normalize_for_asr → raw text)
  → cleaner.rs (S1-mini; skipped when the deterministic gate says the speech is clean
    and no format preset is active — clear dictation goes straight through)
  → text_polish.rs (polish, fix_speech_mechanics, flatten or preserve line breaks)
  → refiner.rs (deterministic corrections dict + trailing voice-command parse) — shared by daemon & CLI;
    corrections run on raw terminal/no-cleanup text and again after model cleanup
  → smart_pad.rs (spacing/capitalization from caret context)
  → injector.rs (AX direct, or clipboard paste for Electron) + trailing-action keystroke
```

**Keycodes** are the real macOS virtual keycodes (`kVK_*`), which are not alphabetical; the one
name↔code table is `hotkeys::KEY_TABLE`. Before 2026-09 the table was arithmetic, so a typed chord
bound a different physical key than it displayed (`cmd+shift+d` watched F). A keycode already saved
in `settings.json` still names the same physical key; only its label is now honest, so there is no
migration. There is no "chord already taken" probe: the Carbon check it used was unreliable and was
removed rather than reassure falsely.

**Hotkey stop boundary:** only a new physical key edge reaches the hold/latch
state machine. Autorepeated chord key-downs and duplicate modifier events do
not count as another tap. Modifier events use the SDK's left/right device
bits (aggregate fallback for keyboards that omit them), so releasing Right
Option while Left Option remains held does not end hands-free recording.
The Option+Space chord is order-tolerant: the keyboard scans keys in hardware
order, so a Space that reaches the tap up to `CHORD_WINDOW` (150 ms) before the
Option edge still arms hands-free, instead of silently degrading into
hold-to-talk and stopping the moment the keys come up. Accepted modifier edges
log the keycode, raw flags, whether the side came from the device bit or the
aggregate fallback, the HID system's flags for the same key, and the event's
poster (pid/state/uid). Space edges are logged when they belong to a dictation
gesture, and a system-disabled event tap logs with a running count — it used to
be silent, and it drops every event in its window. The HID flags are read at the
tap, which sits at the head of the event stream and therefore sees each edge a
few milliseconds before the HID state commits it: a mismatch there is normal and
is not evidence of a synthesized event. There is no silence or duration
auto-stop in the push-to-talk daemon path.

**Focus capture at key-press** (default on): the focused UI element + caret context (`kAXValue` + `kAXSelectedTextRange`, whose UTF-16 offset is converted to a char index by `injector::char_index_from_utf16`) are resolved from the frontmost application's own AX root on a background thread when recording *starts*, so the AX wake / cross-process cost is paid while the user is speaking rather than after the pipelines finish. At write time the injector still resolves the current field fresh, so a real same-app field change wins and an app switch cannot revive the old target. If that fresh lookup transiently returns no focused element (`-25212` is common while Electron reshuffles its AX tree), the retained field is a narrow fallback only when it had positive editable evidence, still reports `AXFocused=true`, and its owning PID is still the OS-frontmost app. A failed capture or changed app still holds/re-resolves normally. A very short hold just blocks on the capture join. The clipboard fallback's snapshot restore runs on a *detached* thread after the Cmd+V settle, so a slow restore can't pin the next dictation. The fallback's own pasteboard write carries the nspasteboard.org markers `org.nspasteboard.TransientType` + `org.nspasteboard.AutoGeneratedType` (empty data, declared on the same item as the text), so clipboard managers — Quick Launch's Clipboard History included — don't record dictated sentences; the snapshot saves and restores every flavour of every pasteboard item (images, RTF, file URLs), except lazily promised file data and anything past a 16 MB byte budget, which are dropped and logged.

**Chromium/Electron on-demand AX wake** (`injector::wake_electron_ax_and_retry_focus`): Chromium builds no accessibility tree until a client asks for one, so Slack, Cursor, VS Code and the rest answer `AXFocusedUIElement` with `kAXErrorNoValue` (-25212) even with the caret visible — the same wall WeChat sits behind. Both the key-press capture and the write-time resolver now, on a failed focused-element query and only for an app its executable classifies as Electron (`injector::is_electron_pid`), set `AXManualAccessibility` (Chromium's on-demand switch) and re-read on a wall-clock budget (`AX_WAKE_POLL_BUDGET_MS`, 0.9 s) under a short AX timeout. The wake is claimed once per app process (`claim_electron_ax_wake`), so steady dictation and every later utterance skip it and pass on the first query. This is a **look, not a write**: the element it wakes must still clear the editability gate below, so waking an app never widens who may receive a paste. The app-internal price is Chromium maintaining an AX tree, the same cost VoiceOver imposes, for as long as the app runs.

**Cancellation (Escape / IPC), out-of-band** (`src/cancel.rs`): the worker is *blocked* inside the release pipeline (ASR / cleanup), so a cancel queued on the event channel would only be processed after the utterance was already injected. Cancellation therefore goes through a lock-free atomic gate scoped to an **utterance generation**, linearized around a single write-commit point:
  * `begin()` marks a recording in flight and returns a monotonically increasing generation.
  * `request_cancel()` (Escape in the event tap, or the IPC `cancel` command) targets exactly the in-flight generation, via a compare-exchange so it is **linearizable** with `commit()`. The tap decides in-flight from `ui_channel::state()` (`Recording`/`Processing`), which the worker sets at begin and reverts at settle, so an idle Escape is never mistaken for a cancel.
  * The worker polls `requested(gen)` at two early decision points (before transcribe, before cleanup) to discard early without burning compute.
  * **`commit(gen)` is the linearization point** claimed *inside the injector's write-authorizer*, immediately before the first write/clipboard side effect (after the focus/context/padding re-resolve): it flips the utterance from *cancellable* → *committing* atomically. A cancel accepted **before** it forces commit to fail → the utterance is discarded and nothing is written. After a successful commit a late cancel is ignored — the write cannot (and is not pretended to be) rolled back. This closes the check-then-act TOCTOU; checking twice is not sufficient.
  * A fresh `begin()` clears the stale target, so a cancel for utterance N never affects utterance N+1, and a *queued* `CancelRecording` from a settled utterance is a no-op (it never discards a fresh recording).

Escape *while idle* passes through untouched (normal Escape use); only an in-flight utterance swallows it. Inference is never hard-interrupted — a mid-ASR/cleanup cancel lets that compute finish, but the text is never written (compute-cancel vs. write-commit are distinct: one discards later, the other suppresses the actual write).

**Cancellation and retry:** accepted cancellation immediately clears the pill and flips the active generation's capture flag, so queued inference cannot admit more audio or write later. The complete captured buffer is retained for the legacy **Retry and paste last recording** action. Durable recovery is separate: completed/cancelled/failed captures use the archive described below, and **Recover to Clipboard** never invokes voice actions, target replacement, or injection.

**No-blind-paste focus gate** (`src/injector.rs`): before any write, the injector classifies the focused surface from *positive, focus-level evidence* — never an app name — into `FocusEditability`:
  * `Editable` — hard AX evidence: a text-entry role (`AXTextField`/`AXTextArea`/`AXComboBox`, or the `AXSearchField`/`AXFullScreenSearchField`/`AXTextArea` subroles) **and** a writable `AXValue`/`AXSelectedText`. Native apps write via the AX ladder: `kAXSelectedText` at the caret, then clipboard paste if that is refused. There is no `kAXValue` rung, because setting the value replaces the whole field with the utterance. The first successful write to each (pid, role/subrole) is verified by comparing the field's value and selection before and after (`injector::ax_write_landed`), not by searching for the utterance, so a repeated phrase cannot pass falsely and normalised text is not pasted twice.
  * `PasteSafe` — the *narrow, justified* fallback: a genuine terminal emulator **or WeChat** whose focused element discloses no clearly non-input role, including an `AXTextArea`/text-entry role with non-settable AX attributes. Terminal grids such as Ghostty, and WeChat's custom message box, accept keyboard + Cmd+V paste but are AX-blind, so writes go through clipboard paste. WeChat's message box never exposes a focused UI element at all (`AXFocusedUIElement` returns `kAXErrorNoValue` -25212 even with the caret visible), so the paste-safe app-type fallback also covers the no-focused-element path. The terminal exception does not distinguish shell prompts from pagers or terminal applications; choose the intended terminal input before dictating. **There is no editor/compose blanket** — a Slack pane, a Notion/Obsidian page, an Outlook window, a Cursor/VS Code editor *window*, or a browser body is only pasted into when its **focused element** reports an editable role, never by app name.
  * `NotEditable` — a positively non-input role (`AXButton`/`AXWindow`/`AXWebArea`/`AXCanvas`/`AXStaticText`/menu/checkbox/radio) denies injection **even for an allowlisted terminal or WeChat**. A password field (subrole `AXSecureTextField`) is denied the same way in every app, and its value is never read for caret context or screen vocabulary. A read-only text-entry field in a **non-paste-safe** app is also denied, as is unknown/absent non-paste-safe focus. The daemon **never** pastes here. It holds the transcript recoverable (menu "Copy last dictation") and surfaces a distinct `UiState::NoEditableField` (pill: "No text field selected").

The daemon runs the focus decision through the tested seam `injector::finalize_utterance` immediately before the write. Crucially, the injector **re-resolves the CURRENT focused element at write time**, so a same-app field change never writes into a stale element, and a failed initial capture still reaches the fresh resolver. It does NOT rely on the systemwide AX proxy (`AXUIElementCreateSystemWide` → focused), which can return `kAXErrorCannotComplete` (-25204) or no value (-25212) even when the app resolves fine. Instead `injector::plan_live_inject(frontmost_pid())` snapshots the OS-frontmost app via `NSWorkspace` and resolves ITS focused element per-app (`AXUIElementCreateApplication(pid)`) — the same robust path `inject_into_pid` uses. Only if this fresh per-app query returns no element may `retained_focus_can_fallback` reuse the positively-editable press-time field, and only while the element still reports focused and its PID still equals the current frontmost PID. With no qualifying retained field, AX absence alone is never proof of input: a terminal or WeChat is a legitimate `PasteSafe` target, while a page body / unknown app is held.

**Recovery paste (menu "Paste last dictation")** — a *live* hold-to-dictate snapshots the OS-frontmost app at write time and resolves that app's focused element per-app, because the field the user is holding the key over IS the field. A menu-initiated recovery must survive status-menu tracking, which can make Electron return no focused AX element. A menu delegate's `menuWillOpen:` therefore takes a lightweight snapshot of the foreign PID, focused element and editability under a 200 ms-per-call AX bound; it deliberately defers caret context and screen text to the background recovery worker. The action restores (activates) and **verifies** that app, resolves focus fresh, and uses the retained field only if the fresh lookup is absent, the element still reports focused, and the same PID remains frontmost. A fresh field or non-input surface still wins, so the action never blindly pastes. The pure guard `recovery::decide_recovery_target` refuses a self PID and any missing target. Both native AX writes and Cmd+V re-check the explicit target PID at the actual write boundary. A recovery write-authorizer also re-checks that no live utterance started during activation/AX work; if one did, recovery abandons the write. Recovery feedback uses `show_transient_if_unowned`, so it can never clear a live dictation generation. A successful menu paste always flashes the success pill even when ordinary result feedback is disabled. Recovery is distinct from `Retry last recording` (which re-transcribes retained audio); it re-inserts the already-captured text only.

**Release tail-capture** (default on, open-on-press only): on key release the daemon used to drop
the mic immediately, clipping a word still being spoken. Instead it now enters a short grace loop
(polling the live level every ~40 ms) that keeps the engine open and stops once the level has stayed
below an *adaptive* speech threshold for ~250 ms continuously, or a `DICTATE_TAIL_CAPTURE_CAP_MS`
cap (default 1500) hits. The threshold is `max(noise_floor × 2, 0.006)` (deliberately *softer* than
the plain ×3 speech threshold, so a trailing word spoken quietly — voices fall at sentence ends —
still reads as speech instead of "released in a pause"; whisper mode is lower still), where the
floor is the low percentile of per-frame RMS across the utterance, so it adapts to the room. It only engages when
the release plausibly happened mid-speech — a release in a pause (last ~300 ms quiet) skips it
entirely for zero added latency. The recovered audio accumulates in the ring and is drained by the
release pipeline (so the streaming final-segment flush includes it). Skipped entirely under always-on
pre-roll (which already keeps rolling past release).

**Warm-window mic reuse** (default on, 60 s): in the default press-to-open mode the engine was
dropped — and the mic re-opened (~120 ms) — on every utterance. Now a stopped engine is *parked*
(stream kept open, idle chunks dropped) for `DICTATE_WARM_WINDOW_MS`; a re-press within that window
reuses the live stream instead of paying the open cost. A warm-window tick (the existing ~120 ms
worker tick) closes the parked mic once idle past the deadline, and a re-press rebuilds the stream if
the default input changed since it was built. `DICTATE_WARM_WINDOW_MS=0` restores today's exact
cold-open behaviour; no-op under always-on pre-roll. No pre-press lookback (the plain SPSC ring isn't
a rolling lookback; the win is skipping the open).

Transform output replaces the user's selection, so it has two extra guards: a selection over
`cleaner::MAX_TRANSFORM_CHARS` (6000) is refused up front, and a rewrite that stops at the token
budget instead of the model's own end is an error, never pasted. `run_job` keeps its `resident`
token bookkeeping in step with the KV cache at every step (truncate after the wipe, set once the
prompt is decoded), so a failed job cannot leave the next one reusing a prefix the cache lost.

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

`ui_channel.rs` carries a generation/revision-guarded snapshot: recording state,
bounded draft preview, capture flag, last output, and audio levels. States include
Transcribing, Cleaning, Pasting, and Copied. `PasteFailed` is the write-failed result: the words are kept
as the last dictation and as a "Not inserted" Output stage, and the pill says "Couldn’t paste ·
Copy from menu" for 4 s with the History button ("Couldn’t transcribe" is for real ASR
failures only). The success pill's Undo posts ⌘Z only while the app that received the text
(`ui_channel::last_inject_pid`) is still frontmost. Only actual model/injection boundaries
publish these phases; Pasting is beyond the cancellation commit point. Old
completion timers and cancelled inference cannot clear or revive a newer pill.
The preview uses existing sentence-based ASR results, capped at 320 Unicode
characters; it adds no decode calls. The UI displays a bounded recent-text tail.

`menubar/pill.rs` uses a nonactivating, non-key panel with a fixed cancel target.
Compact/Standard geometry, waveform sensitivity, processing detail and preview
come from Settings. `waveform.rs` maps real RMS levels with a quiet gate and
fast release. Reduced Motion is read from NSWorkspace; no decorative processing
wave pretends to be microphone activity.

### Durable recovery

`history.rs` retains the existing SQLite database and legacy row IDs.
`history/recovery.rs` adds capture status/audio metadata and append-only
`dictation_stages` (raw, cleaned, transform, correction, output). Raw is ASR text
before personal corrections; a cleaned version appears only when cleanup ran.
One capture has one record, and explicit recovery appends versions to it.

The audio directory is resolved by `app_paths::recovery_audio_dir()` under the
config directory as `recovery-audio/`. Private directories/files are 0700/0600.
The worker sends delta PCM chunks to a bounded background writer; no SQLite,
file I/O, or recovery queue work runs on the audio callback. WAV checkpoints
are attempted at the existing 120 ms worker cadence, which synchronous ASR can
delay. Stop/cancel flushes the remaining ring tail into the ordered queue;
shutdown waits up to two seconds for its barrier. Startup repairs partial WAV
headers and marks interrupted captures. Write/queue failures surface a warning.

Audio defaults to 30 days, independently of text retention; zero size cap means
retention only. Automatic text expiry preserves retained audio; audio expiry
preserves retained text. Explicit record deletion/clear removes both. Active
captures are protected from size-cap eviction. Historical text-only rows remain
readable and correctly report that no original audio was saved.

`reprocess::to_clipboard` is the recovery seam: it transcribes and optionally
cleans with the existing warm models, records versions, then claims cancellation
authorization immediately before copying. It has no voice-command parser or
injector. IPC `recover <positive database id>` resolves only owned archive audio.
`menubar/history_detail.rs` offers playback, ordered versions, copy, and recovery;
its refresh never steals focus, and playback stops on close or selection change.

`screen_context.rs`
harvests proper nouns from the focused window and feeds them to
`cleaner::process_transcript_with_vocab` so the model preserves on-screen spellings. The prompt's
vocabulary block holds at most 64 terms: the curated terms (corrections + dictionary) are capped
once at boot to 48 (`prompts::curated_vocabulary`: `dictionary.json` terms first, then the
correction targets in sorted order, so the cut is the same on every launch and never falls on a
dictionary term while a correction target, which the refiner enforces anyway, could go) and stay
FIRST, so that block is byte-identical
on every utterance and its KV prefix is reused; the last 16 slots are reserved for screen terms
(`prompts::combined_vocabulary`), so a large dictionary can no longer evict them. The extractor
ranks mid-sentence capitals above sentence-start ones, rewards repeats rather than length, drops
ALL-CAPS runs and email boilerplate, and handles `’s`, `Name/Name` and markdown links.
`app_tones.rs` adds a per-app tone word; it matches whole segments of the bundle id or app name
(`com.apple.TextEdit` → `com`, `apple`, `textedit`), never substrings. Normal
cleanup never infers numbered-list structure from words such as "first" or "second"; structural
formatting requires an explicit `numbered` or `bullets` output-format preset.
`audio_duck.rs` mutes other audio during capture; `cues.rs` plays start/stop cues. All six state
cues are original and reproducible: `scripts/audio/generate_cues.py` (python3 stdlib) synthesizes
them into `resources/sounds/`, and a unit test pins the embedded files to the generator's format
and durations. The cue that plays when text lands is selectable: `resources/completion_sounds/`
holds twenty audition candidates (twelve original, eight CC0 by Kenney Vleugels), and a unit test
pins the catalog to their stable ids, order and format. The cue that plays when recording starts is
selectable too: `resources/start_sounds/` holds seven original rising candidates beside the default
(`resources/sounds/start.wav`). `numbers.rs`
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
- `settings.json` (`settings.rs`) — written by the menu bar. Keys: `llm_model`, `hotkey_keycode`,
  `hotkey_modifiers`, `cleanup_enabled`, `active_format`, `streaming_asr`, `preroll_ms`,
  `listen_mode`, `wake_word`, `tail_capture`, `tail_capture_cap_ms`, `warm_window_ms`,
  `translation_targets`, `active_translation_target`, `history_retention`, `whisper_mode`,
  `double_tap_latch`, `show_result`, `completion_sound`, `start_sound`, `appearance`, `pill_size`,
  `waveform_sensitivity`, `show_processing_detail`, `audio_history_enabled`,
  `audio_retention`, `audio_storage_cap_mb`.
  `completion_sound` is a stable id from `cues::COMPLETION_SOUNDS`; absent or unknown falls back to
  the default tight thud (`resources/sounds/success.wav`), so an upgrade keeps the cue it had.
  `start_sound` is the same for `cues::START_SOUNDS`, falling back to the default rising pair
  (`resources/sounds/start.wav`).
  `cohere_language` is the only surviving ASR key (decoder language, default `en`). The legacy
  `asr_backend` / `cohere_quant` / `moonshine_variant` keys were removed from the struct and are now
  silently ignored by serde — an old settings file still parses.
  **`gemma_model` → `llm_model` migration (2026-09):** the cleanup-model key was renamed. `load`
  still reads `gemma_model` when `llm_model` is absent (`llm_model` wins if both exist), folds it
  into `llm_model`, and rewrites the file once with the new key; `save` never writes `gemma_model`.
  The fallback read is kept for one release, then the legacy field is removed.
- `prompts.json` (`prompts.rs`) — hand-edited `cleanup` + `transform` system prompts, plus a
  `formats` map of named output-shape presets. Four presets ship built-in
  (`numbered`/`bullets`/`email`/`code` in `DEFAULT_FORMATS`); a user's `formats` overlays them (same
  key overrides, new key adds). The active preset (`active_format` / `DICTATE_FORMAT`) replaces the
  cleanup prompt for normal dictation, and list presets keep their line breaks (cleanup preserves
  newlines when a preset is active); unknown/blank falls back to default — everywhere: `Prompts::format_is_defined` is the one test, and the daemon asks the engine (`TextCleanupEngine::format_active`), so a stale or mistyped name cannot switch off the deterministic fast path while the default prompt runs. A deterministic
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

**File safety** (`src/config_io.rs`). Every writer of these files writes atomically (temp file in
the same directory, fsync, rename; a new file is `0600`; a symlinked file is written through the
link, so a dotfiles link survives). `settings.json` is parsed one key at a time: a single bad value
falls back to that key's default with a `[warn]` line and the other keys survive; a document that
is not a JSON object loads as defaults. Either kind of file is copied to `settings.json.bak`
before the next save rewrites it.
`corrections.json` and `dictionary.json` cannot be rebuilt from anything, so a file that does not
parse is never overwritten: `save_to` returns an error, the Dictionary window disables Add and
Save, and the Settings pane refuses Add and says why. `_`-prefixed comment keys in
`corrections.json` survive a save. A blank `corrections.json` is an empty dictionary, not a
corrupt one. CSV import strips a UTF-8 BOM.

Precedence is always **env var > JSON file > built-in default**. The active env knobs on the fixed
stack:

| Var | Effect |
| --- | --- |
| `DICTATE_MODELS_DIR` | Override the models base directory |
| `DICTATE_COHERE_MLX_MODEL_DIR` | Cohere Q4 MLX checkpoint directory |
| `DICTATE_COHERE_MLX_BRIDGE` | Native MLX helper binary path |
| `DICTATE_COHERE_LANG` | Decoder language hint (default `en`) |
| `DICTATE_LLM_MODEL_PATH` | Cleanup-LLM GGUF path. `S1_MODEL_PATH` is an accepted alias; `GEMMA_MODEL_PATH` is deprecated (still honoured, warns once). New name wins when several are set |
| `DICTATE_HOTKEY_KEYCODE` | Push-to-talk key (default `hotkeys::DEFAULT_KEYCODE` = `0x3D` Right Option; decimal or `0x` hex) |
| `DICTATE_HOTKEY_MODIFIERS` | CGEventFlags mask that turns the hotkey into a chord (default 0 = bare modifier key). The held modifiers must equal the mask (an extra Shift is the transform gesture). A plain typing key with no modifier is refused (`Hotkey::problem`) and falls back to the default; only a modifier key or F1–F20 may stand alone |
| `DICTATE_DOUBLE_TAP_LATCH` | Double-tap the hotkey to latch hands-free (`0`/`false`/`off` disables) |
| `DICTATE_COPY_LAST_HOTKEY` | Second global hotkey that copies the last dictation to the clipboard, written like `ctrl+cmd+v`. Empty string = off; unset = the `copy_last_hotkey` JSON key, itself unset by default. Validated against the push-to-talk hotkey |
| `DICTATE_COPY_ON_PASTE_FAILURE` | Put the transcript on the clipboard when insertion is refused or fails, default on (`0`/`false`/`off` disables). The text is never lost either way: the pill and "Copy last dictation" still hold it |
| `DICTATE_MAX_RECORDING_MINUTES` | Stop a recording that reaches this many minutes, so a lost key-up cannot record forever (default 10; `0` = no limit) |
| `DICTATE_WHISPER_MODE` | Lower VAD speech threshold for quiet speech |
| `DICTATE_SHOW_RESULT` | Show the "Landed · N words" pill for 2 s after each dictation (default off) |
| `DICTATE_COMPLETION_SOUND` | Completion cue id for the sound that plays when text lands (default the tight thud; e.g. `glass-tick`). Unknown/blank falls back to the default |
| `DICTATE_START_SOUND` | Start cue id for the sound that plays when recording starts (default the rising pair; e.g. `glass-rise`). Unknown/blank falls back to the default |
| `DICTATE_TRANSLATION_TARGETS` / `DICTATE_ACTIVE_TRANSLATION_TARGET` | Translate-mode target list (comma-separated) and the active one |
| `DICTATE_HISTORY_RETENTION` | History pruning window (see `history.rs`) |
| `DICTATE_REDUCED_MOTION` | Force the pill's reduced-motion animation |
| `DICTATE_FORMAT` | Active output-format preset |
| `DICTATE_STREAMING_ASR` | Incremental streaming ASR: transcribe each VAD-paused sentence during the hold so only the final segment is outstanding at release (default on; `0`/`false`/`off` disables). Whole-utterance cleanup still runs at release. Off while pre-roll is active |
| `DICTATE_PREROLL_MS` | Always-on mic lookback ms (default 0 = off; e.g. 400) |
| `DICTATE_TAIL_CAPTURE` | Release tail-capture: keep the mic open briefly after release so the last word isn't clipped (default on; `0`/`false`/`off` disables). Off while pre-roll is active |
| `DICTATE_TAIL_CAPTURE_CAP_MS` | Hard cap on the tail-capture grace loop (default 1500) |
| `DICTATE_WARM_WINDOW_MS` | After a press-to-open recording stops, keep the mic open this long for a cheap back-to-back reuse (default 60000; 0 = open fresh each time). No-op while pre-roll is active |
| `DICTATE_PASTE_SETTLE_MS` | Override the clipboard-paste settle (ms) that waits for the target app to consume the Cmd+V before the background clipboard restore. Default is length-scaled (280 ms base +1 ms/3 chars, capped 1000); `0` disables the wait for the lowest latency |
| `DICTATE_LISTEN_MODE=1` + `DICTATE_WAKE_WORD` | Hands-free wake-word listen mode (default wake word `computer`) |
| `DICTATE_QUIET` | Mute audio cues |
| `DICTATE_STOP_CUE` | Also play a cue when a recording stops (default off; the landed cue already marks the end) |
| `DICTATE_RENDER_UI` | Render-proof harness only: write pill / window PNGs from invented fixtures and exit, never touching user data |
| `DICTATE_DEBUG_STREAM` | Per-segment `⟫ seg` stream logging (default off) |
| `DICTATE_LOG_TEXT` | Debug only: log dictated text, spoken instructions and screen terms verbatim. Default off — the log carries lengths and timings only |
| `DICTATE_NO_MUTE` | Do not duck other audio during capture |
| `DICTATE_NO_NORMALIZE` | Skip ASR amplitude normalization |
| `DICTATE_PROMPTS_PATH` / `DICTATE_CLEANUP_PROMPT` / `DICTATE_TRANSFORM_PROMPT` | Override the prompts file / cleanup prompt / transform prompt |
| `DICTATE_CONTROL_SOCK` | Override the daemon control-socket path |
| `DICTATE_CORRECTIONS_PATH` / `DICTATE_DICTIONARY_PATH` | Override the corrections / dictionary file paths |
| `DICTATE_APPEARANCE` | `dark` (default), `light` or `system` |
| `DICTATE_PILL_SIZE` | `compact` (default) or `standard` |
| `DICTATE_WAVEFORM_SENSITIVITY` | Visual response: `low`, `balanced` (default), `high` |
| `DICTATE_SHOW_PROCESSING_DETAIL` | Show distinct processing labels, default on |
| `DICTATE_AUDIO_HISTORY` | Save captured audio for recovery, default on |
| `DICTATE_AUDIO_RETENTION` | Audio retention, default `1month` (30 days) |
| `DICTATE_AUDIO_STORAGE_CAP_MB` | Optional audio cap in MiB; default 0 means retention only |

The old multi-backend selector env vars (`DICTATE_ASR_BACKEND`, `DICTATE_COHERE_QUANT`,
`DICTATE_MOONSHINE_VARIANT`, `PARAKEET_MODEL_DIR`, `DICTATE_COHERE_MODEL_DIR`,
`DICTATE_MOONSHINE_MODEL_DIR`, `DICTATE_WHISPER_MODEL_PATH`) no longer exist — the engine is fixed
on Cohere MLX Q4. Only `DICTATE_COHERE_LANG` (decoder language) is honored. `FOCUS_APP` /
`INJECT_DIAG` / `INJECT_PROFILE` are scripted-test helpers.

## Debugging

Structured per-utterance logs (asr/cleanup/inject timings, target app, text and screen-vocab
*sizes*) go to `app_paths::daemon_log_path()` (`~/Library/Logs/Local Dictation/daemon.log`) —
`tail -f` it, or `fast-dictate-backend logs` to open it. `src/daemon_log.rs` owns the file: the
directory is `0700`, the file `0600`, the open refuses a symlink (`O_NOFOLLOW`), and a file past
5 MB is renamed to `daemon.log.1` (one generation kept) at boot and again at the end of a
recording. The log does **not** contain dictated text, spoken instructions or harvested on-screen
terms: those sites go through `daemon_log::text` / `daemon_log::terms`, which print lengths
(`<42 chars · 8 words>`, `<5 terms>`). History retention and "Delete all" cannot reach the log, so
text must never land there by default. `DICTATE_LOG_TEXT=1` is the debug switch that logs the
text verbatim; new log lines that carry user text must use the same helpers. (Until 2026-09 the
log was the shared, world-readable `/tmp/dictate-daemon.log`; that file is no longer written and
can be deleted.) The first
line of every run names the version and, for a bundle, the commit it was built from.

A first daemon run needs Microphone + Accessibility granted in System Settings. Neither is fatal
any more: the menu bar comes up, the Setup window says which is missing, and the event tap
installs itself when the grant lands (see "First run, permissions and health"). macOS ties
Accessibility to the *launching* binary, so a `cargo run` from a terminal asks on behalf of that
terminal — an agent cannot grant it either way. `ax-check` surfaces the prompt for the CLI.
