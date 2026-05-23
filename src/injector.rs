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

use crate::smart_pad::{last_non_ws_before, smart_pad};
use std::sync::Mutex;

pub struct AccessibilityInjector;

/// Process-wide memory of the last character we successfully injected.
/// When the focused app doesn't expose `kAXValue` (Electron apps,
/// sandboxed web views, some Java apps), we fall back to using this as
/// the implicit `char_before` so consecutive utterances still get the
/// right spacing + capitalization.
///
/// Reset on first call (None) → first utterance behaves like "start of
/// field." Updated to the last char of every successful inject.
static LAST_TAIL: Mutex<Option<char>> = Mutex::new(None);

/// PID of the focused app that's known to not expose AXValue. Skip the
/// context read for it on subsequent injects (saves ~100 ms per utterance
/// in Electron / web-view targets).
static AX_BLIND_PID: Mutex<Option<i32>> = Mutex::new(None);

fn mark_ax_blind(pid: i32) {
    if let Ok(mut g) = AX_BLIND_PID.lock() {
        *g = Some(pid);
    }
}

fn is_ax_blind(pid: i32) -> bool {
    matches!(AX_BLIND_PID.lock().ok().and_then(|g| *g), Some(p) if p == pid)
}

/// Cache of `pid → uses_electron_ax_quirks`. Looked up via `ps` once per
/// PID. Electron apps (VS Code, Cursor, Slack, Discord, Claude Code,
/// browsers etc.) report AX TextField role and accept kAXSelectedText
/// writes with kAXErrorSuccess, but their renderer never receives the
/// text — so we always route them through clipboard paste.
static ELECTRON_CACHE: Mutex<Vec<(i32, bool)>> = Mutex::new(Vec::new());

pub fn is_clipboard_only_pid(pid: i32) -> bool {
    if let Ok(cache) = ELECTRON_CACHE.lock() {
        for &(p, v) in cache.iter() {
            if p == pid {
                return v;
            }
        }
    }
    let comm = std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase())
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
        || comm.contains("/figma");
    if let Ok(mut cache) = ELECTRON_CACHE.lock() {
        cache.push((pid, is_electron));
    }
    is_electron
}

fn remember_tail(text: &str) {
    let tail = text.chars().rev().find(|c| !c.is_whitespace());
    if let Ok(mut g) = LAST_TAIL.lock() {
        *g = tail;
    }
}

fn recall_tail() -> Option<char> {
    LAST_TAIL.lock().ok().and_then(|g| *g)
}

