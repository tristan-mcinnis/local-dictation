//! The push-to-talk event tap: its install, the callback that turns key
//! events into latch inputs, and the re-arm after macOS disables it.
use super::*;

// Raw CFMachPortRef of the live event tap, stashed after creation so the
// callback can re-enable the tap if macOS ever disables it. Active taps can be
// killed by a slow callback or certain user input; without re-enabling, the
// hotkey would silently die. Null until the tap exists.
pub(super) static TAP_PORT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// How many times macOS has disabled the event tap. A disabled tap drops every
/// key event until it is re-armed, so this count is the difference between
/// "nothing was pressed" and "we were not listening".
pub(super) static TAP_DISABLES: AtomicU64 = AtomicU64::new(0);

/// Set by the worker when it ends a recording without a key (the recording
/// limit). The tap owns the hold/latch state, so it folds the reset in on its
/// next event: otherwise a lost key-up leaves `ST_HOLDING` behind and the next
/// real press is ignored as a duplicate.
pub(super) static PTT_RESYNC: AtomicBool = AtomicBool::new(false);

/// Apply a pending [`PTT_RESYNC`]: back to idle, key considered up. If the key
/// is in fact still held, its eventual release is a no-op edge.
pub(super) fn apply_ptt_resync(pending: &AtomicBool, state: &AtomicU8, hotkey_down: &AtomicBool) -> bool {
    if !pending.swap(false, Ordering::SeqCst) {
        return false;
    }
    state.store(ST_IDLE, Ordering::SeqCst);
    hotkey_down.store(false, Ordering::SeqCst);
    true
}

extern "C" {
    /// CoreGraphics: enable or disable a previously-created event tap.
    /// `void CGEventTapEnable(CFMachPortRef tap, bool enable);`
    fn CGEventTapEnable(tap: *const c_void, enable: bool);

    /// CoreGraphics: the live modifier state for a source.
    /// `CGEventFlags CGEventSourceFlagsState(CGEventSourceStateID stateID);`
    /// Read with `kCGEventSourceStateHIDSystemState` this is what the hardware
    /// reports right now, independent of the flags a single event carries.
    fn CGEventSourceFlagsState(state_id: i32) -> u64;
}

/// The hardware's live modifier flags, bypassing the event under inspection.
/// A physical key press shows up here; a state change another process injected
/// into the event stream does not.
pub(super) fn hid_modifier_flags() -> u64 {
    unsafe { CGEventSourceFlagsState(CGEventSourceStateID::HIDSystemState as i32) }
}

/// Convert a hotkey modifier mask (raw CGEventFlags bits) into a CGEventFlags
/// set. Used to turn the settings-stored `u64` into the flag bits the event tap
/// checks against the live key-event flags.
pub(super) fn modifiers_from_mask(mask: u64) -> CGEventFlags {
    let mut f = CGEventFlags::empty();
    if mask & crate::hotkeys::SHIFT != 0 {
        f |= CGEventFlags::CGEventFlagShift;
    }
    if mask & crate::hotkeys::CONTROL != 0 {
        f |= CGEventFlags::CGEventFlagControl;
    }
    if mask & crate::hotkeys::OPTION != 0 {
        f |= CGEventFlags::CGEventFlagAlternate;
    }
    if mask & crate::hotkeys::COMMAND != 0 {
        f |= CGEventFlags::CGEventFlagCommand;
    }
    if mask & crate::hotkeys::CAPS_LOCK != 0 {
        f |= CGEventFlags::CGEventFlagAlphaShift;
    }
    f
}

