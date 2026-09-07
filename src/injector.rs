//! macOS text injection with smart spacing, capitalization, and clipboard
//! fallback.
//!
//! Strategy (in order):
//!   1. Locate the focused UI element — either via the system-wide
//!      AXUIElement (production hotkey path) or by PID (test path).
//!   2. Read its current value + caret position to discover what character
//!      sits immediately before and after the caret.
//!   3. Run `smart_pad::smart_pad` to decide on a leading space, trailing
//!      space, and whether to capitalize the first letter.
//!   4. Try `kAXSelectedTextAttribute` (insert at caret / replace selection),
//!      then `kAXValueAttribute` (whole-field replace) as a sub-fallback.
//!   5. If both AX writes refuse (Electron / some Java apps), fall back to
//!      save-clipboard → set-clipboard → synthesize Cmd+V → restore.

use crate::smart_pad::smart_pad;
// Only the real AX-injection `imp` module uses caret-context probing; gate the
// import to that build so the default (featureless) build stays warning-clean.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
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
//   * `PasteSafe` — the focused element belongs to a known terminal and is not
//     a positively non-input control. A terminal text grid may report a text
//     role without writable AX attributes; clipboard paste is its input path.
//     There is no editor/compose allowlist exception. Other apps require the
//     positive writable-text evidence above.
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

