//! macOS text injection with smart spacing, capitalization, and clipboard
//! fallback.
//!
//! Strategy (in order):
//!   1. Snapshot the OS-frontmost PID, then locate its focused UI element
//!      through that application's own AX root.
//!   2. Read its current value + caret position to discover what character
//!      sits immediately before and after the caret.
//!   3. Run `smart_pad::smart_pad` to decide on a leading space, trailing
//!      space, and whether to capitalize the first letter.
//!   4. Try `kAXSelectedTextAttribute` (insert at caret / replace selection).
//!      There is deliberately NO `kAXValueAttribute` rung: setting the value
//!      replaces the whole field with the utterance, which destroyed whatever
//!      the field already held.
//!   5. If the AX write is refused (Electron / some Java apps), fall back to
//!      save-clipboard → set-clipboard → synthesize Cmd+V → restore.
//!
//! A password field (`AXSecureTextField`) is never a target: it classifies as
//! `NotEditable`, and its value is never read for caret or screen context.

use crate::smart_pad::smart_pad;
use crate::smart_pad::last_non_ws_before;
use std::sync::Mutex;

// ─── Focus editability + inject decision (pure, featureless, tested) ───────
//
// This is the safety contract the daemon enforces right before it writes
// anywhere. It decides, from the *evidence* about the focused surface, whether
// a finished transcript may be injected — and, when it may not, that the caller
// holds it recoverable and shows a clear "No text field selected" state
// instead of pasting blindly.
//
// Evidence is layered, never guessed:
//   * `Editable`  — hard AX evidence: the focused element reports a text-entry
//     role/subrole AND a writable value/selected-text attribute. Native apps.
//   * `PasteSafe` — the focused element belongs to a known terminal or the
//     AX-blind WeChat message surface and is not a positively non-input control.
//     Clipboard paste is its input path. There is no general editor/compose
//     allowlist exception. Other apps require positive writable-text evidence.
//   * `NotEditable` — nothing positive. Do NOT inject or paste. Hold + copy.
//
// The daemon combines this with the OOB cancel gate (`crate::cancel`) at the
// single decision point before any write action (see `decide_inject`).

/// Whether the currently-focused surface is a legitimate text-input target.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FocusEditability {
    /// Positive AX evidence: a text-entry role with a writable value.
    Editable,
    /// Known terminal focus without a positively non-input role; paste via clipboard.
    PasteSafe,
    /// Nothing positive — do not inject or paste blindly.
    NotEditable,
}

/// The outcome of a real injection attempt, so the caller can distinguish a
/// hard failure from "there was simply no editable field to write into".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InjectOutcome {
    /// The text was written (via AX or the clipboard route).
    Injected,
    /// No editable field was focused; nothing was written, and nothing was
    /// pasted. The caller should hold the transcript recoverable and surface a
    /// clear "No text field selected" state.
    NoEditableField,
    /// The write authorizer refused (an Escape/cancel landed during the focus
    /// lookup, after resolution but before the first side effect). Nothing was
    /// written. The caller should discard the utterance.
    Cancelled,
    /// The expected target was not the OS-frontmost application at the moment a
    /// global Cmd+V was about to be delivered, so it was NOT posted (fail-closed:
    /// a global paste would land in the wrong app). Nothing was written. The
    /// caller should hold the transcript recoverable, like [`NoEditableField`].
    RefusedForeground,
}

/// Claim write authorization exactly once, immediately before the first
/// write/clipboard side effect — i.e. **after** the focus/context/padding
/// resolution, so a cancel that lands during a slow AX lookup is still honored.
/// A `None` authorizer (the default CLI entry points) is always authorized.
/// Returns `Some(Cancelled)` when the authorizer refuses; `None` when the
/// caller may proceed to the write ladder.
pub fn claim_write_authorization(
    authorize: Option<&dyn Fn() -> bool>,
) -> Option<InjectOutcome> {
    match authorize {
        Some(auth) if !auth() => Some(InjectOutcome::Cancelled),
        _ => None,
    }
}

/// Fail-closed guard for the global Cmd+V delivery: may the paste be posted
/// when the OS-frontmost app is `current_frontmost` and we intend to deliver to
/// `expected_pid`? A global `Cmd+V` lands in the OS-frontmost application, so it
/// may only fire when the expected target IS frontmost. `expected_pid: None`
/// means "no specific target" (the systemwide path): that still requires SOME
/// frontmost app, never a paste with no foreground app. Consulted immediately
/// before posting the Cmd+V.
pub fn clipboard_target_is_frontmost(
    expected_pid: Option<i32>,
    current_frontmost: Option<i32>,
) -> bool {
    match expected_pid {
        Some(pid) => current_frontmost == Some(pid),
        None => current_frontmost.is_some(),
    }
}

/// The app a dictation is for: the PID and bundle id read together, once, when
/// the key goes down. Every per-app decision in the release pipeline (raw text
/// for a terminal, the cleanup tone, the log line) reads this one value, so
/// they cannot disagree with each other or drift to whatever app is in front
/// by the time cleanup runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetApp {
    pub pid: i32,
    pub bundle_id: Option<String>,
}

/// Which app's rules apply at release. The text is written into the app that
/// is frontmost at write time (a deliberate, tested rule), so:
///   * the press-time app, when it is still in front, or when the frontmost
///     app cannot be read;
///   * the app now in front, when the user switched during the hold. Its
///     bundle id is looked up once here, by PID, not re-read later.
/// A failed AX focus capture no longer loses the app: a terminal that exposes
/// no focused element still counts as a terminal.
pub fn resolve_target_app(
    press: Option<TargetApp>,
    frontmost_now: Option<i32>,
    bundle_id_for: impl FnOnce(i32) -> Option<String>,
) -> Option<TargetApp> {
    match (press, frontmost_now) {
        (Some(app), Some(now)) if app.pid == now => Some(app),
        (_, Some(now)) => Some(TargetApp { pid: now, bundle_id: bundle_id_for(now) }),
        (press, None) => press,
    }
}

/// What a live (systemwide) injection should do, planned from the current
/// OS-frontmost app at write time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveInjectPlan {
    /// Resolve this app's focused element per-app (fresh at write time).
    ResolveApp { pid: i32 },
    /// No frontmost app — no live field. Hold the transcript recoverable.
    Hold,
}

/// Pure: plan a live injection from the current frontmost PID.
///
/// The live (systemwide) resolver is deliberately seeded from `frontmost_pid()`
/// FRESH at write time — never a retained press-time target, which is stale or
/// backgrounded by now. This is what honors a same-app field change or a
/// foreground switch during the hold. `None` means there is no live field to
/// write to; the caller must hold (`Hold`), never invent a target.
pub fn plan_live_inject(current_frontmost: Option<i32>) -> LiveInjectPlan {
    match current_frontmost {
        Some(pid) => LiveInjectPlan::ResolveApp { pid },
        None => LiveInjectPlan::Hold,
    }
}

/// Whether a retained, positively-editable focus target may rescue a failed
/// fresh AX lookup. The fallback is deliberately narrow: the retained element
/// must belong to the app that is still frontmost. A real fresh element always
/// wins; this decision is consulted only after that lookup returned no element.
pub fn retained_focus_can_fallback(
    captured_pid: Option<i32>,
    current_frontmost: Option<i32>,
    editability: FocusEditability,
    captured_still_focused: bool,
) -> bool {
    captured_still_focused
        && matches!(editability, FocusEditability::Editable | FocusEditability::PasteSafe)
        && captured_pid.is_some()
        && captured_pid == current_frontmost
}

/// The pure decision: what to do with a finished transcript. This is the seam
/// the worker consults immediately before any write action.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InjectDecision {
    Inject,
    /// Don't write. Keep the transcript recoverable; show a clear state.
    Hold {
        reason: HoldReason,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HoldReason {
    /// An Escape / cancel arrived; the utterance is discarded.
    Cancelled,
    /// No editable field is focused; the transcript is held, not written.
    NoEditableField,
}

/// How a `finalize_utterance` call actually settled. This is the observable
/// result the worker + tests branch on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FinalizeResult {
    Injected,
    /// Held (transcript preserved, nothing written because no editable field).
    Held,
    /// Discarded (a cancel arrived; nothing written).
    Cancelled,
    /// The write was attempted but failed.
    Failed,
}

/// A password field. Never a dictation target: nothing is written into it and
/// its value (bullets) is never read for caret context or screen vocabulary.
pub fn is_secure_field(subrole: Option<&str>) -> bool {
    subrole == Some("AXSecureTextField")
}

/// Text-entry AX roles/subroles that read as editable fields.
fn is_text_entry_role(role: Option<&str>, subrole: Option<&str>) -> bool {
    matches!(
        role,
        Some("AXTextField" | "AXTextArea" | "AXComboBox")
    ) || matches!(
        subrole,
        Some("AXSearchField" | "AXFullScreenSearchField" | "AXTextArea" | "AXTextView")
    )
}

/// AX roles that are *positively* not a text-input target. When AX discloses
/// one of these as the focused element, injection is denied **even for an
/// allowlisted app** — a button, a window chrome, a page body, a rendered
/// canvas, or read-only/static text is never a place to paste blindly.
fn is_clearly_noninput_role(role: Option<&str>, _subrole: Option<&str>) -> bool {
    matches!(
        role,
        Some(
            "AXButton"
                | "AXWindow"
                | "AXWebArea"
                | "AXCanvas"
                | "AXStaticText"
                | "AXCheckBox"
                | "AXRadioButton"
                | "AXMenu"
                | "AXMenuBar"
                | "AXMenuItem"
                | "AXPopUpButton"
                | "AXProgressIndicator"
        )
    )
}

/// Pure classification of a focused surface into [`FocusEditability`].
///
/// `role` / `subrole` are the AX attribute strings (None when AX did not
/// disclose them). `value_settable` / `selected_text_settable` report whether
/// AX says the value / selected-text attributes are writable. `paste_safe` is
/// the one app-type fallback: a genuine terminal or an AX-blind native chat
/// app (WeChat) is *always* a text-input surface, and is the only surface
/// allowed to paste when AX discloses no role.
///
/// This is deliberately strict and never guesses above the evidence:
///   * a positively non-input role (button / window / web area / canvas / static
///     text / menu / checkbox / radio) → `NotEditable` **even for a terminal**;
///   * a paste-safe app (a genuine terminal, or an AX-blind native chat app
///     like WeChat) whose focused element is not a positively non-input
///     control → `PasteSafe` — even when its role is a text-entry role
///     (AXTextArea / AXTextField) that AX reports as **not** writable: the app
///     draws its own input and is AX-blind, so that is a text-input surface
///     (clipboard paste), not a read-only label. This is the narrow, justified
///     fallback the architecture sanctions for terminals and WeChat. It is NOT
///     a broad editor/compose blanket — a Slack pane, a Notion page, a browser
///     body, or an Outlook window is only pasted into when its focused element
///     reports an editable role, never by app name;
///   * a non-terminal text-entry role that is writable → `Editable`;
///   * a non-terminal text-entry role that is **not** writable → `NotEditable`
///     (read-only label).
pub fn classify_focus_editability(
    role: Option<&str>,
    subrole: Option<&str>,
    value_settable: bool,
    selected_text_settable: bool,
    paste_safe: bool,
) -> FocusEditability {
    // A positively non-input control (button / window chrome / page body /
    // canvas / static text / menu / checkbox / radio) is never a place to write,
    // even for an allowlisted paste-safe app. A password field is refused the
    // same way, whatever else it reports.
    if is_secure_field(subrole) || is_clearly_noninput_role(role, subrole) {
        FocusEditability::NotEditable
    } else if paste_safe {
        // A paste-safe app (a genuine terminal, or an AX-blind native chat app
        // like WeChat) is ALWAYS a text-input surface. Its focused element
        // frequently reports a text-entry role (AXTextArea / AXTextField /
        // AXComboBox, or a search-field subrole) whose AXValue / AXSelectedText
        // are NOT settable — the app draws its own input and is AX-blind, so
        // writing must go through Cmd+V (see `classify_pid`). That is NOT a
        // read-only field; it is a text-input surface, so it is `PasteSafe`,
        // never `NotEditable`. Only a positively non-input control above ever
        // holds such an app.
        FocusEditability::PasteSafe
    } else if is_text_entry_role(role, subrole) {
        if value_settable || selected_text_settable {
            FocusEditability::Editable
        } else {
            FocusEditability::NotEditable // read-only text field
        }
    } else {
        FocusEditability::NotEditable
    }
}

/// Convert a UTF-16 offset (what `kAXSelectedTextRange` reports) into a `char`
/// index into `text`. The two differ by one for every non-BMP character (most
/// emoji) before the offset. An offset inside a surrogate pair rounds down; one
/// past the end clamps to the char count.
pub fn char_index_from_utf16(text: &str, utf16_offset: usize) -> usize {
    let mut units = 0usize;
    for (i, c) in text.chars().enumerate() {
        units += c.len_utf16();
        if units > utf16_offset {
            return i;
        }
    }
    text.chars().count()
}

/// What the field's text says about the caret, for smart padding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaretContext {
    /// Whether `kAXValue` could be read at all. An EMPTY field, or a caret at
    /// index 0, is readable: it has no character before the caret, but the app
    /// is not AX-blind. Only `false` here may mark a PID as AX-blind.
    pub value_readable: bool,
    pub immediate_before: Option<char>,
    pub last_nws_before: Option<char>,
    pub char_after: Option<char>,
}

