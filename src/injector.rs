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

pub struct AccessibilityInjector;

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
mod imp {
    use super::*;
    use accessibility_sys::{
        kAXErrorSuccess, kAXFocusedUIElementAttribute, kAXSelectedTextAttribute,
        kAXSelectedTextRangeAttribute, kAXTrustedCheckOptionPrompt, kAXValueAttribute,
        kAXValueTypeCFRange, AXIsProcessTrustedWithOptions, AXUIElementCopyAttributeValue,
        AXUIElementCreateApplication, AXUIElementCreateSystemWide, AXUIElementRef,
        AXUIElementSetAttributeValue, AXValueGetValue, AXValueRef,
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
        // 1. Focused element.
        let mut focused_ref: CFTypeRef = ptr::null();
        let focused_attr = CFString::new(kAXFocusedUIElementAttribute);
        let err = AXUIElementCopyAttributeValue(
            root,
            focused_attr.as_concrete_TypeRef(),
            &mut focused_ref,
        );
        if err != kAXErrorSuccess || focused_ref.is_null() {
            // No focused field — try clipboard paste against whatever is
            // frontmost (the user clearly wanted *something* to receive it).
            eprintln!("[inject] no focused AX element (AXError {err}); using clipboard paste");
            return crate::clipboard_paste::paste_via_clipboard(&smart_pad(text, None, None, None));
        }
        let focused = focused_ref as AXUIElementRef;

        // 2. Context: read current value + caret position. Both may be
        //    absent (some fields don't expose AXValue) — that's fine, we
        //    just inject without smart spacing in that case.
        let (immediate_before, last_nws_before, char_after) = read_caret_context(focused);

        // 3. Smart-pad.
        let padded = smart_pad(text, immediate_before, last_nws_before, char_after);
        if padded.is_empty() {
            cf_release(focused as CFTypeRef);
            return Ok(());
        }

        // 4. AX write — try SelectedText, then Value.
        let payload = CFString::new(&padded);
        let sel_attr = CFString::new(kAXSelectedTextAttribute);
        let sel_err = AXUIElementSetAttributeValue(
            focused,
            sel_attr.as_concrete_TypeRef(),
            payload.as_concrete_TypeRef() as CFTypeRef,
        );

        if sel_err == kAXErrorSuccess {
            cf_release(focused as CFTypeRef);
            return Ok(());
        }

        let val_attr = CFString::new(kAXValueAttribute);
        let val_err = AXUIElementSetAttributeValue(
            focused,
            val_attr.as_concrete_TypeRef(),
            payload.as_concrete_TypeRef() as CFTypeRef,
        );
        cf_release(focused as CFTypeRef);

        if val_err == kAXErrorSuccess {
            return Ok(());
        }

        // 5. Clipboard fallback.
        eprintln!(
            "[inject] AX writes refused (selected={sel_err}, value={val_err}); using clipboard paste"
        );
        crate::clipboard_paste::paste_via_clipboard(&padded)
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
}

impl AccessibilityInjector {
    #[cfg(all(target_os = "macos", feature = "ax-inject"))]
    pub fn inject_text(text: &str) -> eyre::Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        imp::inject_systemwide(text)
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
