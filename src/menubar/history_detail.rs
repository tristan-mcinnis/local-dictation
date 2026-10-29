//! A stored dictation's original audio and ordered text layers. Recovery is
//! explicitly to the clipboard; neither opening nor refreshing this window
//! pastes text or changes the foreground application.

use super::*;
use crate::history::{RecoveryRecord, RecoveryStatus, StageKind};
use objc2_app_kit::NSPopUpButton;
use std::cell::RefCell;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64};

const DETAIL_W: f64 = LAYOUT_PANEL_WIDTH;
const DETAIL_H: f64 = house_theme::LAYOUT_CHAT_HEIGHT;
static DETAIL_UI: OnceLock<HistoryDetailUi> = OnceLock::new();
static POLL_INSTALLED: AtomicBool = AtomicBool::new(false);
/// A refresh read is in flight; the next tick skips instead of stacking.
static POLL_READING: AtomicBool = AtomicBool::new(false);
/// Bumped on every open; a read for an older open is dropped.
static DETAIL_GEN: AtomicU64 = AtomicU64::new(0);
const DETAIL_BUSY: &str = "History is busy, try again.";

define_class!(
    #[unsafe(super(NSWindow))]
    #[name = "LDHistoryDetailWindow"]
    pub(super) struct HistoryDetailWindow;

    impl HistoryDetailWindow {
        #[unsafe(method(close))]
        fn close_detail(&self) {
            stop_audio();
            unsafe { let _: () = msg_send![super(self), close]; }
        }

        #[unsafe(method(cancelOperation:))]
        fn cancel_operation(&self, _sender: *mut AnyObject) {
            stop_audio();
            self.orderOut(None);
        }

        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: *mut AnyObject) -> objc2::runtime::Bool {
            unsafe {
                let flags: usize = msg_send![event, modifierFlags];
                let chars: Retained<NSString> = msg_send![event, charactersIgnoringModifiers];
                if let Some(action) = detail_key_action(&chars.to_string(), flags) {
                    perform_action(action);
                    return objc2::runtime::Bool::YES;
                }
                msg_send![super(self), performKeyEquivalent: event]
            }
        }

        #[unsafe(method(selectHistoryLayer:))]
        fn select_layer(&self, _sender: *mut AnyObject) {
            if let Some(ui) = DETAIL_UI.get() { update_layer(ui); }
        }
        #[unsafe(method(copyHistoryLayer:))]
        fn copy_layer(&self, _sender: *mut AnyObject) { perform_action(DetailAction::Copy); }
        #[unsafe(method(playHistoryAudio:))]
        fn play_audio(&self, _sender: *mut AnyObject) { perform_action(DetailAction::Play); }
        #[unsafe(method(revealHistoryAudio:))]
        fn reveal_audio(&self, _sender: *mut AnyObject) { perform_action(DetailAction::Reveal); }
        #[unsafe(method(recoverHistoryAudio:))]
        fn recover_audio(&self, _sender: *mut AnyObject) { perform_action(DetailAction::Recover); }
    }
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailAction {
    Close,
    Copy,
    Play,
    Reveal,
    Recover,
    Raw,
    Cleaned,
    Previous,
    Next,
}

fn detail_key_action(chars: &str, flags: usize) -> Option<DetailAction> {
    const COMMAND: usize = 1 << 20;
    const SHIFT: usize = 1 << 17;
    const OPTION: usize = 1 << 19;
    const CONTROL: usize = 1 << 18;
    let modifiers = flags & (COMMAND | SHIFT | OPTION | CONTROL);
    if modifiers == (COMMAND | SHIFT) && chars.eq_ignore_ascii_case("c") {
        return Some(DetailAction::Copy);
    }
    if modifiers != COMMAND {
        return None;
    }
    match chars.to_ascii_lowercase().as_str() {
        "w" => Some(DetailAction::Close),
        "p" => Some(DetailAction::Play),
        "o" => Some(DetailAction::Reveal),
        "r" => Some(DetailAction::Recover),
        "1" => Some(DetailAction::Raw),
        "2" => Some(DetailAction::Cleaned),
        "[" => Some(DetailAction::Previous),
        "]" => Some(DetailAction::Next),
        _ => None,
    }
}

