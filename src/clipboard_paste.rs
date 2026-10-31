//! Clipboard save → write → Cmd+V → restore. Used as a fallback when the AX
//! injector can't write directly (Electron apps, some Java apps, sandboxed
//! web views all silently refuse `kAXValueAttribute` writes).
//!
//! Our own write is marked transient (nspasteboard.org): every clipboard
//! manager that honours the convention — Quick Launch's Clipboard History
//! included — then skips the dictated text instead of recording every
//! sentence the user speaks. See [`declared_write_types`].
//!
//! The save/restore is a full multi-type, multi-item NSPasteboard round-trip:
//! RTF, images and file URLs on the clipboard at injection time survive a
//! dictation. What is still lost is listed on [`is_restorable_type`] and
//! [`SNAPSHOT_BUDGET_BYTES`]: lazily promised file data (the promising app is
//! the only one that can produce it) and anything past the snapshot byte
//! budget. When that is the whole clipboard, the write is refused instead
//! ([`PasteOutcome::ClipboardProtected`]) and the caller keeps the text. We also guard the restore with the pasteboard's `changeCount` so a
//! user copying something during the paste-settle window isn't clobbered (see
//! [`paste_via_clipboard`]).
//!
//! Latency: `paste_via_clipboard` *queues* the settle + restore on a background
//! thread that keeps holding the clipboard gate, so the dictation worker returns
//! as soon as Cmd+V is posted and can start the next capture/ASR instead of
//! sleeping through the settle. The next clipboard write still waits on the gate
//! until the restore finishes, so ordering and the "don't clobber a fresh copy"
//! guard are unchanged. The settle is a *queued* deadline, not proof the text
//! visibly landed.

use arboard::Clipboard;
use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation, CGKeyCode};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};


/// Outcome of a clipboard paste attempt. The paste is a GLOBAL Cmd+V that lands
/// in the OS-frontmost app, so the delivery guard decides whether it may fire.
pub enum PasteOutcome {
    /// The text was set and Cmd+V was posted to the expected frontmost app.
    Delivered,
    /// The expected target was not frontmost at the moment of delivery, so the
    /// Cmd+V was NOT posted (fail-closed — it would land in the wrong app).
    /// The caller holds the transcript recoverable and never pastes blindly.
    RefusedForeground,
    /// The user's clipboard holds only content that could not be put back (a
    /// promised file, an image past the snapshot budget), so nothing was
    /// written and no Cmd+V was posted. The caller keeps the text and must not
    /// copy it either: that would destroy the same clipboard.
    ClipboardProtected,
}

// ⌘C / ⌘V / ⌘Z are posted at the key that types the letter in the CURRENT
// layout (`crate::key_layout`), not at the US-ANSI position: on Dvorak the
// ANSI V position is ⌘K, and on AZERTY the ANSI Z position is ⌘W.
// Return, Tab and Escape sit at the same keycode on every layout.
const KEY_RETURN: CGKeyCode = 36;
const KEY_TAB: CGKeyCode = 48;
const KEY_ESCAPE: CGKeyCode = 53;
// Time the OS needs to actually consume the synthesized Cmd+V and let the
// receiving app pull from the pasteboard before we restore. Under GPU load
// right after Gemma inference the target app's paste handler can run late, and
// restoring the clipboard too early makes it paste stale/empty/partial content
// — the main cause of "records but doesn't paste everything".
//
// The wait is *adaptive*: a fixed delay that's fine for a short phrase is too
// short for a multi-paragraph dictation (more text → the receiving app's paste
// handler takes longer to pull + render before our restore clobbers the
// pasteboard). Scale a base delay by text length, capped so we never stall the
// hot path unreasonably.
const PASTE_SETTLE_BASE_MS: u64 = 280;
const PASTE_SETTLE_MAX_MS: u64 = 1000;
/// Sanity cap on the `DICTATE_PASTE_SETTLE_MS` override so a bad value can't
/// stall the hot path indefinitely.
const PASTE_SETTLE_ENV_CAP_MS: u64 = 5000;

/// Length-scaled settle time before restoring the clipboard: `+1 ms per ~3
/// chars` on top of the base, clamped to the max. Pure — unit-tested.
fn scaled_paste_settle_ms(text_chars: usize) -> u64 {
    (PASTE_SETTLE_BASE_MS + (text_chars as u64) / 3).min(PASTE_SETTLE_MAX_MS)
}