/// Derive the caret context from the field value and the UTF-16 caret offset
/// (`None` = no range exposed → assume the caret is at the end).
pub fn caret_context_from(value: Option<&str>, caret_utf16: Option<usize>) -> CaretContext {
    let Some(value) = value else {
        return CaretContext {
            value_readable: false,
            immediate_before: None,
            last_nws_before: None,
            char_after: None,
        };
    };
    let chars: Vec<char> = value.chars().collect();
    let caret = match caret_utf16 {
        Some(off) => char_index_from_utf16(value, off),
        None => chars.len(),
    };
    CaretContext {
        value_readable: true,
        immediate_before: caret.checked_sub(1).and_then(|i| chars.get(i).copied()),
        last_nws_before: last_non_ws_before(value, caret),
        char_after: chars.get(caret).copied(),
    }
}

/// A field's observable state around an AX write: its value and its selected
/// range `(location, length)` in UTF-16 units. Either may be unreadable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldSnapshot {
    pub value: Option<String>,
    pub selection: Option<(usize, usize)>,
}

/// Did an AX write that returned success actually reach the field?
///
/// Compares the field BEFORE and AFTER the write instead of searching the
/// value for the utterance. The old needle search passed falsely when the field
/// already held the same words (a repeated "Yes."), and failed falsely when the
/// app normalised the text (newlines, a length limit), which then pasted the
/// same text a second time through the clipboard.
///
///   * value unreadable after the write → not landed (the accept-and-drop apps
///     this check exists for expose nothing useful);
///   * value or selection changed → landed;
///   * nothing changed → not landed;
///   * no "before" to compare with → fall back to the needle search.
pub fn ax_write_landed(before: &FieldSnapshot, after: &FieldSnapshot, written: &str) -> bool {
    let needle = written.trim();
    if needle.is_empty() {
        return true;
    }
    let Some(after_value) = after.value.as_deref() else {
        return false;
    };
    match before.value.as_deref() {
        Some(before_value) => {
            after_value != before_value
                || matches!((before.selection, after.selection), (Some(b), Some(a)) if a != b)
        }
        None => after_value.contains(needle),
    }
}

/// The single decision point: given the cancel state and the focus evidence,
/// should this transcript be injected, held, or dropped?
///
/// `editability: None` means the caller does NOT know the current focus (e.g. a
/// failed press-time capture) — that is NOT a hold. It proceeds to the consumer
/// (the fresh resolver at write time), which re-reads the *current* focused
/// element and reports `Injected` or `NoEditableField`. Only a *known*
/// `NotEditable` (positive negative evidence) holds without writing.
pub fn decide_inject(cancel_requested: bool, editability: Option<FocusEditability>) -> InjectDecision {
    if cancel_requested {
        InjectDecision::Hold {
            reason: HoldReason::Cancelled,
        }
    } else {
        match editability {
            // Unknown → let the fresh resolver decide at write time.
            None => InjectDecision::Inject,
            Some(FocusEditability::Editable | FocusEditability::PasteSafe) => InjectDecision::Inject,
            Some(FocusEditability::NotEditable) => InjectDecision::Hold {
                reason: HoldReason::NoEditableField,
            },
        }
    }
}

/// Run the per-utterance finalization seam: decide inject-vs-hold from the
/// cancel gate + focus evidence, then (when decided) perform the write through
/// the injected sink and report the observable outcome. The sink is the real
/// injector call in production; in tests it is a recording closure, so the
/// suppression is observed through behaviour, not a tautological flag.
///
/// The sink returns `eyre::Result<InjectOutcome>` so a hard write failure is
/// distinguishable from a transcription-time hold. `None` editability reaches
/// the sink so the fresh resolver decides from the *current* focus.
pub fn finalize_utterance(
    cancel_requested: bool,
    editability: Option<FocusEditability>,
    transcript: &str,
    inject: impl FnOnce(&str) -> eyre::Result<InjectOutcome>,
) -> FinalizeResult {
    match decide_inject(cancel_requested, editability) {
        InjectDecision::Inject => match inject(transcript) {
            Ok(InjectOutcome::Injected) => FinalizeResult::Injected,
            // The fresh resolver found no editable field at write time.
            Ok(InjectOutcome::NoEditableField) => FinalizeResult::Held,
            // A fail-closed clipboard guard refused (the expected app was not
            // frontmost at Cmd+V delivery): nothing written, hold recoverable.
            Ok(InjectOutcome::RefusedForeground) => FinalizeResult::Held,
            // The write authorizer refused (a cancel landed during the focus
            // lookup): nothing written, utterance discarded.
            Ok(InjectOutcome::Cancelled) => FinalizeResult::Cancelled,
            Err(_) => FinalizeResult::Failed,
        },
        InjectDecision::Hold {
            reason: HoldReason::Cancelled,
        } => FinalizeResult::Cancelled,
        InjectDecision::Hold {
            reason: HoldReason::NoEditableField,
        } => FinalizeResult::Held,
    }
}

pub struct AccessibilityInjector;

/// Process-wide memory of the last character we successfully injected.
/// When the focused app doesn't expose `kAXValue` (Electron apps,
/// sandboxed web views, some Java apps), we fall back to using this as
/// the implicit `char_before` so consecutive utterances still get the
/// right spacing + capitalization.
///
/// Reset on first call (None) → first utterance behaves like "start of
/// field." Updated to the last char of every successful inject.
///
/// These caret-context / verification helpers are only ever called from the
/// `imp` module, so they're gated behind the same feature — otherwise a
/// default (featureless) build flags them as dead code.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
static LAST_TAIL: Mutex<Option<char>> = Mutex::new(None);

/// PID of the focused app that's known to not expose AXValue. Skip the
/// context read for it on subsequent injects (saves ~100 ms per utterance
/// in Electron / web-view targets).
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
static AX_BLIND_PID: Mutex<Option<i32>> = Mutex::new(None);

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
fn mark_ax_blind(pid: i32) {
    if let Ok(mut g) = AX_BLIND_PID.lock() {
        *g = Some(pid);
    }
}

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
fn is_ax_blind(pid: i32) -> bool {
    matches!(AX_BLIND_PID.lock().ok().and_then(|g| *g), Some(p) if p == pid)
}

/// Cache of `pid → (is_electron, is_terminal)`, looked up via `ps` once per
/// PID. Both injection routing and cleanup gating read from it.
///
/// Two families of apps report an editable AX role (AXTextField / AXTextArea)
/// and accept `kAXSelectedText` writes with `kAXErrorSuccess`, but their
/// renderer never actually receives the text — so the write silently
/// vanishes ("records but doesn't paste"). We route both straight to
/// clipboard paste, which goes through the system-level Cmd+V handler and
/// always works:
///   1. Electron / Chromium web views (VS Code, Cursor, Slack, Discord,
///      Claude, Notion, Chrome, Brave, Arc, Obsidian, Zed, Figma, new
///      Microsoft Outlook …).
///   2. GPU/native terminal emulators (Ghostty, Terminal.app, iTerm2,
///      WezTerm, kitty, Alacritty) — they draw their own text grid and
///      ignore AX writes, confirmed via INJECT_PROFILE: Ghostty returns
///      err=0 on set_selected_text yet nothing renders.
/// Per-PID classification, cached so the `ps` lookup is paid once per PID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PidClass {
    /// App accepts but never renders AX writes (Electron/Chromium web views,
    /// GPU terminals) — route to clipboard paste when the FOCUS evidence says
    /// the surface is editable. App type alone is never the green light.
    is_electron: bool,
    /// GPU/native terminal emulator. The one narrow text-input-surface fallback.
    is_terminal: bool,
    /// AX-blind native chat app (WeChat): its message box is a custom control
    /// that accepts keyboard + Cmd+V paste but exposes no focused UI element
    /// through AX (AXFocusedUIElement → kAXErrorNoValue -25212) even with the
    /// caret visible. Paste-safe like a terminal, but prose cleanup still runs
    /// (chat messages benefit from cleanup; shell commands do not).
    is_wechat: bool,
    /// A Chromium browser whose AX writes render (so it keeps the normal AX
    /// route) but whose web accessibility tree is built on demand, like
    /// Electron's. Only the one-time AX wake reads this; routing does not.
    wakes_ax_only: bool,
}

static CLIPBOARD_ONLY_CACHE: Mutex<Vec<(i32, PidClass)>> = Mutex::new(Vec::new());

/// Executable path of a running process (`ps -o comm=`), untrimmed. `None`
/// when `ps` fails or the PID is gone. Shared by the injector's app
/// classifier and the daemon's log labelling so there is one spawn site.
pub fn process_command_path(pid: i32) -> Option<String> {
    std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
}

