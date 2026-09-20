//! The two macOS grants this app cannot work without, and the System Settings
//! panes that hand them out.
//!
//! * **Accessibility** — the `CGEventTap` that watches the hotkey. Checked
//!   with `AXIsProcessTrusted()`, which does *not* prompt, so it is safe to
//!   poll once a second. The one prompting call
//!   (`AXIsProcessTrustedWithOptions`, in `injector.rs`) still happens once at
//!   first launch, because that is what adds the app to the Accessibility
//!   list; everything after it polls the silent check.
//! * **Microphone** — `AVCaptureDevice.authorizationStatusForMediaType:`. The
//!   app used to never ask: the TCC prompt appeared as a side effect of the
//!   first `cpal` stream open, i.e. in the middle of the user's first
//!   dictation, and a denial then produced a stream of digital silence that
//!   the pipeline reported as "no speech above noise floor".
//!
//! **Why not `objc2-av-foundation`:** the whole need is two class methods and
//! one string constant. `objc2`, `objc2-foundation` and `block2` are already
//! dependencies of the `ax-inject` feature, so the binding below adds no crate
//! and no build step — it links the AVFoundation framework and sends the two
//! messages. A whole framework crate for two selectors is not worth the build
//! time or the version surface.
//!
//! Every function here is safe to call in any feature configuration. Without
//! `ax-inject` (the featureless `cargo test` build) the checks report
//! [`Grant::Unknown`] and `true` for Accessibility, so nothing in the pure
//! pipeline has to be feature-gated to use them.

/// Result of the microphone authorization check, mirroring
/// `AVAuthorizationStatus` but with an extra arm for "we could not ask".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Grant {
    /// The user has not been asked yet. The app asks at boot, not mid-dictation.
    Undecided,
    /// The user said no. Only System Settings can change it.
    Denied,
    /// Blocked by policy (Screen Time, MDM). The user cannot change it either.
    Restricted,
    Granted,
    /// The status could not be read (no AVFoundation in this build). Treated
    /// as usable: refusing to record on a check we could not run would be
    /// worse than letting the OS decide.
    Unknown,
}

impl Grant {
    /// Map a raw `AVAuthorizationStatus`. Any value macOS adds later reads as
    /// [`Grant::Unknown`] rather than silently as a denial.
    pub fn from_authorization_status(raw: i64) -> Grant {
        match raw {
            0 => Grant::Undecided,
            1 => Grant::Restricted,
            2 => Grant::Denied,
            3 => Grant::Granted,
            _ => Grant::Unknown,
        }
    }

    /// Can a recording started now be expected to carry sound?
    pub fn is_usable(self) -> bool {
        matches!(self, Grant::Granted | Grant::Unknown)
    }

    /// Will asking again ever help, or must the user go to System Settings?
    pub fn needs_system_settings(self) -> bool {
        matches!(self, Grant::Denied | Grant::Restricted)
    }

    /// What the Setup window's Microphone row says.
    pub fn label(self) -> &'static str {
        match self {
            Grant::Granted => "Allowed",
            Grant::Undecided => "Not asked yet",
            Grant::Denied => "Denied",
            Grant::Restricted => "Blocked by policy",
            Grant::Unknown => "Unknown",
        }
    }
}

/// Deep link to System Settings → Privacy & Security → Accessibility.
pub const ACCESSIBILITY_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

/// Deep link to System Settings → Privacy & Security → Microphone.
pub const MICROPHONE_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone";

/// Open one of the panes above. Argument array, no shell (repo rule).
pub fn open_settings_pane(url: &str) {
    match std::process::Command::new("/usr/bin/open").arg(url).spawn() {
        Ok(_) => eprintln!("[perm] opened {url}"),
        Err(e) => eprintln!("[perm] could not open {url}: {e}"),
    }
}

/// Is this process trusted for Accessibility *right now*? Never prompts, so
/// the Setup window can poll it once a second while it is open.
pub fn accessibility_trusted() -> bool {
    imp::accessibility_trusted()
}

/// The current microphone authorization.
pub fn microphone_status() -> Grant {
    imp::microphone_status()
}

/// Ask for microphone access. Returns immediately; `then` runs on whatever
/// queue macOS picks, once the user has answered (or at once, when the answer
/// is already on file). A no-op that reports [`Grant::Unknown`] in builds with
/// no AVFoundation.
pub fn request_microphone(then: impl Fn(Grant) + Send + 'static) {
    imp::request_microphone(then);
}

/// Update the microphone health fault from the current status. Returns the
/// status so a caller can act on it too.
pub fn refresh_microphone_health() -> Grant {
    let grant = microphone_status();
    if grant.is_usable() {
        crate::health::clear(crate::health::Fault::MicrophoneBlocked);
    } else {
        crate::health::raise(crate::health::Fault::MicrophoneBlocked, grant.label());
    }
    grant
}

// ─── Deferred work that needs Accessibility ─────────────────────────────