struct DetailButton {
    button: Retained<NSButton>,
    label: Retained<NSTextField>,
    ground: Retained<slate::SlateView>,
}
impl DetailButton {
    fn set_enabled(&self, enabled: bool) {
        self.button.setEnabled(enabled);
        self.ground.setAlphaValue(if enabled { 1.0 } else { 0.45 });
    }
    fn set_title(&self, text: &str) {
        self.label.setStringValue(&NSString::from_str(text));
    }
}

pub(super) struct HistoryDetailUi {
    pub(super) window: Retained<HistoryDetailWindow>,
    pub(super) panel: Retained<slate::SlateView>,
    heading: Retained<NSTextField>,
    audio_info: Retained<NSTextField>,
    layers: Retained<NSPopUpButton>,
    text: Retained<NSTextView>,
    status: Retained<NSTextField>,
    play: DetailButton,
    reveal: DetailButton,
    copy: DetailButton,
    recover: DetailButton,
    record: RefCell<Option<RecoveryRecord>>,
    sound: RefCell<Option<Retained<AnyObject>>>,
    fingerprint: RefCell<String>,
    requested: RefCell<Option<i64>>,
}
// All AppKit objects and RefCells are created/read/written only on the main
// thread. Worker state reaches this UI through the database, never pointers.
unsafe impl Send for HistoryDetailUi {}
unsafe impl Sync for HistoryDetailUi {}

fn button(
    mtm: MainThreadMarker,
    parent: &NSView,
    target: &AnyObject,
    x: f64,
    y: f64,
    width: f64,
    title: &str,
    action: objc2::runtime::Sel,
    shortcut: &str,
) -> DetailButton {
    let ground = slate::SlateView::new(
        mtm,
        NSRect::new(NSPoint::new(x, y), NSSize::new(width, CONTROL_CHIP)),
        slate::Paint::Chip,
        RADIUS_SM,
    );
    let label = slate::label(
        mtm,
        title,
        TYPE_BODY_SMALL,
        WEIGHT_MEDIUM,
        theme::text_primary(),
    );
    slate::place_left(&label, SPACE_SM, CONTROL_CHIP / 2.0);
    ground.addSubview(&label);
    let click = slate::click_overlay(mtm, ground.bounds(), target, action, 0);
    let hint = format!("{title} ({shortcut})");
    click.setToolTip(Some(&NSString::from_str(&hint)));
    unsafe {
        let _: () = msg_send![&*click, setAccessibilityLabel: &*NSString::from_str(&hint)];
    }
    ground.addSubview(&click);
    parent.addSubview(&ground);
    DetailButton {
        button: click,
        label,
        ground,
    }
}