/// Resolve the effective paste-settle duration. `DICTATE_PASTE_SETTLE_MS`
/// overrides the length-scaled default (set it to `0` if you never touch an
/// Electron/Chromium target and want minimum latency; raise it for paste-heavy
/// apps). Reading env per call is cheap and lets a change apply on the next
/// utterance.
fn paste_settle_ms(text_chars: usize) -> u64 {
    let default = scaled_paste_settle_ms(text_chars);
    match std::env::var("DICTATE_PASTE_SETTLE_MS") {
        Ok(v) => v.trim().parse::<u64>().map_or(default, |ms| ms.min(PASTE_SETTLE_ENV_CAP_MS)),
        Err(_) => default,
    }
}

/// Serialises every exclusive pasteboard mutation by this process and keeps the
/// post-`Cmd+V` settle + restore transaction ordered against the next write.
///
/// A plain `MutexGuard` cannot be handed to the restore thread (it is not
/// `Send`), so the gate is a tiny hand-rolled semaphore: a `Mutex<bool>` plus a
/// `Condvar`. [`ClipboardPermit`] is an owned, `Send` RAII handle that releases
/// the gate on drop and can be moved into the background transaction.
struct ClipboardGate {
    held: Mutex<bool>,
    idle: Condvar,
}

/// The process-wide gate every clipboard writer shares: `paste_via_clipboard`,
/// `copy_only`, `copy_selection_via_clipboard` and the deferred restore all take
/// it, so their pasteboard writes never interleave.
static CLIPBOARD_GATE: OnceLock<Arc<ClipboardGate>> = OnceLock::new();

fn clipboard_gate() -> &'static Arc<ClipboardGate> {
    CLIPBOARD_GATE.get_or_init(ClipboardGate::shared)
}

impl ClipboardGate {
    fn shared() -> Arc<Self> {
        Arc::new(Self { held: Mutex::new(false), idle: Condvar::new() })
    }

    /// Block until the gate is free, then take it. The returned permit releases
    /// it on drop and may be moved to another thread.
    fn acquire(self: &Arc<Self>) -> ClipboardPermit {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        while *held {
            held = self.idle.wait(held).unwrap_or_else(|e| e.into_inner());
        }
        *held = true;
        ClipboardPermit { gate: Arc::clone(self) }
    }

    /// Wait at most `timeout` for the gate; `None` on timeout. Test seam: it lets
    /// a test observe the release deterministically without a fixed sleep.
    #[cfg(test)]
    fn acquire_timeout(self: &Arc<Self>, timeout: Duration) -> Option<ClipboardPermit> {
        let deadline = Instant::now() + timeout;
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        while *held {
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (guard, _timed_out) = self
                .idle
                .wait_timeout(held, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            held = guard;
        }
        *held = true;
        Some(ClipboardPermit { gate: Arc::clone(self) })
    }

    /// Take the gate only if it is free right now; `None` when a transaction
    /// holds it. Test seam for asserting the gate stays held.
    #[cfg(test)]
    fn try_acquire(self: &Arc<Self>) -> Option<ClipboardPermit> {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        if *held {
            return None;
        }
        *held = true;
        Some(ClipboardPermit { gate: Arc::clone(self) })
    }

    /// Non-blocking, fail-closed: is the gate held right now (a paste in flight
    /// or a settle + restore transaction pending)? Returns `true` if the state
    /// cannot be read, so a caller errs toward "busy".
    fn is_held(&self) -> bool {
        match self.held.lock() {
            Ok(held) => *held,
            Err(_) => true,
        }
    }

    fn release(&self) {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        *held = false;
        self.idle.notify_all();
    }
}

/// Owned RAII permit for [`ClipboardGate`]. `Send` (it holds an `Arc`, not a
/// `MutexGuard`), so it can move to the background restore thread; always
/// releases the gate on drop, including while unwinding from a panic.
struct ClipboardPermit {
    gate: Arc<ClipboardGate>,
}

impl Drop for ClipboardPermit {
    fn drop(&mut self) {
        self.gate.release();
    }
}

/// Work that must finish under the clipboard gate after `Cmd+V`: the settle wait
/// and the snapshot restore. Boxed so the whole transaction can move to a
/// background thread.
type DeferredPaste = Box<dyn FnOnce() + Send + 'static>;

/// A queued post-paste transaction. Holds the gate permit until `work` returns,
/// so the next clipboard write is ordered after the restore (or after the
/// settle, when there is nothing to restore).
struct ClipboardTransaction {
    _permit: ClipboardPermit,
    work: DeferredPaste,
}

impl ClipboardTransaction {
    fn new(permit: ClipboardPermit, work: DeferredPaste) -> Self {
        Self { _permit: permit, work }
    }

