# Dictation reliability acceptance ledger — Escape cancellation & focus safety

Bounded ledger for the user-approved Local Dictation cancellation / focus-safety
contract. It records what was implemented, what the automated suite proves, and
— honestly — which surfaces still need a **live** macOS check (this work was done
without a real Accessibility session, and the user was asleep, so no live speech
acceptance was claimed).

Status: **implementation + automated verification done; Escape-cancel live-confirmed by user; the new cancel-retains-audio + Retry feature is code-complete and unit-tested, pending a live retry check.**

---

## 1. The contract (as approved)

1. **Escape cancels** an active recording **or** a pending transcription, and
   prevents **any late insertion**.
2. **Escape passes through untouched when idle** (normal Escape use: dismiss a
   dialog, cancel a drag).
3. **No editable field focused** → keep the transcript recoverable, show a clear
   **"No text field selected"** state, **never paste blindly**.
4. A **fresh dictation after a cancel** works normally.
5. **Legitimate clipboard-only terminal / Electron routes are preserved** — but
   only where there is *positive* evidence that the target is a text-input
   surface. **AX absence alone is never proof of "no input".**

---

## 2. What changed and why

### Out-of-band cancellation gate — `src/cancel.rs` (new)
The worker is *blocked* inside the release pipeline (ASR / cleanup), so a cancel
queued on the event channel (`DaemonEvent::CancelRecording`) would only be
processed after the utterance was already injected — too late. Cancellation now
goes through a lock-free atomic gate scoped to an **utterance generation**, with
a single **linearizable write-commit** point:

- The gate state is **one packed atomic** — `(generation << 2) | phase` with
  phases `IDLE` / `ACTIVE` / `CANCELLED` / `COMMITTED` — plus a monotonic
  `gen_counter` for `begin()`'s allocation. (The earlier split-atomic design —
  separate `current` / `cancel_target` / `active` — raced: `request_cancel`
  could snapshot generation N while the worker rolled to N+1, then land its
  compare-exchange on a freshly cleared `cancel_target`, poisoning the next
  utterance's commit.)
- `CancelGate::begin()` stores `pack(gen, ACTIVE)` and returns `gen`; a fresh
  begin therefore always clears a stale cancel (generation N never bleeds into
  N+1).
- `request_cancel()` does a **generation-specific** compare-exchange that can
  only flip the generation it observed; on a lost CAS it re-reads to report the
  outcome **without adopting a newer generation**. Cancellation is **sticky**:
  once a generation is `CANCELLED`, no later operation can resurrect it.
- **`commit(gen)` is exactly-once authorization**: it CASes
  `pack(gen, ACTIVE) → pack(gen, COMMITTED)`, so it validates both the
  generation *and* the phase. A re-commit, a cancelled/ended generation, or a
  stale commit for a superseded generation all return `false`. A cancel accepted
  before it forces commit to fail (discard, nothing written); after a successful
  commit a late cancel is ignored (cannot roll back a write).
- `requested(gen)` / `consume_cancel(gen)` let the worker discard **early**
  (before transcribe, before cleanup); both are sticky reads of the same word,
  so the worker's checkpoints and the final commit agree.
- Process-global `CANCEL` (mirrors the `ipc::DAEMON_RECORDING` pattern) is what
  the tap and IPC use; unit tests drive a private `CancelGate`.