/// Public: clear the cross-utterance state. Useful when the user
/// switches target apps and the previous tail no longer applies.
pub fn reset_inject_state() {
    if let Ok(mut g) = LAST_TAIL.lock() {
        *g = None;
    }
}

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
mod imp {
    use super::*;
    use accessibility_sys::{
        kAXErrorSuccess, kAXFocusedUIElementAttribute, kAXRoleAttribute,
        kAXSelectedTextAttribute, kAXSelectedTextRangeAttribute,
        kAXTrustedCheckOptionPrompt, kAXValueAttribute, kAXValueTypeCFRange,
        AXIsProcessTrustedWithOptions, AXUIElementCopyAttributeValue,
        AXUIElementCreateApplication, AXUIElementCreateSystemWide, AXUIElementGetPid,
        AXUIElementRef, AXUIElementSetAttributeValue, AXValueGetValue, AXValueRef,
    };
    use core_foundation::base::{CFRange, CFTypeRef, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::{CFString, CFStringRef};
    use libc::pid_t;
    use std::ffi::c_void;
    use std::ptr;

    pub fn ensure_trusted_or_prompt() -> bool {
        unsafe {
            let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
            let value = CFBoolean::true_value();
            let opts = CFDictionary::from_CFType_pairs(&[(key, value)]);
            AXIsProcessTrustedWithOptions(opts.as_concrete_TypeRef())
        }
    }

    pub fn inject_systemwide(text: &str) -> eyre::Result<()> {
        if !ensure_trusted_or_prompt() {
            return Err(eyre::eyre!(
                "Accessibility permission not granted — System Settings → Privacy & Security → Accessibility"
            ));
        }
        unsafe {
            let root = AXUIElementCreateSystemWide();
            if root.is_null() {
                return Err(eyre::eyre!("AXUIElementCreateSystemWide returned null"));
            }
            let result = inject_through(root, text);
            cf_release(root as CFTypeRef);
            result
        }
    }

    pub fn inject_into_pid(pid: pid_t, text: &str) -> eyre::Result<()> {
        if !ensure_trusted_or_prompt() {
            return Err(eyre::eyre!("Accessibility permission not granted"));
        }
        unsafe {
            let app = AXUIElementCreateApplication(pid);
            if app.is_null() {
                return Err(eyre::eyre!("AXUIElementCreateApplication({pid}) returned null"));
            }
            let result = inject_through(app, text);
            cf_release(app as CFTypeRef);
            result
        }
    }

    /// Full path: resolve focused element → read context → smart-pad →
    /// attempt AX inject → fall back to clipboard paste.
    unsafe fn inject_through(root: AXUIElementRef, text: &str) -> eyre::Result<()> {
        let prof = std::env::var("INJECT_PROFILE").is_ok();
        let t_total = std::time::Instant::now();

        // 1. Focused element.
        let t = std::time::Instant::now();
        let mut focused_ref: CFTypeRef = ptr::null();
        let focused_attr = CFString::new(kAXFocusedUIElementAttribute);
        let err = AXUIElementCopyAttributeValue(
            root,
            focused_attr.as_concrete_TypeRef(),
            &mut focused_ref,
        );
        if prof { eprintln!("[inject-prof] get_focused_element: {:?}", t.elapsed()); }
        if err != kAXErrorSuccess || focused_ref.is_null() {
            // No focused field exposed — try clipboard paste against whatever
            // is frontmost. Still apply smart-pad using the remembered tail
            // so consecutive utterances don't mash together.
            eprintln!("[inject] no focused AX element (AXError {err}); using clipboard paste");
            let tail = super::recall_tail();
            let padded = smart_pad(text, tail, tail, None);
            let r = crate::clipboard_paste::paste_via_clipboard(&padded);
            if r.is_ok() {
                super::remember_tail(&padded);
            }
            return r;
        }
        let focused = focused_ref as AXUIElementRef;

        // 2. Context: read current value + caret position. Both may be
        //    absent (Electron apps, sandboxed web views, some Java apps
        //    don't expose AXValue) — in that case fall back to our
        //    remembered last-injected-tail character so consecutive
        //    utterances still get the right spacing + capitalization.
        //
        // Optimization: if the focused app's PID has previously refused
        // AXValue reads, skip the (~100 ms) round-trip entirely and go
        // straight to the LAST_TAIL fallback.
        let t = std::time::Instant::now();
        let focused_pid = focused_pid_of(focused);
        if prof { eprintln!("[inject-prof] get_pid:               {:?}  (pid={:?}, blind={:?})",
            t.elapsed(), focused_pid, focused_pid.map(super::is_ax_blind)); }

        let t = std::time::Instant::now();
        let (mut immediate_before, mut last_nws_before, char_after) =
            if let Some(pid) = focused_pid {
                if super::is_ax_blind(pid) {
                    (None, None, None)
                } else {
                    let ctx = read_caret_context(focused);
                    if ctx.0.is_none() && ctx.1.is_none() {
                        super::mark_ax_blind(pid);
                    }
                    ctx
                }
            } else {
                read_caret_context(focused)
            };
        if prof { eprintln!("[inject-prof] read_caret_context:    {:?}  (immediate={:?}, after={:?})",
            t.elapsed(), immediate_before, char_after); }
        if immediate_before.is_none() && last_nws_before.is_none() {
            if let Some(tail) = super::recall_tail() {
                immediate_before = Some(tail);
                last_nws_before = Some(tail);
            }
        }

        // 3. Smart-pad.
        let padded = smart_pad(text, immediate_before, last_nws_before, char_after);
        if padded.is_empty() {
            cf_release(focused as CFTypeRef);
            return Ok(());
        }

        // 4. AX write — try SelectedText, then Value.
        let t = std::time::Instant::now();
        let payload = CFString::new(&padded);
        let sel_attr = CFString::new(kAXSelectedTextAttribute);
        let sel_err = AXUIElementSetAttributeValue(
            focused,
            sel_attr.as_concrete_TypeRef(),
            payload.as_concrete_TypeRef() as CFTypeRef,
        );
        if prof { eprintln!("[inject-prof] set_selected_text:     {:?}  (err={sel_err})", t.elapsed()); }

        if sel_err == kAXErrorSuccess {
            cf_release(focused as CFTypeRef);
            super::remember_tail(&padded);
            if prof { eprintln!("[inject-prof] TOTAL:                {:?}", t_total.elapsed()); }
            return Ok(());
        }

        let t = std::time::Instant::now();
        let val_attr = CFString::new(kAXValueAttribute);
        let val_err = AXUIElementSetAttributeValue(
            focused,
            val_attr.as_concrete_TypeRef(),
            payload.as_concrete_TypeRef() as CFTypeRef,
        );
        if prof { eprintln!("[inject-prof] set_value:             {:?}  (err={val_err})", t.elapsed()); }
        cf_release(focused as CFTypeRef);

        if val_err == kAXErrorSuccess {
            super::remember_tail(&padded);
            return Ok(());
        }

        // 5. Clipboard fallback.
        eprintln!(
            "[inject] AX writes refused (selected={sel_err}, value={val_err}); using clipboard paste"
        );
        let r = crate::clipboard_paste::paste_via_clipboard(&padded);
        if r.is_ok() {
            super::remember_tail(&padded);
        }
        r
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
        let mut value_ref: CFTypeRef = ptr::null();
        let val_attr = CFString::new(kAXValueAttribute);
        let err = AXUIElementCopyAttributeValue(
            focused,
            val_attr.as_concrete_TypeRef(),
            &mut value_ref,
        );
        if err != kAXErrorSuccess || value_ref.is_null() {
            return (None, None, None);
        }
        let value_str = CFString::wrap_under_create_rule(value_ref as CFStringRef).to_string();

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
    pub struct FocusTargetInner {
        focused: AXUIElementRef,
        pub pid: Option<i32>,
        pub immediate_before: Option<char>,
        pub last_nws_before: Option<char>,
        pub char_after: Option<char>,
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

            // Read context immediately while we have the element handy. In
            // AX-blind apps this returns (None, None, None) quickly.
            let focused_pid = focused_pid_of(focused);
            let (immediate_before, last_nws_before, char_after) =
                if let Some(pid) = focused_pid {
                    if super::is_ax_blind(pid) {
                        (None, None, None)
                    } else {
                        let ctx = read_caret_context(focused);
                        if ctx.0.is_none() && ctx.1.is_none() {
                            super::mark_ax_blind(pid);
                        }
                        ctx
                    }
                } else {
                    read_caret_context(focused)
                };

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
                    let s = CFString::wrap_under_create_rule(role_ref as CFStringRef)
                        .to_string();
                    s
                } else {
                    format!("<unknown AXError {r_err}>")
                };
                eprintln!(
                    "[inject-diag] focus capture: pid={focused_pid:?} role={role}"
                );
            }

            // Retain — focused_ref was returned by AXUIElementCopyAttributeValue
            // under the Create rule, so it's already +1. We just hold it.
            Ok(FocusTargetInner {
                focused,
                pid: focused_pid,
                immediate_before,
                last_nws_before,
                char_after,
            })
        }
    }

    /// Write `text` into a pre-captured focus target. Skips the
    /// get_focused_element + context read costs entirely.
    pub fn inject_via_target(target: FocusTargetInner, text: &str) -> eyre::Result<()> {
        let prof = std::env::var("INJECT_PROFILE").is_ok();
        let t_total = std::time::Instant::now();

        // Apply remembered tail fallback if context was empty.
        let (immediate_before, last_nws_before) = if target.immediate_before.is_none()
            && target.last_nws_before.is_none()
        {
            let tail = super::recall_tail();
            (tail, tail)
        } else {
            (target.immediate_before, target.last_nws_before)
        };

        let padded = smart_pad(text, immediate_before, last_nws_before, target.char_after);
        if padded.is_empty() {
            return Ok(());
        }

        // Short-circuit: Electron / browser targets accept AX writes with
        // kAXErrorSuccess but never render the text. Route them straight
        // to clipboard paste, which always works because it uses the
        // system-level Cmd+V handler.
        if let Some(pid) = target.pid {
            if super::is_clipboard_only_pid(pid) {
                if prof {
                    eprintln!("[inject-prof] (via target) Electron pid {pid} — clipboard paste");
                }
                let r = crate::clipboard_paste::paste_via_clipboard(&padded);
                if r.is_ok() {
                    super::remember_tail(&padded);
                }
                return r;
            }
        }

        unsafe {
            let t = std::time::Instant::now();
            let payload = CFString::new(&padded);
            let sel_attr = CFString::new(kAXSelectedTextAttribute);
            let sel_err = AXUIElementSetAttributeValue(
                target.focused,
                sel_attr.as_concrete_TypeRef(),
                payload.as_concrete_TypeRef() as CFTypeRef,
            );
            if prof {
                eprintln!("[inject-prof] (via target) set_selected_text: {:?} err={sel_err}", t.elapsed());
            }

            if sel_err == kAXErrorSuccess {
                super::remember_tail(&padded);
                if prof { eprintln!("[inject-prof] (via target) TOTAL: {:?}", t_total.elapsed()); }
                return Ok(());
            }

            let t = std::time::Instant::now();
            let val_attr = CFString::new(kAXValueAttribute);
            let val_err = AXUIElementSetAttributeValue(
                target.focused,
                val_attr.as_concrete_TypeRef(),
                payload.as_concrete_TypeRef() as CFTypeRef,
            );
            if prof {
                eprintln!("[inject-prof] (via target) set_value: {:?} err={val_err}", t.elapsed());
            }

            if val_err == kAXErrorSuccess {
                super::remember_tail(&padded);
                if prof { eprintln!("[inject-prof] (via target) TOTAL: {:?}", t_total.elapsed()); }
                return Ok(());
            }

            // Both AX writes failed — fall back to clipboard paste.
            eprintln!(
                "[inject] AX writes refused (sel={sel_err}, val={val_err}); using clipboard paste"
            );
            let r = crate::clipboard_paste::paste_via_clipboard(&padded);
            if r.is_ok() {
                super::remember_tail(&padded);
            }
            r
        }
    }
}