    fn run(self) {
        (self.work)();
        // `self._permit` drops here, releasing the gate *after* the work.
    }
}

/// Queue `tx` and return immediately, so the dictation worker is free to start
/// the next capture/ASR instead of sleeping through the paste settle. The gate
/// stays held until `work` finishes. If the thread cannot be created, run the
/// transaction synchronously (still under the permit): the settle and restore
/// still happen, the gate is still released, and nothing is clobbered. A panic
/// inside `work` unwinds through [`ClipboardTransaction`], releasing the gate.
fn run_transaction_async(tx: ClipboardTransaction) {
    // The slot lets a failed spawn reclaim the transaction: the closure handed
    // to `spawn` owns only this clone, not the transaction itself.
    let slot = Arc::new(Mutex::new(Some(tx)));
    let thread_slot = Arc::clone(&slot);
    let spawned = std::thread::Builder::new()
        .name("clipboard-restore".to_string())
        .spawn(move || {
            let tx = thread_slot.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(tx) = tx {
                tx.run();
            }
        });
    if let Err(e) = spawned {
        eprintln!("[clipboard] restore thread spawn failed ({e}); settling synchronously");
        if let Some(tx) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
            tx.run();
        }
    }
}

/// A deliberate recovery copy: serialize with any pending pasteboard restore,
/// write plain text, and never synthesize keys or activate a target app.
pub fn copy_only(text: &str) -> eyre::Result<()> {
    let _permit = clipboard_gate().acquire();
    let mut clipboard = Clipboard::new().map_err(|e| eyre::eyre!("clipboard unavailable: {e}"))?;
    clipboard.set_text(text.to_owned()).map_err(|e| eyre::eyre!("clipboard copy failed: {e}"))
}

/// Non-blocking, fail-closed: is a clipboard paste in flight, or is a settle +
/// snapshot-restore transaction still pending? Callers that must not race a
/// pending paste (the Undo render/handler in the UI worker) gate on
/// `!transaction_pending()`.
pub fn transaction_pending() -> bool {
    clipboard_gate().is_held()
}

/// Block until any in-flight paste has posted Cmd+V and its settle +
/// snapshot-restore transaction has finished, then return. Acquires and
/// immediately releases the gate, so an idle gate is a no-op.
///
/// This waits out the *consume/restore window* only. It is **not pixel proof**
/// that the pasted text visibly landed: the target app's render is not
/// observable from here. Use it to order a trailing key (Enter/Tab) after a
/// paste, never as a delivery guarantee.
pub fn wait_for_pending_transaction() {
    let permit = clipboard_gate().acquire();
    drop(permit);
}

/// Order a trailing keyboard action after paste consumption and restoration,
/// keeping other app clipboard writes behind the action. The callback must
/// only emit keys, never re-enter a clipboard writer.
pub fn after_pending_transaction<T>(emit: impl FnOnce() -> T) -> T {
    let _permit = clipboard_gate().acquire();
    emit()
}

/// True when the pasteboard's `changeCount` advanced past our own write — so we
/// must NOT restore our snapshot (it would clobber a user's fresh copy). A
/// `None` on either side keeps the old unconditional-replace behaviour. Pure —
/// unit-tested.
fn user_copied_during_settle(our_count: Option<isize>, now_count: Option<isize>) -> bool {
    matches!((our_count, now_count), (Some(a), Some(b)) if b != a)
}

