//! The pill poll timer: its adaptive interval, the health signature, and
//! the state transitions that show, hide and restyle the pill.
use super::*;

/// CFRunLoopTimer firing ~30 FPS. On every tick: update bar heights from
/// recent audio levels. On state transitions: show/hide pill, swap icon.
pub(super) fn install_poll_timer() {
    unsafe extern "C-unwind" fn timer_cb(_t: *mut CFRunLoopTimer, _info: *mut c_void) {
        let globals = match GLOBALS.get() {
            Some(g) => g,
            None => return,
        };
        let state = ui_channel::state();
        let now = state as u8;
        let prev = globals.last_state.swap(now, Ordering::SeqCst);
        if now != prev {
            apply_state_transition(globals, state);
        }
        if state == UiState::Recording {
            update_bars(globals);
        }
        if state.is_processing() {
            update_progress_pulse(globals);
        }
        if state == UiState::Success {
            // A settle can finish while the success pill is up; keep its Undo
            // affordance in step with the clipboard transaction gate.
            globals
                .pill
                .undo
                .setEnabled(!crate::clipboard_paste::transaction_pending());
        }
        if state.is_active() {
            update_live_feedback(globals, state);
            update_pill_hover(globals);
        }
        // About once a second, on this main-thread timer: follow a keyboard
        // layout switch (Text Input Source calls are main-thread only) and
        // notice Secure Input, which silences the hotkeys.
        let now_time = CFAbsoluteTimeGetCurrent();
        let last_slow = f64::from_bits(globals.last_slow_check.load(Ordering::Relaxed));
        if now_time - last_slow >= SLOW_CHECK_SECONDS - POLL_ACTIVE_SECONDS / 2.0 {
            globals
                .last_slow_check
                .store(now_time.to_bits(), Ordering::Relaxed);
            // The grant can land at any moment in System Settings. Installing
            // the event tap here is what makes "turn it on and it works"
            // true, with no relaunch.
            let was_waiting = crate::permissions::accessibility_task_pending();
            crate::permissions::poll_pending_accessibility();
            if was_waiting && !crate::permissions::accessibility_task_pending() {
                apply_state_transition(globals, state);
            }
            crate::permissions::refresh_microphone_health();
            super::setup::refresh();
            // The status item is only repainted on a UiState transition, and
            // most health changes do not move UiState: "Starting up…" clears
            // when the worker is ready, a microphone grant lands in System
            // Settings, the helper dies between utterances. Without this the
            // icon and its tooltip kept whatever the last transition painted
            // — an app that had finished booting still wore the danger glyph
            // and said "Starting up…" until the first dictation.
            let health = health_signature(crate::health::current().as_ref());
            if globals.last_health.swap(health, Ordering::SeqCst) != health {
                apply_state_transition(globals, state);
            }
            crate::key_layout::refresh();
            if let Some(on) = crate::secure_input::refresh() {
                eprintln!(
                    "[hotkey] Secure Input {} · hotkeys {}",
                    if on { "ON" } else { "off" },
                    if on { "paused (control socket still works)" } else { "back" }
                );
                apply_state_transition(globals, state);
            }
        }
        schedule_next_poll(state);
    }

    unsafe {
        let mut ctx = CFRunLoopTimerContext {
            version: 0,
            info: std::ptr::null_mut(),
            retain: None,
            release: None,
            copyDescription: None,
        };
        let timer = CFRunLoopTimer::new(
            None,
            CFAbsoluteTimeGetCurrent() + POLL_ACTIVE_SECONDS,
            POLL_ACTIVE_SECONDS,
            0,
            0,
            Some(timer_cb),
            &mut ctx,
        )
        .expect("CFRunLoopTimer::new");
        let run_loop = CFRunLoop::main().expect("main run loop");
        run_loop.add_timer(Some(&timer), kCFRunLoopCommonModes);
        // Kept for the life of the process (the run loop also retains it), so
        // `wake_poll_timer` may use the pointer from any thread.
        POLL_TIMER.store(
            CFRetained::into_raw(timer).as_ptr() as usize,
            Ordering::Release,
        );
    }
    ui_channel::set_wake_hook(wake_poll_timer);
}