/// Name for the app a hold or clipboard-fallback log line is about: the bundle
/// id, else the executable file name (from `ps -o comm=`), else `unknown`.
/// Pure so the choice is unit-testable; never takes text, titles or values.
pub fn app_label(bundle_id: Option<&str>, command_path: Option<&str>) -> String {
    let clean = |s: &str| s.trim().to_string();
    if let Some(b) = bundle_id.map(clean).filter(|b| !b.is_empty()) {
        return b;
    }
    command_path
        .map(clean)
        .and_then(|c| c.rsplit('/').next().map(str::to_string))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// The one format for an injector hold or clipboard-fallback log line:
/// `[inject] <reason> app=<name>`. `reason` is a fixed phrase plus an AX error
/// or role key, never transcript text, a window title or a field value.
pub fn hold_log_line(reason: &str, app: &str) -> String {
    let app = if app.trim().is_empty() { "unknown" } else { app.trim() };
    format!("[inject] {reason} app={app}")
}

/// [`app_label`] for a PID, looked up only on hold/fallback paths. May spawn
/// `ps`, so never call it before a paste; use [`app_label_for_pid_cheap`]
/// there. Without the `ax-inject` feature it is always `unknown`.
pub fn app_label_for_pid(pid: Option<i32>) -> String {
    #[cfg(all(target_os = "macos", feature = "ax-inject"))]
    if let Some(pid) = pid {
        let bundle = bundle_id_for_pid(pid);
        if bundle.is_some() {
            return app_label(bundle.as_deref(), None);
        }
        return app_label(None, process_command_path(pid).as_deref());
    }
    let _ = pid;
    app_label(None, None)
}

/// Bundle-id-only label (one `NSRunningApplication` read, no `ps`), for log
/// lines written just before a clipboard paste.
pub fn app_label_for_pid_cheap(pid: Option<i32>) -> String {
    #[cfg(all(target_os = "macos", feature = "ax-inject"))]
    if let Some(pid) = pid {
        return app_label(bundle_id_for_pid(pid).as_deref(), None);
    }
    let _ = pid;
    app_label(None, None)
}

/// Pure app-name check for AX-blind native chat apps (WeChat). Split out of
/// `classify_pid` so the name matching is unit-testable without shelling to
/// `ps`. Case-insensitive. Matches only WeChat's main app process
/// (`…/MacOS/WeChat`), not the Chromium-based `WeChatAppEx` mini-program
/// engine (whose helpers end in `wechatappex`, `wechatappex helper (renderer)`,
/// etc.).
fn is_ax_blind_chat_app_comm(comm: &str) -> bool {
    comm.trim().to_lowercase().ends_with("/wechat")
}

/// Pure app-name check for a Chromium browser that needs the on-demand AX
/// wake but is not on the clipboard-only list. Helium's executable
/// (`/Applications/Helium.app/Contents/MacOS/Helium`) matches none of the
/// `/chrome`, `/brave` or `/arc` names, so a focus miss there never woke its
/// tree. Its writes keep the AX route: verify-after-write already falls back
/// to the clipboard (and promotes the PID) when a write does not render, so
/// forcing clipboard-only would only add the paste cost. Case-insensitive.
fn is_on_demand_ax_browser_comm(comm: &str) -> bool {
    comm.trim().to_lowercase().ends_with("/helium")
}

/// Classify a PID by its executable name into a [`PidClass`]. The `is_electron`
/// flag routes to clipboard paste; `is_terminal` additionally lets the daemon
/// skip prose cleanup (you almost never want Gemma rewriting a shell command);
/// `is_editor_surface` is the curated non-browser allowlist used by the
/// focus-editability gate. Result is cached per PID.
fn classify_pid(pid: i32) -> PidClass {
    if let Ok(cache) = CLIPBOARD_ONLY_CACHE.lock() {
        for &(p, cls) in cache.iter() {
            if p == pid {
                return cls;
            }
        }
    }
    let comm = process_command_path(pid)
        .map(|s| s.to_lowercase())
        .unwrap_or_default();
    let is_electron = comm.contains("/electron")
        || comm.contains("visual studio code")
        || comm.contains("/code helper")
        || comm.contains("/code.app")
        || comm.contains("/cursor")
        || comm.contains("/slack")
        || comm.contains("/discord")
        || comm.contains("/claude")
        || comm.contains("/notion")
        || comm.contains("/chrome")
        || comm.contains("/brave")
        || comm.contains("/arc")
        || comm.contains("/obsidian")
        || comm.contains("/zed")
        || comm.contains("/figma")
        // New Outlook for Mac is a WebView2/Chromium app: it reports an editable
        // AX role and accepts kAXSelectedText writes with kAXErrorSuccess, but
        // the compose web view never renders them. Route it to clipboard paste.
        || comm.contains("/microsoft outlook");
    // Native terminal emulators: own-drawn text grids that accept but ignore
    // AX writes. The `/term` anchor catches Terminal.app and iTerm2's exec
    // (`.../macos/iterm2`); the rest match their app/exec name.
    let is_terminal = comm.contains("/ghostty")
        || comm.contains("/terminal")
        || comm.contains("/iterm")
        || comm.contains("/wezterm")
        || comm.contains("/kitty")
        || comm.contains("/alacritty");
    let is_wechat = is_ax_blind_chat_app_comm(&comm);
    let wakes_ax_only = is_on_demand_ax_browser_comm(&comm);
    // The editor/compose blanket (VS Code, Slack, Notion, Obsidian, Figma, …)
    // is deliberately ABSENT from a paste-safe allowlist. A paste is justified
    // only by the FOCUSED element's AX role at write time, never by the app
    // name — so a Slack pane or a browser body never receives a blind paste.
    let cls = PidClass { is_electron, is_terminal, is_wechat, wakes_ax_only };
    if let Ok(mut cache) = CLIPBOARD_ONLY_CACHE.lock() {
        cache.push((pid, cls));
    }
    cls
}

pub fn is_clipboard_only_pid(pid: i32) -> bool {
    let cls = classify_pid(pid);
    cls.is_electron || cls.is_terminal || cls.is_wechat
}

/// True when the focused app is a native terminal emulator. The daemon uses
/// this to inject the raw transcript without Gemma cleanup, and the focus gate
/// uses it (narrowly) as the one text-input-surface fallback when AX discloses
/// no editable role.
pub fn is_terminal_pid(pid: i32) -> bool {
    classify_pid(pid).is_terminal
}

/// True when the focused app is an AX-blind native chat app (WeChat). Used by
/// the focus gate as a paste-safe fallback when AX discloses no focused element
/// (see `is_wechat` on [`PidClass`]), and by `is_clipboard_only_pid` to route
/// any editable WeChat field straight to clipboard paste.
pub fn is_wechat_pid(pid: i32) -> bool {
    classify_pid(pid).is_wechat
}

/// True when the app is an Electron / Chromium web view, including a Chromium
/// browser that keeps the AX write route (Helium). Used only to decide
/// whether to flip Chromium's on-demand accessibility switch before treating a
/// missing focused element as final (see
/// `imp::wake_electron_ax_and_retry_focus`). It never widens who may receive a
/// paste — the editability gate still owns that.
pub fn is_electron_pid(pid: i32) -> bool {
    let cls = classify_pid(pid);
    cls.is_electron || cls.wakes_ax_only
}

/// PIDs whose AX write we've *verified* actually rendered (read the value
/// back and saw our text). Once a PID is here we trust its AX path and skip
/// the post-write verification read on every subsequent utterance.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
static AX_VERIFIED_PID: Mutex<Vec<(i32, String)>> = Mutex::new(Vec::new());

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
fn is_ax_verified(pid: i32, role: &str) -> bool {
    AX_VERIFIED_PID
        .lock()
        .map(|g| g.iter().any(|(p, r)| *p == pid && r == role))
        .unwrap_or(false)
}

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
fn mark_ax_verified(pid: i32, role: &str) {
    if let Ok(mut g) = AX_VERIFIED_PID.lock() {
        if !g.iter().any(|(p, r)| *p == pid && r == role) {
            g.push((pid, role.to_string()));
        }
    }
}

/// Promote a PID to clipboard-only at runtime. Used when an AX write returns
/// success but the value read-back proves nothing rendered — i.e. an app with
/// the lying-AX trait that isn't on the static name list. Updates the existing
/// cache entry (the name lookup will have inserted `false`) or inserts a fresh
/// entry so all future injects skip the AX path for this PID.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
fn mark_clipboard_only(pid: i32) {
    if let Ok(mut cache) = CLIPBOARD_ONLY_CACHE.lock() {
        if let Some(entry) = cache.iter_mut().find(|(p, _)| *p == pid) {
            entry.1.is_electron = true;
        } else {
            cache.push((
                pid,
                PidClass {
                    is_electron: true,
                    is_terminal: false,
                    is_wechat: false,
                    wakes_ax_only: false,
                },
            ));
        }
    }
}

/// Chromium/Electron apps build no accessibility tree until a client asks for
/// one, so their `AXFocusedUIElement` answers `kAXErrorNoValue` (-25212) until
/// the on-demand switch is set. We flip it at most once per app process; this
/// records the PIDs already asked, so a later miss never re-triggers the wake
/// and its short retry loop.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
static ELECTRON_AX_WAKE_CLAIMED: Mutex<Vec<i32>> = Mutex::new(Vec::new());

/// Claim the one-time Chromium AX wake for `pid`. True only on the first call
/// for a PID. Pure bookkeeping — the AX attribute write lives in `imp`.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
fn claim_electron_ax_wake(pid: i32) -> bool {
    match ELECTRON_AX_WAKE_CLAIMED.lock() {
        Ok(mut claimed) => {
            if claimed.contains(&pid) {
                false
            } else {
                claimed.push(pid);
                true
            }
        }
        // A poisoned lock must not disable the fix; the attribute write is
        // idempotent, so allowing repeated attempts is safe.
        Err(_) => true,
    }
}

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
fn remember_tail(text: &str) {
    let tail = text.chars().rev().find(|c| !c.is_whitespace());
    if let Ok(mut g) = LAST_TAIL.lock() {
        *g = tail;
    }
}

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
fn recall_tail() -> Option<char> {
    LAST_TAIL.lock().ok().and_then(|g| *g)
}

/// The PID of the OS-frontmost application, or `None` when it can't be read
/// (no frontmost app, or the AppKit read failed). Best-effort; used to detect
/// whether the focused app changed between key-press and inject time.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub fn frontmost_pid() -> Option<i32> {
    use objc2_app_kit::NSWorkspace;
    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .map(|app| app.processIdentifier())
}

/// Bundle identifier of a running app, by PID.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub fn bundle_id_for_pid(pid: i32) -> Option<String> {
    use objc2_app_kit::NSRunningApplication;
    NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        .and_then(|app| app.bundleIdentifier())
        .map(|s| s.to_string())
}

/// The frontmost app's PID and bundle id, from one `NSRunningApplication`.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub fn frontmost_target_app() -> Option<TargetApp> {
    use objc2_app_kit::NSWorkspace;
    NSWorkspace::sharedWorkspace().frontmostApplication().map(|app| TargetApp {
        pid: app.processIdentifier(),
        bundle_id: app.bundleIdentifier().map(|s| s.to_string()),
    })
}

/// True when a press-time [`FocusTarget`] still points at the OS-frontmost app
/// at release time. Trust the retained element when AX didn't disclose its PID
/// (nothing to verify), when the current frontmost PID can't be read, or when
/// the two match. A mismatch means the user switched apps during the hold —
/// writing into the retained element would land in a background app, so the
/// caller must re-resolve fresh.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub fn focus_target_still_current(t: &FocusTarget) -> bool {
    match t.inner_pid() {
        Some(target_pid) => frontmost_pid().is_none_or(|cur| cur == target_pid),
        None => true,
    }
}

/// Activate a running application by PID and confirm it becomes the OS-frontmost
/// application. Used by the recovery paste to bring back the app that was
/// frontmost before the menu opened.
///
/// Returns `true` only when the app is running, activation succeeds, and the
/// frontmost verification passes (activation is asynchronous, so this briefly
/// polls the OS frontmost application). A `false` verdict means the app did not
/// (or would not) come forward — never inject into a target that will not take
/// focus, because that is exactly the "wrong target" case recovery must avoid.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub fn activate_app(pid: i32) -> bool {
    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
    let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) else {
        return false;
    };
    // Bare activation brings the app's main/key window forward. We do not use
    // the (deprecated, no-op on macOS 14+) `ActivateIgnoringOtherApps` flag;
    // default options already activate the receiver, and the target app was
    // frontmost before the menu anyway.
    let requested = app.activateWithOptions(NSApplicationActivationOptions::empty());
    if !requested {
        return false;
    }
    // Activation is async. Poll the OS frontmost application for a short window.
    for _ in 0..24 {
        if frontmost_pid() == Some(pid) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    frontmost_pid() == Some(pid)
}

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
mod imp {
    use super::*;
    use accessibility_sys::{
        kAXErrorSuccess, kAXFocusedAttribute, kAXFocusedUIElementAttribute, kAXRoleAttribute, kAXSelectedTextAttribute,
        kAXSelectedTextRangeAttribute, kAXSubroleAttribute, kAXTrustedCheckOptionPrompt,
        kAXValueAttribute, kAXValueTypeCFRange, AXIsProcessTrustedWithOptions,
        AXUIElementCopyAttributeValue, AXUIElementCreateApplication,
        AXUIElementGetPid, AXUIElementIsAttributeSettable, AXUIElementSetMessagingTimeout,
        AXUIElementRef, AXUIElementSetAttributeValue, AXValueGetValue, AXValueRef,
    };
    use core_foundation::base::{CFRange, CFTypeRef, TCFType};
    use core_foundation::boolean::{CFBoolean, CFBooleanRef};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::{CFString, CFStringRef};
    use libc::pid_t;
    use std::ffi::c_void;
    use std::ptr;

    /// Bound AX messaging so a hung target app can't freeze the daemon. Without
    /// a timeout an AX API call to an unresponsive app blocks indefinitely;
    /// with a bound it returns after ~[`AX_MESSAGING_TIMEOUT_SECS`] and the
    /// inject ladder demotes to the clipboard-paste fallback (see
    /// `write_padded`), exactly as it does for any other non-success error.
    const AX_MESSAGING_TIMEOUT_SECS: f32 = 1.5;
    /// Menu opening runs on AppKit's main thread. Keep its focus snapshot
    /// bounded; any deeper context reads happen later on the recovery worker.
    const MENU_AX_MESSAGING_TIMEOUT_SECS: f32 = 0.2;
    /// Chromium on-demand accessibility attributes. `AXManualAccessibility` is
    /// the modern switch Chromium/Electron read; `AXEnhancedUserInterface` is
    /// the older AppKit one VoiceOver and Safari set. Both are set so the wake
    /// holds across Chromium versions.
    const AX_MANUAL_ACCESSIBILITY: &str = "AXManualAccessibility";
    const AX_ENHANCED_UI: &str = "AXEnhancedUserInterface";
    /// Bounded retry after the Chromium AX wake. The first enable is the slow
    /// one: Chromium builds the tree asynchronously, so the initial read can
    /// keep missing for a few hundred milliseconds while the tree populates.
    /// Poll on a wall-clock budget (not a fixed attempt count) so a slow first
    /// build is waited out, while a hung renderer still cannot pin us: each
    /// query is separately bounded by `AX_WAKE_MESSAGING_TIMEOUT_SECS`.
    const AX_WAKE_POLL_BUDGET_MS: u64 = 900;
    const AX_WAKE_POLL_INTERVAL_MS: u64 = 40;
    const AX_WAKE_MESSAGING_TIMEOUT_SECS: f32 = 0.3;

    /// Best-effort: set the AX messaging timeout on an element so a slow or hung
    /// target app can't pin the daemon on an AX call. Ignores the AXError — if
    /// the OS declines the bound we just fall back to the unbounded default for
    /// that element.
    unsafe fn set_messaging_timeout(el: AXUIElementRef, seconds: f32) {
        let _ = AXUIElementSetMessagingTimeout(el, seconds);
    }

    unsafe fn bound_messaging_timeout(el: AXUIElementRef) {
        set_messaging_timeout(el, AX_MESSAGING_TIMEOUT_SECS);
    }

    /// Chromium/Electron builds no accessibility tree until a client asks for
    /// one, so Slack, Cursor, VS Code and the rest answer
    /// `AXFocusedUIElement` with `kAXErrorNoValue` (-25212) until the on-demand
    /// switch is set. Flip it once per app process, then re-read the focused
    /// element within a bounded window.
    ///
    /// Returns the focused element on success, or null. This is a *look*, not
    /// a write: whatever comes back still has to pass the editability gate
    /// before any text moves, so waking an app never enables a blind paste.
    ///
    /// Cost sits on the miss path only: one attribute set per app process
    /// (claimed by `claim_electron_ax_wake`), then a few short retries. Steady
    /// dictation never calls it. The app-internal price is Chromium keeping an
    /// AX tree, the same cost VoiceOver imposes for as long as the app runs.
    unsafe fn wake_electron_ax_and_retry_focus(
        app: AXUIElementRef,
        pid: Option<i32>,
        restore_timeout_secs: f32,
    ) -> CFTypeRef {
        let Some(pid) = pid else {
            return ptr::null();
        };
        // A native app's tree is always live; only Chromium needs the switch.
        // Claimed once per process, so a later miss never re-triggers the wait.
        if !super::is_electron_pid(pid) || !super::claim_electron_ax_wake(pid) {
            return ptr::null();
        }
        // `AXManualAccessibility` is Chromium/Electron's on-demand switch;
        // `AXEnhancedUserInterface` is the older AppKit signal. A refusal is
        // "unsupported", not fatal.
        for attr in [AX_MANUAL_ACCESSIBILITY, AX_ENHANCED_UI] {
            let a = CFString::new(attr);
            let value = CFBoolean::true_value();
            let _ = AXUIElementSetAttributeValue(
                app,
                a.as_concrete_TypeRef(),
                value.as_concrete_TypeRef() as CFTypeRef,
            );
        }
        // Chromium populates the tree asynchronously once the switch is on.
        // Poll on a wall-clock budget under a short AX timeout so the first
        // (slow) enable is waited out, while a hung renderer still costs only
        // a few bounded queries. Later utterances find the tree already live
        // and pass on the very first query.
        set_messaging_timeout(app, AX_WAKE_MESSAGING_TIMEOUT_SECS);
        let focused_attr = CFString::new(kAXFocusedUIElementAttribute);
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(AX_WAKE_POLL_BUDGET_MS);
        let mut focused_ref: CFTypeRef = ptr::null();
        loop {
            let mut attempt_ref: CFTypeRef = ptr::null();
            let err = AXUIElementCopyAttributeValue(
                app,
                focused_attr.as_concrete_TypeRef(),
                &mut attempt_ref,
            );
            if err == kAXErrorSuccess && !attempt_ref.is_null() {
                focused_ref = attempt_ref;
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(AX_WAKE_POLL_INTERVAL_MS));
        }
        set_messaging_timeout(app, restore_timeout_secs);
        focused_ref
    }

