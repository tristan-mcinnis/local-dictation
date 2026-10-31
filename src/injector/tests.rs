#[test]
fn hold_log_line_names_the_app() {
    let l = hold_log_line("no focused AX element (AXError -25212); holding", "com.apple.Safari");
    assert!(l.starts_with("[inject] "));
    assert!(l.ends_with("app=com.apple.Safari"));
    assert!(l.contains("AXError -25212"));
}

#[test]
fn hold_log_line_handles_missing_app() {
    assert!(hold_log_line("holding", "").contains("app=unknown"));
    assert!(hold_log_line("holding", "  ").ends_with("app=unknown"));
}

#[test]
fn app_label_prefers_bundle_then_exe_then_unknown() {
    assert_eq!(app_label(Some("com.x.App"), Some("/A/B/App")), "com.x.App");
    assert_eq!(app_label(None, Some("/Applications/Foo.app/Contents/MacOS/Foo\n")), "Foo");
    assert_eq!(app_label(Some(" "), Some("/a/b/Bar")), "Bar");
    assert_eq!(app_label(None, None), "unknown");
    assert_eq!(app_label(None, Some("")), "unknown");
}

#[test]
fn every_hold_and_fallback_log_line_names_the_app() {
    const MARKERS: [&str; 5] =
        ["holding", "text kept", "transcript kept", "result kept", "clipboard paste"];
    let files = [
        ("injector/mod.rs", include_str!("mod.rs")),
        ("injector/pid_class.rs", include_str!("pid_class.rs")),
        ("injector/imp.rs", include_str!("imp.rs")),
        ("daemon/capture.rs", include_str!("../daemon/capture.rs")),
        ("daemon/cleanup.rs", include_str!("../daemon/cleanup.rs")),
        ("daemon/deliver.rs", include_str!("../daemon/deliver.rs")),
        ("daemon/listen.rs", include_str!("../daemon/listen.rs")),
        ("daemon/recover.rs", include_str!("../daemon/recover.rs")),
        ("daemon/release.rs", include_str!("../daemon/release.rs")),
        ("daemon/tail.rs", include_str!("../daemon/tail.rs")),
        ("daemon/transcribe.rs", include_str!("../daemon/transcribe.rs")),
        ("daemon/transform.rs", include_str!("../daemon/transform.rs")),
        ("daemon/worker.rs", include_str!("../daemon/worker.rs")),
    ];
    let mut bad = Vec::new();
    for (name, src) in files {
        // Production code only: stop at the test module.
        let prod = src.split("\n#[cfg(test)]").next().unwrap_or(src);
        let lines: Vec<&str> = prod.lines().collect();
        for (n, line) in lines.iter().enumerate() {
            let t = line.trim_start();
            if t.starts_with("//") || !line.contains('"') || line.contains("[limit]") || !MARKERS.iter().any(|m| line.contains(m)) {
                continue;
            }
            // Statement window: this line and the next 6 (multi-line eprintln!/format!).
            let from = n.saturating_sub(3);
            let win = lines[from..(n + 7).min(lines.len())].join("\n");
            if !(win.contains("hold_log_line") || win.contains("app=")) {
                bad.push(format!("{name}:{}", n + 1));
            }
        }
    }
    assert!(bad.is_empty(), "hold/fallback log lines without an app: {bad:?}");
}


fn app(pid: i32, bundle: &str) -> TargetApp {
    TargetApp { pid, bundle_id: Some(bundle.to_string()) }
}

#[test]
fn the_press_time_app_is_used_while_it_is_still_in_front() {
    let got = resolve_target_app(Some(app(10, "com.mitchellh.ghostty")), Some(10), |_| {
        panic!("no second lookup when the app did not change")
    });
    assert_eq!(got, Some(app(10, "com.mitchellh.ghostty")));
}