pub(super) fn build_history_detail(mtm: MainThreadMarker) -> HistoryDetailUi {
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(DETAIL_W, DETAIL_H));
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::FullSizeContentView;
    let window: Retained<HistoryDetailWindow> = unsafe {
        msg_send![HistoryDetailWindow::alloc(mtm), initWithContentRect: frame, styleMask: style, backing: NSBackingStoreType::Buffered, defer: false]
    };
    let panel = slate::SlateView::new(mtm, frame, slate::Paint::Surface, 0.0);
    let heading = slate::label(
        mtm,
        "Dictation",
        house_theme::TYPE_HEADING,
        WEIGHT_SEMIBOLD,
        theme::text_primary(),
    );
    heading.setFrame(NSRect::new(
        NSPoint::new(SPACE_LG, DETAIL_H - TITLEBAR_H - CONTROL_ROW),
        NSSize::new(DETAIL_W - SPACE_LG * 2.0, CONTROL_ROW),
    ));
    panel.addSubview(&heading);
    let audio_y = DETAIL_H - TITLEBAR_H - CONTROL_ROW - CONTROL_INPUT;
    let audio_info = slate::label(
        mtm,
        "Original audio",
        TYPE_META,
        WEIGHT_REGULAR,
        theme::text_primary(),
    );
    audio_info.setFrame(NSRect::new(
        NSPoint::new(SPACE_LG, audio_y),
        NSSize::new(DETAIL_W - SPACE_LG * 2.0, CONTROL_ROW),
    ));
    panel.addSubview(&audio_info);
    let control_y = audio_y - CONTROL_CHIP - SPACE_XS;
    let play = button(
        mtm,
        &panel,
        &window,
        SPACE_LG,
        control_y,
        CONTROL_INPUT * 2.0,
        "Play audio",
        sel!(playHistoryAudio:),
        "⌘P",
    );
    let reveal = button(
        mtm,
        &panel,
        &window,
        SPACE_LG + CONTROL_INPUT * 2.0 + SPACE_SM,
        control_y,
        CONTROL_INPUT * 2.0,
        "Show in Finder",
        sel!(revealHistoryAudio:),
        "⌘O",
    );
    let layer_y = control_y - CONTROL_ROW - SPACE_LG;
    let layers: Retained<NSPopUpButton> = unsafe {
        msg_send![NSPopUpButton::alloc(mtm), initWithFrame: NSRect::new(NSPoint::new(SPACE_LG, layer_y), NSSize::new(DETAIL_W / 2.0, CONTROL_ROW)), pullsDown: false]
    };
    unsafe {
        let _: () = msg_send![&*layers, setTarget: &*window];
        let _: () = msg_send![&*layers, setAction: sel!(selectHistoryLayer:)];
        let _: () = msg_send![&*layers, setAccessibilityLabel: &*NSString::from_str("Text layer. Command 1 Raw, Command 2 Cleaned")];
    }
    panel.addSubview(&layers);
    let copy = button(
        mtm,
        &panel,
        &window,
        DETAIL_W - SPACE_LG - CONTROL_INPUT * 2.5,
        layer_y + (CONTROL_ROW - CONTROL_CHIP) / 2.0,
        CONTROL_INPUT * 2.5,
        "Copy layer",
        sel!(copyHistoryLayer:),
        "⇧⌘C",
    );
    let text_y = CONTROL_FOOTER * 2.0 + SPACE_SM;
    let scroll_frame = NSRect::new(
        NSPoint::new(SPACE_LG, text_y),
        NSSize::new(DETAIL_W - SPACE_LG * 2.0, layer_y - SPACE_SM - text_y),
    );
    let scroll: Retained<NSScrollView> =
        unsafe { msg_send![NSScrollView::alloc(mtm), initWithFrame: scroll_frame] };
    scroll.setHasVerticalScroller(true);
    scroll.setAutohidesScrollers(true);
    scroll.setDrawsBackground(false);
    let text: Retained<NSTextView> = unsafe {
        msg_send![NSTextView::alloc(mtm), initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), scroll.contentSize())]
    };
    unsafe {
        text.setEditable(false);
        text.setSelectable(true);
        text.setRichText(false);
        text.setDrawsBackground(false);
        text.setFont(Some(&slate::font(house_theme::TYPE_BODY, WEIGHT_REGULAR)));
        let _: () = msg_send![&*text, setTextColor: &*theme::text_primary()];
        let _: () = msg_send![&*text, setTextContainerInset: NSSize::new(SPACE_XS, SPACE_XS)];
        let _: () =
            msg_send![&*text, setAccessibilityLabel: &*NSString::from_str("Stored text layer")];
        text.setMinSize(NSSize::new(0.0, scroll.contentSize().height));
        text.setMaxSize(NSSize::new(f64::MAX, f64::MAX));
        text.setVerticallyResizable(true);
        text.setHorizontallyResizable(false);
    }
    scroll.setDocumentView(Some(&*text));
    panel.addSubview(&scroll);
    let status = slate::label(
        mtm,
        "Original audio and text stay on this Mac.",
        TYPE_META,
        WEIGHT_REGULAR,
        theme::text_primary(),
    );
    status.setFrame(NSRect::new(
        NSPoint::new(SPACE_LG, CONTROL_FOOTER + SPACE_XS),
        NSSize::new(DETAIL_W - SPACE_LG * 2.0, CONTROL_ROW),
    ));
    panel.addSubview(&status);
    let recover = button(
        mtm,
        &panel,
        &window,
        SPACE_LG,
        SPACE_SM,
        CONTROL_INPUT * 4.0,
        "Recover to Clipboard",
        sel!(recoverHistoryAudio:),
        "⌘R",
    );
    let destination = slate::label(
        mtm,
        "Re-transcribe saved audio · copies only",
        TYPE_META,
        WEIGHT_REGULAR,
        theme::text_primary(),
    );
    slate::place_left(
        &destination,
        SPACE_LG + CONTROL_INPUT * 4.0 + SPACE_MD,
        SPACE_SM + CONTROL_CHIP / 2.0,
    );
    panel.addSubview(&destination);
    unsafe {
        window.setReleasedWhenClosed(false);
    }
    window.setTitle(&NSString::from_str("Dictation History"));
    window.setContentView(Some(&*panel));
    slate::apply_panel_chrome(&window);
    window.center();
    HistoryDetailUi {
        window,
        panel,
        heading,
        audio_info,
        layers,
        text,
        status,
        play,
        reveal,
        copy,
        recover,
        record: RefCell::new(None),
        sound: RefCell::new(None),
        fingerprint: RefCell::new(String::new()),
        requested: RefCell::new(None),
    }
}