/// The pill poll timer, stored once by `install_poll_timer` and never freed.
pub(super) static POLL_TIMER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Set by `wake_poll_timer`, cleared by `schedule_next_poll`: a state change
/// arrived while the timer callback was deciding its next fire date.
pub(super) static POLL_WAKE_PENDING: AtomicBool = AtomicBool::new(false);

/// Seconds between polls while an utterance or a result pill is on screen
/// (30 FPS: waveform, hover, live preview).
pub(super) const POLL_ACTIVE_SECONDS: f64 = 1.0 / 30.0;

/// Seconds between polls while idle. Nothing animates; the poll only runs
/// the once-a-second checks. A state change wakes the timer at once through
/// `wake_poll_timer`, so the pill still appears at the press.
pub(super) const POLL_IDLE_SECONDS: f64 = SLOW_CHECK_SECONDS;

/// Seconds between the slow checks (permissions, health, key layout,
/// Secure Input).
pub(super) const SLOW_CHECK_SECONDS: f64 = 1.0;

/// How long until the next poll for a given state. Pure, for tests.
pub(super) fn poll_interval(state: UiState) -> f64 {
    if state == UiState::Idle {
        POLL_IDLE_SECONDS
    } else {
        POLL_ACTIVE_SECONDS
    }
}

/// Called at the end of each poll on the main thread. A repeating CF timer
/// honours a later fire date set during its own callout, so idle polls drop
/// to once a second. The pending check runs after the new date is set: a wake
/// from another thread either lands after it (and wins) or set the flag
/// before it (and is replayed here), so a press is never delayed.
pub(super) fn schedule_next_poll(state: UiState) {
    let Some(timer) = poll_timer() else { return };
    let now = CFAbsoluteTimeGetCurrent();
    timer.set_next_fire_date(now + poll_interval(state));
    if POLL_WAKE_PENDING.swap(false, Ordering::SeqCst) {
        timer.set_next_fire_date(now);
    }
}

/// Fire the poll timer now. Safe from any thread: `ui_channel` calls it on
/// every state change, from the worker, the event tap and the audio path.
pub(super) fn wake_poll_timer() {
    let Some(timer) = poll_timer() else { return };
    POLL_WAKE_PENDING.store(true, Ordering::SeqCst);
    timer.set_next_fire_date(CFAbsoluteTimeGetCurrent());
    if let Some(main) = CFRunLoop::main() {
        main.wake_up();
    }
}

pub(super) fn poll_timer() -> Option<&'static CFRunLoopTimer> {
    let raw = POLL_TIMER.load(Ordering::Acquire);
    // SAFETY: the pointer came from `CFRetained::into_raw` and is never
    // released, so it stays valid for the process. CFRunLoop timer calls are
    // thread-safe in Core Foundation.
    (raw != 0).then(|| unsafe { &*(raw as *const CFRunLoopTimer) })
}

/// One byte standing for what the status item currently says about health:
/// `0` for "nothing wrong", otherwise the fault's discriminant plus one.
///
/// Only a fault that blocks dictation reaches the status item, so only those
/// move the signature — a lost microphone stream or a missing cleanup model
/// must not make the menu bar repaint once a second.
pub(super) fn health_signature(problem: Option<&crate::health::Problem>) -> u8 {
    match problem {
        Some(p) if p.fault.blocks_dictation() => p.fault as u8 + 1,
        _ => 0,
    }
}

