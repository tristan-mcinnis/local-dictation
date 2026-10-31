//! Per-pid memory: the classify cache (terminal, Electron, WeChat,
//! clipboard-only), AX-blind and AX-verified pids, the last injected tail,
//! and the app labels every hold and fallback log line carries.
use super::*;

/// Process-wide memory of the last character we successfully injected.
/// When the focused app doesn't expose `kAXValue` (Electron apps,
/// sandboxed web views, some Java apps), we fall back to using this as
/// the implicit `char_before` so consecutive utterances still get the
/// right spacing + capitalization.
///
/// Reset on first call (None) → first utterance behaves like "start of
/// field." Updated to the last char of every successful inject.
///
/// These caret-context / verification helpers are only ever called from the
/// `imp` module, so they're gated behind the same feature — otherwise a
/// default (featureless) build flags them as dead code.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub(super) static LAST_TAIL: Mutex<Option<char>> = Mutex::new(None);

/// PID of the focused app that's known to not expose AXValue. Skip the
/// context read for it on subsequent injects (saves ~100 ms per utterance
/// in Electron / web-view targets).
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub(super) static AX_BLIND_PID: Mutex<Option<i32>> = Mutex::new(None);

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub(super) fn mark_ax_blind(pid: i32) {
    if let Ok(mut g) = AX_BLIND_PID.lock() {
        *g = Some(pid);
    }
}

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub(super) fn is_ax_blind(pid: i32) -> bool {
    matches!(AX_BLIND_PID.lock().ok().and_then(|g| *g), Some(p) if p == pid)
}

/// Cache of `pid → (is_electron, is_terminal)`, looked up via `ps` once per
/// PID. Both injection routing and cleanup gating read from it.
///
/// Two families of apps report an editable AX role (AXTextField / AXTextArea)
/// and accept `kAXSelectedText` writes with `kAXErrorSuccess`, but their
/// renderer never actually receives the text — so the write silently
/// vanishes ("records but doesn't paste"). We route both straight to
/// clipboard paste, which goes through the system-level Cmd+V handler and
/// always works:
///   1. Electron / Chromium web views (VS Code, Cursor, Slack, Discord,
///      Claude, Notion, Chrome, Brave, Arc, Obsidian, Zed, Figma, new
///      Microsoft Outlook …).
///   2. GPU/native terminal emulators (Ghostty, Terminal.app, iTerm2,
///      WezTerm, kitty, Alacritty) — they draw their own text grid and
///      ignore AX writes, confirmed via INJECT_PROFILE: Ghostty returns
///      err=0 on set_selected_text yet nothing renders.
/// Per-PID classification, cached so the `ps` lookup is paid once per PID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PidClass {
    /// App accepts but never renders AX writes (Electron/Chromium web views,
    /// GPU terminals) — route to clipboard paste when the FOCUS evidence says
    /// the surface is editable. App type alone is never the green light.
    is_electron: bool,
    /// GPU/native terminal emulator. The one narrow text-input-surface fallback.
    is_terminal: bool,
    /// AX-blind native chat app (WeChat): its message box is a custom control
    /// that accepts keyboard + Cmd+V paste but exposes no focused UI element
    /// through AX (AXFocusedUIElement → kAXErrorNoValue -25212) even with the
    /// caret visible. Paste-safe like a terminal, but prose cleanup still runs
    /// (chat messages benefit from cleanup; shell commands do not).
    is_wechat: bool,
    /// A Chromium browser whose AX writes render (so it keeps the normal AX
    /// route) but whose web accessibility tree is built on demand, like
    /// Electron's. Only the one-time AX wake reads this; routing does not.
    wakes_ax_only: bool,
}

pub(super) static CLIPBOARD_ONLY_CACHE: Mutex<Vec<(i32, PidClass)>> = Mutex::new(Vec::new());

/// Executable path of a running process (`ps -o comm=`), untrimmed. `None`
/// when `ps` fails or the PID is gone. Shared by the injector's app
/// classifier and the daemon's log labelling so there is one spawn site.
pub fn process_command_path(pid: i32) -> Option<String> {
    std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
}