    pub fn ensure_trusted_or_prompt() -> bool {
        unsafe {
            let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
            let value = CFBoolean::true_value();
            let opts = CFDictionary::from_CFType_pairs(&[(key, value)]);
            AXIsProcessTrustedWithOptions(opts.as_concrete_TypeRef())
        }
    }

    /// Read a single string AX attribute off an element, or `None` when it is
    /// absent or not a string. Used for role / subrole probes.
    unsafe fn copy_string_attr(element: AXUIElementRef, attr: &str) -> Option<String> {
        let mut value_ref: CFTypeRef = ptr::null();
        let a = CFString::new(attr);
        let err =
            AXUIElementCopyAttributeValue(element, a.as_concrete_TypeRef(), &mut value_ref);
        if err != kAXErrorSuccess || value_ref.is_null() {
            return None;
        }
        Some(CFString::wrap_under_create_rule(value_ref as CFStringRef).to_string())
    }

    /// Positive check that a retained element is still the focused field. This
    /// preserves same-app field-change safety when the application root's fresh
    /// `AXFocusedUIElement` query transiently returns no value.
    unsafe fn element_reports_focused(element: AXUIElementRef) -> bool {
        let mut value_ref: CFTypeRef = ptr::null();
        let attr = CFString::new(kAXFocusedAttribute);
        let err = AXUIElementCopyAttributeValue(
            element,
            attr.as_concrete_TypeRef(),
            &mut value_ref,
        );
        if err != kAXErrorSuccess || value_ref.is_null() {
            return false;
        }
        bool::from(CFBoolean::wrap_under_create_rule(
            value_ref as CFBooleanRef,
        ))
    }

    /// True when AX reports an attribute as writable on the element. A settable
    /// `AXValue` / `AXSelectedText` is the hard, positive evidence that a
    /// focused surface is a real text-input field (its counterpart: a role that
    /// merely *looks* like a field but is not writable is a label/static).
    unsafe fn attribute_is_settable(element: AXUIElementRef, attr: &str) -> bool {
        let mut settable: std::ffi::c_uchar = 0;
        let a = CFString::new(attr);
        let err = AXUIElementIsAttributeSettable(element, a.as_concrete_TypeRef(), &mut settable);
        err == kAXErrorSuccess && settable != 0
    }

    /// Classify the editability of a focused element from its AX role/subrole +
    /// writability, plus the (single, narrow) app-type signal from its PID: is
    /// it a genuine terminal or an AX-blind native chat app (WeChat)? This is
    /// the focus gate the write ladder consults — it is what turns "no blind
    /// paste" from a comment into a decision. The editor/compose app blanket is
    /// deliberately absent.
    unsafe fn focused_editability(focused: AXUIElementRef, pid: Option<i32>) -> FocusEditability {
        let role = copy_string_attr(focused, kAXRoleAttribute);
        let subrole = copy_string_attr(focused, kAXSubroleAttribute);
        let value_settable = attribute_is_settable(focused, kAXValueAttribute);
        let sel_settable = attribute_is_settable(focused, kAXSelectedTextAttribute);
        let paste_safe = pid
            .map(|p| super::is_terminal_pid(p) || super::is_wechat_pid(p))
            .unwrap_or(false);
        classify_focus_editability(
            role.as_deref(),
            subrole.as_deref(),
            value_settable,
            sel_settable,
            paste_safe,
        )
    }

    /// The one write path. With a `pid` the app is named (a menu recovery
    /// paste); otherwise the OS-frontmost app is snapshotted and its focused
    /// element resolved per-app, never through the systemwide AX proxy, which
    /// answers -25204 or -25212 even when the app resolves fine. The fail-closed
    /// Cmd+V guard still protects delivery and the write authorizer still holds
    /// the cancel gate; both pass through to `inject_through`. If AX transiently
    /// returns no focused element, `inject_through` may use `retained` only
    /// when the same app remains frontmost and the capture was positively
    /// editable.
    pub fn inject(
        pid: Option<pid_t>,
        retained: Option<FocusTargetInner>,
        text: &str,
        authorize: Option<&dyn Fn() -> bool>,
    ) -> eyre::Result<InjectOutcome> {
        if !ensure_trusted_or_prompt() {
            return Err(eyre::eyre!(
                "Accessibility permission not granted: System Settings > Privacy & Security > Accessibility"
            ));
        }
        let pid = match pid {
            Some(pid) => pid,
            None => match plan_live_inject(super::frontmost_pid()) {
                LiveInjectPlan::ResolveApp { pid } => pid,
                LiveInjectPlan::Hold => {
                    eprintln!("{}", super::hold_log_line("no frontmost app at write time, holding (no live field)", "unknown"));
                    return Ok(InjectOutcome::NoEditableField);
                }
            },
        };
        unsafe {
            let app = AXUIElementCreateApplication(pid);
            if app.is_null() {
                return Err(eyre::eyre!(
                    "AXUIElementCreateApplication({pid}) returned null"
                ));
            }
            bound_messaging_timeout(app);
            let result = inject_through(app, text, authorize, Some(pid), retained.as_ref());
            cf_release(app as CFTypeRef);
            result
        }
    }

    /// Resolve-now path: find the focused element → read context → smart-pad
    /// → hand off to the shared write ladder. Used by every entry point; a
    /// request that carries a retained target shares
    /// the same ladder; the only difference is *when* the focus element is
    /// resolved.
    ///
    /// `expected_app` is the app that must be frontmost before any global
    /// Cmd+V is posted: `Some(pid)` for the by-PID entry point (the explicit
    /// target), `None` for the systemwide path (resolved from the focused
    /// element). Clipboard delivery is fail-closed on it.
    unsafe fn inject_through(
        root: AXUIElementRef,
        text: &str,
        authorize: Option<&dyn Fn() -> bool>,
        expected_app: Option<i32>,
        retained: Option<&FocusTargetInner>,
    ) -> eyre::Result<InjectOutcome> {
        let prof = std::env::var("INJECT_PROFILE").is_ok();

        // 1. Focused element.
        let t = std::time::Instant::now();
        let mut focused_ref: CFTypeRef = ptr::null();
        let focused_attr = CFString::new(kAXFocusedUIElementAttribute);
        let err = AXUIElementCopyAttributeValue(
            root,
            focused_attr.as_concrete_TypeRef(),
            &mut focused_ref,
        );
        if prof {
            eprintln!("[inject-prof] get_focused_element: {:?}", t.elapsed());
        }
        if err != kAXErrorSuccess || focused_ref.is_null() {
            // Chromium/Electron builds no AX tree until a client asks for one,
            // so Slack, Cursor and VS Code answer this exact query with
            // kAXErrorNoValue (-25212). Flip Chromium's on-demand switch once
            // and re-read before calling the miss final. This is a look, not a
            // write: the element still has to pass the editability gate below.
            focused_ref = wake_electron_ax_and_retry_focus(
                root,
                expected_app.or_else(super::frontmost_pid),
                AX_MESSAGING_TIMEOUT_SECS,
            );
        }
        if focused_ref.is_null() {
            // AX can transiently lose the focused element while a status menu
            // is tracking or while Electron reshuffles its accessibility tree.
            // A retained target is usable only when it carried positive
            // editability evidence and its owning app is STILL frontmost. This
            // rescues that exact false-negative without reviving a stale field
            // after an app switch. A real fresh element always wins above.
            if let Some(target) = retained {
                // Menu snapshots use a short main-thread timeout. Restore the
                // normal worker-thread bound before probing or reading context.
                bound_messaging_timeout(target.focused);
                if retained_focus_can_fallback(
                    target.pid,
                    super::frontmost_pid(),
                    target.editability,
                    element_reports_focused(target.focused),
                ) {
                    eprintln!(
                        "[inject] fresh focus unavailable (AXError {err}); reusing same-app captured field"
                    );
                    let (immediate_before, last_nws_before, char_after) =
                        if target.context_captured {
                            (
                                target.immediate_before,
                                target.last_nws_before,
                                target.char_after,
                            )
                        } else {
                            read_context_with_blind(target.focused, target.pid)
                        };
                    let padded = smart_pad(
                        text,
                        immediate_before,
                        last_nws_before,
                        char_after,
                    );
                    if padded.is_empty() {
                        return Ok(InjectOutcome::Injected);
                    }
                    return write_padded(
                        target.focused,
                        target.pid,
                        &padded,
                        prof,
                        target.editability,
                        authorize,
                        expected_app,
                    );
                }
            }

            // No focused field exposed. This is NOT a green light to paste
            // blindly: only a genuine terminal or an AX-blind native chat app
            // (WeChat) gets the narrow clipboard fallback. Anything else — a
            // browser page body, a Slack pane, an editor window, a menu, an
            // unknown app — holds the transcript. AX absence is alone never
            // proof of a text field.
            // The expected app for the Cmd+V guard: the explicit target for the
            // by-PID entry, or (systemwide) whatever is frontmost right now.
            // Never a foreign app we didn't intend — and never the frontmost
            // bolted in place of the explicit target.
            let expected_pid = expected_app.or_else(super::frontmost_pid);
            let front_paste_safe = expected_pid
                .map(|p| super::is_terminal_pid(p) || super::is_wechat_pid(p))
                .unwrap_or(false);
            if front_paste_safe {
                eprintln!(
                    "{}",
                    super::hold_log_line(
                        &format!("no focused AX element (AXError {err}); paste-safe app → clipboard paste"),
                        &super::app_label_for_pid_cheap(expected_pid),
                    )
                );
                let tail = super::recall_tail();
                let padded = smart_pad(text, tail, tail, None);
                // Claim authorization immediately before the first clipboard
                // side effect (the no-focused-element terminal route shares the
                // same guard as the native AX ladder in `write_padded`).
                if let Some(cancelled) = claim_write_authorization(authorize) {
                    return Ok(cancelled);
                }
                match crate::clipboard_paste::paste_via_clipboard(&padded, expected_pid) {
                    Ok(crate::clipboard_paste::PasteOutcome::Delivered) => {
                        super::remember_tail(&padded);
                        return Ok(InjectOutcome::Injected);
                    }
                    Ok(crate::clipboard_paste::PasteOutcome::RefusedForeground) => {
                        eprintln!(
                            "{}",
                            super::hold_log_line(
                                &format!("target {expected_pid:?} not frontmost at Cmd+V — holding (not pasted)"),
                                &super::app_label_for_pid(expected_pid),
                            )
                        );
                        return Ok(InjectOutcome::RefusedForeground);
                    }
                    Err(e) => return Err(e),
                }
            }
            eprintln!(
                "{}",
                super::hold_log_line(
                    &format!("no focused AX element (AXError {err}); not a known text-input app → holding (never paste blindly)"),
                    &super::app_label_for_pid(expected_pid),
                )
            );
            return Ok(InjectOutcome::NoEditableField);
        }
        let focused = focused_ref as AXUIElementRef;
        bound_messaging_timeout(focused);

        // 2. Editability gate: is there positive evidence that this is a
        //    writable text field? Decided before any audio work is spent.
        let pid = focused_pid_of(focused);
        let editability = focused_editability(focused, pid);

        // 3. Context (with the ax-blind shortcut), then tail fallback when the
        //    element doesn't expose AXValue (Electron / web views / some Java).
        let (mut immediate_before, mut last_nws_before, char_after) =
            read_context_with_blind(focused, pid);
        if immediate_before.is_none() && last_nws_before.is_none() {
            if let Some(tail) = super::recall_tail() {
                immediate_before = Some(tail);
                last_nws_before = Some(tail);
            }
        }

        // 4. Smart-pad.
        let padded = smart_pad(text, immediate_before, last_nws_before, char_after);
        if padded.is_empty() {
            cf_release(focused as CFTypeRef);
            return Ok(InjectOutcome::Injected);
        }

        // 5. Shared write ladder. Caller owns `focused`, so release after.
        // `expected_pid` is what must be frontmost before a global Cmd+V. For the
        // by-PID entry it is the explicit target; for the systemwide path it is
        // the focused element's app (or, if AX won't disclose the PID, the
        // frontmost app captured here at resolve time) — so a foreground switch
        // between resolve and the Cmd+V is caught.
        let expected_pid = expected_app.or(pid).or_else(super::frontmost_pid);
        let r = write_padded(focused, pid, &padded, prof, editability, authorize, expected_pid);
        cf_release(focused as CFTypeRef);
        r
    }