/// Build a CGEvent source that does **not** merge the live hardware modifier
/// state into the events we post.
///
/// This is the crux of the intermittent-paste bug: the push-to-talk hotkey is
/// a *modifier* key (Right Option by default). With `HIDSystemState` the
/// synthesized Cmd+V inherits whatever modifiers are physically down at post
/// time, so if Option hasn't fully registered as released yet the event
/// becomes Cmd+Option+V — which is not the paste shortcut, so nothing pastes.
/// `Private` gives us an isolated state table, so the only modifiers on the
/// event are the ones we set explicitly.
fn clean_event_source() -> eyre::Result<CGEventSource> {
    CGEventSource::new(CGEventSourceStateID::Private)
        .map_err(|_| eyre::eyre!("CGEventSource::new failed"))
}

/// The general pasteboard's `changeCount` — bumps whenever anything writes to
/// the clipboard. We read it right after our own write, then again before
/// restoring, to detect a user (or another app) copying something during the
/// paste-settle window. `None` if the read fails (then we fall back to the old
/// unconditional restore).
fn pasteboard_change_count() -> Option<isize> {
    use objc2_app_kit::NSPasteboard;
    // NSPasteboard is thread-safe; changeCount is a plain read.
    let pb = NSPasteboard::generalPasteboard();
    Some(pb.changeCount())
}

/// The markers clipboard managers honour (nspasteboard.org): transient means
/// "do not record", auto-generated means "an app wrote this, not the user".
/// Both go on the same pasteboard item as the text, each with empty data, so a
/// manager sees them whichever way it checks.
const TRANSIENT_MARKER_TYPES: [&str; 2] = [
    "org.nspasteboard.TransientType",
    "org.nspasteboard.AutoGeneratedType",
];

/// Everything our own pasteboard write declares, in order: the text type
/// first, then the transient markers. Pure — unit-tested.
fn declared_write_types(text_type: &str) -> Vec<String> {
    let mut types = Vec::with_capacity(1 + TRANSIENT_MARKER_TYPES.len());
    types.push(text_type.to_string());
    types.extend(TRANSIENT_MARKER_TYPES.iter().map(|t| t.to_string()));
    types
}

/// Cap on the bytes we copy off the pasteboard for the restore snapshot. A
/// dictation must not hold a multi-hundred-MB image in memory (twice: the
/// snapshot plus the restored copy); anything past the cap is dropped from the
/// snapshot and logged, so that flavour is lost on restore.
const SNAPSHOT_BUDGET_BYTES: usize = 16 * 1024 * 1024;

/// Whether a pasteboard flavour is worth carrying through the snapshot.
///
/// Dropped: our own transient markers (they describe *our* write, not the
/// user's clipboard) and promised-file flavours, whose data only the promising
/// app can produce — re-writing the metadata without a live promise would put
/// a dangling file promise on the clipboard. Pure — unit-tested.
fn is_restorable_type(ty: &str) -> bool {
    if TRANSIENT_MARKER_TYPES.contains(&ty) {
        return false;
    }
    !(ty.starts_with("com.apple.pasteboard.promised")
        || ty == "com.apple.NSFilePromiseItemMetaData")
}

/// Whether one more flavour of `len` bytes still fits the snapshot budget.
/// Pure — unit-tested.
fn fits_budget(used: usize, len: usize, budget: usize) -> bool {
    used.saturating_add(len) <= budget
}

/// Every item on the general pasteboard, as plain bytes: one entry per
/// pasteboard item, each a list of `(type, data)` flavours. Plain data (not
/// Objective-C objects) so it can cross to the detached restore thread.
type ClipboardSnapshot = Vec<Vec<(String, Vec<u8>)>>;

/// Snapshot the clipboard before one of our writes, or `None` when the user's
/// clipboard holds something (a promised file, an image past the budget) of
/// which nothing could be put back: writing would destroy it.
fn restorable_snapshot() -> Option<ClipboardSnapshot> {
    let (snapshot, item_count) = snapshot_pasteboard();
    (!would_lose_clipboard(item_count, &snapshot)).then_some(snapshot)
}

/// True when the clipboard had items but none of them is restorable. Pure.
fn would_lose_clipboard(item_count: usize, snapshot: &ClipboardSnapshot) -> bool {
    item_count > 0 && snapshot.is_empty()
}