/// Name for the app a hold or clipboard-fallback log line is about: the bundle
/// id, else the executable file name (from `ps -o comm=`), else `unknown`.
/// Pure so the choice is unit-testable; never takes text, titles or values.
pub fn app_label(bundle_id: Option<&str>, command_path: Option<&str>) -> String {
    let clean = |s: &str| s.trim().to_string();
    if let Some(b) = bundle_id.map(clean).filter(|b| !b.is_empty()) {
        return b;
    }
    command_path
        .map(clean)
        .and_then(|c| c.rsplit('/').next().map(str::to_string))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// The one format for an injector hold or clipboard-fallback log line:
/// `[inject] <reason> app=<name>`. `reason` is a fixed phrase plus an AX error
/// or role key, never transcript text, a window title or a field value.
pub fn hold_log_line(reason: &str, app: &str) -> String {
    let app = if app.trim().is_empty() { "unknown" } else { app.trim() };
    format!("[inject] {reason} app={app}")
}

/// [`app_label`] for a PID, looked up only on hold/fallback paths. May spawn
/// `ps`, so never call it before a paste; use [`app_label_for_pid_cheap`]
/// there. Without the `ax-inject` feature it is always `unknown`.
pub fn app_label_for_pid(pid: Option<i32>) -> String {
    #[cfg(all(target_os = "macos", feature = "ax-inject"))]
    if let Some(pid) = pid {
        let bundle = bundle_id_for_pid(pid);
        if bundle.is_some() {
            return app_label(bundle.as_deref(), None);
        }
        return app_label(None, process_command_path(pid).as_deref());
    }
    let _ = pid;
    app_label(None, None)
}

/// Bundle-id-only label (one `NSRunningApplication` read, no `ps`), for log
/// lines written just before a clipboard paste.
pub fn app_label_for_pid_cheap(pid: Option<i32>) -> String {
    #[cfg(all(target_os = "macos", feature = "ax-inject"))]
    if let Some(pid) = pid {
        return app_label(bundle_id_for_pid(pid).as_deref(), None);
    }
    let _ = pid;
    app_label(None, None)
}

/// Pure app-name check for AX-blind native chat apps (WeChat). Split out of
/// `classify_pid` so the name matching is unit-testable without shelling to
/// `ps`. Case-insensitive. Matches only WeChat's main app process
/// (`…/MacOS/WeChat`), not the Chromium-based `WeChatAppEx` mini-program
/// engine (whose helpers end in `wechatappex`, `wechatappex helper (renderer)`,
/// etc.).
pub(super) fn is_ax_blind_chat_app_comm(comm: &str) -> bool {
    comm.trim().to_lowercase().ends_with("/wechat")
}

/// Pure app-name check for a Chromium browser that needs the on-demand AX
/// wake but is not on the clipboard-only list. Helium's executable
/// (`/Applications/Helium.app/Contents/MacOS/Helium`) matches none of the
/// `/chrome`, `/brave` or `/arc` names, so a focus miss there never woke its
/// tree. Its writes keep the AX route: verify-after-write already falls back
/// to the clipboard (and promotes the PID) when a write does not render, so
/// forcing clipboard-only would only add the paste cost. Case-insensitive.
pub(super) fn is_on_demand_ax_browser_comm(comm: &str) -> bool {
    comm.trim().to_lowercase().ends_with("/helium")
}

/// Classify a PID by its executable name into a [`PidClass`]. The `is_electron`
/// flag routes to clipboard paste; `is_terminal` additionally lets the daemon
/// skip prose cleanup (you almost never want Gemma rewriting a shell command);
/// `is_editor_surface` is the curated non-browser allowlist used by the
/// focus-editability gate. Result is cached per PID.
pub(super) fn classify_pid(pid: i32) -> PidClass {
    if let Ok(cache) = CLIPBOARD_ONLY_CACHE.lock() {
        for &(p, cls) in cache.iter() {
            if p == pid {
                return cls;
            }
        }
    }
    let comm = process_command_path(pid)
        .map(|s| s.to_lowercase())
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
        || comm.contains("/figma")
        // New Outlook for Mac is a WebView2/Chromium app: it reports an editable
        // AX role and accepts kAXSelectedText writes with kAXErrorSuccess, but
        // the compose web view never renders them. Route it to clipboard paste.
        || comm.contains("/microsoft outlook");
    // Native terminal emulators: own-drawn text grids that accept but ignore
    // AX writes. The `/term` anchor catches Terminal.app and iTerm2's exec
    // (`.../macos/iterm2`); the rest match their app/exec name.
    let is_terminal = comm.contains("/ghostty")
        || comm.contains("/terminal")
        || comm.contains("/iterm")
        || comm.contains("/wezterm")
        || comm.contains("/kitty")
        || comm.contains("/alacritty");
    let is_wechat = is_ax_blind_chat_app_comm(&comm);
    let wakes_ax_only = is_on_demand_ax_browser_comm(&comm);
    // The editor/compose blanket (VS Code, Slack, Notion, Obsidian, Figma, …)
    // is deliberately ABSENT from a paste-safe allowlist. A paste is justified
    // only by the FOCUSED element's AX role at write time, never by the app
    // name — so a Slack pane or a browser body never receives a blind paste.
    let cls = PidClass { is_electron, is_terminal, is_wechat, wakes_ax_only };
    if let Ok(mut cache) = CLIPBOARD_ONLY_CACHE.lock() {
        cache.push((pid, cls));
    }
    cls
}

pub fn is_clipboard_only_pid(pid: i32) -> bool {
    let cls = classify_pid(pid);
    cls.is_electron || cls.is_terminal || cls.is_wechat
}

/// True when the focused app is a native terminal emulator. The daemon uses
/// this to inject the raw transcript without Gemma cleanup, and the focus gate
/// uses it (narrowly) as the one text-input-surface fallback when AX discloses
/// no editable role.
pub fn is_terminal_pid(pid: i32) -> bool {
    classify_pid(pid).is_terminal
}

/// True when the focused app is an AX-blind native chat app (WeChat). Used by
/// the focus gate as a paste-safe fallback when AX discloses no focused element
/// (see `is_wechat` on [`PidClass`]), and by `is_clipboard_only_pid` to route
/// any editable WeChat field straight to clipboard paste.
pub fn is_wechat_pid(pid: i32) -> bool {
    classify_pid(pid).is_wechat
}

/// True when the app is an Electron / Chromium web view, including a Chromium
/// browser that keeps the AX write route (Helium). Used only to decide
/// whether to flip Chromium's on-demand accessibility switch before treating a
/// missing focused element as final (see
/// `imp::wake_electron_ax_and_retry_focus`). It never widens who may receive a
/// paste — the editability gate still owns that.
pub fn is_electron_pid(pid: i32) -> bool {
    let cls = classify_pid(pid);
    cls.is_electron || cls.wakes_ax_only
}

/// PIDs whose AX write we've *verified* actually rendered (read the value
/// back and saw our text). Once a PID is here we trust its AX path and skip
/// the post-write verification read on every subsequent utterance.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub(super) static AX_VERIFIED_PID: Mutex<Vec<(i32, String)>> = Mutex::new(Vec::new());

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub(super) fn is_ax_verified(pid: i32, role: &str) -> bool {
    AX_VERIFIED_PID
        .lock()
        .map(|g| g.iter().any(|(p, r)| *p == pid && r == role))
        .unwrap_or(false)
}

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub(super) fn mark_ax_verified(pid: i32, role: &str) {
    if let Ok(mut g) = AX_VERIFIED_PID.lock() {
        if !g.iter().any(|(p, r)| *p == pid && r == role) {
            g.push((pid, role.to_string()));
        }
    }
}

/// Promote a PID to clipboard-only at runtime. Used when an AX write returns
/// success but the value read-back proves nothing rendered — i.e. an app with
/// the lying-AX trait that isn't on the static name list. Updates the existing
/// cache entry (the name lookup will have inserted `false`) or inserts a fresh
/// entry so all future injects skip the AX path for this PID.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub(super) fn mark_clipboard_only(pid: i32) {
    if let Ok(mut cache) = CLIPBOARD_ONLY_CACHE.lock() {
        if let Some(entry) = cache.iter_mut().find(|(p, _)| *p == pid) {
            entry.1.is_electron = true;
        } else {
            cache.push((
                pid,
                PidClass {
                    is_electron: true,
                    is_terminal: false,
                    is_wechat: false,
                    wakes_ax_only: false,
                },
            ));
        }
    }
}