    /// Read caret context while honoring the per-PID ax-blind cache: skip the
    /// (~100 ms) AXValue round-trip for PIDs we've already learned don't expose
    /// it, and learn new ones on the fly. Returns
    /// `(immediate_before, last_nws_before, char_after)`.
    unsafe fn read_context_with_blind(
        focused: AXUIElementRef,
        pid: Option<i32>,
    ) -> (Option<char>, Option<char>, Option<char>) {
        match pid {
            Some(pid) if super::is_ax_blind(pid) => (None, None, None),
            Some(pid) => {
                let ctx = read_caret_context_full(focused);
                // Blind means kAXValue could not be READ. An empty field or a
                // caret at index 0 also has no character before it, but the
                // app is fully readable and must keep its caret context.
                if !ctx.value_readable {
                    super::mark_ax_blind(pid);
                }
                (ctx.immediate_before, ctx.last_nws_before, ctx.char_after)
            }
            None => read_caret_context(focused),
        }
    }

    /// The single AX write ladder shared by both injection entry points:
    /// focus gate → `kAXSelectedText` → clipboard paste.
    /// Records the injected tail on any success. Does **not**
    /// release `focused` — the caller owns its lifetime.
    ///
    /// The `editability` argument is the focus gate: `NotEditable` returns
    /// [`InjectOutcome::NoEditableField`] *before* any write so nothing is ever
    /// pasted into a non-input surface. The clipboard route is used only when
    /// there is positive editable evidence (`PasteSafe`, i.e. a terminal /
    /// curated editor, or an `Editable` role in an AX-blind-for-writing app).
    unsafe fn write_padded(
        focused: AXUIElementRef,
        pid: Option<i32>,
        padded: &str,
        prof: bool,
        editability: FocusEditability,
        authorize: Option<&dyn Fn() -> bool>,
        expected_pid: Option<i32>,
    ) -> eyre::Result<InjectOutcome> {
        let t_total = std::time::Instant::now();

        // Focus gate: no positive editable evidence → hold, never paste blindly.
        // A hold never claims write authorization.
        if editability == FocusEditability::NotEditable {
            // Name the focused role/subrole (never its value), so a hold in a
            // browser can be told apart from a focus miss without a live
            // INJECT_DIAG repro. An empty key means no element or no role.
            let role = if focused.is_null() {
                "none".to_string()
            } else {
                verification_role_key(focused)
            };
            eprintln!(
                "{}",
                super::hold_log_line(
                    &format!("no editable field focused (role {role}) — holding transcript (never paste blindly)"),
                    &super::app_label_for_pid(pid.or(expected_pid)),
                )
            );
            return Ok(InjectOutcome::NoEditableField);
        }

        // Both AX writes and global clipboard pastes are fail-closed on the
        // intended foreground app at the actual write boundary.
        if !clipboard_target_is_frontmost(expected_pid, super::frontmost_pid()) {
            eprintln!(
                "{}",
                super::hold_log_line(
                    &format!("expected app {expected_pid:?} not frontmost at write — holding (not written)"),
                    &super::app_label_for_pid(expected_pid),
                )
            );
            return Ok(InjectOutcome::RefusedForeground);
        }

        // Write authorization: claimed exactly once, after the focus/context/
        // padding resolution above and immediately before the first write or
        // clipboard side effect below. A cancel that landed during the (up to
        // 1.5s) AX lookup is honored here; the existing write ladder (and any
        // fallback) proceeds on one successful claim.
        if let Some(cancelled) = claim_write_authorization(authorize) {
            return Ok(cancelled);
        }

        // Electron / browser / terminal targets accept AX writes with
        // kAXErrorSuccess but never render the text. Route them to clipboard
        // paste — but only because we already have positive editable evidence
        // (an `Editable` role, or `PasteSafe` for a known text-input surface).
        // App type alone is never the green light; editability is.
        let clipboard_route =
            editability == FocusEditability::PasteSafe
                || pid.map(super::is_clipboard_only_pid).unwrap_or(false);
        if clipboard_route {
            if prof {
                eprintln!("[inject-prof] clipboard route (editability {editability:?}, pid {pid:?})");
            }
            match crate::clipboard_paste::paste_via_clipboard(padded, expected_pid) {
                Ok(crate::clipboard_paste::PasteOutcome::Delivered) => {
                    super::remember_tail(padded);
                    return Ok(InjectOutcome::Injected);
                }
                Ok(crate::clipboard_paste::PasteOutcome::RefusedForeground) => {
                    eprintln!(
                        "{}",
                        super::hold_log_line(
                            &format!("expected app {expected_pid:?} not frontmost at Cmd+V — holding (not pasted)"),
                            &super::app_label_for_pid(expected_pid),
                        )
                    );
                    return Ok(InjectOutcome::RefusedForeground);
                }
                Err(e) => return Err(e),
            }
        }

        let payload = CFString::new(padded);

        // First write to this (app, kind of field): snapshot the field so the
        // read-back can compare before and after. Trusted pairs skip the reads.
        let role_key = verification_role_key(focused);
        let before = match pid {
            Some(p) if !super::is_ax_verified(p, &role_key) => Some(field_snapshot(focused)),
            _ => None,
        };

        let t = std::time::Instant::now();
        let sel_attr = CFString::new(kAXSelectedTextAttribute);
        let sel_err = AXUIElementSetAttributeValue(
            focused,
            sel_attr.as_concrete_TypeRef(),
            payload.as_concrete_TypeRef() as CFTypeRef,
        );
        if prof {
            eprintln!(
                "[inject-prof] set_selected_text: {:?} (err={sel_err})",
                t.elapsed()
            );
        }
        if sel_err == kAXErrorSuccess {
            let r = confirm_or_fallback(focused, pid, &role_key, before, padded, expected_pid);
            if prof {
                eprintln!("[inject-prof] TOTAL: {:?}", t_total.elapsed());
            }
            return r;
        }

        // The caret write was refused — clipboard fallback. Only reachable when
        // editability was Editable; the app refused the write, so Cmd+V is the
        // remaining write path (and we already proved there is a field). There
        // is no kAXValue rung: setting the value replaces the WHOLE field with
        // the utterance and destroys what the user already wrote there.
        eprintln!(
            "{}",
            super::hold_log_line(
                &format!("AX caret write refused (selected={sel_err}); using clipboard paste"),
                &super::app_label_for_pid_cheap(pid.or(expected_pid)),
            )
        );
        match crate::clipboard_paste::paste_via_clipboard(padded, expected_pid) {
            Ok(crate::clipboard_paste::PasteOutcome::Delivered) => {
                super::remember_tail(padded);
                Ok(InjectOutcome::Injected)
            }
            Ok(crate::clipboard_paste::PasteOutcome::RefusedForeground) => {
                eprintln!(
                    "{}",
                    super::hold_log_line(
                        &format!("expected app {expected_pid:?} not frontmost at Cmd+V — holding (not pasted)"),
                        &super::app_label_for_pid(expected_pid),
                    )
                );
                Ok(InjectOutcome::RefusedForeground)
            }
            Err(e) => Err(e),
        }
    }

    /// Called after an AX write returns `kAXErrorSuccess`. Some apps (Electron
    /// web views, native terminals) accept the write and report success but
    /// their renderer silently discards it — the static name list in
    /// `is_clipboard_only_pid` only knows the ones we've named. To catch the
    /// rest automatically, the *first* successful AX write to a PID is verified
    /// by comparing the field before and after the write ([`ax_write_landed`]).
    /// Trust is kept per (pid, role/subrole):
    ///
    ///   * rendered  → mark the PID AX-verified (skip this check forever after)
    ///                 and return `Ok`.
    ///   * vanished  → promote the PID to clipboard-only and paste via Cmd+V
    ///                 instead, so this and every future utterance render.
    ///
    /// PIDs already AX-verified short-circuit with zero extra AX calls, so the
    /// read-back cost is paid at most once per kind of field per app. A field
    /// whose value cannot be read at all reads as "vanished" and is downgraded
    /// to clipboard paste.
    unsafe fn confirm_or_fallback(
        focused: AXUIElementRef,
        pid: Option<i32>,
        role_key: &str,
        before: Option<FieldSnapshot>,
        padded: &str,
        expected_pid: Option<i32>,
    ) -> eyre::Result<InjectOutcome> {
        let (pid, before) = match (pid, before) {
            (Some(p), Some(b)) => (p, b),
            // No PID to cache against, or already trusted — believe the success.
            _ => {
                super::remember_tail(padded);
                return Ok(InjectOutcome::Injected);
            }
        };

        if ax_write_landed(&before, &field_snapshot(focused), padded) {
            super::mark_ax_verified(pid, role_key);
            super::remember_tail(padded);
            return Ok(InjectOutcome::Injected);
        }

        eprintln!(
            "{}",
            super::hold_log_line(
                "AX write reported success but value didn't change; marking clipboard-only, falling back to Cmd+V (clipboard paste)",
                &super::app_label_for_pid_cheap(Some(pid)),
            )
        );
        super::mark_clipboard_only(pid);
        match crate::clipboard_paste::paste_via_clipboard(padded, expected_pid) {
            Ok(crate::clipboard_paste::PasteOutcome::Delivered) => {
                super::remember_tail(padded);
                Ok(InjectOutcome::Injected)
            }
            Ok(crate::clipboard_paste::PasteOutcome::RefusedForeground) => {
                eprintln!(
                    "{}",
                    super::hold_log_line(
                        &format!("expected app {expected_pid:?} not frontmost at Cmd+V — holding (not pasted)"),
                        &super::app_label_for_pid(expected_pid),
                    )
                );
                Ok(InjectOutcome::RefusedForeground)
            }
            Err(e) => Err(e),
        }
    }

    /// The field's value + selected range, for the before/after comparison in
    /// [`ax_write_landed`].
    unsafe fn field_snapshot(focused: AXUIElementRef) -> FieldSnapshot {
        FieldSnapshot {
            value: read_value(focused),
            selection: read_selected_range(focused),
        }
    }

    /// Key for the AX-verified cache: one app can hold a native field and a web
    /// view (Mail, Safari), so trust is per (pid, role/subrole), not per pid.
    unsafe fn verification_role_key(focused: AXUIElementRef) -> String {
        format!(
            "{}/{}",
            copy_string_attr(focused, kAXRoleAttribute).unwrap_or_default(),
            copy_string_attr(focused, kAXSubroleAttribute).unwrap_or_default()
        )
    }

    /// ±chars of focused-field text harvested around the caret. The text *near
    /// your cursor* is what surrounds the words you're about to dictate — the
    /// highest-signal, lowest-noise slice. Deliberately small so a focused
    /// document can't flood the cleanup prompt with unrelated proper nouns.
    const HARVEST_RADIUS: usize = 600;

    /// Read `kAXSelectedTextRange` as `(location, length)` in UTF-16 units, or
    /// `None` if the element doesn't expose it. Convert the location with
    /// [`char_index_from_utf16`] before indexing a Rust string by `char`.
    unsafe fn read_selected_range(focused: AXUIElementRef) -> Option<(usize, usize)> {
        let mut range_ref: CFTypeRef = ptr::null();
        let range_attr = CFString::new(kAXSelectedTextRangeAttribute);
        let err = AXUIElementCopyAttributeValue(
            focused,
            range_attr.as_concrete_TypeRef(),
            &mut range_ref,
        );
        if err != kAXErrorSuccess || range_ref.is_null() {
            return None;
        }
        let mut cf_range = CFRange {
            location: 0,
            length: 0,
        };
        let got = AXValueGetValue(
            range_ref as AXValueRef,
            kAXValueTypeCFRange,
            &mut cf_range as *mut _ as *mut c_void,
        );
        cf_release(range_ref);
        if got && cf_range.location >= 0 && cf_range.length >= 0 {
            Some((cf_range.location as usize, cf_range.length as usize))
        } else {
            None
        }
    }

    /// True when the element is a password field (never read, never written).
    unsafe fn element_is_secure(focused: AXUIElementRef) -> bool {
        is_secure_field(copy_string_attr(focused, kAXSubroleAttribute).as_deref())
    }

    /// Harvest the screen-context vocabulary source: a small window of the
    /// FOCUSED element's text centred on the caret. NOT the window title, NOT
    /// other panes/sidebars/terminals (those are separate AX elements we never
    /// touch). Best-effort — `None` when AX exposes no field text (e.g. Electron
    /// editors, empty fields), which correctly contributes nothing rather than
    /// clutter. Runs in the parallel focus-capture thread, so it's off the
    /// critical path.
    unsafe fn harvest_screen_text(focused: AXUIElementRef) -> Option<String> {
        if element_is_secure(focused) {
            return None;
        }
        let value = read_value(focused)?;
        if value.trim().is_empty() {
            return None;
        }
        // Anchor at the caret; default to end-of-field (dictation usually
        // appends). AX reports a UTF-16 offset; the window indexes chars.
        let caret = match read_selected_range(focused) {
            Some((loc, _)) => char_index_from_utf16(&value, loc),
            None => value.chars().count(),
        };
        let window = crate::screen_context::window_around(&value, caret, HARVEST_RADIUS);
        if window.trim().is_empty() {
            None
        } else {
            Some(window.to_string())
        }
    }