fn fingerprint(record: &RecoveryRecord) -> String {
    format!(
        "{:?}:{}:{:?}:{:?}:{:?}",
        record.status,
        record.stages.len(),
        record.stages.last().map(|s| s.id),
        record.error,
        record
            .audio
            .as_ref()
            .map(|audio| (audio.expires_at, audio.path.is_file()))
    )
}

pub(super) fn populate_detail(ui: &HistoryDetailUi, record: RecoveryRecord) {
    let selected_id = selected_stage(ui).map(|s| s.id);
    ui.heading.setStringValue(&NSString::from_str(&format!(
        "Dictation #{} · {}",
        record.entry.id,
        record.status.label()
    )));
    let audio_line = match &record.audio {
        Some(audio) => {
            let seconds = audio.duration_ms / 1_000;
            let expiry = audio.expires_at.map(|stamp| {
                let formatter = NSDateFormatter::new();
                formatter.setDateFormat(Some(&NSString::from_str("MMM d, yyyy")));
                formatter
                    .stringFromDate(&NSDate::dateWithTimeIntervalSince1970(stamp as f64))
                    .to_string()
            });
            format!(
                "Original audio · {}:{:02}{}",
                seconds / 60,
                seconds % 60,
                expiry
                    .map(|day| format!(" · Kept until {day}"))
                    .unwrap_or_default()
            )
        }
        None if record.status == RecoveryStatus::Recording => {
            "Recording in progress · Original audio is not yet available".to_string()
        }
        None if record.stages.is_empty() => {
            "Original audio unavailable · No text was saved".to_string()
        }
        None => "Original audio unavailable · Text layers are still available".to_string(),
    };
    ui.audio_info
        .setStringValue(&NSString::from_str(&audio_line));
    let available = record.audio.as_ref().is_some_and(|a| a.path.is_file());
    ui.play.set_enabled(available);
    ui.reveal.set_enabled(available);
    ui.recover.set_enabled(
        available
            && record.status != RecoveryStatus::Recovering
            && record.status != RecoveryStatus::Recording,
    );
    ui.layers.removeAllItems();
    for title in layer_menu_titles(&record.stages) {
        ui.layers.addItemWithTitle(&NSString::from_str(&title));
    }
    let index = selected_id
        .and_then(|id| record.stages.iter().position(|s| s.id == id))
        .unwrap_or_else(|| record.stages.len().saturating_sub(1));
    if record.stages.is_empty() {
        ui.layers
            .addItemWithTitle(&NSString::from_str("No text layers yet"));
    }
    ui.layers.selectItemAtIndex(index as isize);
    ui.layers.setEnabled(!record.stages.is_empty());
    ui.copy.set_enabled(!record.stages.is_empty());
    *ui.fingerprint.borrow_mut() = fingerprint(&record);
    let note = record.error.clone().unwrap_or_else(|| match record.status {
        RecoveryStatus::Recovering => "Recovering saved audio…".to_string(),
        RecoveryStatus::Copied => {
            "Recovered text is saved. Choose a layer to copy it again.".to_string()
        }
        RecoveryStatus::Recording => {
            "Recording in progress. Audio and text will appear as they are saved.".to_string()
        }
        _ if record.stages.is_empty() && record.audio.is_some() => {
            "Recover saved audio to create a transcript.".to_string()
        }
        _ if record.stages.is_empty() => {
            "No saved text or audio is available for this dictation.".to_string()
        }
        _ => {
            "Choose a text layer to inspect or copy. Recovery copies to the clipboard.".to_string()
        }
    });
    *ui.record.borrow_mut() = Some(record);
    update_layer(ui);
    status(ui, &note);
}

/// Stored ordinals are already 1-based (`COALESCE(MAX(ordinal),0)+1`), so they
/// are shown as they are. Adding one made every real record start at
/// "2. Raw transcript"; the 0-based render fixtures hid it.
fn layer_menu_titles(stages: &[crate::history::TextStage]) -> Vec<String> {
    stages
        .iter()
        .map(|stage| format!("{}. {}", stage.ordinal, stage.title))
        .collect()
}

