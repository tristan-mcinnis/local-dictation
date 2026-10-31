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

use crate::last_dictation::KeptCause;
use crate::smart_pad::smart_pad;
use crate::smart_pad::last_non_ws_before;
use std::sync::Mutex;

mod pid_class;
pub use pid_class::{
    app_label,
    app_label_for_pid,
    app_label_for_pid_cheap,
    hold_log_line,
    is_clipboard_only_pid,
    is_electron_pid,
    is_terminal_pid,
    is_wechat_pid,
    process_command_path,
};
#[cfg(any(test, all(target_os = "macos", feature = "ax-inject")))]
use pid_class::*;

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
// single decision point before any write action (see `finalize_utterance`).

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

/// What the caller does with a finished text after the write attempt: nothing
/// (it landed), keep it (nothing was written; the cause is what the pill and
/// the History note say), or drop it (a cancel beat the write).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Delivery {
    Delivered,
    Held(KeptCause),
    Cancelled,
}

/// Turn the injector's answer into what happens to the text, logging a refused
/// or failed write once, with the app it was for. `target_pid` is only looked
/// up on those paths (the label may shell out to `ps`).
pub fn delivery_of(result: eyre::Result<InjectOutcome>, target_pid: Option<i32>) -> Delivery {
    match result {
        Ok(InjectOutcome::Injected) => Delivery::Delivered,
        Ok(InjectOutcome::Cancelled) => Delivery::Cancelled,
        Ok(InjectOutcome::NoEditableField) => {
            eprintln!("  ◌    no text field selected, text kept app={}", app_label_for_pid(target_pid));
            Delivery::Held(KeptCause::NoEditableField)
        }
        // Fail-closed: the expected app was not frontmost at the global Cmd+V,
        // so nothing was written. Held, like a missing field.
        Ok(InjectOutcome::RefusedForeground) => {
            eprintln!("  ◌    target not frontmost at paste, text kept app={}", app_label_for_pid(target_pid));
            Delivery::Held(KeptCause::NoEditableField)
        }
        Err(e) => {
            eprintln!("  ✗    inject failed: {e:?}, text kept app={}", app_label_for_pid(target_pid));
            Delivery::Held(KeptCause::PasteFailed)
        }
    }
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

/// The per-utterance finalization seam, run immediately before any write. A
/// `hold` the caller already decided (the recording limit) keeps the text
/// without touching the injector; a pending cancel drops it the same way.
/// Otherwise the write goes through `inject`, the real injector in production
/// and a recording closure in tests, which re-reads the current focus and
/// answers for itself. The text, not a flag, is what the sink sees.
pub fn finalize_utterance(
    cancel_requested: bool,
    hold: Option<KeptCause>,
    transcript: &str,
    target_pid: Option<i32>,
    inject: impl FnOnce(&str) -> eyre::Result<InjectOutcome>,
) -> Delivery {
    if cancel_requested {
        return Delivery::Cancelled;
    }
    match hold {
        Some(cause) => Delivery::Held(cause),
        None => delivery_of(inject(transcript), target_pid),
    }
}

pub struct AccessibilityInjector;

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
mod imp;

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
mod tests;