/// Install the push-to-talk event tap on the main run loop, or arm it to
/// install the moment Accessibility is granted. Returns whether the hotkey
/// is modifier-only, which the boot log reports.
pub(super) fn install_event_tap(
    tx: &mpsc::Sender<DaemonEvent>,
    hotkey: crate::hotkeys::Hotkey,
    double_tap_latch: bool,
    copy_last: Option<crate::hotkeys::Hotkey>,
) -> bool {
    // The CGEventTap callback runs on the main CFRunLoop thread. Keep it
    // FAST — just send to the worker.
    let tx_for_callback = tx.clone();
    let hotkey_keycode = hotkey.keycode;
    let modifier_only = hotkey.is_modifier_only();
    // A modifier-only hotkey (Right Option etc.) is detected via FlagsChanged
    // on its own modifier bit; a chord hotkey is detected via KeyDown/KeyUp of
    // the non-modifier key while the required modifiers are held.
    let required_mask = if modifier_only {
        CGEventFlags::empty()
    } else {
        modifiers_from_mask(hotkey.modifiers)
    };

    // Hands-free latch state shared with the callback. `state` is the ST_*
    // machine driven by `ptt_transition`; `space_down` lets us swallow the
    // Space key-up that matches a swallowed key-down so no stray space ever
    // lands in the focused field.
    let state = Arc::new(AtomicU8::new(ST_IDLE));
    let space_down = Arc::new(AtomicBool::new(false));
    let state_cb = state.clone();
    let space_down_cb = space_down.clone();
    // When Space was last pressed. The keyboard scans keys in hardware order,
    // so the Space half of Option+Space can reach the tap before the Option
    // edge; the press handler consults this to still recognise the chord
    // (see `CHORD_WINDOW`).
    let last_space_down = Arc::new(Mutex::new(None::<Instant>));
    let last_space_down_cb = last_space_down.clone();
    // Track physical edges separately from latch state. Repeated key-downs
    // must never be mistaken for a new tap that stops hands-free capture.
    let hotkey_down = Arc::new(AtomicBool::new(false));
    let hotkey_down_cb = hotkey_down.clone();

    // Double-tap-to-latch: the callback records press/release timing to
    // recognise a hotkey double-tap (the no-Spacebar route into hands-free).
    let detector_cb = Arc::new(Mutex::new(DoubleTapDetector::default()));

    // Drive the latch state machine once, forwarding any Start/Stop decision to
    // the worker. Shared by the modifier-only and chord hotkey detection paths.
    let drive = {
        let tx = tx_for_callback.clone();
        let scb = state_cb.clone();
        move |input: PttInput| {
            let prev = scb.load(Ordering::SeqCst);
            let (next, decision) = ptt_transition(prev, input);
            scb.store(next, Ordering::SeqCst);
            if !matches!(decision, PttDecision::Nothing | PttDecision::SwallowSpace) || prev != next {
                eprintln!(
                    "[hotkey] {} {input:?} · state {prev}→{next} · {decision:?}",
                    stamp()
                );
            }
            match decision {
                PttDecision::Start { transform } => {
                    if !report_if_unavailable() {
                        let _ = tx.send(DaemonEvent::StartRecording { transform });
                    }
                }
                // Double-tap from idle: begin recording AND cue latched,
                // mirroring the Space-chord Start + LatchArmed sequence.
                PttDecision::StartLatched { transform } => {
                    if !report_if_unavailable() {
                        let _ = tx.send(DaemonEvent::StartRecording { transform });
                        let _ = tx.send(DaemonEvent::LatchArmed);
                    }
                }
                PttDecision::Stop => {
                    let why = if prev == ST_LATCHED { "hotkey tap ended hands-free" } else { "hotkey released" };
                    eprintln!("[hotkey] {} stop · {why}", stamp());
                    let _ = tx.send(DaemonEvent::StopRecording);
                }
                _ => {}
            }
        }
    };

    // The second global hotkey: "Copy last dictation". The callback only sends
    // on this channel; the clipboard write and the pill happen on a small
    // dedicated thread, never behind the worker (which blocks during ASR).
    match copy_last {
        Some(h) => eprintln!("[boot] copy-last  hotkey {}", h.display()),
        None => eprintln!("[boot] copy-last  hotkey off"),
    }
    let (copy_tx, copy_rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        while copy_rx.recv().is_ok() {
            crate::recovery::copy_last_dictation("hotkey");
        }
    });
    let copy_key_down = Arc::new(AtomicBool::new(false));
    let copy_key_down_cb = copy_key_down.clone();
    let resync_state = state.clone();
    let resync_down = hotkey_down.clone();
    let own_pid = i64::from(std::process::id());

    // Install CGEventTap manually so we can hand the main thread over to
    // NSApplication.run() (needed for the menu-bar item + floating pill).
    //
    // This is an ACTIVE tap (not ListenOnly) because hands-free mode must
    // *swallow* the Space key while the PTT key is held — otherwise the held
    // modifier + Space would insert a character (e.g. Option+Space → nbsp).
    // The callback stays trivial (atomics + non-blocking channel sends) so it
    // can never stall the input stream; TapDisabled events re-enable the tap.
    //
    // The whole installation is deferred into a closure because it is the one
    // part of the daemon that needs Accessibility. `run_when_accessibility_
    // granted` runs it now when the grant is already there, and otherwise
    // holds it until the Setup window's polling sees the grant land — no
    // relaunch, and no exit in the meantime.
    let install_tap = move || -> Result<(), String> {
    let tap = CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        vec![
            CGEventType::FlagsChanged,
            CGEventType::KeyDown,
            CGEventType::KeyUp,
        ],
        move |_proxy, etype, event| {
            match etype {
                CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                    // macOS disabled the tap. Every key event in the disabled
                    // window is dropped, so this must be visible in the log: a
                    // dropped release leaves the hold state stale and the next
                    // edge is then read against the wrong physical state.
                    // Re-arm immediately; the callback stays short.
                    let n = TAP_DISABLES.fetch_add(1, Ordering::SeqCst) + 1;
                    eprintln!(
                        "[hotkey] {} TAP DISABLED ({etype:?}) · #{n} · re-arming",
                        stamp()
                    );
                    let p = TAP_PORT.load(Ordering::SeqCst);
                    if !p.is_null() {
                        unsafe { CGEventTapEnable(p, true) };
                    }
                }
                CGEventType::FlagsChanged => {
                    apply_ptt_resync(&PTT_RESYNC, &resync_state, &resync_down);
                    // Only the modifier-only hotkey uses FlagsChanged; a chord
                    // hotkey reads its modifiers off the KeyDown event's flags.
                    if !modifier_only {
                        return CallbackResult::Keep;
                    }
                    let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                    if keycode != hotkey_keycode {
                        return CallbackResult::Keep;
                    }
                    let flags = event.get_flags();
                    let raw = flags.bits();
                    // Provenance probe. Every edge is logged with where it came
                    // from, because a stop with no finger behind it is
                    // otherwise indistinguishable from a real tap:
                    //   pid/state/uid — who posted it (0 = the HID system)
                    //   read          — device bit (side known) vs aggregate
                    //   hw            — the HID system's flags for the same key
                    // `hw` trails the event by a few milliseconds as a rule: the
                    // tap sits at the head of the stream, so it sees each edge
                    // before the HID state commits it. A mismatch is therefore
                    // expected on every edge and is NOT evidence of injection.
                    // What it does prove is that the key really was down for the
                    // interval the tap believed it was.
                    let src_pid = event.get_integer_value_field(EventField::EVENT_SOURCE_UNIX_PROCESS_ID);
                    let src_state = event.get_integer_value_field(EventField::EVENT_SOURCE_STATE_ID);
                    let src_uid = event.get_integer_value_field(EventField::EVENT_SOURCE_USER_ID);
                    let hw = hid_modifier_flags();
                    let down = crate::hotkeys::modifier_key_is_down(hotkey_keycode, raw);
                    eprintln!(
                        "[hotkey] {} edge · key {hotkey_keycode:#x} · {} · flags {raw:#x} · read {} · hw {hw:#x} · src pid {src_pid} state {src_state} uid {src_uid}",
                        stamp(),
                        if down { "DOWN" } else { "UP" },
                        if crate::hotkeys::modifier_key_read_via_aggregate(hotkey_keycode, raw) {
                            "aggregate"
                        } else {
                            "device-bit"
                        },
                    );
                    // Only a physical key controls the session. A FlagsChanged
                    // posted by another process (AppleScript, an automation
                    // tool, a remote's companion app) carries that process's
                    // PID here; a physical keyboard carries 0.
                    // Without this guard, any app posting Right Option ends a
                    // hands-free session and pastes the transcript wherever
                    // focus is. A poster that leaves the PID unset still slips
                    // through, so `src pid 0` is a weak signal, not a strong one.
                    if src_pid != 0 {
                        eprintln!(
                            "[hotkey] {} ignored synthesized hotkey event from pid {src_pid} (flags {raw:#x})",
                            stamp()
                        );
                        return CallbackResult::Keep;
                    }
                    // One physical edge only. Repeated key-downs must never be
                    // mistaken for a fresh tap that stops hands-free capture;
                    // log the repeats so a stuck or bouncing device is visible.
                    if !hotkey_edge(&hotkey_down_cb, down) {
                        eprintln!(
                            "[hotkey] {} duplicate {} edge ignored · flags {raw:#x} · hw {hw:#x}",
                            stamp(),
                            if down { "down" } else { "up" }
                        );
                        return CallbackResult::Keep;
                    }
                    if down {
                        // Shift co-held at press → transform mode (rewrite the
                        // selection). The user holds Shift *before* the PTT key.
                        let press = Instant::now();
                        let double_tap = double_tap_latch
                            && detector_cb.lock().unwrap().on_press(press);
                        // Did a Space just precede this edge? The keyboard scans
                        // keys in hardware order, so the chord can arrive
                        // Space-first; without this the press reads as a bare
                        // hold and the session dies the moment the keys come up.
                        let space_first = last_space_down_cb
                            .lock()
                            .unwrap()
                            .map(|t| t.elapsed() <= CHORD_WINDOW)
                            .unwrap_or(false);
                        if space_first {
                            eprintln!(
                                "[hotkey] {} space landed before the press · reading it as the hands-free chord",
                                stamp()
                            );
                            drive(PttInput::HotkeyChordSpace);
                        } else {
                            drive(PttInput::HotkeyPress {
                                shift: flags.contains(CGEventFlags::CGEventFlagShift),
                                double_tap,
                            });
                        }
                    } else {
                        detector_cb.lock().unwrap().on_release(Instant::now());
                        drive(PttInput::HotkeyRelease);
                    }
                    // FlagsChanged is never swallowed — let the modifier through.
                }
                CGEventType::KeyDown => {
                    apply_ptt_resync(&PTT_RESYNC, &resync_state, &resync_down);
                    let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                    let autorepeat =
                        event.get_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT) != 0;
                    // "Copy last dictation". Exact modifiers, so our own
                    // synthesized ⌘V can never match; events we posted are
                    // ignored outright. Autorepeats of the held chord are
                    // swallowed without copying again.
                    if let Some(chord) = copy_last {
                        if crate::hotkeys::copy_last_matches(&chord, keycode, event.get_flags().bits())
                            && event.get_integer_value_field(EventField::EVENT_SOURCE_UNIX_PROCESS_ID) != own_pid
                        {
                            if !autorepeat && !copy_key_down_cb.swap(true, Ordering::SeqCst) {
                                let _ = copy_tx.send(());
                            }
                            return CallbackResult::Drop;
                        }
                    }
                    // A fresh key press dismisses a "Copied, press ⌘V" pill.
                    if !autorepeat {
                        ui_channel::key_pressed();
                    }
                    // Escape cancels an in-flight utterance (recording or pending
                    // transcription) and prevents any late insertion. It swallows
                    // the keystroke while active so it never leaks into the field;
                    // when idle it passes through so normal Escape use (dismiss a
                    // dialog, end a drag) is unaffected.
                    if keycode == ESCAPE_KEYCODE {
                        // An utterance is in flight while the worker is capturing
                        // (`Recording`) or running the release pipeline
                        // (`Processing`). This is the reliable signal — the worker
                        // sets it at begin and reverts to `Idle`/`Success`/
                        // `Error` at settle, so an idle Escape (no utterance) is
                        // never mistaken for a cancel.
                        let in_flight = ui_channel::state().is_cancellable();
                        // Only swallow + wake + unwind when a cancel was actually
                        // ACCEPTED. `request_cancel` returns false when idle or when
                        // the utterance already committed to write — in that case the
                        // Escape must pass through untouched, not be silently eaten
                        // (and the PTT latch must not be reset out from under a live
                        // post-commit settle).
                        if in_flight && ui_channel::request_cancel() {
                            // Wake a worker blocked waiting for an event (recording)
                            // so capture stops now rather than at release. Harmless
                            // if it's already mid-pipeline: the out-of-band gate is
                            // what suppresses the eventual injection.
                            let _ = tx_for_callback.send(DaemonEvent::CancelRecording);
                            // Cancellation ends the session, so unwind the PTT latch
                            // state machine too — otherwise a hands-free session left
                            // a `ST_LATCHED` the next PTT tap would have to clear.
                            state_cb.store(ST_IDLE, Ordering::SeqCst);
                            return CallbackResult::Drop;
                        }
                        return CallbackResult::Keep;
                    }
                    if !modifier_only && keycode == hotkey_keycode {
                        // Chord hotkey: the non-modifier key went down with the
                        // required modifiers held. Swallow it so the keystroke
                        // never reaches the app (no stray character / menu).
                        // The four modifier bits must EQUAL the chord's (an
                        // extra Shift is the transform gesture), so ⌘⇧D does
                        // not also swallow another app's ⌘⇧⌥D.
                        let flags = event.get_flags();
                        if crate::hotkeys::chord_modifiers_match(flags.bits(), hotkey.modifiers) {
                            // Key repeat is still the same physical press. In
                            // double-tap hands-free, treating it as a fresh tap
                            // would immediately stop capture before release.
                            if !hotkey_edge(&hotkey_down_cb, true) {
                                return CallbackResult::Drop;
                            }
                            let shift = flags.contains(CGEventFlags::CGEventFlagShift)
                                && !required_mask.contains(CGEventFlags::CGEventFlagShift);
                            let press = Instant::now();
                            let double_tap = double_tap_latch
                                && detector_cb.lock().unwrap().on_press(press);
                            drive(PttInput::HotkeyPress { shift, double_tap });
                            return CallbackResult::Drop;
                        }
                        return CallbackResult::Keep;
                    }
                    if modifier_only && keycode == SPACE_KEYCODE {
                        // Half of the Option+Space chord. Remember when it landed
                        // so the modifier edge that follows can still recognise
                        // the pair if the hardware sent Space first.
                        *last_space_down_cb.lock().unwrap() = Some(Instant::now());
                        let prev = state_cb.load(Ordering::SeqCst);
                        let (next, decision) = ptt_transition(prev, PttInput::SpaceDown);
                        state_cb.store(next, Ordering::SeqCst);
                        let swallowed = matches!(
                            decision,
                            PttDecision::ArmLatch | PttDecision::SwallowSpace
                        );
                        if swallowed {
                            space_down_cb.store(true, Ordering::SeqCst);
                        }
                        // Space used to be invisible in this log, so a chord that
                        // never armed left no trace of why. Only dictation-relevant
                        // Spaces are logged; ordinary typing would flood the file.
                        if swallowed || prev != ST_IDLE {
                            eprintln!(
                                "[hotkey] {} space DOWN · state {prev}→{next} · {decision:?} · {}",
                                stamp(),
                                if swallowed { "swallowed" } else { "passed through" }
                            );
                        }
                        match decision {
                            PttDecision::ArmLatch => {
                                // Chord recognised: cue + swallow so the
                                // Option+Space character never reaches the field.
                                let _ = tx_for_callback.send(DaemonEvent::LatchArmed);
                                return CallbackResult::Drop;
                            }
                            PttDecision::SwallowSpace => return CallbackResult::Drop,
                            _ => {}
                        }
                    }
                }
                CGEventType::KeyUp => {
                    let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                    // Swallow the key-up that matches a swallowed copy-last
                    // key-down, whatever modifiers are still held by now.
                    if copy_last.is_some_and(|c| c.keycode == keycode)
                        && copy_key_down_cb.swap(false, Ordering::SeqCst)
                    {
                        return CallbackResult::Drop;
                    }
                    if !modifier_only && keycode == hotkey_keycode {
                        // Chord hotkey release: swallow the matching key-up.
                        if hotkey_edge(&hotkey_down_cb, false) {
                            detector_cb.lock().unwrap().on_release(Instant::now());
                            drive(PttInput::HotkeyRelease);
                            return CallbackResult::Drop;
                        }
                        return CallbackResult::Keep;
                    }
                    if modifier_only && keycode == SPACE_KEYCODE {
                        let was_swallowed = space_down_cb.swap(false, Ordering::SeqCst);
                        if was_swallowed || state_cb.load(Ordering::SeqCst) != ST_IDLE {
                            eprintln!(
                                "[hotkey] {} space UP · {}",
                                stamp(),
                                if was_swallowed { "swallowed" } else { "passed through" }
                            );
                        }
                        // Swallow the key-up matching a swallowed key-down.
                        if was_swallowed {
                            return CallbackResult::Drop;
                        }
                    }
                }
                _ => {}
            }
            CallbackResult::Keep
        },
    )
    .map_err(|_| {
        "could not install the event tap — Accessibility is missing or was revoked".to_string()
    })?;

    // Stash the tap's port so the callback can re-enable it after a
    // system-initiated disable (see TapDisabled handling above).
    TAP_PORT.store(
        tap.mach_port().as_concrete_TypeRef() as *mut c_void,
        Ordering::SeqCst,
    );

    let loop_source = tap
        .mach_port()
        .create_runloop_source(0)
        .map_err(|_| "create_runloop_source failed".to_string())?;
    CFRunLoop::get_current().add_source(&loop_source, unsafe { kCFRunLoopCommonModes });
    tap.enable();
    // Leak the tap — it must outlive the run loop. Safe to drop only on
    // process exit (which NSApp.terminate handles).
    std::mem::forget(tap);
    std::mem::forget(loop_source);
    eprintln!("[boot] event tap  installed");
    Ok(())
    };
    crate::permissions::run_when_accessibility_granted(install_tap);
    modifier_only
}