/// Work that is waiting for the Accessibility grant.
///
/// The event tap must be created on the thread that owns the CFRunLoop AppKit
/// pumps, i.e. the main thread, so the pending task is kept in a thread-local
/// rather than a `Send` static: [`run_when_accessibility_granted`] and
/// [`poll_pending_accessibility`] are both main-thread calls and the value
/// never crosses a thread boundary.
type PendingTask = Box<dyn FnOnce() -> Result<(), String>>;

thread_local! {
    static PENDING: std::cell::RefCell<Option<PendingTask>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `task` as soon as this process is trusted for Accessibility.
///
/// Granted already: `task` runs now. Not granted: the fault is raised, the
/// menu bar and the Setup window say so, and `task` runs the moment
/// [`poll_pending_accessibility`] sees the grant land — with no relaunch. The
/// daemon must never exit over a missing grant; that is how a first launch
/// used to end with a permission dialog and then nothing at all.
pub fn run_when_accessibility_granted(task: impl FnOnce() -> Result<(), String> + 'static) {
    if accessibility_trusted() {
        finish_accessibility_task(Box::new(task));
        return;
    }
    crate::health::raise(
        crate::health::Fault::AccessibilityMissing,
        "not trusted — waiting for the grant",
    );
    PENDING.with(|slot| *slot.borrow_mut() = Some(Box::new(task)));
}

/// Poll the grant and run any pending task. Called about once a second from
/// the menu-bar timer, on the main thread. Cheap: one non-prompting syscall.
pub fn poll_pending_accessibility() {
    let waiting = PENDING.with(|slot| slot.borrow().is_some());
    if !waiting {
        return;
    }
    if !accessibility_trusted() {
        return;
    }
    let Some(task) = PENDING.with(|slot| slot.borrow_mut().take()) else {
        return;
    };
    eprintln!("[perm] Accessibility granted — continuing without a relaunch");
    finish_accessibility_task(task);
}

fn finish_accessibility_task(task: PendingTask) {
    match task() {
        Ok(()) => crate::health::clear(crate::health::Fault::AccessibilityMissing),
        Err(why) => crate::health::raise(crate::health::Fault::AccessibilityMissing, why),
    }
}

/// Is something still waiting for the grant? Used by the Setup window so it
/// can say "the hotkey starts working as soon as you turn this on".
pub fn accessibility_task_pending() -> bool {
    PENDING.with(|slot| slot.borrow().is_some())
}

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
mod imp {
    use super::Grant;
    use block2::RcBlock;
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, Bool};
    use objc2_foundation::NSString;

    #[link(name = "AVFoundation", kind = "framework")]
    extern "C" {
        /// `AVMediaTypeAudio` — the media type constant, not a literal, so a
        /// future OS cannot make this binding wrong.
        static AVMediaTypeAudio: &'static NSString;
    }

    /// `AVCaptureDevice`, or `None` if AVFoundation is somehow not loaded.
    /// Looked up rather than `class!`-ed so a missing framework degrades to
    /// [`Grant::Unknown`] instead of aborting the daemon.
    fn capture_device_class() -> Option<&'static AnyClass> {
        AnyClass::get(c"AVCaptureDevice")
    }

    pub(super) fn accessibility_trusted() -> bool {
        // SAFETY: no arguments, no ownership; the non-prompting variant.
        unsafe { accessibility_sys::AXIsProcessTrusted() }
    }

    pub(super) fn microphone_status() -> Grant {
        let Some(class) = capture_device_class() else {
            return Grant::Unknown;
        };
        // SAFETY: `+[AVCaptureDevice authorizationStatusForMediaType:]` takes
        // an `AVMediaType` (an `NSString *`) and returns an `NSInteger`.
        let raw: isize =
            unsafe { msg_send![class, authorizationStatusForMediaType: AVMediaTypeAudio] };
        Grant::from_authorization_status(raw as i64)
    }

    pub(super) fn request_microphone(then: impl Fn(Grant) + Send + 'static) {
        let Some(class) = capture_device_class() else {
            then(Grant::Unknown);
            return;
        };
        let block = RcBlock::new(move |granted: Bool| {
            then(if granted.as_bool() {
                Grant::Granted
            } else {
                Grant::Denied
            });
        });
        // SAFETY: `+[AVCaptureDevice requestAccessForMediaType:completionHandler:]`
        // takes the media type and a `void (^)(BOOL)`. `RcBlock` keeps the
        // block alive until AVFoundation has copied it.
        unsafe {
            let _: () = msg_send![
                class,
                requestAccessForMediaType: AVMediaTypeAudio,
                completionHandler: &*block,
            ];
        }
    }
}

#[cfg(not(all(target_os = "macos", feature = "ax-inject")))]
mod imp {
    use super::Grant;

    pub(super) fn accessibility_trusted() -> bool {
        true
    }

    pub(super) fn microphone_status() -> Grant {
        Grant::Unknown
    }