/// The pure decision: what to do with a finished transcript. This is the seam
/// the worker consults immediately before any write action.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InjectDecision {
    Inject,
    /// Don't write. Keep the transcript recoverable; show a clear state.
    Hold {
        #[allow(dead_code)] // used by tests + the worker's logging
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
/// AX says the value / selected-text attributes are writable. `is_terminal` is
/// the one app-type fallback: a genuine terminal is *always* a text-input
/// surface, and is the only surface allowed to paste when AX discloses no role.
///
/// This is deliberately strict and never guesses above the evidence:
///   * a positively non-input role (button / window / web area / canvas / static
///     text / menu / checkbox / radio) → `NotEditable` **even for a terminal**;
///   * a genuine terminal whose focused element is not a positively non-input
///     control → `PasteSafe` — even when its role is a text-entry role
///     (AXTextArea / AXTextField) that AX reports as **not** writable: a GPU
///     terminal draws its own text grid and is AX-blind, so that is a text-input
///     surface (clipboard paste), not a read-only label. This is the narrow,
///     justified fallback the architecture sanctions for terminals. It is NOT a
///     broad editor/compose blanket — a Slack pane, a Notion page, a browser
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
    is_terminal: bool,
) -> FocusEditability {
    // A positively non-input control (button / window chrome / page body /
    // canvas / static text / menu / checkbox / radio) is never a place to write,
    // even for an allowlisted terminal.
    if is_clearly_noninput_role(role, subrole) {
        FocusEditability::NotEditable
    } else if is_terminal {
        // A genuine terminal is ALWAYS a text-input surface. Its focused element
        // frequently reports a text-entry role (AXTextArea / AXTextField /
        // AXComboBox, or a search-field subrole) whose AXValue / AXSelectedText
        // are NOT settable — the terminal draws its own text grid and is
        // AX-blind, so writing must go through Cmd+V (see `classify_pid`). That
        // is NOT a read-only field; it is a text-input surface, so it is
        // `PasteSafe`, never `NotEditable`. Only a positively non-input control
        // above ever holds a terminal.
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
    // The editor/compose blanket (VS Code, Slack, Notion, Obsidian, Figma, …)
    // is deliberately ABSENT from a paste-safe allowlist. A paste is justified
    // only by the FOCUSED element's AX role at write time, never by the app
    // name — so a Slack pane or a browser body never receives a blind paste.
    let cls = PidClass { is_electron, is_terminal };
    if let Ok(mut cache) = CLIPBOARD_ONLY_CACHE.lock() {
        cache.push((pid, cls));
    }
    cls
}

pub fn is_clipboard_only_pid(pid: i32) -> bool {
    let cls = classify_pid(pid);
    cls.is_electron || cls.is_terminal
}

/// True when the focused app is a native terminal emulator. The daemon uses
/// this to inject the raw transcript without Gemma cleanup, and the focus gate
/// uses it (narrowly) as the one text-input-surface fallback when AX discloses
/// no editable role.
pub fn is_terminal_pid(pid: i32) -> bool {
    classify_pid(pid).is_terminal
}

/// PIDs whose AX write we've *verified* actually rendered (read the value
/// back and saw our text). Once a PID is here we trust its AX path and skip
/// the post-write verification read on every subsequent utterance.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
static AX_VERIFIED_PID: Mutex<Vec<i32>> = Mutex::new(Vec::new());

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
fn is_ax_verified(pid: i32) -> bool {
    AX_VERIFIED_PID
        .lock()
        .map(|g| g.contains(&pid))
        .unwrap_or(false)
}

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
fn mark_ax_verified(pid: i32) {
    if let Ok(mut g) = AX_VERIFIED_PID.lock() {
        if !g.contains(&pid) {
            g.push(pid);
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
            cache.push((pid, PidClass { is_electron: true, is_terminal: false }));
        }
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

/// Public: clear the cross-utterance state. Useful when the user
/// switches target apps and the previous tail no longer applies.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub fn reset_inject_state() {
    if let Ok(mut g) = LAST_TAIL.lock() {
        *g = None;
    }
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
        kAXErrorSuccess, kAXFocusedUIElementAttribute, kAXRoleAttribute, kAXSelectedTextAttribute,
        kAXSelectedTextRangeAttribute, kAXSubroleAttribute, kAXTrustedCheckOptionPrompt,
        kAXValueAttribute, kAXValueTypeCFRange, AXIsProcessTrustedWithOptions,
        AXUIElementCopyAttributeValue, AXUIElementCreateApplication, AXUIElementCreateSystemWide,
        AXUIElementGetPid, AXUIElementIsAttributeSettable, AXUIElementSetMessagingTimeout,
        AXUIElementRef, AXUIElementSetAttributeValue, AXValueGetValue, AXValueRef,
    };
    use core_foundation::base::{CFRange, CFTypeRef, TCFType};
    use core_foundation::boolean::CFBoolean;
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

    /// Best-effort: set the AX messaging timeout on an element so a slow or hung
    /// target app can't pin the daemon on an AX call. Ignores the AXError — if
    /// the OS declines the bound we just fall back to the unbounded default for
    /// that element.
    unsafe fn bound_messaging_timeout(el: AXUIElementRef) {
        let _ = AXUIElementSetMessagingTimeout(el, AX_MESSAGING_TIMEOUT_SECS);
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
    /// it a genuine terminal? This is the focus gate the write ladder consults
    /// — it is what turns "no blind paste" from a comment into a decision. The
    /// editor/compose app blanket is deliberately absent.
    unsafe fn focused_editability(focused: AXUIElementRef, pid: Option<i32>) -> FocusEditability {
        let role = copy_string_attr(focused, kAXRoleAttribute);
        let subrole = copy_string_attr(focused, kAXSubroleAttribute);
        let value_settable = attribute_is_settable(focused, kAXValueAttribute);
        let sel_settable = attribute_is_settable(focused, kAXSelectedTextAttribute);
        let is_terminal = pid.map(super::is_terminal_pid).unwrap_or(false);
        classify_focus_editability(
            role.as_deref(),
            subrole.as_deref(),
            value_settable,
            sel_settable,
            is_terminal,
        )
    }

    pub fn inject_systemwide(text: &str) -> eyre::Result<InjectOutcome> {
        inject_systemwide_with(text, None)
    }

    /// Cancellable systemwide injection: `authorize` is polled once, after
    /// focus/context/padding resolution and immediately before the first
    /// write/clipboard side effect. Refusal yields [`InjectOutcome::Cancelled`].
    pub fn inject_systemwide_with(
        text: &str,
        authorize: Option<&dyn Fn() -> bool>,
    ) -> eyre::Result<InjectOutcome> {
        if !ensure_trusted_or_prompt() {
            return Err(eyre::eyre!(
                "Accessibility permission not granted — System Settings → Privacy & Security → Accessibility"
            ));
        }
        // Live resolution: snapshot the OS-frontmost app and resolve ITS focused
        // element per-app (fresh at write time). We deliberately do NOT rely on
        // the systemwide proxy (AXUIElementCreateSystemWide → focused), which
        // can return kAXErrorCannotComplete (-25204) even when the frontmost
        // app's focused element resolves fine. The fail-closed Cmd+V guard still
        // protects the actual delivery, and the write-authorizer still holds the
        // cancellation gate (passed through to `inject_through`).
        match plan_live_inject(frontmost_pid()) {
            LiveInjectPlan::Hold => {
                eprintln!("[inject] no frontmost app at write time — holding (no live field)");
                return Ok(InjectOutcome::NoEditableField);
            }
            LiveInjectPlan::ResolveApp { pid } => unsafe {
                let app = AXUIElementCreateApplication(pid);
                if app.is_null() {
                    return Err(eyre::eyre!(
                        "AXUIElementCreateApplication({pid}) returned null"
                    ));
                }
                bound_messaging_timeout(app);
                let result = inject_through(app, text, authorize, Some(pid));
                cf_release(app as CFTypeRef);
                result
            },
        }
    }

    pub fn inject_into_pid(pid: pid_t, text: &str) -> eyre::Result<InjectOutcome> {
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
            bound_messaging_timeout(app);
            let result = inject_through(app, text, None, Some(pid));
            cf_release(app as CFTypeRef);
            result
        }
    }

    /// Resolve-now path: find the focused element → read context → smart-pad
    /// → hand off to the shared write ladder. Used by the systemwide + by-PID
    /// entry points. The pre-captured-target path (`inject_via_target`) shares
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
            // No focused field exposed. This is NOT a green light to paste
            // blindly: only a genuine terminal gets the narrow clipboard fallback.
            // Anything else — a browser page body, a Slack pane, an editor
            // window, a menu, an unknown app — holds the transcript. AX absence
            // is alone never proof of a text field.
            // The expected app for the Cmd+V guard: the explicit target for the
            // by-PID entry, or (systemwide) whatever is frontmost right now.
            // Never a foreign app we didn't intend — and never the frontmost
            // bolted in place of the explicit target.
            let expected_pid = expected_app.or_else(super::frontmost_pid);
            let front_paste_safe =
                expected_pid.map(super::is_terminal_pid).unwrap_or(false);
            if front_paste_safe {
                eprintln!(
                    "[inject] no focused AX element (AXError {err}); terminal → clipboard paste"
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
                            "[inject] target {expected_pid:?} not frontmost at Cmd+V — holding (not pasted)"
                        );
                        return Ok(InjectOutcome::RefusedForeground);
                    }
                    Err(e) => return Err(e),
                }
            }
            eprintln!(
                "[inject] no focused AX element (AXError {err}); not a known text-input app → holding (never paste blindly)"
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
                let ctx = read_caret_context(focused);
                if ctx.0.is_none() && ctx.1.is_none() {
                    super::mark_ax_blind(pid);
                }
                ctx
            }
            None => read_caret_context(focused),
        }
    }

    /// The single AX write ladder shared by both injection entry points:
    /// focus gate → `kAXSelectedText` → `kAXValue` → clipboard paste.
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
            eprintln!(
                "[inject] no editable field focused ({:?}) — holding transcript (never paste blindly)",
                editability
            );
            return Ok(InjectOutcome::NoEditableField);
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
                        "[inject] expected app {expected_pid:?} not frontmost at Cmd+V — holding (not pasted)"
                    );
                    return Ok(InjectOutcome::RefusedForeground);
                }
                Err(e) => return Err(e),
            }
        }

        let payload = CFString::new(padded);

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
            let r = confirm_or_fallback(focused, pid, padded, prof, expected_pid);
            if prof {
                eprintln!("[inject-prof] TOTAL: {:?}", t_total.elapsed());
            }
            return r;
        }

        let t = std::time::Instant::now();
        let val_attr = CFString::new(kAXValueAttribute);
        let val_err = AXUIElementSetAttributeValue(
            focused,
            val_attr.as_concrete_TypeRef(),
            payload.as_concrete_TypeRef() as CFTypeRef,
        );
        if prof {
            eprintln!("[inject-prof] set_value: {:?} (err={val_err})", t.elapsed());
        }
        if val_err == kAXErrorSuccess {
            let r = confirm_or_fallback(focused, pid, padded, prof, expected_pid);
            if prof {
                eprintln!("[inject-prof] TOTAL: {:?}", t_total.elapsed());
            }
            return r;
        }

        // Both AX writes refused — clipboard fallback. Only reachable when
        // editability was Editable; the app refused the write, so Cmd+V is the
        // remaining write path (and we already proved there is a field).
        eprintln!(
            "[inject] AX writes refused (selected={sel_err}, value={val_err}); using clipboard paste"
        );
        match crate::clipboard_paste::paste_via_clipboard(padded, expected_pid) {
            Ok(crate::clipboard_paste::PasteOutcome::Delivered) => {
                super::remember_tail(padded);
                Ok(InjectOutcome::Injected)
            }
            Ok(crate::clipboard_paste::PasteOutcome::RefusedForeground) => {
                eprintln!(
                    "[inject] expected app {expected_pid:?} not frontmost at Cmd+V — holding (not pasted)"
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
    /// by reading the value back and checking our text actually landed:
    ///
    ///   * rendered  → mark the PID AX-verified (skip this check forever after)
    ///                 and return `Ok`.
    ///   * vanished  → promote the PID to clipboard-only and paste via Cmd+V
    ///                 instead, so this and every future utterance render.
    ///
    /// PIDs already AX-verified short-circuit with zero extra AX calls, so the
    /// read-back cost is paid at most once per app. A genuinely-good field that
    /// doesn't expose a readable value reads as "vanished" and is downgraded to
    /// clipboard paste — slightly more clipboard churn, but still correct.
    unsafe fn confirm_or_fallback(
        focused: AXUIElementRef,
        pid: Option<i32>,
        padded: &str,
        prof: bool,
        expected_pid: Option<i32>,
    ) -> eyre::Result<InjectOutcome> {
        let pid = match pid {
            Some(p) if !super::is_ax_verified(p) => p,
            // No PID to cache against, or already trusted — believe the success.
            _ => {
                super::remember_tail(padded);
                return Ok(InjectOutcome::Injected);
            }
        };

        if ax_write_rendered(focused, padded) {
            super::mark_ax_verified(pid);
            super::remember_tail(padded);
            return Ok(InjectOutcome::Injected);
        }

        if prof {
            eprintln!(
                "[inject-prof] AX write reported success but value didn't change (pid {pid}); marking clipboard-only + falling back to Cmd+V"
            );
        }
        super::mark_clipboard_only(pid);
        match crate::clipboard_paste::paste_via_clipboard(padded, expected_pid) {
            Ok(crate::clipboard_paste::PasteOutcome::Delivered) => {
                super::remember_tail(padded);
                Ok(InjectOutcome::Injected)
            }
            Ok(crate::clipboard_paste::PasteOutcome::RefusedForeground) => {
                eprintln!(
                    "[inject] expected app {expected_pid:?} not frontmost at Cmd+V — holding (not pasted)"
                );
                Ok(InjectOutcome::RefusedForeground)
            }
            Err(e) => Err(e),
        }
    }

    /// True if the focused element's value now contains the text we just wrote.
    /// The needle is the trimmed payload (smart-pad's leading/trailing spaces
    /// don't survive into a terminal grid anyway). A missing/unreadable value
    /// counts as "did not render".
    unsafe fn ax_write_rendered(focused: AXUIElementRef, padded: &str) -> bool {
        let needle = padded.trim();
        if needle.is_empty() {
            return true;
        }
        match read_value(focused) {
            Some(value) => value.contains(needle),
            None => false,
        }
    }

    /// ±chars of focused-field text harvested around the caret. The text *near
    /// your cursor* is what surrounds the words you're about to dictate — the
    /// highest-signal, lowest-noise slice. Deliberately small so a focused
    /// document can't flood the cleanup prompt with unrelated proper nouns.
    const HARVEST_RADIUS: usize = 600;

    /// Read the caret position (char index) from `kAXSelectedTextRange`, or
    /// `None` if the element doesn't expose it. The location is a UTF-16 index;
    /// for ASCII (the dominant dictation case) it equals the char index, and
    /// for non-BMP text it's a close-enough anchor for windowing.
    unsafe fn read_caret_index(focused: AXUIElementRef) -> Option<usize> {
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
        if got && cf_range.location >= 0 {
            Some(cf_range.location as usize)
        } else {
            None
        }
    }

    /// Harvest the screen-context vocabulary source: a small window of the
    /// FOCUSED element's text centred on the caret. NOT the window title, NOT
    /// other panes/sidebars/terminals (those are separate AX elements we never
    /// touch). Best-effort — `None` when AX exposes no field text (e.g. Electron
    /// editors, empty fields), which correctly contributes nothing rather than
    /// clutter. Runs in the parallel focus-capture thread, so it's off the
    /// critical path.
    unsafe fn harvest_screen_text(focused: AXUIElementRef) -> Option<String> {
        let value = read_value(focused)?;
        if value.trim().is_empty() {
            return None;
        }
        // Anchor at the caret; default to end-of-field (dictation usually appends).
        let caret = read_caret_index(focused).unwrap_or_else(|| value.chars().count());
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
        let value_str = match read_value(focused) {
            Some(v) => v,
            None => return (None, None, None),
        };

        // Read selected range.
        let mut range_ref: CFTypeRef = ptr::null();
        let range_attr = CFString::new(kAXSelectedTextRangeAttribute);
        let err = AXUIElementCopyAttributeValue(
            focused,
            range_attr.as_concrete_TypeRef(),
            &mut range_ref,
        );
        let caret_chars = if err == kAXErrorSuccess && !range_ref.is_null() {
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
            if got {
                // CFRange.location is UTF-16 index. For ASCII (the dominant
                // case for dictation) this equals char count; for non-BMP
                // it's an approximation that's fine in practice.
                cf_range.location as usize
            } else {
                value_str.chars().count()
            }
        } else {
            // No range exposed — assume caret at end of value.
            value_str.chars().count()
        };

        let chars: Vec<char> = value_str.chars().collect();
        let immediate_before = if caret_chars > 0 && caret_chars <= chars.len() {
            Some(chars[caret_chars - 1])
        } else {
            None
        };
        let char_after = if caret_chars < chars.len() {
            Some(chars[caret_chars])
        } else {
            None
        };
        let last_nws = last_non_ws_before(&value_str, caret_chars);
        (immediate_before, last_nws, char_after)
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
    // The caret-context fields are retained for diagnostics; the write path
    // re-resolves the CURRENT focus at inject time (freshness over the stale
    // key-press snapshot), so they are not read by the write ladder anymore.
    #[allow(dead_code)]
    pub struct FocusTargetInner {
        focused: AXUIElementRef,
        pub pid: Option<i32>,
        pub immediate_before: Option<char>,
        pub last_nws_before: Option<char>,
        pub char_after: Option<char>,
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

    /// Resolve the currently-focused UI element from the system-wide AX
    /// proxy + read its caret context. Suitable to call on a background
    /// thread in parallel with inference. Retains the focused element so
    /// the caller can use it later from any thread.
    pub fn capture_focus_target() -> eyre::Result<FocusTargetInner> {
        if !ensure_trusted_or_prompt() {
            return Err(eyre::eyre!("Accessibility permission not granted"));
        }
        unsafe {
            let root = AXUIElementCreateSystemWide();
            if root.is_null() {
                return Err(eyre::eyre!("AXUIElementCreateSystemWide returned null"));
            }
            bound_messaging_timeout(root);
            let mut focused_ref: CFTypeRef = ptr::null();
            let focused_attr = CFString::new(kAXFocusedUIElementAttribute);
            let err = AXUIElementCopyAttributeValue(
                root,
                focused_attr.as_concrete_TypeRef(),
                &mut focused_ref,
            );
            cf_release(root as CFTypeRef);
            if err != kAXErrorSuccess || focused_ref.is_null() {
                return Err(eyre::eyre!(
                    "no focused UI element (AXError {err}); click into a text field first"
                ));
            }
            let focused = focused_ref as AXUIElementRef;
            bound_messaging_timeout(focused);

            // Read context immediately while we have the element handy. In
            // AX-blind apps this returns (None, None, None) quickly.
            let focused_pid = focused_pid_of(focused);
            let (immediate_before, last_nws_before, char_after) =
                read_context_with_blind(focused, focused_pid);
            // Classify editability from the AX role/subrole + writability and
            // the app type, so the write ladder can gate on positive evidence.
            let editability = focused_editability(focused, focused_pid);
            // Harvest visible text for screen-context vocabulary while we hold
            // the element. AX-blind apps return None here, same as the caret read.
            let screen_text = harvest_screen_text(focused);

            // Diagnostic: log what we're about to write into. Enable with
            // INJECT_DIAG=1. Helps explain "no text appears" when an
            // element accepts the AX call but doesn't actually render.
            if std::env::var("INJECT_DIAG").is_ok() {
                let mut role_ref: CFTypeRef = ptr::null();
                let role_attr = CFString::new(kAXRoleAttribute);
                let r_err = AXUIElementCopyAttributeValue(
                    focused,
                    role_attr.as_concrete_TypeRef(),
                    &mut role_ref,
                );
                let role = if r_err == kAXErrorSuccess && !role_ref.is_null() {
                    let s = CFString::wrap_under_create_rule(role_ref as CFStringRef).to_string();
                    s
                } else {
                    format!("<unknown AXError {r_err}>")
                };
                eprintln!("[inject-diag] focus capture: pid={focused_pid:?} role={role} editability={editability:?}");
            }

            // Retain — focused_ref was returned by AXUIElementCopyAttributeValue
            // under the Create rule, so it's already +1. We just hold it.
            Ok(FocusTargetInner {
                focused,
                pid: focused_pid,
                immediate_before,
                last_nws_before,
                char_after,
                screen_text,
                editability,
            })
        }
    }

    /// Write `text` at the CURRENT focused element, not the press-time capture.
    ///
    /// The retained `target` (and its caret context / editability) is a
    /// key-press snapshot; by write time the user may have moved focus to a
    /// different field in the SAME app, or the frontmost app changed. Writing
    /// into the stale element would be wrong. So we RE-RESOLVE the current
    /// focused element just before the write and gate there — "use fresh focused
    /// field not stale original". The captured target still feeds the cleanup
    /// vocabulary (screen_text) and the daemon's summary log, but never the
    /// write target.
    pub fn inject_via_target(_target: FocusTargetInner, text: &str) -> eyre::Result<InjectOutcome> {
        // Re-resolve the current focus fresh via the system-wide proxy.
        inject_systemwide(text)
    }

    /// Cancellable variant of [`inject_via_target`]: same fresh re-resolve, but
    /// with a write authorizer polled immediately before the first side effect.
    pub fn inject_via_target_with(
        _target: FocusTargetInner,
        text: &str,
        authorize: Option<&dyn Fn() -> bool>,
    ) -> eyre::Result<InjectOutcome> {
        inject_systemwide_with(text, authorize)
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
    pub fn inner_pid(&self) -> Option<i32> {
        None
    }
    pub fn editability(&self) -> FocusEditability {
        FocusEditability::NotEditable
    }
}

impl AccessibilityInjector {
    #[cfg(all(target_os = "macos", feature = "ax-inject"))]
    pub fn inject_text(text: &str) -> eyre::Result<InjectOutcome> {
        if text.trim().is_empty() {
            return Ok(InjectOutcome::Injected);
        }
        imp::inject_systemwide(text)
    }

    /// Inject using a target captured earlier (parallel-with-inference path).
    /// Skips `get_focused_element` and `read_caret_context` — saves the
    /// ~80–150 ms those cost in Electron apps.
    #[cfg(all(target_os = "macos", feature = "ax-inject"))]
    pub fn inject_with_target(target: FocusTarget, text: &str) -> eyre::Result<InjectOutcome> {
        if text.trim().is_empty() {
            return Ok(InjectOutcome::Injected);
        }
        imp::inject_via_target(target.inner, text)
    }

    #[cfg(not(all(target_os = "macos", feature = "ax-inject")))]
    pub fn inject_with_target(_target: FocusTarget, text: &str) -> eyre::Result<InjectOutcome> {
        Self::inject_text(text)
    }

    /// Cancellable systemwide injection. `authorize` is polled exactly once,
    /// after the focus/context/padding resolution and immediately before the
    /// first write/clipboard side effect; refusal yields
    /// [`InjectOutcome::Cancelled`] with nothing written.
    #[cfg(all(target_os = "macos", feature = "ax-inject"))]
    pub fn inject_text_cancellable(
        text: &str,
        authorize: &dyn Fn() -> bool,
    ) -> eyre::Result<InjectOutcome> {
        if text.trim().is_empty() {
            return Ok(InjectOutcome::Injected);
        }
        imp::inject_systemwide_with(text, Some(authorize))
    }

    /// Cancellable pre-captured-target injection: same fresh re-resolve, same
    /// write authorizer immediately before the first side effect.
    #[cfg(all(target_os = "macos", feature = "ax-inject"))]
    pub fn inject_with_target_cancellable(
        target: FocusTarget,
        text: &str,
        authorize: &dyn Fn() -> bool,
    ) -> eyre::Result<InjectOutcome> {
        if text.trim().is_empty() {
            return Ok(InjectOutcome::Injected);
        }
        imp::inject_via_target_with(target.inner, text, Some(authorize))
    }

    #[cfg(not(all(target_os = "macos", feature = "ax-inject")))]
    pub fn inject_text_cancellable(
        text: &str,
        authorize: &dyn Fn() -> bool,
    ) -> eyre::Result<InjectOutcome> {
        if !authorize() {
            return Ok(InjectOutcome::Cancelled);
        }
        Self::inject_text(text)
    }

    #[cfg(not(all(target_os = "macos", feature = "ax-inject")))]
    pub fn inject_with_target_cancellable(
        _target: FocusTarget,
        text: &str,
        authorize: &dyn Fn() -> bool,
    ) -> eyre::Result<InjectOutcome> {
        if !authorize() {
            return Ok(InjectOutcome::Cancelled);
        }
        Self::inject_text(text)
    }

    #[cfg(all(target_os = "macos", feature = "ax-inject"))]
    pub fn inject_into_pid(pid: i32, text: &str) -> eyre::Result<InjectOutcome> {
        if text.trim().is_empty() {
            return Ok(InjectOutcome::Injected);
        }
        imp::inject_into_pid(pid, text)
    }

    #[cfg(not(all(target_os = "macos", feature = "ax-inject")))]
    pub fn inject_text(text: &str) -> eyre::Result<InjectOutcome> {
        let padded = smart_pad(text, None, None, None);
        if padded.is_empty() {
            return Ok(InjectOutcome::Injected);
        }
        println!("[INJECT-FALLBACK] {padded}");
        Ok(InjectOutcome::Injected)
    }

    #[cfg(not(all(target_os = "macos", feature = "ax-inject")))]
    pub fn inject_into_pid(_pid: i32, text: &str) -> eyre::Result<InjectOutcome> {
        let padded = smart_pad(text, None, None, None);
        if padded.is_empty() {
            return Ok(InjectOutcome::Injected);
        }
        println!("[INJECT-FALLBACK-PID] {padded}");
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
    use super::*;

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