fn recovery_command(
    record: &RecoveryRecord,
    requested: bool,
) -> Result<crate::ipc::Command, &'static str> {
    if requested
        || matches!(
            record.status,
            RecoveryStatus::Recording | RecoveryStatus::Recovering
        )
    {
        return Err("Recovery is already running or this recording has not finished.");
    }
    if record.entry.id <= 0 {
        return Err("This dictation was deleted.");
    }
    Ok(crate::ipc::Command::Recover(record.entry.id))
}

fn selected_stage(ui: &HistoryDetailUi) -> Option<crate::history::TextStage> {
    let index = ui.layers.indexOfSelectedItem();
    if index < 0 {
        return None;
    }
    ui.record
        .borrow()
        .as_ref()?
        .stages
        .get(index as usize)
        .cloned()
}
fn update_layer(ui: &HistoryDetailUi) {
    let text = selected_stage(ui).map(|s| s.text).unwrap_or_else(|| {
        if ui.record.borrow().as_ref().is_some_and(|record| record.audio.is_some()) {
            "No transcript was saved. Recover the original audio to transcribe it and copy the result to the clipboard.".to_string()
        } else {
            "No text was saved for this dictation, and its original audio is unavailable.".to_string()
        }
    });
    if ui.text.string().to_string() != text {
        ui.text.setString(&NSString::from_str(&text));
    }
}

fn status(ui: &HistoryDetailUi, message: &str) {
    ui.status.setStringValue(&NSString::from_str(message));
}

/// Open the detail window. The read runs on a background thread with a short
/// wait, so a writer finishing a dictation cannot freeze the main thread.
pub(crate) fn show_history_detail(id: i64) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let ui = DETAIL_UI.get_or_init(|| build_history_detail(mtm));
    stop_audio();
    *ui.requested.borrow_mut() = None;
    let generation = DETAIL_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || {
        let read = crate::history::entry_detail_ui(id);
        run_on_main(move || finish_show_detail(generation, read));
    });
}

fn finish_show_detail(
    generation: u64,
    read: crate::history::UiRead<Option<RecoveryRecord>>,
) {
    use crate::history::UiRead;
    let (Some(mtm), Some(ui)) = (MainThreadMarker::new(), DETAIL_UI.get()) else {
        return;
    };
    if generation != DETAIL_GEN.load(Ordering::SeqCst) {
        return;
    }
    match read {
        UiRead::Ok(Some(record)) => populate_detail(ui, record),
        UiRead::Ok(None) => clear_deleted_detail(ui),
        UiRead::Busy => status(ui, DETAIL_BUSY),
        UiRead::Failed(_) => status(
            ui,
            "Could not load this dictation. Close History and try again.",
        ),
    }
    ui.window.makeKeyAndOrderFront(None);
    ui.window.makeFirstResponder(Some(&*ui.layers));
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
    install_detail_poll();
}

/// A deletion removes the displayed content as well as disabling actions.
/// This same transition is exercised by the offscreen native regression.
pub(super) fn clear_deleted_detail(ui: &HistoryDetailUi) {
    stop_audio();
    *ui.record.borrow_mut() = None;
    *ui.requested.borrow_mut() = None;
    ui.fingerprint.borrow_mut().clear();
    ui.heading
        .setStringValue(&NSString::from_str("Dictation deleted"));
    ui.audio_info.setStringValue(&NSString::from_str(
        "Original audio and text are no longer available",
    ));
    ui.text.setString(&NSString::from_str(""));
    ui.layers.removeAllItems();
    ui.layers.setEnabled(false);
    ui.recover.set_enabled(false);
    ui.copy.set_enabled(false);
    ui.play.set_enabled(false);
    ui.reveal.set_enabled(false);
    status(ui, "This dictation was deleted.");
}

pub(super) fn verify_detail_layers(
    ui: &HistoryDetailUi,
    expected: &RecoveryRecord,
) -> eyre::Result<()> {
    let count = expected.stages.len();
    eyre::ensure!(
        ui.layers.numberOfItems() as usize == count.max(1),
        "A stored text layer disappeared from the native picker"
    );
    for (index, stage) in expected.stages.iter().enumerate() {
        ui.layers.selectItemAtIndex(index as isize);
        update_layer(ui);
        eyre::ensure!(
            selected_stage(ui).as_ref().map(|value| value.id) == Some(stage.id),
            "Native picker selected the wrong stored stage"
        );
        eyre::ensure!(
            ui.text.string().to_string() == stage.text,
            "Native picker displayed the wrong stored text"
        );
    }
    Ok(())
}

