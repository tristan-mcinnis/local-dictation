//! Sparkle 2 auto-update (optional, `sparkle` feature).
//!
//! Local Dictation ships two ways: a direct download whose app updates itself
//! through Sparkle, and a Homebrew cask. This module is the direct-download
//! half.
//!
//! Sparkle 2 has no Info.plist-only auto-start: a host must create an
//! `SPUStandardUpdaterController` on the main thread. The app does not link
//! Sparkle, so this module `dlopen`s the framework from the running bundle and
//! only when the bundle is genuinely a Sparkle build — it carries both
//! `SUFeedURL` (HTTPS) and `SUPublicEDKey`, and `Sparkle.framework` is
//! embedded. A dev build, a cask install without the keys, or a build with the
//! framework missing all resolve to a quiet no-op.
//!
//! The controller is created on the main thread and kept there for the life of
//! the process; Sparkle keeps no singleton of its own.

use crate::app_paths;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::AnyClass;
use objc2::runtime::AnyObject;
use objc2_foundation::MainThreadMarker;
use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

thread_local! {
    static CONTROLLER: RefCell<Option<Retained<AnyObject>>> = const { RefCell::new(None) };
}

/// Parse a bundle's Sparkle configuration from raw `Info.plist` XML.
///
/// Returns `(feed_url, public_key)` only for a real feed: HTTPS, non-empty
/// public key. A missing, blank, placeholder or non-HTTPS value means "not a
/// Sparkle build", which is exactly what the updater must not start on.
pub fn parse_feed(plist: &str) -> Option<(String, String)> {
    let feed = app_paths::plist_string_value(plist, "SUFeedURL")?;
    let key = app_paths::plist_string_value(plist, "SUPublicEDKey")?;
    let feed = feed.trim().to_string();
    let key = key.trim().to_string();
    if !feed.starts_with("https://") || key.is_empty() {
        return None;
    }
    Some((feed, key))
}

/// The running bundle's feed, if it has one.
pub fn configured_feed() -> Option<(String, String)> {
    let plist = std::fs::read_to_string(app_paths::bundle_info_plist()?).ok()?;
    parse_feed(&plist)
}

/// The framework binary inside the running bundle, if embedded.
fn framework_binary() -> Option<PathBuf> {
    let info = app_paths::bundle_info_plist()?;
    let contents = info.parent()?;
    let versions = contents.join("Frameworks/Sparkle.framework/Versions");
    for entry in std::fs::read_dir(versions).ok()? {
        let binary = entry.ok()?.path().join("Sparkle");
        if binary.exists() {
            return Some(binary);
        }
    }
    None
}

/// Start the updater on the main thread. Returns true only when the Sparkle
/// controller was actually created. Safe to call unconditionally: every
/// not-a-Sparkle-build case returns false.
pub fn start() -> bool {
    // Outside the shipped bundle (dev, tests, CLI) there is nothing to grow.
    if !app_paths::running_in_bundle() {
        return false;
    }
    if configured_feed().is_none() {
        return false;
    }
    if MainThreadMarker::new().is_none() {
        eprintln!("[updater] Sparkle must be started on the main thread; skipping");
        return false;
    }
    let Some(framework) = framework_binary() else {
        eprintln!("[updater] SUFeedURL is set but Sparkle.framework is not embedded; skipping");
        return false;
    };

    let Ok(c_path) = CString::new(framework.as_os_str().as_bytes()) else {
        eprintln!("[updater] Sparkle path is not a valid C string; skipping");
        return false;
    };
    // SAFETY: dlopen with a path we just built from the running bundle.
    let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        eprintln!("[updater] could not load Sparkle.framework; skipping");
        return false;
    }

    let Some(class) =
        AnyClass::get(CStr::from_bytes_with_nul(b"SPUStandardUpdaterController\0").unwrap())
    else {
        eprintln!("[updater] Sparkle loaded but SPUStandardUpdaterController is absent; skipping");
        return false;
    };

    // SAFETY: `alloc`/`init…` is the standard ownership pair; the initializer
    // returns +1, which `Retained` takes over. Both delegates are nil, which
    // Sparkle accepts (it uses its default standard user driver).
    let retained: Retained<AnyObject> = unsafe {
        let allocated: *mut AnyObject = msg_send![class, alloc];
        if allocated.is_null() {
            return false;
        }
        let controller: *mut AnyObject = msg_send![
            allocated,
            initWithStartingUpdater: true,
            updaterDelegate: std::ptr::null_mut::<AnyObject>(),
            userDriverDelegate: std::ptr::null_mut::<AnyObject>(),
        ];
        match Retained::from_raw(controller) {
            Some(c) => c,
            None => {
                eprintln!("[updater] Sparkle returned no controller; skipping");
                return false;
            }
        }
    };
    CONTROLLER.with(|slot| *slot.borrow_mut() = Some(retained));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLIST_OK: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>SUFeedURL</key>
    <string>https://example.com/appcast.xml</string>
    <key>SUPublicEDKey</key>
    <string>pfIShU4dEXqPd5ObYNfDBiQWcXozk7estwzTnF9BamQ=</string>
</dict>
</plist>"#;

    #[test]
    fn a_configured_feed_is_recognised() {
        let (feed, key) = parse_feed(PLIST_OK).expect("a real feed parses");
        assert_eq!(feed, "https://example.com/appcast.xml");
        assert!(key.starts_with("pfIShU4d"));
    }

    #[test]
    fn a_plain_bundle_is_not_a_sparkle_build() {
        assert!(parse_feed("").is_none());
        assert!(parse_feed("<plist><dict></dict></plist>").is_none());
    }

    #[test]
    fn a_non_https_or_empty_key_is_refused() {
        let http = PLIST_OK.replace("https://", "http://");
        assert!(parse_feed(&http).is_none());
        let blank = PLIST_OK.replace("pfIShU4dEXqPd5ObYNfDBiQWcXozk7estwzTnF9BamQ=", "");
        assert!(parse_feed(&blank).is_none());
    }

    #[test]
    fn start_is_a_noop_outside_a_sparkle_bundle() {
        // The test binary is not inside a bundle with a feed, so the updater
        // must decline rather than touch the Objective-C runtime.
        assert!(!start());
    }
}
