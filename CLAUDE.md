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

Run `cargo test` after every source change. `scripts/build-app.sh` signs with
the first Apple Development identity in the keychain so Microphone +
Accessibility grants survive rebuilds; `SIGN_IDENTITY=-` forces ad-hoc, which
makes macOS re-ask. Only the user can grant the permission.

**A missing grant is never fatal.** The daemon boots the menu bar and loads the
models whatever the permissions say; only the event tap needs Accessibility,
and it installs itself the moment the grant lands (`src/permissions.rs`). The
Setup window (`src/menubar/setup.rs`) states what is missing. Anything that
stops dictation working is a `src/health.rs` fault with a reason and a next
step, shown in the status item, the menu and the pill — never an idle icon
over a dead hotkey.

## Boundaries (do not widen without a design)

- The ASR is fixed: Cohere MLX Q4. The only ASR knob is the decoder
  language. No model picker for ASR, no cloud providers, no accounts, no
  telemetry. **No network while dictating**: the one exception is
  `src/model_fetch.rs`, and only for a download the user starts from the Setup
  window. Everything the app may fetch is a compiled-in manifest row with a
  URL, a byte count, a SHA-256 and a licence; bytes that do not match are
  deleted, never loaded, and a model with no published artefact carries no URL
  at all.
- Features are the architecture: `cohere-mlx`, `cleaner`, `ax-inject`,
  `full`. Gate new code behind the right feature; keep featureless
  `cargo test` green and dependency-free.
- Lowest-latency primitive wins. Measure with the `examples/*_lab` harnesses
  before adding anything on the critical path. No OCR, vision LLMs, or
  whole-window scraping.
- The cleanup model gets the tightest relevant context only.
- Speech inference runs in a resident native Swift child helper. Cleanup and
  optional instruction inference run in this Rust process through llama.cpp.
  Neither uses the local-models daemon. This is a deliberate latency exception
  to the house serving layer; model files share the `~/Models` weight store.
- One macOS floor: 14.0, set by the Swift helper (`Package.swift`
  `.macOS(.v14)`). `scripts/build-app.sh` `MIN_MACOS` (plist +
  `MACOSX_DEPLOYMENT_TARGET`) and the `install.sh` check follow it; change
  them together.

## One way of doing things

- Paths: every per-user location resolves through `src/app_paths.rs`
  (`home_dir`, `config_dir`, `config_file`, `cache_dir`, `app_support_dir`,
  `models_base_dir`, `daemon_log_path`). Never re-join `$HOME` elsewhere.
- Precedence is always env var > JSON file > built-in default, resolved in
  `settings.rs` `resolve_*` methods. Read config once at boot; relaunch to
  apply.
- Constants: default hotkey is `hotkeys::DEFAULT_KEYCODE`; keycode names come
  from `hotkeys::modifier_keycode_name`. No scattered `0x3D`.
- Process lookups by PID go through `injector::process_command_path`.
- `daemon/` and `menubar/` are module directories (`mod.rs` + one file per
  concern). Child files start with `use super::*;` and expose cross-file items
  as `pub(super)`; only `mod.rs` re-exports the crate-public API.
- Logging is `eprintln!` with a `[tag]` prefix to the daemon log
  (`~/Library/Logs/Local Dictation/daemon.log`, `0600`, rotated at 5 MB by
  `src/daemon_log.rs`). No logging crate. User text never goes in a log line
  raw: wrap it in `daemon_log::text` / `daemon_log::terms`, which print
  lengths unless `DICTATE_LOG_TEXT=1`.
- Child processes use `std::process::Command` with argument arrays. No
  shell interpolation. That includes the model download (`/usr/bin/curl`) and
  the folder picker (`/usr/bin/osascript`).
- One daemon per user: `src/single_instance.rs` holds an `flock` for the
  process lifetime. It WAITS for a departing instance rather than exiting,
  because "Apply and restart" spawns the child before the parent quits. Do not
  turn that into an exit-on-first-refusal check.
- User data on disk (`~/.config/local-dictation/*.json`, `history.db`) keeps
  its format. Unknown keys are ignored by serde; never break an old file.
  Renaming a key means a migration: read the old key when the new one is
  missing, write the new key, keep the old read for one release. Current
  one: `gemma_model` → `llm_model` (env `GEMMA_MODEL_PATH` →
  `DICTATE_LLM_MODEL_PATH`, `S1_MODEL_PATH` stays an alias), 2026-09.
- Names are model-neutral: the cleanup LLM is `llm_*` in code, keys, and
  env vars, never the name of whichever model is loaded today.

## Repo hygiene

- `.memory/` is agent session notes. It stays on disk and is gitignored:
  never commit it, because notes carry local paths and project context.
- `tests/scrub.sh` must pass before every commit: no personal paths, key
  shapes, or names from the untracked `.scrub-private` list in tracked files.

## Privacy

Never commit model bytes, API keys, personal dictionary or corrections
content, dictation history, logs, or `~/.config/local-dictation` data.
Example files (`*.example.json`) are the only config in the tree.

Attribution is a licence obligation, not a courtesy: the cleanup model is
**"S1-mini" by "Superwhisper"** (Apache-2.0 with a naming clause that requires
that exact capitalization wherever the model is used), and the speech model is
Cohere Transcribe 03-2026 by Cohere Labs (Apache-2.0). Keep both names, their
Hugging Face links and `THIRD_PARTY_NOTICES.md` accurate; `download-models.sh`
saves the model's own LICENSE and NOTICE next to the weights.

Recovery audio and transcript versions are user data: private local files only,
30-day audio retention by default, independent text retention. Render proofs
use invented settings/history/dictionary/audio fixtures and must not query the
user stores. Saved-audio recovery copies to the clipboard; it never replays
spoken actions or injects into the current app.

## Workflow

Work on `main`. Commit after each change; push to `origin/main` when asked
or when the session contract says so. Keep the tree clean.