pub(super) fn verify_deleted_detail(ui: &HistoryDetailUi) -> eyre::Result<()> {
    eyre::ensure!(
        ui.record.borrow().is_none() && ui.requested.borrow().is_none(),
        "Deleted detail retained record state"
    );
    eyre::ensure!(
        ui.text.string().to_string().is_empty(),
        "Deleted detail retained transcript text"
    );
    eyre::ensure!(
        ui.layers.numberOfItems() == 0 && !ui.layers.isEnabled(),
        "Deleted detail retained a selectable layer"
    );
    eyre::ensure!(
        !ui.copy.button.isEnabled()
            && !ui.play.button.isEnabled()
            && !ui.reveal.button.isEnabled()
            && !ui.recover.button.isEnabled(),
        "Deleted detail retained an enabled action"
    );
    eyre::ensure!(
        ui.heading.stringValue().to_string() == "Dictation deleted",
        "Deleted detail retained an old heading"
    );
    Ok(())
}

fn stop_audio() {
    if let Some(ui) = DETAIL_UI.get() {
        if let Some(sound) = ui.sound.borrow_mut().take() {
            unsafe {
                let _: () = msg_send![&*sound, stop];
            }
        }
        ui.play.set_title("Play audio");
    }
}

fn audio_for_action(
    id: Option<i64>,
    lookup: impl FnOnce(i64) -> eyre::Result<Option<PathBuf>>,
) -> Result<PathBuf, String> {
    let id = id.ok_or("This dictation was deleted.")?;
    lookup(id)
        .map_err(|_| "Could not read the original audio.".to_string())?
        .ok_or_else(|| {
            "Original audio is unavailable. It may have been deleted, expired, or never saved."
                .to_string()
        })
}

/// Look up the saved audio for the shown record off the main thread (the
/// lookup opens the store and may sweep expired audio, which writes), then run
/// `then` on the main thread with the answer.
fn with_validated_audio(
    ui: &HistoryDetailUi,
    then: impl FnOnce(&HistoryDetailUi, Result<PathBuf, String>) + Send + 'static,
) {
    let id = ui.record.borrow().as_ref().map(|r| r.entry.id);
    std::thread::spawn(move || {
        let found = audio_for_action(id, crate::history::recovery_audio_path);
        run_on_main(move || {
            if let Some(ui) = DETAIL_UI.get() {
                then(ui, found);
            }
        });
    });
}

fn play_file(ui: &HistoryDetailUi, path: &Path) {
    if ui.sound.borrow().is_some() {
        stop_audio();
        status(ui, "Playback stopped.");
        return;
    }
    let sound: Option<Retained<AnyObject>> = unsafe {
        let allocated: *mut AnyObject = msg_send![objc2::class!(NSSound), alloc];
        let initialized: *mut AnyObject = msg_send![allocated,
            initWithContentsOfFile: &*NSString::from_str(&path.to_string_lossy()), byReference: true];
        Retained::from_raw(initialized)
    };
    if let Some(sound) = sound {
        let playing: bool = unsafe { msg_send![&*sound, play] };
        if playing {
            *ui.sound.borrow_mut() = Some(sound);
            ui.play.set_title("Stop audio");
            status(ui, "Playing original audio · ⌘P stops playback");
            return;
        }
    }
    status(
        ui,
        "Could not play the original audio. Show it in Finder to inspect the file.",
    );
}