/// Copy every restorable flavour of every pasteboard item into plain bytes,
/// with the number of items on the pasteboard. An empty snapshot with a
/// non-zero count means the clipboard holds only promised or oversized content.
fn snapshot_pasteboard() -> (ClipboardSnapshot, usize) {
    use objc2_app_kit::NSPasteboard;

    let pb = NSPasteboard::generalPasteboard();
    let mut snapshot: ClipboardSnapshot = Vec::new();
    let Some(items) = pb.pasteboardItems() else {
        return (snapshot, 0);
    };
    let mut used = 0usize;
    for item in items.iter() {
        let mut flavours: Vec<(String, Vec<u8>)> = Vec::new();
        for ty in item.types().iter() {
            let name = ty.to_string();
            if !is_restorable_type(&name) {
                continue;
            }
            let Some(data) = item.dataForType(&ty) else {
                continue;
            };
            let bytes = data.to_vec();
            if !fits_budget(used, bytes.len(), SNAPSHOT_BUDGET_BYTES) {
                eprintln!(
                    "[clipboard] snapshot over budget: dropping {name} ({} bytes) — that flavour will not come back",
                    bytes.len()
                );
                continue;
            }
            used += bytes.len();
            flavours.push((name, bytes));
        }
        if !flavours.is_empty() {
            snapshot.push(flavours);
        }
    }
    (snapshot, items.count())
}

/// Put a snapshot back on the general pasteboard, all items and flavours in
/// one `clearContents` + `writeObjects` cycle.
fn restore_pasteboard(snapshot: &ClipboardSnapshot) -> bool {
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardWriting};
    use objc2_foundation::{NSArray, NSData, NSString};

    let items: Vec<_> = snapshot
        .iter()
        .map(|flavours| {
            let item = NSPasteboardItem::new();
            for (ty, bytes) in flavours {
                item.setData_forType(&NSData::with_bytes(bytes), &NSString::from_str(ty));
            }
            item
        })
        .collect();
    let writable: Vec<&ProtocolObject<dyn NSPasteboardWriting>> = items
        .iter()
        .map(|i| ProtocolObject::from_ref(&**i))
        .collect();

    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    pb.writeObjects(&NSArray::from_slice(&writable))
}

/// Write `text` to the general pasteboard marked transient + auto-generated,
/// so clipboard managers don't record the dictation. One `clearContents` →
/// `declareTypes` → write cycle, exactly as nspasteboard.org specifies.
fn write_text_transient(text: &str) -> eyre::Result<()> {
    write_text_transient_tracked(text, &std::cell::Cell::new(None))
}

fn write_text_transient_tracked(text: &str, owned_count: &std::cell::Cell<Option<isize>>) -> eyre::Result<()> {
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardTypeString, NSPasteboardWriting};
    use objc2_foundation::{NSArray, NSData, NSString};

    // Prepare every flavour before touching the general pasteboard. A failed
    // item write cannot erase the user's clipboard or publish half an item.
    let item = NSPasteboardItem::new();
    let text_type = unsafe { NSPasteboardTypeString };
    eyre::ensure!(item.setString_forType(&NSString::from_str(text), text_type), "could not prepare clipboard text");
    for marker in declared_write_types(&text_type.to_string()).into_iter().skip(1) {
        eyre::ensure!(item.setData_forType(&NSData::new(), &NSString::from_str(&marker)), "could not prepare clipboard marker");
    }
    let writable: &ProtocolObject<dyn NSPasteboardWriting> = ProtocolObject::from_ref(&*item);
    let pb = NSPasteboard::generalPasteboard();
    owned_count.set(Some(pb.clearContents()));
    eyre::ensure!(pb.writeObjects(&NSArray::from_slice(&[writable])), "could not publish clipboard text");
    owned_count.set(Some(pb.changeCount()));
    Ok(())
}

fn rollback_on_error<T, E>(operation: impl FnOnce() -> Result<T, E>, rollback: impl FnOnce()) -> Result<T, E> {
    let result = operation();
    if result.is_err() { rollback(); }
    result
}

fn write_preserving_snapshot(text: &str, saved: &ClipboardSnapshot) -> eyre::Result<()> {
    let owned_count = std::cell::Cell::new(None);
    rollback_on_error(|| write_text_transient_tracked(text, &owned_count), || {
        // Never guess ownership from a fresh count after a failed write: that
        // could belong to a concurrent user copy. Use our recorded claim only.
        if let Some(count) = owned_count.get() { restore_snapshot_if_owned(saved, Some(count)); }
    })
}