#[test]
fn an_app_switch_during_the_hold_uses_the_app_the_text_goes_to() {
    // Regression (INJ:TGT-01): the old app's raw/cleanup choice was applied
    // to the new app, while the tone was read from yet another moment.
    let got = resolve_target_app(Some(app(10, "com.mitchellh.ghostty")), Some(22), |pid| {
        assert_eq!(pid, 22);
        Some("com.apple.mail".to_string())
    });
    assert_eq!(got, Some(app(22, "com.apple.mail")));
}

#[test]
fn a_failed_press_capture_still_identifies_the_app() {
    // Regression: a terminal with no focused AX element had no target PID,
    // so LLM-cleaned prose was pasted into the shell.
    let got = resolve_target_app(None, Some(31), |_| Some("com.googlecode.iterm2".into()));
    assert_eq!(got, Some(app(31, "com.googlecode.iterm2")));
    // Nothing readable at all: no target, and no invented one.
    assert_eq!(resolve_target_app(None, None, |_| None), None);
    // Frontmost unreadable at release: keep what the press saw.
    assert_eq!(resolve_target_app(Some(app(5, "a.b")), None, |_| None), Some(app(5, "a.b")));
}

use super::*;

#[test]
fn password_fields_are_never_a_target() {
    // Regression: role AXTextField + any subrole used to be Editable.
    for paste_safe in [false, true] {
        assert_eq!(
            classify_focus_editability(
                Some("AXTextField"),
                Some("AXSecureTextField"),
                true,
                true,
                paste_safe
            ),
            FocusEditability::NotEditable
        );
    }
    assert!(is_secure_field(Some("AXSecureTextField")));
    assert!(!is_secure_field(Some("AXSearchField")));
    assert!(!is_secure_field(None));
}

#[test]
fn utf16_offsets_convert_to_char_indices() {
    assert_eq!(char_index_from_utf16("hello", 0), 0);
    assert_eq!(char_index_from_utf16("hello", 3), 3);
    assert_eq!(char_index_from_utf16("hello", 99), 5);
    // CJK is BMP: one unit per char.
    assert_eq!(char_index_from_utf16("你好世界", 2), 2);
    // Each emoji is two UTF-16 units but one char.
    let s = "😀😀😀😀😀 Beatrix";
    assert_eq!(char_index_from_utf16(s, 10), 5);
    assert_eq!(char_index_from_utf16(s, 11), 6);
    // Inside a surrogate pair rounds down to the emoji itself.
    assert_eq!(char_index_from_utf16(s, 1), 0);
    assert_eq!(char_index_from_utf16("", 4), 0);
}

#[test]
fn caret_context_after_an_emoji_reads_the_right_neighbours() {
    // "ok 👍|done": UTF-16 caret 5, char caret 4. The old code indexed
    // chars with the UTF-16 offset and read 'd' as the char before.
    let ctx = caret_context_from(Some("ok 👍done"), Some(5));
    assert!(ctx.value_readable);
    assert_eq!(ctx.immediate_before, Some('👍'));
    assert_eq!(ctx.char_after, Some('d'));
    assert_eq!(ctx.last_nws_before, Some('👍'));
}

#[test]
fn empty_field_and_caret_at_start_are_readable_not_blind() {
    // Regression: both used to mark the app AX-blind (no char before the
    // caret), which switched caret context off for that app.
    let empty = caret_context_from(Some(""), Some(0));
    assert!(empty.value_readable);
    assert_eq!((empty.immediate_before, empty.last_nws_before, empty.char_after), (None, None, None));
    let at_start = caret_context_from(Some("Hello"), Some(0));
    assert!(at_start.value_readable);
    assert_eq!(at_start.immediate_before, None);
    assert_eq!(at_start.char_after, Some('H'));
    // Only an unreadable value is blind.
    assert!(!caret_context_from(None, None).value_readable);
    assert!(!caret_context_from(None, Some(3)).value_readable);
}