fn perform_action(action: DetailAction) {
    let Some(ui) = DETAIL_UI.get() else {
        return;
    };
    match action {
        DetailAction::Close => ui.window.close(),
        DetailAction::Copy => {
            let Some(stage) = selected_stage(ui) else {
                status(ui, "No text layer is available to copy.");
                return;
            };
            let message = format!("Copied {} to the clipboard.", stage.title);
            status(ui, "Copying…");
            copy_to_clipboard(stage.text, move |copied| {
                if let Some(ui) = DETAIL_UI.get() {
                    status(ui, if copied { &message } else { "Could not copy this layer. Select the text and press ⌘C." });
                }
            });
        }
        DetailAction::Play | DetailAction::Reveal => with_validated_audio(ui, move |ui, found| match found {
            Ok(path) if action == DetailAction::Play => play_file(ui, &path),
            Ok(path) => match std::process::Command::new("/usr/bin/open")
                .arg("-R")
                .arg(path)
                .spawn()
            {
                Ok(_) => status(ui, "Original audio shown in Finder."),
                Err(_) => status(ui, "Could not open Finder for this audio."),
            },
            Err(message) => {
                stop_audio();
                status(ui, &message);
            }
        }),
        DetailAction::Recover => with_validated_audio(ui, |ui, found| {
            if let Err(message) = found {
                status(ui, &message);
                return;
            }
            start_recovery(ui);
        }),
        DetailAction::Raw | DetailAction::Cleaned | DetailAction::Previous | DetailAction::Next => {
            let record = ui.record.borrow();
            let Some(record) = record.as_ref() else {
                return;
            };
            if record.stages.is_empty() {
                return;
            }
            let index = match action {
                DetailAction::Raw => record.stages.iter().rposition(|s| s.kind == StageKind::Raw),
                DetailAction::Cleaned => record
                    .stages
                    .iter()
                    .rposition(|s| s.kind == StageKind::Cleaned),
                DetailAction::Previous => {
                    Some((ui.layers.indexOfSelectedItem() - 1).max(0) as usize)
                }
                _ => Some(
                    (ui.layers.indexOfSelectedItem() as usize + 1).min(record.stages.len() - 1),
                ),
            };
            if let Some(index) = index {
                ui.layers.selectItemAtIndex(index as isize);
                update_layer(ui);
            } else {
                status(ui, "That text layer has not been saved for this dictation.");
            }
        }
    }
}

fn start_recovery(ui: &HistoryDetailUi) {
    let command = {
        let record = ui.record.borrow();
        let Some(record) = record.as_ref() else {
            return;
        };
        match recovery_command(record, ui.requested.borrow().is_some()) {
            Ok(command) => command,
            Err(message) => {
                status(ui, message);
                return;
            }
        }
    };
    let crate::ipc::Command::Recover(id) = command else {
        return;
    };
    stop_audio();
    match crate::ipc::send(command) {
        Ok(reply) if reply.starts_with("ok ") => {
            *ui.requested.borrow_mut() = Some(id);
            ui.recover.set_enabled(false);
            status(
                ui,
                "Recovery requested · The result will be copied to the clipboard.",
            );
        }
        Ok(reply) => status(
            ui,
            reply
                .strip_prefix("err ")
                .unwrap_or("Recovery could not start. Try again."),
        ),
        Err(_) => status(
            ui,
            "Local Dictation is not running. Launch it, then try Recover again.",
        ),
    }
}

fn poll_detail() {
    let Some(ui) = DETAIL_UI.get().filter(|ui| ui.window.isVisible()) else {
        return;
    };
    let playback_finished = ui.sound.borrow().as_ref().is_some_and(|sound| {
        let playing: bool = unsafe { msg_send![&**sound, isPlaying] };
        !playing
    });
    if playback_finished {
        ui.sound.borrow_mut().take();
        ui.play.set_title("Play audio");
    }
    let Some(id) = ui.record.borrow().as_ref().map(|r| r.entry.id) else {
        return;
    };
    if POLL_READING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let read = crate::history::entry_detail_ui(id);
        run_on_main(move || {
            POLL_READING.store(false, Ordering::SeqCst);
            apply_poll(id, read);
        });
    });
}

fn apply_poll(id: i64, read: crate::history::UiRead<Option<RecoveryRecord>>) {
    use crate::history::UiRead;
    let Some(ui) = DETAIL_UI.get().filter(|ui| ui.window.isVisible()) else {
        return;
    };
    // The user may have opened another record while this read was in flight.
    if ui.record.borrow().as_ref().map(|r| r.entry.id) != Some(id) {
        return;
    }
    match read {
        UiRead::Ok(Some(record)) => {
            let changed = *ui.fingerprint.borrow() != fingerprint(&record);
            if changed {
                if record.audio.is_none() {
                    stop_audio();
                }
                let recovering = record.status == RecoveryStatus::Recovering;
                let note = if recovering {
                    "Recovering saved audio…".to_string()
                } else if ui.requested.borrow().is_some() {
                    record.error.clone().unwrap_or_else(|| match record.status {
                        RecoveryStatus::Completed | RecoveryStatus::Copied => {
                            "Copied recovered text to the clipboard.".to_string()
                        }
                        RecoveryStatus::Cancelled => {
                            "Recovery cancelled. Saved audio and earlier text layers remain."
                                .to_string()
                        }
                        _ => "Recovery finished. Inspect the saved text layers.".to_string(),
                    })
                } else {
                    record.error.clone().unwrap_or_default()
                };
                populate_detail(ui, record);
                if !note.is_empty() {
                    status(ui, &note);
                }
                if !recovering {
                    *ui.requested.borrow_mut() = None;
                }
            }
        }
        UiRead::Ok(None) => clear_deleted_detail(ui),
        // A busy store is a writer at work; the next tick tries again.
        UiRead::Busy => {}
        UiRead::Failed(_) => status(
            ui,
            "Could not refresh this dictation. Stored audio and text have not been changed.",
        ),
    }
}