pub fn paste_via_clipboard(text: &str, expected_pid: Option<i32>) -> eyre::Result<PasteOutcome> {
    // Take the gate before touching the pasteboard: if a previous dictation's
    // settle + restore is still running, this waits for it. The wait is timed
    // only for the timing log below.
    let queued_at = Instant::now();
    let permit = clipboard_gate().acquire();
    let gate_wait_ms = queued_at.elapsed().as_millis();

    // Fail-closed foreground guard, immediately before the Cmd+V: a global
    // Cmd+V lands in the OS-frontmost app, so it must not be posted unless the
    // expected target IS frontmost. A mismatch means we'd paste into the wrong
    // app — refuse with nothing modified (checked before any clipboard write).
    if !crate::injector::clipboard_target_is_frontmost(expected_pid, crate::injector::frontmost_pid()) {
        eprintln!(
            "[clipboard] refusing Cmd+V: expected pid {expected_pid:?} is not frontmost — not pasting"
        );
        return Ok(PasteOutcome::RefusedForeground);
    }

    let Some(saved) = restorable_snapshot() else {
        eprintln!("[clipboard] the clipboard holds content that cannot be restored; not overwriting it");
        return Ok(PasteOutcome::ClipboardProtected);
    };

    write_preserving_snapshot(text, &saved)?;

    // changeCount right after our write. Cmd+V only reads the pasteboard, so this
    // stays put unless the user copies something during the settle wait below.
    let our_count = pasteboard_change_count();

    // Pasteboard access can itself yield. Check again at the delivery boundary.
    // A refusal here means our transient text is already on the clipboard but no
    // Cmd+V was sent, so put the user's snapshot back now, still under the
    // permit — the changeCount guard means a concurrent user copy still wins.
    if !crate::injector::clipboard_target_is_frontmost(expected_pid, crate::injector::frontmost_pid()) {
        eprintln!("[clipboard] refusing Cmd+V after write; restoring snapshot, no keys sent");
        restore_clipboard_inner(saved, our_count);
        return Ok(PasteOutcome::RefusedForeground);
    }
    rollback_on_error(synthesize_cmd_v, || restore_snapshot_if_owned(&saved, our_count))?;

    // Cmd+V is queued. Hand the settle + restore to a background thread that
    // keeps holding `permit`, so this function returns now — the worker can start
    // the next capture/ASR while the next clipboard write still waits on the gate
    // for the restore. We do NOT claim the text visibly landed: only that Cmd+V
    // was posted and the restore is ordered after the settle.
    let settle_ms = paste_settle_ms(text.chars().count());
    eprintln!("[clipboard] paste queued: gate_wait_ms={gate_wait_ms} settle_ms={settle_ms}");

    let work: DeferredPaste = Box::new(move || {
        if settle_ms > 0 {
            std::thread::sleep(Duration::from_millis(settle_ms));
        }
        // An empty snapshot here means the clipboard was empty before (an
        // unrestorable one was refused above): leave our injected text on it
        // as a manual paste fallback. The settle above still ran, so the gate
        // stayed held for the consume window.
        if !saved.is_empty() {
            let restored_at = Instant::now();
            restore_clipboard_inner(saved, our_count);
            eprintln!(
                "[clipboard] restore finished in {} ms",
                restored_at.elapsed().as_millis()
            );
        }
    });
    run_transaction_async(ClipboardTransaction::new(permit, work));
    Ok(PasteOutcome::Delivered)
}

/// The clipboard restore itself. Runs on the deferred transaction thread while
/// that transaction holds the gate permit — so it must NOT re-acquire the gate
/// (that would deadlock). If the user copied during the paste-settle window,
/// leave their copy intact.
fn restore_clipboard_inner(orig: ClipboardSnapshot, our_count: Option<isize>) {
    restore_snapshot_if_owned(&orig, our_count);
}

fn restore_snapshot_if_owned(orig: &ClipboardSnapshot, our_count: Option<isize>) {
    if user_copied_during_settle(our_count, pasteboard_change_count()) {
        eprintln!("[clipboard] user copied during paste; leaving their clipboard intact");
        return;
    }
    if !restore_pasteboard(orig) {
        eprintln!("[clipboard] restore: writeObjects failed; clipboard left as-is");
    }
}