    /// Read `kAXValue` off an element as a `String`, or `None` if it doesn't
    /// expose a readable string value.
    unsafe fn read_value(focused: AXUIElementRef) -> Option<String> {
        let mut value_ref: CFTypeRef = ptr::null();
        let val_attr = CFString::new(kAXValueAttribute);
        let err =
            AXUIElementCopyAttributeValue(focused, val_attr.as_concrete_TypeRef(), &mut value_ref);
        if err != kAXErrorSuccess || value_ref.is_null() {
            return None;
        }
        Some(CFString::wrap_under_create_rule(value_ref as CFStringRef).to_string())
    }

    /// Get the PID of the process that owns the focused AXUIElement, or None
    /// if AX refuses to tell us.
    unsafe fn focused_pid_of(focused: AXUIElementRef) -> Option<i32> {
        let mut pid: libc::pid_t = 0;
        let err = AXUIElementGetPid(focused, &mut pid);
        if err == kAXErrorSuccess && pid > 0 {
            Some(pid)
        } else {
            None
        }
    }

    /// Read kAXValue + kAXSelectedTextRange, return (immediate_before,
    /// last_non_ws_before, char_after). All three are None when the element
    /// doesn't expose the attributes.
    unsafe fn read_caret_context(
        focused: AXUIElementRef,
    ) -> (Option<char>, Option<char>, Option<char>) {
        let ctx = read_caret_context_full(focused);
        (ctx.immediate_before, ctx.last_nws_before, ctx.char_after)
    }

    /// [`read_caret_context`] plus whether the value was readable at all (the
    /// only evidence that may mark a PID AX-blind). The UTF-16 caret offset is
    /// converted to a char index in the pure [`caret_context_from`].
    unsafe fn read_caret_context_full(focused: AXUIElementRef) -> CaretContext {
        if element_is_secure(focused) {
            // Bullets, not text: report "readable, nothing around the caret".
            return caret_context_from(Some(""), None);
        }
        let value = read_value(focused);
        let caret = match value {
            Some(_) => read_selected_range(focused).map(|(loc, _)| loc),
            None => None,
        };
        caret_context_from(value.as_deref(), caret)
    }

    extern "C" {
        fn CFRelease(cf: CFTypeRef);
    }
    fn cf_release(ptr: CFTypeRef) {
        if !ptr.is_null() {
            unsafe { CFRelease(ptr) }
        }
    }

    /// Owned focus target — releases the retained AXUIElement on drop.
    /// AXUIElement is a CFType, thread-safe to use after CFRetain.
    pub struct FocusTargetInner {
        focused: AXUIElementRef,
        pub pid: Option<i32>,
        pub immediate_before: Option<char>,
        pub last_nws_before: Option<char>,
        pub char_after: Option<char>,
        /// Whether caret context was read during capture. Menu snapshots skip
        /// it to keep AppKit's main thread responsive and read it only if the
        /// retained field is actually needed later on the recovery worker.
        pub context_captured: bool,
        /// Visible text near the focus (field value + window title), harvested
        /// at capture time for screen-context vocabulary. `None` in AX-blind apps.
        pub screen_text: Option<String>,
        /// The focus-editability classification, computed at capture time from
        /// the AX role/subrole/writability + the app type. The write ladder
        /// gates on this; `NotEditable` holds the transcript instead of pasting.
        pub editability: FocusEditability,
    }

    impl Drop for FocusTargetInner {
        fn drop(&mut self) {
            cf_release(self.focused as CFTypeRef);
        }
    }

    /// Build an owned focus target from an application AX root. `expected_pid`
    /// is retained when the focused child does not disclose its owner.
    unsafe fn capture_focus_target_from_root(
        root: AXUIElementRef,
        expected_pid: Option<i32>,
        capture_context: bool,
        messaging_timeout_secs: f32,
    ) -> eyre::Result<FocusTargetInner> {
        let mut focused_ref: CFTypeRef = ptr::null();
        let focused_attr = CFString::new(kAXFocusedUIElementAttribute);
        let err = AXUIElementCopyAttributeValue(
            root,
            focused_attr.as_concrete_TypeRef(),
            &mut focused_ref,
        );
        if err != kAXErrorSuccess || focused_ref.is_null() {
            // Same Chromium on-demand wake as the write ladder: without it a
            // Slack/Electron capture at key-press would retain no field, so the
            // write-time fallback could never rescue the utterance.
            focused_ref = wake_electron_ax_and_retry_focus(
                root,
                expected_pid,
                messaging_timeout_secs,
            );
        }
        if focused_ref.is_null() {
            return Err(eyre::eyre!(
                "no focused UI element (AXError {err}); click into a text field first"
            ));
        }
        let focused = focused_ref as AXUIElementRef;
        set_messaging_timeout(focused, messaging_timeout_secs);

        let focused_pid = expected_pid.or_else(|| focused_pid_of(focused));
        let (immediate_before, last_nws_before, char_after, screen_text) =
            if capture_context {
                let (immediate, last_nws, after) =
                    read_context_with_blind(focused, focused_pid);
                (immediate, last_nws, after, harvest_screen_text(focused))
            } else {
                (None, None, None, None)
            };
        let editability = focused_editability(focused, focused_pid);

        if std::env::var("INJECT_DIAG").is_ok() {
            let mut role_ref: CFTypeRef = ptr::null();
            let role_attr = CFString::new(kAXRoleAttribute);
            let r_err = AXUIElementCopyAttributeValue(
                focused,
                role_attr.as_concrete_TypeRef(),
                &mut role_ref,
            );
            let role = if r_err == kAXErrorSuccess && !role_ref.is_null() {
                CFString::wrap_under_create_rule(role_ref as CFStringRef).to_string()
            } else {
                format!("<unknown AXError {r_err}>")
            };
            eprintln!("[inject-diag] focus capture: pid={focused_pid:?} role={role} editability={editability:?}");
        }

        Ok(FocusTargetInner {
            focused,
            pid: focused_pid,
            immediate_before,
            last_nws_before,
            char_after,
            context_captured: capture_context,
            screen_text,
            editability,
        })
    }

    /// Capture through the frontmost application's own AX root. The old
    /// system-wide proxy intermittently returned no value even when the app
    /// root could resolve the focused field.
    pub fn capture_focus_target() -> eyre::Result<FocusTargetInner> {
        let pid = super::frontmost_pid()
            .ok_or_else(|| eyre::eyre!("no frontmost application"))?;
        capture_focus_target_for_pid_mode(pid, true, AX_MESSAGING_TIMEOUT_SECS)
    }

    /// Lightweight menu snapshot: retain the field and positive editability
    /// evidence, but defer caret context and screen text to the recovery worker.
    pub fn capture_focus_target_for_pid(pid: i32) -> eyre::Result<FocusTargetInner> {
        capture_focus_target_for_pid_mode(pid, false, MENU_AX_MESSAGING_TIMEOUT_SECS)
    }

    fn capture_focus_target_for_pid_mode(
        pid: i32,
        capture_context: bool,
        messaging_timeout_secs: f32,
    ) -> eyre::Result<FocusTargetInner> {
        if !ensure_trusted_or_prompt() {
            return Err(eyre::eyre!("Accessibility permission not granted"));
        }
        unsafe {
            let app = AXUIElementCreateApplication(pid);
            if app.is_null() {
                return Err(eyre::eyre!(
                    "AXUIElementCreateApplication({pid}) returned null"
                ));
            }
            set_messaging_timeout(app, messaging_timeout_secs);
            let result = capture_focus_target_from_root(
                app,
                Some(pid),
                capture_context,
                messaging_timeout_secs,
            );
            cf_release(app as CFTypeRef);
            result
        }
    }

}

/// Pre-captured focus target — owned, Send across threads. Created at
/// key-release time and used at inject time after Cohere+S1-mini finish.
/// Holds a retained AXUIElement and the surrounding caret context.
///
/// Why this exists: `get_focused_element` is the dominant cost in the
/// inject path (89% in TextEdit, even more in Electron where AX hops
/// through multiple processes). Capturing it in parallel with the
/// inference pipeline removes it from the critical path.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub struct FocusTarget {
    inner: imp::FocusTargetInner,
}

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
unsafe impl Send for FocusTarget {}

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
impl FocusTarget {
    /// Capture the currently-focused UI element + its caret context.
    /// Safe to call from any thread.
    pub fn capture() -> eyre::Result<Self> {
        Ok(Self {
            inner: imp::capture_focus_target()?,
        })
    }

    /// Capture from one explicit application's AX root. Used by status-menu
    /// recovery before menu tracking can make the field disappear.
    pub fn capture_for_pid(pid: i32) -> eyre::Result<Self> {
        Ok(Self {
            inner: imp::capture_focus_target_for_pid(pid)?,
        })
    }

    /// PID of the process that owns the captured focused element, if AX
    /// disclosed it. Used by the daemon for log output.
    pub fn inner_pid(&self) -> Option<i32> {
        self.inner.pid
    }

    /// Up to `cap` proper-noun / domain-term candidates harvested from the
    /// visible text near the focus (field value + window title), for the
    /// screen-context vocabulary fed to cleanup. Empty in AX-blind apps.
    pub fn screen_terms(&self, cap: usize) -> Vec<String> {
        match &self.inner.screen_text {
            Some(t) => crate::screen_context::extract_terms(t, cap),
            None => Vec::new(),
        }
    }

    /// The raw harvested screen text, for the `context-probe` diagnostic.
    pub fn screen_text(&self) -> Option<&str> {
        self.inner.screen_text.as_deref()
    }

    /// The focus-editability classification computed at capture time. The
    /// daemon reads this to decide whether to inject at all (a `NotEditable`
    /// focus holds the transcript instead of writing anywhere).
    pub fn editability(&self) -> FocusEditability {
        self.inner.editability
    }
}

#[cfg(not(all(target_os = "macos", feature = "ax-inject")))]
pub struct FocusTarget;

#[cfg(not(all(target_os = "macos", feature = "ax-inject")))]
impl FocusTarget {
    pub fn capture() -> eyre::Result<Self> {
        Ok(Self)
    }
    pub fn capture_for_pid(_pid: i32) -> eyre::Result<Self> {
        Ok(Self)
    }
    pub fn inner_pid(&self) -> Option<i32> {
        None
    }
    pub fn editability(&self) -> FocusEditability {
        FocusEditability::NotEditable
    }
}

/// One write. `pid` names the app explicitly (a menu recovery paste); `None`
/// writes into the app that is OS-frontmost at write time. `target` is the
/// field captured at key-press, reused only when the fresh lookup finds no
/// element and `retained_focus_can_fallback` allows it. `authorize` is polled
/// once, immediately before the first write or clipboard side effect; `None`
/// (the CLI entry points) is always authorized.
#[derive(Default)]
pub struct InjectRequest<'a> {
    pub pid: Option<i32>,
    pub target: Option<FocusTarget>,
    pub authorize: Option<&'a dyn Fn() -> bool>,
}

impl AccessibilityInjector {
    /// Write `text` at the caret. Empty text is a successful no-op.
    #[cfg(all(target_os = "macos", feature = "ax-inject"))]
    pub fn inject(request: InjectRequest<'_>, text: &str) -> eyre::Result<InjectOutcome> {
        if text.trim().is_empty() {
            return Ok(InjectOutcome::Injected);
        }
        imp::inject(request.pid, request.target.map(|t| t.inner), text, request.authorize)
    }

    #[cfg(not(all(target_os = "macos", feature = "ax-inject")))]
    pub fn inject(request: InjectRequest<'_>, text: &str) -> eyre::Result<InjectOutcome> {
        if request.authorize.is_some_and(|authorize| !authorize()) {
            return Ok(InjectOutcome::Cancelled);
        }
        let padded = smart_pad(text, None, None, None);
        if padded.is_empty() {
            return Ok(InjectOutcome::Injected);
        }
        match request.pid {
            Some(pid) => println!("[INJECT-FALLBACK pid {pid}] {padded}"),
            None => println!("[INJECT-FALLBACK] {padded}"),
        }
        Ok(InjectOutcome::Injected)
    }

    #[cfg(all(target_os = "macos", feature = "ax-inject"))]
    pub fn check_permission() -> bool {
        imp::ensure_trusted_or_prompt()
    }

    #[cfg(not(all(target_os = "macos", feature = "ax-inject")))]
    pub fn check_permission() -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn hold_log_line_names_the_app() {
        let l = hold_log_line("no focused AX element (AXError -25212); holding", "com.apple.Safari");
        assert!(l.starts_with("[inject] "));
        assert!(l.ends_with("app=com.apple.Safari"));
        assert!(l.contains("AXError -25212"));
    }

    #[test]
    fn hold_log_line_handles_missing_app() {
        assert!(hold_log_line("holding", "").contains("app=unknown"));
        assert!(hold_log_line("holding", "  ").ends_with("app=unknown"));
    }