/// Chromium/Electron apps build no accessibility tree until a client asks for
/// one, so their `AXFocusedUIElement` answers `kAXErrorNoValue` (-25212) until
/// the on-demand switch is set. We flip it at most once per app process; this
/// records the PIDs already asked, so a later miss never re-triggers the wake
/// and its short retry loop.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub(super) static ELECTRON_AX_WAKE_CLAIMED: Mutex<Vec<i32>> = Mutex::new(Vec::new());

/// Claim the one-time Chromium AX wake for `pid`. True only on the first call
/// for a PID. Pure bookkeeping — the AX attribute write lives in `imp`.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub(super) fn claim_electron_ax_wake(pid: i32) -> bool {
    match ELECTRON_AX_WAKE_CLAIMED.lock() {
        Ok(mut claimed) => {
            if claimed.contains(&pid) {
                false
            } else {
                claimed.push(pid);
                true
            }
        }
        // A poisoned lock must not disable the fix; the attribute write is
        // idempotent, so allowing repeated attempts is safe.
        Err(_) => true,
    }
}

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub(super) fn remember_tail(text: &str) {
    let tail = text.chars().rev().find(|c| !c.is_whitespace());
    if let Ok(mut g) = LAST_TAIL.lock() {
        *g = tail;
    }
}

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub(super) fn recall_tail() -> Option<char> {
    LAST_TAIL.lock().ok().and_then(|g| *g)
}