pub(super) fn apply_state_transition(globals: &UiGlobals, state: UiState) {
    let mtm = match MainThreadMarker::new() {
        Some(m) => m,
        None => return,
    };

    // A fault outranks the utterance state: an idle-looking icon over a dead
    // hotkey is the one thing the menu bar must never show. The rule allows
    // `danger` on the status item for exactly this and for recording.
    let fault = crate::health::current().filter(|p| p.fault.blocks_dictation());

    // One glyph family — the waveform, matching the app icon. Idle is the
    // template ink; recording is the same shape tinted `danger`; processing is
    // the template at 40 %, which is the rule's "busy" treatment.
    let icon = if state == UiState::Recording || fault.is_some() {
        &globals.icon_recording
    } else if state.is_processing() || (state == UiState::Error || state.is_kept_result()) {
        &globals.icon_processing
    } else {
        &globals.icon_idle
    };
    if let Some(button) = globals.status_item.button(mtm) {
        if let Some(img) = icon {
            button.setImage(Some(img));
        } else {
            button.setTitle(&NSString::from_str(if fault.is_some() {
                "!"
            } else if state == UiState::Recording {
                "●"
            } else if state.is_processing() {
                "◌"
            } else if state == UiState::Error || state.is_kept_result() {
                "!"
            } else {
                "◯"
            }));
        }
        // Secure Input silences the hotkeys; while idle the glyph dims and the
        // tooltip says why (the menu carries the same line).
        let paused = state == UiState::Idle && crate::secure_input::active();
        button.setAlphaValue(if state.is_processing() || paused {
            PROCESSING_ALPHA
        } else {
            1.0
        });
        button.setToolTip(Some(&NSString::from_str(&format!(
            "Local Dictation · {}",
            match (&fault, paused) {
                // A word beside the colour: the rule never lets a state be
                // carried by the tint alone.
                (Some(problem), _) => problem.pill_line(),
                (None, true) => crate::secure_input::PAUSED_TEXT.to_string(),
                (None, false) =>
                    pill_status(state, globals.show_processing_detail).to_string(),
            }
        ))));
    }
    globals.pill_hovered.store(false, Ordering::Relaxed);
    globals.pill.storage_warning.set(false);
    globals.pill.status.setToolTip(None);
    if let Ok(mut feedback) = globals.pill_feedback.lock() {
        *feedback = None;
    }
    unsafe {
        let _: () = msg_send![&*globals.pill.dot, setAccessibilityLabel: &*NSString::from_str(pill_status(state, globals.show_processing_detail))];
    }

    match state {
        UiState::Idle => {
            // A dictation just completed (or we were cancelled) — refresh the
            // "Last: …" preview so the menu shows the newest text.
            {
                globals
                    .last_item
                    .setTitle(&NSString::from_str(&last_dictation_label()));
            }
            ui_channel::reset_levels();
            collapse_bars(globals);
            globals.pill.status.setStringValue(&NSString::from_str(""));
            set_pill_layout(globals, PillLayout::Recording);
            globals.pill_window.orderOut(None);
        }
        UiState::Recording => {
            globals.pill.waveform.set_muted(false);
            globals.pill.status.setStringValue(&NSString::from_str(""));
            set_pill_layout(globals, PillLayout::Recording);
            set_cancel_hover(&globals.pill, state, false);
            show_pill(globals);
        }
        UiState::Processing | UiState::Transcribing | UiState::Cleaning | UiState::Pasting => {
            globals
                .pill
                .status
                .setStringValue(&NSString::from_str(pill_status(
                    state,
                    globals.show_processing_detail,
                )));
            set_pill_layout(globals, PillLayout::Processing);
            set_cancel_hover(&globals.pill, state, false);
            show_pill(globals);
        }
        UiState::Copied => {
            globals
                .pill
                .status
                .setStringValue(&NSString::from_str("Copied to clipboard"));
            set_pill_layout(globals, PillLayout::Copied);
            show_pill(globals);
        }
        UiState::Success => {
            let words = ui_channel::last_dictation().split_whitespace().count();
            let text = if words == 1 {
                "Landed · 1 word".to_string()
            } else {
                format!("Landed · {words} words")
            };
            globals
                .pill
                .status
                .setStringValue(&NSString::from_str(&text));
            globals
                .pill
                .hint
                .setStringValue(&NSString::from_str("Undo"));
            set_pill_layout(globals, PillLayout::Success);
            show_pill(globals);
            // Undo is only safe once no clipboard transaction is still
            // settling: while one is in flight the visible text may belong to
            // that write, so an early ⌘Z would revert unrelated prior content.
            globals
                .pill
                .undo
                .setEnabled(!crate::clipboard_paste::transaction_pending());
        }
        UiState::Error => {
            globals
                .pill
                .status
                .setStringValue(&NSString::from_str("Couldn’t transcribe"));
            // A real ASR failure: offer the one quick action that helps,
            // Retry. It is a click, never an automatic retry.
            globals
                .pill
                .retry_hint
                .setStringValue(&NSString::from_str("Retry"));
            globals
                .pill
                .hint
                .setStringValue(&NSString::from_str("History"));
            set_hint_action(globals, sel!(openHistory:), "Open Dictation History");
            set_pill_layout(globals, PillLayout::Retryable);
            show_pill(globals);
        }
        UiState::RewriteFailed => {
            // The transform failed; the selection was left untouched. Say that
            // plainly instead of the generic transcription error. No Retry
            // here: re-running ASR would not fix a rewrite failure.
            globals.pill.status.setStringValue(&NSString::from_str(
                "Couldn’t rewrite · Selection unchanged",
            ));
            globals
                .pill
                .hint
                .setStringValue(&NSString::from_str("History"));
            set_hint_action(globals, sel!(openHistory:), "Open Dictation History");
            set_pill_layout(globals, PillLayout::Error);
            show_pill(globals);
        }
        UiState::NothingToCopy => {
            globals
                .pill
                .status
                .setStringValue(&NSString::from_str(NOTHING_TO_COPY_TEXT));
            set_pill_layout(globals, PillLayout::Copied);
            show_pill(globals);
        }
        UiState::Unavailable => {
            // The reason lives in the health channel, not in the state byte.
            let line = crate::health::current()
                .map(|p| p.pill_line())
                .unwrap_or_else(|| "Dictation is not ready".to_string());
            globals.pill.status.setStringValue(&NSString::from_str(&line));
            globals
                .pill
                .hint
                .setStringValue(&NSString::from_str("Setup"));
            // The same hint slot, pointed at the window that fixes it rather
            // than at History, which holds nothing relevant here.
            set_hint_action(globals, sel!(openSetup:), "Open Setup & permissions");
            set_pill_layout(globals, PillLayout::Error);
            show_pill(globals);
        }
        UiState::NoEditableField
        | UiState::PasteFailed
        | UiState::HeldCopied
        | UiState::PasteFailedCopied
        | UiState::LimitReached
        | UiState::LimitReachedCopied => {
            // The words exist but did not reach the app. One sentence: what
            // happened, then where the text is ("Copied, press ⌘V" when it is
            // on the clipboard). The Error layout carries the History button,
            // so the pill itself is a route to the transcript. It stays about
            // four seconds, or until the next key press.
            globals.pill.status.setStringValue(&NSString::from_str(
                crate::last_dictation::kept_pill_text(state).unwrap_or("Text kept · Copy from menu"),
            ));
            globals
                .pill
                .hint
                .setStringValue(&NSString::from_str("History"));
            set_hint_action(globals, sel!(openHistory:), "Open Dictation History");
            set_pill_layout(globals, PillLayout::Error);
            show_pill(globals);
        }
    }
}

/// Point the error pill's hint button at one action. The slot is shared by the
/// kept-result pills (History) and the not-ready pill (Setup), so it is always
/// set explicitly rather than left on whatever the previous state wanted.
pub(super) fn set_hint_action(globals: &UiGlobals, action: objc2::runtime::Sel, tooltip: &str) {
    unsafe {
        let _: () = msg_send![&*globals.pill.history, setAction: action];
    }
    globals
        .pill
        .history
        .setToolTip(Some(&NSString::from_str(tooltip)));
}