    #[test]
    fn app_label_prefers_bundle_then_exe_then_unknown() {
        assert_eq!(app_label(Some("com.x.App"), Some("/A/B/App")), "com.x.App");
        assert_eq!(app_label(None, Some("/Applications/Foo.app/Contents/MacOS/Foo\n")), "Foo");
        assert_eq!(app_label(Some(" "), Some("/a/b/Bar")), "Bar");
        assert_eq!(app_label(None, None), "unknown");
        assert_eq!(app_label(None, Some("")), "unknown");
    }

    #[test]
    fn every_hold_and_fallback_log_line_names_the_app() {
        const MARKERS: [&str; 5] =
            ["holding", "text kept", "transcript kept", "result kept", "clipboard paste"];
        let files = [
            ("injector.rs", include_str!("injector.rs")),
            ("daemon/listen.rs", include_str!("daemon/listen.rs")),
            ("daemon/worker.rs", include_str!("daemon/worker.rs")),
        ];
        let mut bad = Vec::new();
        for (name, src) in files {
            // Production code only: stop at the test module.
            let prod = src.split("\n#[cfg(test)]").next().unwrap_or(src);
            let lines: Vec<&str> = prod.lines().collect();
            for (n, line) in lines.iter().enumerate() {
                let t = line.trim_start();
                if t.starts_with("//") || !line.contains('"') || line.contains("[limit]") || !MARKERS.iter().any(|m| line.contains(m)) {
                    continue;
                }
                // Statement window: this line and the next 6 (multi-line eprintln!/format!).
                let from = n.saturating_sub(3);
                let win = lines[from..(n + 7).min(lines.len())].join("\n");
                if !(win.contains("hold_log_line") || win.contains("app=")) {
                    bad.push(format!("{name}:{}", n + 1));
                }
            }
        }
        assert!(bad.is_empty(), "hold/fallback log lines without an app: {bad:?}");
    }


    fn app(pid: i32, bundle: &str) -> TargetApp {
        TargetApp { pid, bundle_id: Some(bundle.to_string()) }
    }

    #[test]
    fn the_press_time_app_is_used_while_it_is_still_in_front() {
        let got = resolve_target_app(Some(app(10, "com.mitchellh.ghostty")), Some(10), |_| {
            panic!("no second lookup when the app did not change")
        });
        assert_eq!(got, Some(app(10, "com.mitchellh.ghostty")));
    }

    #[test]
    fn an_app_switch_during_the_hold_uses_the_app_the_text_goes_to() {
        // Regression (INJ:TGT-01): the old app's raw/cleanup choice was applied
        // to the new app, while the tone was read from yet another moment.
        let got = resolve_target_app(Some(app(10, "com.mitchellh.ghostty")), Some(22), |pid| {
            assert_eq!(pid, 22);
            Some("com.apple.mail".to_string())
        });
        assert_eq!(got, Some(app(22, "com.apple.mail")));
    }

    #[test]
    fn a_failed_press_capture_still_identifies_the_app() {
        // Regression: a terminal with no focused AX element had no target PID,
        // so LLM-cleaned prose was pasted into the shell.
        let got = resolve_target_app(None, Some(31), |_| Some("com.googlecode.iterm2".into()));
        assert_eq!(got, Some(app(31, "com.googlecode.iterm2")));
        // Nothing readable at all: no target, and no invented one.
        assert_eq!(resolve_target_app(None, None, |_| None), None);
        // Frontmost unreadable at release: keep what the press saw.
        assert_eq!(resolve_target_app(Some(app(5, "a.b")), None, |_| None), Some(app(5, "a.b")));
    }

    use super::*;

    #[test]
    fn password_fields_are_never_a_target() {
        // Regression: role AXTextField + any subrole used to be Editable.
        for paste_safe in [false, true] {
            assert_eq!(
                classify_focus_editability(
                    Some("AXTextField"),
                    Some("AXSecureTextField"),
                    true,
                    true,
                    paste_safe
                ),
                FocusEditability::NotEditable
            );
        }
        assert!(is_secure_field(Some("AXSecureTextField")));
        assert!(!is_secure_field(Some("AXSearchField")));
        assert!(!is_secure_field(None));
        assert_eq!(
            decide_inject(false, Some(FocusEditability::NotEditable)),
            InjectDecision::Hold { reason: HoldReason::NoEditableField }
        );
    }

    #[test]
    fn utf16_offsets_convert_to_char_indices() {
        assert_eq!(char_index_from_utf16("hello", 0), 0);
        assert_eq!(char_index_from_utf16("hello", 3), 3);
        assert_eq!(char_index_from_utf16("hello", 99), 5);
        // CJK is BMP: one unit per char.
        assert_eq!(char_index_from_utf16("你好世界", 2), 2);
        // Each emoji is two UTF-16 units but one char.
        let s = "😀😀😀😀😀 Beatrix";
        assert_eq!(char_index_from_utf16(s, 10), 5);
        assert_eq!(char_index_from_utf16(s, 11), 6);
        // Inside a surrogate pair rounds down to the emoji itself.
        assert_eq!(char_index_from_utf16(s, 1), 0);
        assert_eq!(char_index_from_utf16("", 4), 0);
    }

    #[test]
    fn caret_context_after_an_emoji_reads_the_right_neighbours() {
        // "ok 👍|done": UTF-16 caret 5, char caret 4. The old code indexed
        // chars with the UTF-16 offset and read 'd' as the char before.
        let ctx = caret_context_from(Some("ok 👍done"), Some(5));
        assert!(ctx.value_readable);
        assert_eq!(ctx.immediate_before, Some('👍'));
        assert_eq!(ctx.char_after, Some('d'));
        assert_eq!(ctx.last_nws_before, Some('👍'));
    }

    #[test]
    fn empty_field_and_caret_at_start_are_readable_not_blind() {
        // Regression: both used to mark the app AX-blind (no char before the
        // caret), which switched caret context off for that app.
        let empty = caret_context_from(Some(""), Some(0));
        assert!(empty.value_readable);
        assert_eq!((empty.immediate_before, empty.last_nws_before, empty.char_after), (None, None, None));
        let at_start = caret_context_from(Some("Hello"), Some(0));
        assert!(at_start.value_readable);
        assert_eq!(at_start.immediate_before, None);
        assert_eq!(at_start.char_after, Some('H'));
        // Only an unreadable value is blind.
        assert!(!caret_context_from(None, None).value_readable);
        assert!(!caret_context_from(None, Some(3)).value_readable);
    }

    #[test]
    fn caret_context_defaults_to_end_of_field_without_a_range() {
        let ctx = caret_context_from(Some("Hello. "), None);
        assert_eq!(ctx.immediate_before, Some(' '));
        assert_eq!(ctx.last_nws_before, Some('.'));
        assert_eq!(ctx.char_after, None);
    }

    fn snap(value: Option<&str>, selection: Option<(usize, usize)>) -> FieldSnapshot {
        FieldSnapshot { value: value.map(str::to_string), selection }
    }

    #[test]
    fn write_verification_compares_before_and_after() {
        // Landed: the value changed.
        assert!(ax_write_landed(&snap(Some("Hi. "), Some((4, 0))), &snap(Some("Hi. Yes."), Some((8, 0))), "Yes."));
        // Regression (false pass): the field already held the same words and
        // the write vanished. `contains` said rendered; nothing changed.
        assert!(!ax_write_landed(&snap(Some("Yes."), Some((4, 0))), &snap(Some("Yes."), Some((4, 0))), "Yes."));
        // Regression (double insert): the app normalised the text, so the
        // needle is absent, but the write did land.
        assert!(ax_write_landed(
            &snap(Some(""), Some((0, 0))),
            &snap(Some("line one line two"), Some((17, 0))),
            "line one\nline two"
        ));
        // A selection replaced by identical text: value equal, caret moved.
        assert!(ax_write_landed(&snap(Some("Yes."), Some((0, 4))), &snap(Some("Yes."), Some((4, 0))), "Yes."));
        // Unreadable after the write: treat as vanished (accept-and-drop apps).
        assert!(!ax_write_landed(&snap(Some("x"), None), &snap(None, None), "Yes."));
        // No "before": fall back to the needle search.
        assert!(ax_write_landed(&snap(None, None), &snap(Some("so Yes. ok"), None), " Yes. "));
        assert!(!ax_write_landed(&snap(None, None), &snap(Some("nothing"), None), "Yes."));
        // Nothing to write is trivially fine.
        assert!(ax_write_landed(&snap(None, None), &snap(None, None), "  "));
    }

    #[test]
    fn editable_role_with_writable_value_reads_editable() {
        assert_eq!(
            classify_focus_editability(Some("AXTextField"), None, true, false, false),
            FocusEditability::Editable
        );
        assert_eq!(
            classify_focus_editability(Some("AXTextArea"), None, false, true, false),
            FocusEditability::Editable
        );
        // A search field via subrole counts as text-entry.
        assert_eq!(
            classify_focus_editability(None, Some("AXSearchField"), true, false, false),
            FocusEditability::Editable
        );
    }

    #[test]
    fn editable_role_but_not_writable_is_not_editable() {
        // A role that looks like a field but has no writable value is not a
        // place to write — never treat a label/static as an input.
        assert_eq!(
            classify_focus_editability(Some("AXTextField"), None, false, false, false),
            FocusEditability::NotEditable
        );
        // A static label is never editable, even though AX discloses it.
        assert_eq!(
            classify_focus_editability(Some("AXStaticText"), None, true, true, false),
            FocusEditability::NotEditable
        );
    }

    #[test]
    fn terminal_without_a_noninput_role_is_paste_safe() {
        // A terminal whose focused element exposes no clearly non-input role
        // (here the ambiguous AXGroup / no role) is a legitimate text-input
        // surface. This is the narrow, justified fallback.
        assert_eq!(
            classify_focus_editability(Some("AXGroup"), None, false, false, true),
            FocusEditability::PasteSafe
        );
        assert_eq!(
            classify_focus_editability(None, None, false, false, true),
            FocusEditability::PasteSafe
        );
    }

    // REGRESSION (2026-09-06, Ghostty): a GPU/native terminal emulator exposes
    // its shell grid to Accessibility as a TEXT-ENTRY role (AXTextArea) whose
    // AXValue / AXSelectedText are NOT settable — the terminal draws its own
    // text grid and is AX-blind (writes must go through Cmd+V). This is NOT a
    // read-only field; it is a legitimate text-input surface. The previous
    // classifier read "text-entry role but not AX-writable" as "read-only
    // field" and held the transcript ("No text field selected"), which broke
    // dictation into the terminal that used to work. A real terminal is always
    // a text-input surface (see `classify_pid`), so a text-entry role there is
    // `PasteSafe` (clipboard), never a hold.
    #[test]
    fn terminal_ax_textarea_nonwritable_is_paste_safe_not_readonly() {
        for role in [Some("AXTextArea"), Some("AXTextField"), Some("AXComboBox")] {
            // value and selected-text both non-settable (the AX-blind grid).
            assert_eq!(
                classify_focus_editability(role, None, false, false, true),
                FocusEditability::PasteSafe,
                "terminal role {role:?} with non-settable value must be PasteSafe, not NotEditable"
            );
        }
        // Search-field subrole variant.
        assert_eq!(
            classify_focus_editability(None, Some("AXSearchField"), false, false, true),
            FocusEditability::PasteSafe
        );
    }

    // Reviewer: "ANY positively noneditable role denies even allowlisted". The
    // app (terminal) is allowlisted, but the FOCUS role is not input.
    #[test]
    fn allowlisted_terminal_with_noninput_role_is_denied() {
        // A button focused in a terminal is still a button — not input.
        assert_eq!(
            classify_focus_editability(Some("AXButton"), None, false, false, true),
            FocusEditability::NotEditable
        );
        // A text-entry role in a terminal that is not AX-writable is still the
        // terminal's shell grid (AX-blind, written via Cmd+V), not a read-only
        // label — so it is PasteSafe, not denied. See the regression test
        // `terminal_ax_textarea_nonwritable_is_paste_safe_not_readonly`.
        assert_eq!(
            classify_focus_editability(Some("AXTextField"), None, false, false, true),
            FocusEditability::PasteSafe
        );
        // Static/read-only text, a window chrome, a web page body, a canvas,
        // a menu — all deny even for the allowlisted terminal.
        assert_eq!(
            classify_focus_editability(Some("AXStaticText"), None, false, false, true),
            FocusEditability::NotEditable
        );
        assert_eq!(
            classify_focus_editability(Some("AXWindow"), None, false, false, true),
            FocusEditability::NotEditable
        );
        assert_eq!(
            classify_focus_editability(Some("AXWebArea"), None, false, false, true),
            FocusEditability::NotEditable
        );
        assert_eq!(
            classify_focus_editability(Some("AXCanvas"), None, false, false, true),
            FocusEditability::NotEditable
        );
    }

    // Reviewer: "not all editor windows" — the app-level editor/compose
    // blanket is gone. A black-holed editor surface is NOT PasteSafe.
    #[test]
    fn app_level_editor_blanket_is_removed() {
        // An editor/compose app with a black-holed role but that is NOT a
        // terminal is NotEditable — pasting must be justified by FOCUS role,
        // not by app name. (An editable editor pane exposes an AXTextArea role
        // at write time and reaches Editable that way.)
        assert_eq!(
            classify_focus_editability(None, None, false, false, false),
            FocusEditability::NotEditable
        );
    }