### Escape binding — `src/daemon/mod.rs`
The CGEventTap never watched Escape (the pill advertised "Cancel recording
(Escape)" via a click-only key-equivalent; there was no global binding). Now in
`KeyDown`:
- **utterance in flight** — decided from the worker's `ui_channel::state()`
  (`Recording` or `Processing`), which the worker sets at begin and reverts to
  `Idle`/`Success`/`Error` at settle, so an idle Escape is never mistaken for a
  cancel → call `CANCEL.request_cancel()` and, **only when it returns `true`
  (a cancel was actually accepted)**, wake the worker with a `CancelRecording`
  event (so capture stops now, not at release), unwind the PTT state machine to
  `ST_IDLE` (so a latched/hands-free session is not left stuck), and **swallow**
  the key.
- **idle or already committed** → `Keep` (Escape passes through). An Escape
  pressed at boot (before any utterance) is likewise passed through, not
  swallowed, and an Escape that races a just-committed write no longer eats the
  key or resets the latch out from under the settle.
- The IPC `cancel` command also signals the gate (so an IPC cancel lands even if
  the worker is mid-pipeline), and the boot log now says "Esc cancels".

### Worker checkpoints — `src/daemon/worker.rs`
The worker holds `cur_gen` (set via `begin()` at every recording-start path —
cold open, warm-window reuse, always-on pre-roll, tail-capture re-press). It
checks `consume_cancel(cur_gen)` at three points:
1. **before transcribing** (discard during capture / tail grace),
2. **after ASR, before cleanup** (don't burn the cleanup model),
3. **immediately before injection** — the decisive gate. The worker passes a
   write-authorizer into the injector; the injector claims `commit(gen)` once,
   after the focus/context/padding re-resolve and immediately before the first
   write/clipboard side effect, and maps a refusal to `Cancelled` via
   `finalize_utterance`.

### Cancel retains audio + Retry last recording — `src/daemon/worker.rs`, `src/daemon/listen.rs`, `src/menubar/mod.rs`
A cancel no longer throws the audio away. The daemon drains the in-flight
recording (the already-drained streaming backup plus the still-queued ring tail)
into an **in-memory** `last_audio` buffer via `retain_in_flight_audio`, replacing
whatever was there. Only the most-recent recording is kept; it is cleared when a
new recording starts (all four `CANCEL.begin()` sites) and on app quit, never
written to disk. The menu item is now **"Retry last recording"**; it re-transcribes
that buffer through the same pipeline, beginning a fresh cancel generation and
claiming `commit` through the same write-authorizer (`inject_refined_authorized`),
so retry obeys the identical Escape-cancel and current-editable-focus/no-blind-paste
guard as a live utterance. A cancelled retry keeps the retained audio until the
next recording start. Full-feature tests: `cancel.rs` unchanged (14), `injector.rs`
now 25 (write-authorizer seam), `worker.rs` +3 (`retain_in_flight_audio` synthetic-
audio fixtures).

### No-blind-paste focus gate — `src/injector.rs`
`classify_focus_editability(role, subrole, value_settable, selected_settable,
is_terminal)` maps a focused surface to `FocusEditability`:
- `Editable` — text-entry AX role **and** a writable `AXValue`/`AXSelectedText`.
- `PasteSafe` — the **narrow, justified** fallback: a genuine *terminal* whose
  focused element discloses no clearly non-input role, including a text-entry
  role whose AX attributes are non-settable (Ghostty's clipboard-only grid).
  The exception does not distinguish shell prompts from terminal pagers. **The app-level editor/compose blanket was removed** —
  Slack, Discord, Figma, Notion, Obsidian, Outlook, Cursor, Zed, VS Code are NOT
  paste-safe by name.
- `NotEditable` — a **positively non-input role** (`AXButton`/`AXWindow`/
  `AXWebArea`/`AXCanvas`/`AXStaticText`/menu/checkbox/radio) denies injection
  **even for an allowlisted terminal**. Read-only text-entry fields and
  unknown/black-holed focus are denied in **non-terminal** apps. Never pastes. Owned and authored by
  the FOCUS role, never the app name.

`finalize_utterance(cancel, editability, text, inject)` is the single tested seam;
the worker calls it right before the write, **passing `None` editability** so the
injector re-resolves and gates on the CURRENT focus (freshness). Capture now uses
the frontmost application's own AX root instead of the less reliable system-wide
proxy. At write time `write_padded`/`inject_through`/`inject_via_target` still
prefer a fresh element, so a real same-app field change wins. If and only if the
fresh query returns no element, a retained positively-editable capture may be
used while it still reports focused and its PID equals the OS-frontmost PID. This targets the logged
`-25212` sequence without reviving a field after an app switch; the retained-element probe still needs live verification in the affected app. The old
`is_clipboard_only_pid` short-circuit (which pasted blindly for any Electron app)
remains replaced by a focus-evidence gate.

### UI "No text field selected" — `src/ui_channel.rs`, `src/menubar/pill.rs`
New `UiState::NoEditableField`. On a held transcript the daemon records it to
`ui_channel::set_last_dictation` (the menu's "Copy last dictation" is the explicit
recoverable copy) and surfaces the pill state. The pill's hint is labeled
"Copy: menu" (accurate), not a fake "Copy" affordance — there is no copy button
on the pill. No clipboard is touched (never clobber a user's copy).

### Recovery paste + recovery-first menu — `src/recovery.rs`, `src/menubar/mod.rs`
The status menu is now recovery-first: the top level is the last dictation
(preview + Copy + an explicit **"Paste last dictation"**), Settings, a single
**"More"** submenu (history / retranscribe / config / diagnostics), and Quit.
`Paste last dictation` is NOT a re-transcribe — it re-inserts the already-captured
text into the app that was frontmost *before* the menu opened. `menuWillOpen:`
takes a lightweight snapshot of the app, focused field and editability under a
short AX timeout; caret context and screen text are deferred to the recovery
worker. The action restores + verifies the PID, resolves focus fresh, and uses
the retained field only when the fresh query has no element, the retained field
still reports focused, and that PID is still frontmost. A real fresh field or
non-input surface wins. Both native AX writes and global Cmd+V verify the target
PID again at the actual write boundary. A recovery write-authorizer checks on
entry, after activation, and at write commit that no live utterance has started;
if one has, recovery abandons the write. Recovery UI feedback is published only
when no live generation owns the presentation, so it cannot discard a concurrent
dictation. Success always flashes the result pill even when ordinary feedback is
off. The pure guards refuse self/missing targets, app switches, non-editable or
no-longer-focused captures, and every live pipeline phase. Recovery remains
serialized, and clipboard work retains its shared write lock and foreground
check.

### 2026-09-07 parent verification

- `cargo test`: 329 library tests plus 4 integration tests passed.
- `cargo test --features full`: 354 library tests plus 4 integration tests passed.
- `design-lint --strict --changed`: clean, zero hits.
- Live regression: opened an owned temporary TextEdit file, activated TextEdit,
  confirmed it was frontmost, ran `inject-test` without `FOCUS_APP`, and read the
  exact document text back. Before: AX -25204 and empty document. After: injected
  marker matched exactly. This exercises the production injector, not audio/ASR.
- Installed signed app: repeated marker injection with its bundled executable;
  exact TextEdit readback passed. Installed executable matches the signed build.
- Installed menu: AX inspection and screenshot review confirmed preview, Copy,
  Paste, Settings, More, Quit with no clipping. Empty-state Paste safely refused.
  Recovery with a fresh spoken dictation remains unverified after restart.
- Full spoken-dictation and terminal focus-switch checks remain separate live
  acceptance items; the TextEdit check does not prove every app's AX behavior.

### Test coverage (behavioral, observable-sink driven)
- `cancel.rs`: 14 tests — packed generation+phase gate, sticky idempotent cancel,
  generation ordering, idle can't cancel, stale-cancel-cleared-on-fresh-begin,
  end-returns-to-idle, exactly-once commit, late-cancel-after-commit ignored,
  superseded-generation commit refused, stale-cancel-never-suppresses-fresh-commit,
  `&'static` global usability, and a deterministic rollover-vs-cancel interleaving
  test (commit authorization must mirror the next generation's own cancel state).
- `injector.rs`: 12 tests — editable-by-role, editable-but-not-writable reads
  NotEditable, terminal/editor PasteSafe, unknown/browser NotEditable, cancel-holds,
  cancel never reaches the injector sink, fresh-dictation-after-cancel injects,
  NotEditable never reaches the sink (never pastes blindly), hard-write-failure →
  Failed. All assertions are on **observable behaviour** (a recording sink closure),
  not a tautological `should_abort` flag.

---

## 3. Historical automated verification (before this recovery change)

Run from the repo root (`/Users/user/Documents/code/house/local-dictation`),
on `main` (HEAD `69fff3e`), against a clean tree at start.

| Command | Result |
| --- | --- |
| `cargo test` | lib **322 passed**; 0 failed. Plus `tests/s1_eval_corpus.rs` 2 passed, `tests/verification.rs` 2 passed. |
| `cargo check --features full` | Finished clean (full feature-gated code type-checks: `ax-inject` `imp` module, daemon, menubar, cleaner). |
| `cargo test --features full` | lib **347 passed**; 0 failed. Plus `s1_eval_corpus` 2, `verification` 2. |

Baseline before the change was `cargo test` = 286 lib passed. The delta is the new
`cancel` (14, incl. the packed generation+phase gate, exactly-once commit, sticky
cancel, the superseded-generation commit regression, and a deterministic
rollover-vs-cancel interleaving test) +
`injector` tests (now 25, incl. the reviewer's negatives: allowlisted-terminal
+ button/read-only → denied, removed editor blanket, unknown/`None` focus reaches
the fresh resolver, click-out/click-back, and the write-authorizer seam —
refused authorization → Cancelled with the sink untouched, uncancelled → one
write, known-NotEditable hold never claims authorization).

The full-feature suite compiling + passing is load-bearing: it proves the AX
`imp` module rework (role/subrole/settability reads, `InjectOutcome` plumbing,
the gate in `write_padded`) type-checks and the feature-gated `cleaner` /
`injector` / `menubar` / `history` tests still pass. `cargo check --features full`
(no codegen) is the fast gate for that surface.

---

## 4. Live checks (escape cancellation LIVE-CONFIRMED; the rest still pending)

**User LIVE-confirmed (installed binary hash `4da6a8…`):** both
`1.` Escape-during-recording and `2.` Escape-during-processing leave the focused
field **empty** (no late insertion).

Remaining checks need a **real macOS Accessibility session** and, for the Escape
hotkey, the user's own hand — an agent cannot grant Accessibility and cannot claim
live speech behaviour. Each is marked what to observe.

1. **Escape cancels a live recording.** Hold PTT, speak, press Escape → capture
   stops immediately, nothing is injected, the log says `⌫ cancelled · discarded`.
   Verify in the daemon log (`fast-dictate-backend logs` / `tail -f
   /tmp/dictate-daemon.log`). **✅ LIVE-CONFIRMED by user (field stays empty).**
2. **Escape cancels a pending transcription.** Hold PTT, release, press Escape
   *during* the release pipeline (watch for `Processing`) → transcript is discarded,
   **no** text lands in the focused app, and a subsequent dictation injects
   normally. This validates the out-of-band gate does not just fire when idle.
   **✅ LIVE-CONFIRMED by user (field stays empty).**
3. **Escape passes through when idle.** With no utterance in flight, Escape must
   reach the focused app (e.g. dismiss a dialog) — the tap must not swallow it.
4. **No editable field → "No text field selected".** Focus, say, Finder (no text
   field), dictate → `UiState::NoEditableField` pill text is shown, the menu
   "Copy last dictation" holds the text, and **nothing** is typed anywhere.
5. **Native field still injects.** TextEdit / a native field → normal AX injection.
6. **Terminal still accepts text.** A terminal (Ghostty / Terminal.app / iTerm2):
   raw transcript, no cleanup, lands in the grid — the `PasteSafe` terminal route.
   Also verify a **button focused *inside* a terminal is held** (not pasted).
7. **Known editor pane still accepts text, by FOCUS role, not app name.** VS Code /
   Cursor / Obsidian: a focused text pane (AXTextArea) injects; a focus on a
   non-input pane, a sidebar, a launcher, or a Menu in the SAME app is **held**
   (No text field selected), even though the app is a "known editor". This is the
   no-blanket, focus-level behaviour.
8. **Browser page body is NOT pasted into, form input IS.** In Chrome/Arc focused
   on a page (no input focused): dictation is **held** (No text field selected),
   not pasted. A focused *form input* in the browser does paste (editable role).
9. **Same-app field change is fresh.** Focus field A, begin dictation, click into
   field B (same app) before release → the injected text lands in **B**, not the
   stale A. (The write re-resolves the current focus, not the key-press target.)
10. **Menu "Copy last dictation"** reflects a held transcript so the user can
    recover it.
11. **`ax-check` / `context-probe` / `inject-test`** sanity — run
    `fast-dictate-backend ax-check`, `context-probe`, and
    `inject-test "hello"` after re-granting Accessibility to confirm the
    permission and focus probe work as before.

---

### Three verification tiers — keep them distinct

These are different guarantees; the reviewer asked that tests/reports never blur
them:

1. **Model-compute cancel (discard later).** The utterance is dropped before (or
   after) it is transcribed/cleaned, no text written. This is what the gate's
   early `requested(gen)` discard implements; only the gate logic is unit-tested
   (the model itself is not run here).
2. **Writing-commit (suppress the actual write).** The linearizable `commit(gen)`
   point: a cancel accepted before it suppresses the write. This IS unit-tested
   (gate interleavings + `finalize_utterance` + observable sink).
3. **Actual native UI injection.** Whether the real AX/clipboard write renders in
   the target app. This is **NOT** tested here — it needs a live Accessibility
   session (Section 4).

---

## 5. Hand-off / signing-preserving build & install (for the parent)

Implementation is intentionally **not installed or run** — this work only owns
repo source/tests/docs. To ship it with the Mac grants intact:

```bash
# Bundle + copy to /Applications + login item + launch. build-app.sh signs with
# the first Apple Development identity in the keychain so Mic + Accessibility
# grants survive the rebuild. Use SIGN_IDENTITY=- ONLY as a fallback (it forces
# ad-hoc, which makes macOS re-ask — that requires the user).
./scripts/build-app.sh --install
```

`build-app.sh` builds the native MLX helper first, then the Rust release binary
(`--features full`), then assembles the bundle. After an `--install`, a real
relaunch is still needed (`./scripts/reload-daemon.sh`, or quit from the menu bar)
because `open` only activates an already-running instance.

If this is a code-path change (it is — daemon/worker/injector), the user will
likely need to re-grant Accessibility in System Settings → Privacy & Security if
the signature hash changed; only the user can do that.

---

## 6. Bounded scope / known limitations

- **Inference is not hard-interrupted.** A cancel mid-ASR/cleanup lets that compute
  run to completion (the models are single-flight), but the text is **never**
  written. The daemon does not pretend to be instantly idle while it is still
  blocked; the gate only suppresses the write.
- **TOCTOU closed at commit, not by re-checking.** `commit(gen)` is the single
  linearization point: a cancel accepted before it suppresses the write; a cancel
  after it is ignored (the write cannot be rolled back and is not reported as a
  cancellation). Authorization is **claimed inside the injector**, after the
  focus/context/padding re-resolve and immediately before the first write/clipboard
  side effect (both the native AX ladder and the no-focused-element terminal
  clipboard route share the guard), so a cancel that lands during a slow (up to
  1.5s) AX lookup is still honored. One claim, exactly once; the existing write
  ladder's fallbacks proceed without a second claim.
- **Freshness remains primary; retained focus is only a rescue.** The write
  always re-resolves the current focused element first, so a same-app field
  change is honoured. A retained key-press/menu target is used only when that
  fresh query returns no element, the capture was positively editable, the
  element still reports focused, and its PID matches the OS-frontmost app. The AX query cost remains on the
  release/inject path; capture still supplies cleanup vocabulary and the narrow
  transient-focus fallback.
- **Focus evidence is best-effort for black-holed apps.** `PasteSafe` is the narrow
  terminal-only fallback; there is no editor/compose blanket. A genuinely unknown
  or black-holed non-terminal focus reads `NotEditable` (held), which is the safe
  default. A real editor pane reaches `Editable` by exposing an AX text role.
- **No migration, no model/config changes, no install/restart/commit/push** were
  performed. Working tree was clean at start; source, tests, and docs only.