#[test]
fn caret_context_defaults_to_end_of_field_without_a_range() {
    let ctx = caret_context_from(Some("Hello. "), None);
    assert_eq!(ctx.immediate_before, Some(' '));
    assert_eq!(ctx.last_nws_before, Some('.'));
    assert_eq!(ctx.char_after, None);
}

fn snap(value: Option<&str>, selection: Option<(usize, usize)>) -> FieldSnapshot {
    FieldSnapshot { value: value.map(str::to_string), selection }
}

#[test]
fn write_verification_compares_before_and_after() {
    // Landed: the value changed.
    assert!(ax_write_landed(&snap(Some("Hi. "), Some((4, 0))), &snap(Some("Hi. Yes."), Some((8, 0))), "Yes."));
    // Regression (false pass): the field already held the same words and
    // the write vanished. `contains` said rendered; nothing changed.
    assert!(!ax_write_landed(&snap(Some("Yes."), Some((4, 0))), &snap(Some("Yes."), Some((4, 0))), "Yes."));
    // Regression (double insert): the app normalised the text, so the
    // needle is absent, but the write did land.
    assert!(ax_write_landed(
        &snap(Some(""), Some((0, 0))),
        &snap(Some("line one line two"), Some((17, 0))),
        "line one\nline two"
    ));
    // A selection replaced by identical text: value equal, caret moved.
    assert!(ax_write_landed(&snap(Some("Yes."), Some((0, 4))), &snap(Some("Yes."), Some((4, 0))), "Yes."));
    // Unreadable after the write: treat as vanished (accept-and-drop apps).
    assert!(!ax_write_landed(&snap(Some("x"), None), &snap(None, None), "Yes."));
    // No "before": fall back to the needle search.
    assert!(ax_write_landed(&snap(None, None), &snap(Some("so Yes. ok"), None), " Yes. "));
    assert!(!ax_write_landed(&snap(None, None), &snap(Some("nothing"), None), "Yes."));
    // Nothing to write is trivially fine.
    assert!(ax_write_landed(&snap(None, None), &snap(None, None), "  "));
}

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

#[test]
fn wechat_main_app_is_ax_blind_chat_app() {
    // WeChat's main chat process is AX-blind (no focused element), so it is
    // paste-safe. Match the main app only, not the Chromium mini-program
    // engine or other apps.
    assert!(is_ax_blind_chat_app_comm(
        "/applications/wechat.app/contents/macos/wechat"
    ));
    assert!(is_ax_blind_chat_app_comm(
        "/Applications/WeChat.app/Contents/MacOS/WeChat\n"
    ));
    assert!(!is_ax_blind_chat_app_comm(
        "/applications/wechat.app/contents/macos/wechatappex.app/contents/macos/wechatappex"
    ));
    assert!(!is_ax_blind_chat_app_comm(
        "/applications/wechat.app/contents/frameworks/crashpad_handler"
    ));
    assert!(!is_ax_blind_chat_app_comm(
        "/applications/safari.app/contents/macos/safari"
    ));
    assert!(!is_ax_blind_chat_app_comm(""));
}

#[test]
fn helium_gets_the_ax_wake_but_keeps_the_ax_route() {
    // `ps -o comm=` for the running Helium main process.
    assert!(is_on_demand_ax_browser_comm(
        "/Applications/Helium.app/Contents/MacOS/Helium\n"
    ));
    // Its helpers are separate processes, never the frontmost app.
    assert!(!is_on_demand_ax_browser_comm(
        "/applications/helium.app/contents/frameworks/helium framework.framework/helpers/helium helper (renderer).app/contents/macos/helium helper (renderer)"
    ));
    assert!(!is_on_demand_ax_browser_comm(
        "/applications/safari.app/contents/macos/safari"
    ));
    assert!(!is_on_demand_ax_browser_comm(""));
}