    pub(super) fn request_microphone(then: impl Fn(Grant) + Send + 'static) {
        then(Grant::Unknown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_av_authorization_status_maps_to_the_right_grant() {
        assert_eq!(Grant::from_authorization_status(0), Grant::Undecided);
        assert_eq!(Grant::from_authorization_status(1), Grant::Restricted);
        assert_eq!(Grant::from_authorization_status(2), Grant::Denied);
        assert_eq!(Grant::from_authorization_status(3), Grant::Granted);
        // A status macOS adds later must not read as a denial.
        assert_eq!(Grant::from_authorization_status(9), Grant::Unknown);
        assert_eq!(Grant::from_authorization_status(-1), Grant::Unknown);
    }

    #[test]
    fn only_a_real_grant_or_an_unreadable_one_lets_recording_start() {
        assert!(Grant::Granted.is_usable());
        assert!(Grant::Unknown.is_usable());
        // Regression: an undecided mic used to be discovered by opening the
        // stream mid-dictation, which lost that dictation.
        assert!(!Grant::Undecided.is_usable());
        assert!(!Grant::Denied.is_usable());
        assert!(!Grant::Restricted.is_usable());
    }

    #[test]
    fn asking_again_only_helps_before_the_user_has_answered() {
        assert!(!Grant::Undecided.needs_system_settings());
        assert!(Grant::Denied.needs_system_settings());
        assert!(Grant::Restricted.needs_system_settings());
        assert!(!Grant::Granted.needs_system_settings());
    }

    #[test]
    fn the_settings_deep_links_name_the_two_privacy_panes() {
        assert!(ACCESSIBILITY_PANE.starts_with("x-apple.systempreferences:"));
        assert!(ACCESSIBILITY_PANE.ends_with("Privacy_Accessibility"));
        assert!(MICROPHONE_PANE.starts_with("x-apple.systempreferences:"));
        assert!(MICROPHONE_PANE.ends_with("Privacy_Microphone"));
    }

    /// Proves the hand-written AVFoundation binding actually links and that
    /// the selector exists: a wrong symbol or a missing framework would abort
    /// here rather than in the shipped app. Only the non-prompting status call
    /// is exercised — a test must never put a TCC dialog on someone's screen.
    #[cfg(all(target_os = "macos", feature = "ax-inject"))]
    #[test]
    fn the_microphone_binding_returns_a_real_status_on_this_mac() {
        let grant = microphone_status();
        assert_ne!(
            grant,
            Grant::Unknown,
            "AVCaptureDevice did not answer — the AVFoundation binding is wrong"
        );
        // And the silent Accessibility check runs without prompting.
        let _ = accessibility_trusted();
    }

    /// The deferred install is the difference between "grant it, then find
    /// the app has quit" and "grant it, and the hotkey starts working". The
    /// grant itself cannot be faked in a test, so this covers the mechanics:
    /// a task queued while untrusted is held, runs exactly once, and reports
    /// its own failure as the fault detail.
    #[test]
    fn a_task_waiting_on_accessibility_runs_once_and_reports_its_result() {
        let _serial = crate::health::test_lock();
        use std::cell::Cell;
        use std::rc::Rc;

        // On a build with no AX check, `accessibility_trusted()` is true, so
        // the task runs immediately — which is also the behaviour a granted
        // Mac gets.
        let runs = Rc::new(Cell::new(0));
        let counter = runs.clone();
        run_when_accessibility_granted(move || {
            counter.set(counter.get() + 1);
            Ok(())
        });
        if accessibility_trusted() {
            assert_eq!(runs.get(), 1, "a granted Mac installs the tap at boot");
            assert!(!accessibility_task_pending());
        } else {
            assert_eq!(runs.get(), 0, "an untrusted Mac must not run it yet");
            assert!(accessibility_task_pending());
            assert!(crate::health::is_raised(
                crate::health::Fault::AccessibilityMissing
            ));
            // Polling while still untrusted must keep waiting, not give up.
            poll_pending_accessibility();
            assert!(accessibility_task_pending());
            PENDING.with(|slot| *slot.borrow_mut() = None);
            crate::health::clear(crate::health::Fault::AccessibilityMissing);
        }

        // A task that fails leaves the fault raised, carrying its own reason.
        run_when_accessibility_granted(|| Err("tap refused".to_string()));
        if accessibility_trusted() {
            let problem = crate::health::current().expect("a failed install is a fault");
            assert_eq!(problem.fault, crate::health::Fault::AccessibilityMissing);
            assert_eq!(problem.detail, "tap refused");
            crate::health::clear(crate::health::Fault::AccessibilityMissing);
        } else {
            PENDING.with(|slot| *slot.borrow_mut() = None);
            crate::health::clear(crate::health::Fault::AccessibilityMissing);
        }
    }

    #[test]
    fn every_grant_has_a_label_for_the_setup_row() {
        for grant in [
            Grant::Granted,
            Grant::Undecided,
            Grant::Denied,
            Grant::Restricted,
            Grant::Unknown,
        ] {
            assert!(!grant.label().is_empty(), "{grant:?}");
        }
    }
}
