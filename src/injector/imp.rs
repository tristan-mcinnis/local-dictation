//! The macOS Accessibility side of injection: trust prompt, focus capture,
//! caret and field reads, the AX write and its clipboard fallback.
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
            return clipboard_paste_outcome(&padded, expected_pid);
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
        return clipboard_paste_outcome(padded, expected_pid);
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
    clipboard_paste_outcome(padded, expected_pid)
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
///     and return `Ok`.
///   * vanished  → promote the PID to clipboard-only and paste via Cmd+V
///     instead, so this and every future utterance render.
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
    clipboard_paste_outcome(padded, expected_pid)
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

/// Paste `padded` through the clipboard route and translate the answer. A
/// paste whose expected app left the front, or whose clipboard could not be
/// protected, writes nothing; both are held, never pasted blindly.
fn clipboard_paste_outcome(padded: &str, expected_pid: Option<i32>) -> eyre::Result<InjectOutcome> {
    use crate::clipboard_paste::PasteOutcome;
    match crate::clipboard_paste::paste_via_clipboard(padded, expected_pid)? {
        PasteOutcome::Delivered => {
            super::remember_tail(padded);
            Ok(InjectOutcome::Injected)
        }
        PasteOutcome::RefusedForeground => {
            eprintln!(
                "{}",
                super::hold_log_line(
                    &format!("expected app {expected_pid:?} not frontmost at Cmd+V — holding (not pasted)"),
                    &super::app_label_for_pid(expected_pid),
                )
            );
            Ok(InjectOutcome::RefusedForeground)
        }
        PasteOutcome::ClipboardProtected => Ok(InjectOutcome::ClipboardProtected),
    }
}