// ORCHESTRATION (2026-09-23): a Chromium/Electron app (Slack was the
// reproduced case) answers AXFocusedUIElement with -25212 until its
// accessibility tree is requested. The wake is claimed at most once per
// app process; a second claim must be refused so a later miss can't spin
// the retry loop again.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
#[test]
fn electron_ax_wake_is_claimed_once_per_pid() {
    let pid = 990_001;
    assert!(claim_electron_ax_wake(pid), "first claim wins");
    assert!(!claim_electron_ax_wake(pid), "second claim is refused");
    assert!(claim_electron_ax_wake(990_002), "a different app may claim");
}

// ── finalize_utterance: observable-sink behaviour ──────

#[test]
fn a_decided_hold_keeps_its_cause_and_never_writes() {
    // The recording limit holds before the write ladder. Sink never called.
    let mut sink_called = false;
    let result = finalize_utterance(false, Some(KeptCause::LimitReached), "hello", None, |_| {
        sink_called = true;
        Ok(InjectOutcome::Injected)
    });
    assert_eq!(result, Delivery::Held(KeptCause::LimitReached));
    assert!(!sink_called, "a decided hold must never reach the injector");
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
fn missing_fresh_focus_reuses_only_same_frontmost_editable_capture() {
    // Exact logged failure: press/menu capture found an editable field, but
    // the write-time AX query returned -25212. If the same app is still
    // frontmost, the retained positive evidence is the safe fallback.
    assert!(retained_focus_can_fallback(
        Some(32975),
        Some(32975),
        FocusEditability::Editable,
        true,
    ));
    assert!(retained_focus_can_fallback(
        Some(11476),
        Some(11476),
        FocusEditability::PasteSafe,
        true,
    ));

    // Never revive a field after an app switch, with an unknown PID, or
    // when the captured element was positively non-editable.
    assert!(!retained_focus_can_fallback(
        Some(32975),
        Some(4659),
        FocusEditability::Editable,
        true,
    ));
    assert!(!retained_focus_can_fallback(
        None,
        Some(32975),
        FocusEditability::Editable,
        true,
    ));
    assert!(!retained_focus_can_fallback(
        Some(32975),
        Some(32975),
        FocusEditability::NotEditable,
        true,
    ));
    assert!(!retained_focus_can_fallback(
        Some(32975),
        Some(32975),
        FocusEditability::Editable,
        false,
    ));
}

#[test]
fn refused_foreground_holds_recoverable_not_failed() {
    // A fail-closed foreground refusal at Cmd+V delivery is a HOLD (the
    // transcript stays recoverable), never a hard failure.
    let result = finalize_utterance(false, None, "dictated", None, |_| {
        Ok(InjectOutcome::RefusedForeground)
    });
    assert_eq!(result, Delivery::Held(KeptCause::NoEditableField));
}

#[test]
fn failed_capture_then_current_editable_injects() {
    // Reviewer: "failed initial capture then current editable". captured
    // target is None → editability None → the sink (fresh resolver) runs
    // and, seeing the CURRENT field editable, injects.
    let mut injected = Vec::new();
    let result = finalize_utterance(false, None, "dictated", None, |t| {
        injected.push(t.to_string());
        Ok(InjectOutcome::Injected)
    });
    assert_eq!(result, Delivery::Delivered);
    assert_eq!(injected, vec!["dictated"]);
}

#[test]
fn failed_capture_then_current_not_editable_holds() {
    // The fresh resolver at write time finds no editable field → Held.
    let result = finalize_utterance(false, None, "dictated", None, |_| {
        Ok(InjectOutcome::NoEditableField)
    });
    assert_eq!(result, Delivery::Held(KeptCause::NoEditableField));
}

#[test]
fn click_out_still_holds_but_click_back_injects() {
    // Click-out (same app, focus moved to a non-input role): the injector's
    // fresh gate reports NoEditableField → Held, sink still called (gate
    // decided) but no write happened / nothing pasted.
    let mut writes = 0usize;
    let out = finalize_utterance(false, None, "x", None, |_| {
        writes += 1;
        Ok(InjectOutcome::NoEditableField)
    });
    assert_eq!(out, Delivery::Held(KeptCause::NoEditableField));
    // Click back into the field: next utterance injects.
    let in_ = finalize_utterance(false, None, "y", None, |_| {
        Ok(InjectOutcome::Injected)
    });
    assert_eq!(in_, Delivery::Delivered);
}

#[test]
fn cancel_never_reaches_the_injector() {
    let mut sink_called = false;
    let result = finalize_utterance(true, None, "hello", None, |_| {
        sink_called = true;
        Ok(InjectOutcome::Injected)
    });
    assert_eq!(result, Delivery::Cancelled);
    assert!(!sink_called, "a cancelled utterance must never be injected");
}

#[test]
fn fresh_dictation_after_cancel_injects() {
    // The headlining user-visible guarantee: cancel one utterance, then a
    // fresh dictation goes straight through to the injector.
    let mut injected = Vec::new();
    let result = finalize_utterance(true, None, "cancelled", None, |_| {
        Ok(InjectOutcome::Injected)
    });
    assert_eq!(result, Delivery::Cancelled);
    assert!(injected.is_empty());

    let result = finalize_utterance(false, None, "fresh", None, |t| {
        injected.push(t.to_string());
        Ok(InjectOutcome::Injected)
    });
    assert_eq!(result, Delivery::Delivered);
    assert_eq!(injected, vec!["fresh"]);
}

#[test]
fn injector_discovering_no_field_reports_held() {
    // Even with apparent positive evidence, the injector's fresh gate may
    // find black-holed focus at write time → Held.
    let result = finalize_utterance(false, None, "x", None, |_| {
        Ok(InjectOutcome::NoEditableField)
    });
    assert_eq!(result, Delivery::Held(KeptCause::NoEditableField));
}

#[test]
fn a_protected_clipboard_is_held_with_its_own_cause() {
    // Regression: the refusal used to be a generic Err, so it was held as a
    // failed paste and the copy-on-paste-failure setting overwrote the very
    // clipboard the refusal protected.
    assert_eq!(
        delivery_of(Ok(InjectOutcome::ClipboardProtected), None),
        Delivery::Held(KeptCause::ClipboardProtected)
    );
}

#[test]
fn hard_write_failure_is_kept_as_a_failed_paste() {
    let result = finalize_utterance(false, None, "x", None, |_| {
        Err(eyre::eyre!("AX refused"))
    });
    assert_eq!(result, Delivery::Held(KeptCause::PasteFailed));
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
    let result = finalize_utterance(false, None, "dictated", None, |_| {
        // ...lookup resumes here; claim authorization at the last moment.
        if !gate.commit(gen) {
            return Ok(InjectOutcome::Cancelled);
        }
        writes += 1;
        Ok(InjectOutcome::Injected)
    });
    assert_eq!(result, Delivery::Cancelled);
    assert_eq!(writes, 0, "a cancelled utterance must never reach the write");
}

#[test]
fn uncancelled_authorization_writes_exactly_once() {
    let gate = crate::cancel::CancelGate::new();
    let gen = gate.begin();
    let mut writes = 0usize;
    let result = finalize_utterance(false, None, "dictated", None, |_| {
        if !gate.commit(gen) {
            return Ok(InjectOutcome::Cancelled);
        }
        writes += 1;
        Ok(InjectOutcome::Injected)
    });
    assert_eq!(result, Delivery::Delivered);
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
        finalize_utterance(false, Some(KeptCause::NoEditableField), "x", None, |_| {
            authorized += 1;
            Ok(InjectOutcome::Injected)
        });
    assert_eq!(result, Delivery::Held(KeptCause::NoEditableField));
    assert_eq!(authorized, 0);
}