/// Pre-captured focus target — owned, Send across threads. Created at
/// key-release time and used at inject time after Parakeet+Gemma finish.
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
}

impl AccessibilityInjector {
    #[cfg(all(target_os = "macos", feature = "ax-inject"))]
    pub fn inject_text(text: &str) -> eyre::Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        imp::inject_systemwide(text)
    }

    /// Inject using a target captured earlier (parallel-with-inference path).
    /// Skips `get_focused_element` and `read_caret_context` — saves the
    /// ~80–150 ms those cost in Electron apps.
    #[cfg(all(target_os = "macos", feature = "ax-inject"))]
    pub fn inject_with_target(target: FocusTarget, text: &str) -> eyre::Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        imp::inject_via_target(target.inner, text)
    }

    #[cfg(not(all(target_os = "macos", feature = "ax-inject")))]
    pub fn inject_with_target(_target: FocusTarget, text: &str) -> eyre::Result<()> {
        Self::inject_text(text)
    }

    #[cfg(all(target_os = "macos", feature = "ax-inject"))]
    pub fn inject_into_pid(pid: i32, text: &str) -> eyre::Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        imp::inject_into_pid(pid, text)
    }

    #[cfg(not(all(target_os = "macos", feature = "ax-inject")))]
    pub fn inject_text(text: &str) -> eyre::Result<()> {
        let padded = smart_pad(text, None, None, None);
        if padded.is_empty() {
            return Ok(());
        }
        println!("[INJECT-FALLBACK] {padded}");
        Ok(())
    }

    #[cfg(not(all(target_os = "macos", feature = "ax-inject")))]
    pub fn inject_into_pid(_pid: i32, text: &str) -> eyre::Result<()> {
        let padded = smart_pad(text, None, None, None);
        if padded.is_empty() {
            return Ok(());
        }
        println!("[INJECT-FALLBACK-PID] {padded}");
        Ok(())
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