fn install_detail_poll() {
    if POLL_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe extern "C-unwind" fn callback(_timer: *mut CFRunLoopTimer, _info: *mut c_void) {
        poll_detail();
    }
    unsafe {
        let mut context = CFRunLoopTimerContext {
            version: 0,
            info: std::ptr::null_mut(),
            retain: None,
            release: None,
            copyDescription: None,
        };
        if let Some(timer) = CFRunLoopTimer::new(
            None,
            CFAbsoluteTimeGetCurrent() + 0.5,
            0.5,
            0,
            0,
            Some(callback),
            &mut context,
        ) {
            if let Some(run_loop) = CFRunLoop::main() {
                run_loop.add_timer(Some(&timer), kCFRunLoopCommonModes);
                std::mem::forget(timer);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record() -> RecoveryRecord {
        RecoveryRecord {
            entry: Entry {
                id: 42,
                text: "Saved text".to_string(),
                created_at: 0,
            },
            status: RecoveryStatus::Cancelled,
            audio: None,
            stages: vec![],
            error: None,
        }
    }

    #[test]
    fn recovery_action_is_by_record_id_and_never_the_inserting_retry() {
        let mut record = record();
        assert_eq!(
            recovery_command(&record, false).unwrap(),
            crate::ipc::Command::Recover(42)
        );
        assert!(recovery_command(&record, true).is_err());
        record.status = RecoveryStatus::Recovering;
        assert!(recovery_command(&record, false).is_err());
        record.status = RecoveryStatus::Failed;
        assert_eq!(
            recovery_command(&record, false).unwrap(),
            crate::ipc::Command::Recover(42)
        );
    }

    #[test]
    fn repeated_raw_and_cleaned_layers_have_distinct_menu_titles() {
        let stage = |id, ordinal| crate::history::TextStage {
            id,
            ordinal,
            kind: StageKind::Raw,
            title: "Raw".to_string(),
            text: "Transcript".to_string(),
            metadata: "{}".to_string(),
            created_at: 0,
        };
        assert_eq!(
            layer_menu_titles(&[stage(1, 1), stage(2, 3)]),
            ["1. Raw", "3. Raw"]
        );
    }

    #[test]
    fn stale_and_deleted_audio_have_recoverable_errors() {
        let missing = audio_for_action(Some(9), |_| Ok(None)).unwrap_err();
        assert!(missing.contains("unavailable"));
        assert!(!missing.contains("text layers are still available"));
        assert_eq!(
            audio_for_action(None, |_| panic!("deleted record must not look up a path"))
                .unwrap_err(),
            "This dictation was deleted."
        );
        let failure =
            audio_for_action(Some(9), |_| Err(eyre::eyre!("private storage failure"))).unwrap_err();
        assert_eq!(failure, "Could not read the original audio.");
        let valid = audio_for_action(Some(42), |id| {
            assert_eq!(id, 42);
            Ok(Some(PathBuf::from("/tmp/fixture.wav")))
        })
        .unwrap();
        assert_eq!(valid, PathBuf::from("/tmp/fixture.wav"));
    }

    #[test]
    fn detail_shortcuts_are_explicit_clipboard_and_inspection_actions() {
        let command = 1 << 20;
        assert_eq!(detail_key_action("w", command), Some(DetailAction::Close));
        assert_eq!(detail_key_action("r", command), Some(DetailAction::Recover));
        assert_eq!(detail_key_action("p", command), Some(DetailAction::Play));
        assert_eq!(detail_key_action("1", command), Some(DetailAction::Raw));
        assert_eq!(detail_key_action("2", command), Some(DetailAction::Cleaned));
        assert_eq!(
            detail_key_action("c", command | (1 << 17)),
            Some(DetailAction::Copy)
        );
        assert_eq!(detail_key_action("v", command), None);
        assert_eq!(detail_key_action("r", 0), None);
    }
}