    #[test]
    fn unknown_focus_is_not_editable() {
        // No positive evidence, not a terminal → NotEditable. Never guess.
        assert_eq!(
            classify_focus_editability(Some("AXWebArea"), None, false, false, false),
            FocusEditability::NotEditable
        );
        assert_eq!(
            classify_focus_editability(None, None, false, false, false),
            FocusEditability::NotEditable
        );
    }

    #[test]
    fn wechat_main_app_is_ax_blind_chat_app() {
        // WeChat's main chat process is AX-blind (no focused element), so it is
        // paste-safe. Match the main app only, not the Chromium mini-program
        // engine or other apps.
        assert!(is_ax_blind_chat_app_comm(
            "/applications/wechat.app/contents/macos/wechat"
        ));
        assert!(is_ax_blind_chat_app_comm(
            "/Applications/WeChat.app/Contents/MacOS/WeChat\n"
        ));
        assert!(!is_ax_blind_chat_app_comm(
            "/applications/wechat.app/contents/macos/wechatappex.app/contents/macos/wechatappex"
        ));
        assert!(!is_ax_blind_chat_app_comm(
            "/applications/wechat.app/contents/frameworks/crashpad_handler"
        ));
        assert!(!is_ax_blind_chat_app_comm(
            "/applications/safari.app/contents/macos/safari"
        ));
        assert!(!is_ax_blind_chat_app_comm(""));
    }

    #[test]
    fn helium_gets_the_ax_wake_but_keeps_the_ax_route() {
        // `ps -o comm=` for the running Helium main process.
        assert!(is_on_demand_ax_browser_comm(
            "/Applications/Helium.app/Contents/MacOS/Helium\n"
        ));
        // Its helpers are separate processes, never the frontmost app.
        assert!(!is_on_demand_ax_browser_comm(
            "/applications/helium.app/contents/frameworks/helium framework.framework/helpers/helium helper (renderer).app/contents/macos/helium helper (renderer)"
        ));
        assert!(!is_on_demand_ax_browser_comm(
            "/applications/safari.app/contents/macos/safari"
        ));
        assert!(!is_on_demand_ax_browser_comm(""));
    }

    // ORCHESTRATION (2026-09-23): a Chromium/Electron app (Slack was the
    // reproduced case) answers AXFocusedUIElement with -25212 until its
    // accessibility tree is requested. The wake is claimed at most once per
    // app process; a second claim must be refused so a later miss can't spin
    // the retry loop again.
    #[cfg(all(target_os = "macos", feature = "ax-inject"))]
    #[test]
    fn electron_ax_wake_is_claimed_once_per_pid() {
        let pid = 990_001;
        assert!(claim_electron_ax_wake(pid), "first claim wins");
        assert!(!claim_electron_ax_wake(pid), "second claim is refused");
        assert!(claim_electron_ax_wake(990_002), "a different app may claim");
    }

    // ── decide_inject / finalize_utterance: observable-sink behaviour ──────

    #[test]
    fn cancel_holds_even_when_editable() {
        assert_eq!(
            decide_inject(true, Some(FocusEditability::Editable)),
            InjectDecision::Hold {
                reason: HoldReason::Cancelled
            }
        );
    }

    #[test]
    fn editable_and_paste_safe_inject() {
        assert_eq!(
            decide_inject(false, Some(FocusEditability::Editable)),
            InjectDecision::Inject
        );
        assert_eq!(
            decide_inject(false, Some(FocusEditability::PasteSafe)),
            InjectDecision::Inject
        );
    }

    #[test]
    fn unknown_focus_does_not_hold_without_writing() {
        // Reviewer: a failed initial capture (editability unknown = None) must
        // reach the fresh resolver at write time, NOT hold as if NotEditable.
        assert_eq!(decide_inject(false, None), InjectDecision::Inject);
    }

    #[test]
    fn not_editable_holds_and_never_writes() {
        // ONLY a KNOWN NotEditable holds without writing. Sink never called.
        let mut sink_called = false;
        let result = finalize_utterance(false, Some(FocusEditability::NotEditable), "hello", |_| {
            sink_called = true;
            Ok(InjectOutcome::Injected)
        });
        assert_eq!(result, FinalizeResult::Held);
        assert!(!sink_called, "a known NotEditable focus must never reach the injector");
    }

    // ORCHESTRATION (2026-09-07): a global Cmd+V lands in the OS-frontmost app,
    // so it must not be posted unless the intended target IS frontmost. This is
    // the fail-closed seam consulted immediately before the Cmd+V. A naive
    // "always deliver" guard (or one that falls back to the current frontmost)
    // would make this test go red.
    #[test]
    fn clipboard_delivery_fail_closed_when_expected_not_frontmost() {
        // Explicit target PID: only deliver when it is the frontmost app.
        assert!(clipboard_target_is_frontmost(Some(100), Some(100)));
        assert!(!clipboard_target_is_frontmost(Some(100), Some(200)));
        assert!(!clipboard_target_is_frontmost(Some(100), None));
        // No specific target (systemwide): require SOME frontmost, never a paste
        // with no foreground app, and never guess a foreign app.
        assert!(clipboard_target_is_frontmost(None, Some(200)));
        assert!(!clipboard_target_is_frontmost(None, None));
    }

    // ORCHESTRATION (2026-09-07): the live (systemwide) resolver must target the
    // CURRENT OS-frontmost app resolved FRESH at write time — never the retained
    // press-time PID, and never the systemwide proxy that returned -25204 in the
    // reproduced red loop. A naive "use the retained target" or "fall back to
    // the press-time PID" resolver makes this go red.
    #[test]
    fn live_plan_targets_current_frontmost_and_holds_with_none() {
        assert_eq!(plan_live_inject(Some(555)), LiveInjectPlan::ResolveApp { pid: 555 });
        // No frontmost app → no live field → hold (never invent a target).
        assert_eq!(plan_live_inject(None), LiveInjectPlan::Hold);
    }

    #[test]
    fn missing_fresh_focus_reuses_only_same_frontmost_editable_capture() {
        // Exact logged failure: press/menu capture found an editable field, but
        // the write-time AX query returned -25212. If the same app is still
        // frontmost, the retained positive evidence is the safe fallback.
        assert!(retained_focus_can_fallback(
            Some(32975),
            Some(32975),
            FocusEditability::Editable,
            true,
        ));
        assert!(retained_focus_can_fallback(
            Some(11476),
            Some(11476),
            FocusEditability::PasteSafe,
            true,
        ));

        // Never revive a field after an app switch, with an unknown PID, or
        // when the captured element was positively non-editable.
        assert!(!retained_focus_can_fallback(
            Some(32975),
            Some(4659),
            FocusEditability::Editable,
            true,
        ));
        assert!(!retained_focus_can_fallback(
            None,
            Some(32975),
            FocusEditability::Editable,
            true,
        ));
        assert!(!retained_focus_can_fallback(
            Some(32975),
            Some(32975),
            FocusEditability::NotEditable,
            true,
        ));
        assert!(!retained_focus_can_fallback(
            Some(32975),
            Some(32975),
            FocusEditability::Editable,
            false,
        ));
    }

    #[test]
    fn refused_foreground_holds_recoverable_not_failed() {
        // A fail-closed foreground refusal at Cmd+V delivery is a HOLD (the
        // transcript stays recoverable), never a hard failure.
        let result = finalize_utterance(false, None, "dictated", |_| {
            Ok(InjectOutcome::RefusedForeground)
        });
        assert_eq!(result, FinalizeResult::Held);
    }

    #[test]
    fn failed_capture_then_current_editable_injects() {
        // Reviewer: "failed initial capture then current editable". captured
        // target is None → editability None → the sink (fresh resolver) runs
        // and, seeing the CURRENT field editable, injects.
        let mut injected = Vec::new();
        let result = finalize_utterance(false, None, "dictated", |t| {
            injected.push(t.to_string());
            Ok(InjectOutcome::Injected)
        });
        assert_eq!(result, FinalizeResult::Injected);
        assert_eq!(injected, vec!["dictated"]);
    }

    #[test]
    fn failed_capture_then_current_not_editable_holds() {
        // The fresh resolver at write time finds no editable field → Held.
        let result = finalize_utterance(false, None, "dictated", |_| {
            Ok(InjectOutcome::NoEditableField)
        });
        assert_eq!(result, FinalizeResult::Held);
    }

    #[test]
    fn click_out_still_holds_but_click_back_injects() {
        // Click-out (same app, focus moved to a non-input role): the injector's
        // fresh gate reports NoEditableField → Held, sink still called (gate
        // decided) but no write happened / nothing pasted.
        let mut writes = 0usize;
        let out = finalize_utterance(false, None, "x", |_| {
            writes += 1;
            Ok(InjectOutcome::NoEditableField)
        });
        assert_eq!(out, FinalizeResult::Held);
        // Click back into the field: next utterance injects.
        let in_ = finalize_utterance(false, None, "y", |_| {
            Ok(InjectOutcome::Injected)
        });
        assert_eq!(in_, FinalizeResult::Injected);
    }

    #[test]
    fn cancel_never_reaches_the_injector() {
        let mut sink_called = false;
        let result = finalize_utterance(true, Some(FocusEditability::Editable), "hello", |_| {
            sink_called = true;
            Ok(InjectOutcome::Injected)
        });
        assert_eq!(result, FinalizeResult::Cancelled);
        assert!(!sink_called, "a cancelled utterance must never be injected");
    }

    #[test]
    fn fresh_dictation_after_cancel_injects() {
        // The headlining user-visible guarantee: cancel one utterance, then a
        // fresh dictation goes straight through to the injector.
        let mut injected = Vec::new();
        let result = finalize_utterance(true, Some(FocusEditability::Editable), "cancelled", |_| {
            Ok(InjectOutcome::Injected)
        });
        assert_eq!(result, FinalizeResult::Cancelled);
        assert!(injected.is_empty());

        let result = finalize_utterance(false, Some(FocusEditability::Editable), "fresh", |t| {
            injected.push(t.to_string());
            Ok(InjectOutcome::Injected)
        });
        assert_eq!(result, FinalizeResult::Injected);
        assert_eq!(injected, vec!["fresh"]);
    }

    #[test]
    fn injector_discovering_no_field_reports_held() {
        // Even with apparent positive evidence, the injector's fresh gate may
        // find black-holed focus at write time → Held.
        let result = finalize_utterance(false, Some(FocusEditability::Editable), "x", |_| {
            Ok(InjectOutcome::NoEditableField)
        });
        assert_eq!(result, FinalizeResult::Held);
    }

    #[test]
    fn hard_write_failure_reports_failed() {
        let result = finalize_utterance(false, Some(FocusEditability::Editable), "x", |_| {
            Err(eyre::eyre!("AX refused"))
        });
        assert_eq!(result, FinalizeResult::Failed);
    }

    // ── write-authorizer seam (the commit moved to just before the write) ──

    #[test]
    fn authorizer_none_is_always_authorized() {
        assert_eq!(claim_write_authorization(None), None);
    }

    #[test]
    fn authorizer_true_proceeds_false_cancels() {
        assert_eq!(claim_write_authorization(Some(&|| true)), None);
        assert_eq!(
            claim_write_authorization(Some(&|| false)),
            Some(InjectOutcome::Cancelled)
        );
    }

    #[test]
    fn cancel_during_lookup_then_resume_leaves_sink_untouched() {
        // Mirrors the worker: the injector resolves focus/context/padding (a
        // possibly-slow, up to 1.5s AX lookup), THEN claims write authorization
        // via gate.commit(gen) immediately before the first side effect. A
        // cancel that landed during that lookup (before the claim) must map to
        // Cancelled with zero writes — the "prevent late insertion" guarantee.
        let gate = crate::cancel::CancelGate::new();
        let gen = gate.begin();

        // The cancel arrives while the (simulated) AX lookup is blocked.
        assert!(gate.request_cancel());

        let mut writes = 0usize;
        let result = finalize_utterance(false, None, "dictated", |_| {
            // ...lookup resumes here; claim authorization at the last moment.
            if !gate.commit(gen) {
                return Ok(InjectOutcome::Cancelled);
            }
            writes += 1;
            Ok(InjectOutcome::Injected)
        });
        assert_eq!(result, FinalizeResult::Cancelled);
        assert_eq!(writes, 0, "a cancelled utterance must never reach the write");
    }

    #[test]
    fn uncancelled_authorization_writes_exactly_once() {
        let gate = crate::cancel::CancelGate::new();
        let gen = gate.begin();
        let mut writes = 0usize;
        let result = finalize_utterance(false, None, "dictated", |_| {
            if !gate.commit(gen) {
                return Ok(InjectOutcome::Cancelled);
            }
            writes += 1;
            Ok(InjectOutcome::Injected)
        });
        assert_eq!(result, FinalizeResult::Injected);
        assert_eq!(writes, 1);
        // A second claim for the same generation is refused (exactly-once).
        assert!(!gate.commit(gen));
    }

    #[test]
    fn no_editable_hold_never_claims_authorization() {
        // A hold (known NotEditable) is decided before the write ladder, so the
        // authorizer is never polled and nothing is written.
        let mut authorized = 0usize;
        let result =
            finalize_utterance(false, Some(FocusEditability::NotEditable), "x", |_| {
                authorized += 1;
                Ok(InjectOutcome::Injected)
            });
        assert_eq!(result, FinalizeResult::Held);
        assert_eq!(authorized, 0);
    }
}
