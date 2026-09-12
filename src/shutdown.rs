//! Ordered model release at quit.
//!
//! macOS quits this app through `-[NSApplication terminate:]`, which calls
//! `exit()` from inside `NSApp.run()`. `exit()` runs the C++ static
//! destructors, and ggml keeps its Metal devices in a function-local
//! `static std::vector<ggml_metal_device_ptr>` (`ggml_metal_device_get`). That
//! destructor reaches `ggml_metal_rsets_free`, which asserts every Metal buffer
//! was already freed:
//!
//! ```text
//! GGML_ASSERT([rsets->data count] == 0) failed   (ggml-metal-device.m:612)
//! ```
//!
//! So a live `LlamaModel` / `LlamaContext` at exit aborts the process — the
//! quit crash (SIGABRT) in the 2026-09-06 / 2026-09-09 reports. The daemon's
//! models live on the worker thread, and the code after
//! `menubar::init_and_run()` never runs (terminate: exits from inside
//! `NSApp.run()`), so nothing ever dropped them.
//!
//! This module is the handshake that fixes the ordering: the daemon registers a
//! request hook, and the AppKit delegate calls [`release_models`] from
//! `applicationWillTerminate:` — inside terminate:, *before* exit(), while
//! Metal is still alive. The worker drops its engine and calls
//! [`mark_released`].
//!
//! Remove this once ggml can free its Metal devices at exit without asserting
//! (or exposes an explicit backend teardown that llama-cpp-2 runs on drop).

use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// Asks the model owner (the daemon worker thread) to drop everything.
/// `None` when no owner is running — the render-ui / CLI paths hold no models.
static REQUEST: Mutex<Option<Box<dyn Fn() + Send>>> = Mutex::new(None);

/// Set once the owner has actually dropped its models.
static RELEASED: Mutex<bool> = Mutex::new(false);
static RELEASED_CV: Condvar = Condvar::new();

/// Register how to ask the model owner to shut down. Called once by the daemon
/// before it hands the main thread to AppKit.
pub fn on_release_requested(request: impl Fn() + Send + 'static) {
    if let Ok(mut slot) = REQUEST.lock() {
        *slot = Some(Box::new(request));
    }
}

/// Called by the model owner once its models are dropped and Metal is clear.
pub fn mark_released() {
    if let Ok(mut done) = RELEASED.lock() {
        *done = true;
    }
    RELEASED_CV.notify_all();
}

/// Ask the model owner to release, and block until it has — or until `timeout`.
///
/// Returns true when the models are known to be gone (so exit is safe), false
/// on timeout. Also true when no owner was ever registered: nothing holds a
/// Metal buffer, so exit is safe anyway.
pub fn release_models(timeout: Duration) -> bool {
    let request = match REQUEST.lock() {
        Ok(mut slot) => slot.take(),
        Err(_) => None,
    };
    let Some(request) = request else {
        return true;
    };
    request();

    let Ok(done) = RELEASED.lock() else {
        return false;
    };
    match RELEASED_CV.wait_timeout_while(done, timeout, |done| !*done) {
        Ok((done, _)) => *done,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// Reset the process-wide handshake state between scenarios.
    fn reset() {
        *REQUEST.lock().unwrap() = None;
        *RELEASED.lock().unwrap() = false;
    }

    /// One test, because the handshake is process-wide state (it exists to be
    /// called once per process) and parallel test threads would race it.
    #[test]
    fn quit_waits_for_the_release_and_reports_whether_it_happened() {
        // 1. No owner: nothing holds a Metal buffer, so exit is already safe.
        reset();
        assert!(release_models(Duration::from_millis(50)));

        // 2. An owner that finishes: quit must block until the models are gone,
        //    which is the whole point — exiting over live buffers is the crash.
        reset();
        let asked = Arc::new(AtomicBool::new(false));
        let asked_cb = asked.clone();
        on_release_requested(move || {
            asked_cb.store(true, Ordering::SeqCst);
            // Stands in for the worker thread finishing its drop.
            std::thread::spawn(mark_released);
        });
        assert!(
            release_models(Duration::from_secs(5)),
            "quit must wait for the models to be released, not exit over them"
        );
        assert!(asked.load(Ordering::SeqCst), "the owner must be asked to stop");

        // 3. An owner that never acks (wedged mid-utterance): the caller must
        //    learn the release did NOT happen rather than assume it did.
        reset();
        on_release_requested(|| {});
        assert!(!release_models(Duration::from_millis(50)));
        reset();
    }
}