/// How long the target app gets to service the synthesized Cmd+C. We poll
/// `changeCount` and read as soon as it moves, so a real selection returns at
/// the first write. Only "nothing selected" waits the whole window, and that
/// path is already a failure; a shorter window would misread a slow app's copy
/// (an Electron renderer under load) as no selection.
const COPY_WAIT_MAX: Duration = Duration::from_millis(400);
const COPY_POLL: Duration = Duration::from_millis(10);

/// Poll `read` until it reports a count other than `start`, or `max` passes.
/// Returns the last count seen. Pure over its inputs: unit-tested with a fake
/// counter.
fn wait_for_change_count(
    start: Option<isize>,
    mut read: impl FnMut() -> Option<isize>,
    max: Duration,
    poll: Duration,
) -> Option<isize> {
    if start.is_none() {
        // Nothing to compare against: give the copy the whole window.
        std::thread::sleep(max);
        return read();
    }
    let deadline = Instant::now() + max;
    loop {
        let now = read();
        if now != start || Instant::now() >= deadline {
            return now;
        }
        std::thread::sleep(poll);
    }
}

/// Whether the selection probe may put the user's clipboard back. Yes when
/// nothing wrote after the copy we read, or when what is there now is the text
/// we read (an app that writes one copy twice). `current_text` is read only
/// when the count moved. Pure over its inputs: unit-tested.
fn probe_may_restore(
    copied_count: Option<isize>,
    now_count: Option<isize>,
    probed: &str,
    current_text: impl FnOnce() -> String,
) -> bool {
    !user_copied_during_settle(copied_count, now_count) || current_text() == probed
}

/// Grab the focused app's current selection via a Cmd+C round-trip, returning
/// `None` when nothing is selected. Used by transform mode to read the text the
/// user wants rewritten — universal across AX-blind apps (Electron, terminals)
/// where reading `kAXSelectedText` is unreliable, since Cmd+C always works.
///
/// The original clipboard is saved and restored. A unique sentinel is placed
/// before the copy so an unchanged pasteboard (nothing was selected) is
/// distinguishable from a real empty selection.
pub fn copy_selection_via_clipboard() -> eyre::Result<Option<String>> {
    const SENTINEL: &str = "\u{2063}__dictate_selection_probe__\u{2063}";
    // Hold the clipboard gate for the whole read round-trip so a pending
    // background restore from a previous dictation can't clobber the sentinel
    // between our set and the Cmd+C that reads it.
    let _permit = clipboard_gate().acquire();
    let mut cb = Clipboard::new().map_err(|e| eyre::eyre!("Clipboard::new failed: {e}"))?;
    let saved = restorable_snapshot().ok_or_else(|| {
        eyre::eyre!("the clipboard holds content that cannot be restored; not overwriting it")
    })?;

    // The probe is one of our own writes too: mark it transient so clipboard
    // managers don't record the sentinel (or the selection Cmd+C pulls in).
    write_preserving_snapshot(SENTINEL, &saved)?;
    let our_count = pasteboard_change_count();
    rollback_on_error(synthesize_cmd_c, || restore_snapshot_if_owned(&saved, our_count))?;
    let copied_count = wait_for_change_count(our_count, pasteboard_change_count, COPY_WAIT_MAX, COPY_POLL);

    let after = cb.get_text().unwrap_or_default();

    // Put the user's clipboard back, unless something else was written after
    // the copy we read (the user copied, or a late copy changed the text):
    // restoring then would replace what is there now. An app that writes the
    // same selection twice still gets restored. An empty snapshot means the
    // clipboard was empty: clear rather than leave the sentinel or the
    // selection behind.
    let current_text = || cb.get_text().unwrap_or_default();
    if !probe_may_restore(copied_count, pasteboard_change_count(), &after, current_text) {
        eprintln!("[clipboard] clipboard changed after the selection probe; leaving it as-is");
    } else if saved.is_empty() {
        let _ = write_text_transient("");
    } else {
        restore_pasteboard(&saved);
    }

    if after == SENTINEL || after.is_empty() {
        Ok(None)
    } else {
        Ok(Some(after))
    }
}

