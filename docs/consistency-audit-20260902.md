# Consistency audit, 2026-09-02

One-session pass for "one way of doing things" across the repo. Tree was clean
on `main` at `800f3d5` before the pass.

## Found

**Repo contract**
- No `AGENTS.md`. `CLAUDE.md` was 345 lines: a full architecture write-up
  with the build/test commands buried in the middle. Its settings-key list
  and env-var table were stale (six settings keys and seven `DICTATE_*` env
  vars the code honours were undocumented).
- README said "no model picker" and, three sections later, "the menu-bar
  app holds the model picker". It also listed always-on pre-roll as a shipped
  default when the default is off.

**Duplicated mechanisms**
- Daemon log path `/tmp/dictate-daemon.log` was a literal in four places
  (`main.rs` twice, `menubar.rs`, `settings_window.rs`).
- Default hotkey `0x3D` was a literal in four places (`daemon.rs` had its own
  `DEFAULT_HOTKEY_KEYCODE`, `hotkeys.rs`, `menubar.rs` twice).
- Two keycode-to-name tables: `settings::hotkey_name` over `HOTKEY_CHOICES`
  and `hotkeys::modifier_keycode_name`, same strings, same fallback format.
- `ps -o comm= -p <pid>` spawned identically in `injector.rs` and
  `daemon.rs`.
- `$HOME` re-read and re-joined in `cues.rs` (Caches dir) and `menubar.rs`
  (Downloads export) instead of going through `app_paths`.

**Dead code and stale text**
- `settings::DEFAULT_GEMMA_REL`: unused const, comment recommended
  "Qwen 2.5 1.5B" and pointed at a non-existent `examples/model_bakeoff.rs`.
- `Cargo.toml` comment referenced the removed Parakeet stack.
- `.gitignore` pointed at a non-existent `scripts/gen-streaming-corpus.sh`.
- `prompts.rs` said the format presets were "tuned against Gemma 3 1B".

**Tests**: `cargo test` ran green before the pass (283 lib + 4 integration).

**Secrets / personal data**: none. No keys, no `.env`, no dictionary or
history content. `.memory/` (agent session notes) is committed; it holds
project notes only, no secrets. Left as-is, see below.

## Fixed

- `CLAUDE.md` rewritten as a 73-line contract (build/test/run, boundaries,
  the one-way rules, privacy). `AGENTS.md` is now a symlink to it. The
  architecture write-up moved intact to `docs/architecture.md` with the
  settings-key list and env-var table corrected.
- `app_paths.rs` gains `home_dir`, `cache_dir`, `DAEMON_LOG_PATH`. All four
  log-path literals and both stray `$HOME` joins now resolve through it.
- `hotkeys::DEFAULT_KEYCODE` replaces every `0x3D` default; `daemon.rs`'s
  private copy deleted. `settings::hotkey_name` delegates to the single
  `hotkeys::modifier_keycode_name` table (`HOTKEY_CHOICES` stays as the
  menu's offered subset).
- `injector::process_command_path` is the one `ps` spawn site; `daemon.rs`
  uses it.
- Deleted `DEFAULT_GEMMA_REL`; fixed the Cargo.toml, `.gitignore`, and
  `prompts.rs` comments; fixed the README contradictions.

Behaviour is unchanged: same paths, same defaults, same strings, no on-disk
format touched.

## Left, and why

- `gemma_model` settings key and `gemma_default_path` / `GEMMA_MODEL_PATH`
  names: on-disk and env compatibility aliases. Renaming needs a migration.
- `.memory/` in git: not code, not secrets. Whether an agent's session notes
  belong in the repo is the owner's call, so it was not removed.
- `menubar.rs` (2650 lines) and `daemon.rs` (2606 lines) are large but not
  duplicated; splitting them is a refactor, not a consistency fix.
- `docs/baseline-2026-08-24.md`, `final-verification-2026-08-24.md`,
  `software-one-prune.md`: dated records, kept as history.