fn synthesize_cmd_c() -> eyre::Result<()> {
    let source = clean_event_source()?;
    let key_c: CGKeyCode = crate::key_layout::copy_key();

    let down = CGEvent::new_keyboard_event(source.clone(), key_c, true)
        .map_err(|_| eyre::eyre!("Cmd+C keydown create failed"))?;
    down.set_flags(CGEventFlags::CGEventFlagCommand);
    let up = CGEvent::new_keyboard_event(source, key_c, false)
        .map_err(|_| eyre::eyre!("Cmd+C keyup create failed"))?;
    up.set_flags(CGEventFlags::CGEventFlagNull);
    // An allocation error must not follow an already-queued copy.
    down.post(CGEventTapLocation::HID);
    up.post(CGEventTapLocation::HID);

    Ok(())
}

fn synthesize_cmd_v() -> eyre::Result<()> {
    let source = clean_event_source()?;
    let key_v: CGKeyCode = crate::key_layout::paste_key();

    let key_down = CGEvent::new_keyboard_event(source.clone(), key_v, true)
        .map_err(|_| eyre::eyre!("CGEvent keydown create failed"))?;
    key_down.set_flags(CGEventFlags::CGEventFlagCommand);
    let key_up = CGEvent::new_keyboard_event(source, key_v, false)
        .map_err(|_| eyre::eyre!("CGEvent keyup create failed"))?;
    // Allocate both events before delivery so Err permits safe rollback.
    key_up.set_flags(CGEventFlags::CGEventFlagNull);
    key_down.post(CGEventTapLocation::HID);
    key_up.post(CGEventTapLocation::HID);

    Ok(())
}

/// Synthesize a bare key press + release with no modifier flags.
///
/// Posting with `CGEventFlagNull` is load-bearing: a held push-to-talk
/// modifier must not turn this into e.g. Option-Return (insert newline / no
/// submit) or Option-Tab (app switch). `clean_event_source` plus an explicit
/// null flag guarantee the event carries only the key itself.
fn synthesize_plain_key(key: CGKeyCode) -> eyre::Result<()> {
    let source = clean_event_source()?;

    let down = CGEvent::new_keyboard_event(source.clone(), key, true)
        .map_err(|_| eyre::eyre!("keydown create failed"))?;
    down.set_flags(CGEventFlags::CGEventFlagNull);
    down.post(CGEventTapLocation::HID);

    let up = CGEvent::new_keyboard_event(source, key, false)
        .map_err(|_| eyre::eyre!("keyup create failed"))?;
    up.set_flags(CGEventFlags::CGEventFlagNull);
    up.post(CGEventTapLocation::HID);

    Ok(())
}

/// Synthesize a Return key press + release. Used for trailing voice commands
/// ("press enter", "new line") after the body text is in the field.
pub fn synthesize_return() -> eyre::Result<()> {
    synthesize_plain_key(KEY_RETURN)
}

/// Synthesize a Tab key press + release ("press tab" voice command) — useful
/// for advancing between form fields by voice.
pub fn synthesize_tab() -> eyre::Result<()> {
    synthesize_plain_key(KEY_TAB)
}

/// Synthesize an Escape key press + release ("press escape" voice command) —
/// dismiss dialogs, clear a field, or leave an editor's insert mode by voice.
pub fn synthesize_escape() -> eyre::Result<()> {
    synthesize_plain_key(KEY_ESCAPE)
}

/// Synthesize Cmd+Z ("undo that" voice command) to revert the previous action.
/// Same flag discipline as Cmd+V: set Command only on key-down, clear on key-up
/// so a held push-to-talk modifier can't turn it into a different chord.
pub fn synthesize_undo() -> eyre::Result<()> {
    let source = clean_event_source()?;
    let key_z: CGKeyCode = crate::key_layout::undo_key();

    let down = CGEvent::new_keyboard_event(source.clone(), key_z, true)
        .map_err(|_| eyre::eyre!("Cmd+Z keydown create failed"))?;
    down.set_flags(CGEventFlags::CGEventFlagCommand);
    down.post(CGEventTapLocation::HID);

    let up = CGEvent::new_keyboard_event(source, key_z, false)
        .map_err(|_| eyre::eyre!("Cmd+Z keyup create failed"))?;
    up.set_flags(CGEventFlags::CGEventFlagNull);
    up.post(CGEventTapLocation::HID);

    Ok(())
}

#[cfg(test)]
mod tests;
