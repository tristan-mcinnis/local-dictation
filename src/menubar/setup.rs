//! Setup & permissions — the house "Setup checklist".
//!
//! One window that gets a fresh install from a launch to a working dictation,
//! and says which of the three requirements is missing when one is. Before it,
//! a first launch without Accessibility showed a system prompt and then
//! nothing at all: the daemon returned an error and the process was gone.
//!
//! Anatomy (registered in `design-system/components.json` as
//! `setup-checklist`): a panel at `LAYOUT_PANEL_WIDTH` on the opaque surface,
//! a `title` heading over a `meta` subtitle, a card of three `CONTROL_ROW`
//! rows — Microphone, Accessibility, Models — each a 26 px icon tile, the
//! requirement in `label` over its live state in `meta`, a status dot and one
//! chip carrying its action, then a footer well with the hint.
//!
//! **Live.** [`refresh`] runs from the menu bar's once-a-second tick while the
//! window is visible, so a grant made in System Settings lands here without a
//! relaunch, the event tap installs itself, and the row's action drops away.

use super::*;
use crate::menubar::house_theme::{TYPE_LABEL, TYPE_TITLE};
use crate::model_fetch;
use crate::permissions::{self, Grant};

pub(super) struct SetupUi {
    pub(super) window: Retained<NSWindow>,
    /// The painted panel ground — what the offscreen renderer snapshots.
    pub(super) panel: Retained<NSView>,
    pub(super) subtitle: Retained<NSTextField>,
    rows: Vec<SetupRow>,
    footer_status: Retained<NSTextField>,
}
// Only ever touched on the main thread (menu actions and the UI timer), so
// parking the AppKit pointers in a static is safe — same as the other windows.
unsafe impl Send for SetupUi {}
unsafe impl Sync for SetupUi {}

struct SetupRow {
    requirement: Requirement,
    detail: Retained<NSTextField>,
    /// Rebuilt on every refresh: the dot's paint and the chip's text both
    /// change, and repainting beats keeping two mutable views in step.
    holder: Retained<slate::SlateView>,
    button: Retained<NSButton>,
    mid_y: f64,
}

/// The three things a dictation needs. The order is the order a user fixes
/// them in, which is also [`crate::health::Fault`]'s priority order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Requirement {
    Microphone,
    Accessibility,
    Models,
}

impl Requirement {
    /// Tag carried by the row's button, so one selector serves all three.
    pub(super) fn tag(self) -> isize {
        match self {
            Requirement::Microphone => 0,
            Requirement::Accessibility => 1,
            Requirement::Models => 2,
        }
    }

    pub(super) fn from_tag(tag: isize) -> Option<Requirement> {
        match tag {
            0 => Some(Requirement::Microphone),
            1 => Some(Requirement::Accessibility),
            2 => Some(Requirement::Models),
            _ => None,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Requirement::Microphone => "Microphone",
            Requirement::Accessibility => "Accessibility",
            Requirement::Models => "Models",
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            Requirement::Microphone => "mic",
            Requirement::Accessibility => "keyboard",
            Requirement::Models => "waveform",
        }
    }
}

/// What one row shows right now. Pure data, so the wording and the
/// satisfied/unsatisfied decision are unit-testable without AppKit.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct RowState {
    pub(crate) satisfied: bool,
    pub(crate) detail: String,
    /// `None` when there is nothing left to do.
    pub(crate) action: Option<String>,
}

const SETUP_W: f64 = LAYOUT_PANEL_WIDTH;
/// Titlebar + heading + gutter + three rows + gutter + footer, all tokens.
const SETUP_H: f64 =
    TITLEBAR_H + HEADER_H + SPACE_MD + CARD_H + SPACE_MD + CONTROL_FOOTER;
const CARD_H: f64 = 3.0 * ROW_H + 2.0 * SPACE_SM;
/// Room for the traffic lights on a full-size-content window.
const TITLEBAR_H: f64 = 28.0;
/// The heading band: `title` over a `meta` subtitle.
const HEADER_H: f64 = 60.0;
const CARD_PAD: f64 = SPACE_LG;
const SETUP_HINT: &str = "You can close this window; the menu bar keeps the same status.";

// ─── State, as plain data ───────────────────────────────────────────────

/// The microphone row.
pub(crate) fn microphone_state(grant: Grant) -> RowState {
    match grant {
        Grant::Granted | Grant::Unknown => RowState {
            satisfied: true,
            detail: "Allowed".to_string(),
            action: None,
        },
        Grant::Undecided => RowState {
            satisfied: false,
            detail: "Not asked yet — nothing is recorded until you allow it.".to_string(),
            action: Some("Allow".to_string()),
        },
        Grant::Denied => RowState {
            satisfied: false,
            detail: "Denied. Recordings would be silent.".to_string(),
            action: Some("Open Settings".to_string()),
        },
        Grant::Restricted => RowState {
            satisfied: false,
            detail: "Blocked by a policy on this Mac.".to_string(),
            action: Some("Open Settings".to_string()),
        },
    }
}

/// The one chip title that means "restart the daemon": both the Accessibility
/// and the Models row can reach a state that only a relaunch clears, and
/// [`row_action`] keys off this string so they cannot drift apart.
pub(crate) const RELAUNCH: &str = "Relaunch";

/// The Accessibility row.
///
/// `install_failed` is the awkward third state: macOS says this process is
/// trusted, but creating the event tap still failed (the TCC change can be
/// reported before it has propagated). The grant is on, so "Open Settings"
/// would send the user to a switch that is already correct; the hotkey is dead
/// until the tap is created again, and the tap can only be created at startup.
pub(crate) fn accessibility_state(trusted: bool, install_failed: bool) -> RowState {
    if trusted && install_failed {
        return RowState {
            satisfied: false,
            detail: "Allowed, but the hotkey did not start. Relaunch to try again.".to_string(),
            action: Some(RELAUNCH.to_string()),
        };
    }
    if trusted {
        return RowState {
            satisfied: true,
            detail: "Allowed".to_string(),
            action: None,
        };
    }
    RowState {
        satisfied: false,
        detail: "Off. The hotkey starts working the moment you turn it on — no restart."
            .to_string(),
        action: Some("Open Settings".to_string()),
    }
}

/// The Models row, from the stack status and any fetch in flight.
pub(crate) fn models_state(
    statuses: &[model_fetch::ModelStatus],
    phase: Option<&model_fetch::Phase>,
) -> RowState {
    if let Some(phase) = phase.filter(|p| p.is_running()) {
        return RowState {
            satisfied: false,
            detail: phase.label(),
            action: Some("Cancel".to_string()),
        };
    }
    let missing: Vec<&model_fetch::ModelStatus> =
        statuses.iter().filter(|s| s.is_blocking()).collect();
    if missing.is_empty() {
        let note = match phase {
            // The worker loads its models once at boot, so a model that only
            // just landed is on disk but not in memory. Say so, rather than
            // leaving the user to wonder why the hotkey is still dead.
            Some(model_fetch::Phase::Done) => "Downloaded · relaunch to load it".to_string(),
            Some(model_fetch::Phase::Failed(why)) => format!("Ready · last attempt: {why}"),
            _ => "Ready".to_string(),
        };
        // A model that only just landed is on disk but not in memory, so the
        // row stays actionable: telling the user to relaunch without giving
        // them the button is the kind of dead end this window exists to remove.
        let action = matches!(phase, Some(model_fetch::Phase::Done)).then(|| RELAUNCH.to_string());
        return RowState {
            satisfied: true,
            detail: note,
            action,
        };
    }
    // One row, so it speaks for the first thing that is missing; fixing it
    // reveals the next.
    let first = missing[0];
    if first.entry.is_fetchable() {
        let failed = match phase {
            Some(model_fetch::Phase::Failed(why)) => format!(" · last attempt: {why}"),
            Some(model_fetch::Phase::Cancelled) => " · cancelled".to_string(),
            _ => String::new(),
        };
        return RowState {
            satisfied: false,
            detail: format!("{} is missing{failed}", first.entry.credit),
            action: Some("Get model".to_string()),
        };
    }
    RowState {
        satisfied: false,
        detail: format!("{} {}", first.entry.manual_hint, first.path.display()),
        action: Some("Choose model folder…".to_string()),
    }
}

/// The subtitle under the heading: how far off a working dictation this Mac is.
pub(crate) fn headline(rows: &[RowState]) -> String {
    let missing = rows.iter().filter(|r| !r.satisfied).count();
    match missing {
        0 => "Everything is ready. Hold your hotkey and speak.".to_string(),
        1 => "One thing left.".to_string(),
        n => format!("{n} things left."),
    }
}

/// Read the live state of all three rows, in row order.
pub(crate) fn current_states() -> Vec<RowState> {
    // `resolved_status` is the one place model paths are resolved, so this
    // window can never disagree with the daemon about what is on disk.
    let statuses = model_fetch::resolved_status();
    let phase = model_fetch::fetch_phase();
    vec![
        microphone_state(permissions::microphone_status()),
        accessibility_state(
            permissions::accessibility_trusted(),
            permissions::accessibility_install_failed(),
        ),
        models_state(&statuses, phase.as_ref()),
    ]
}

// ─── The window ─────────────────────────────────────────────────────────

pub(super) static SETUP_UI: OnceLock<SetupUi> = OnceLock::new();

pub(super) fn setup_ui(mtm: MainThreadMarker) -> &'static SetupUi {
    SETUP_UI.get_or_init(|| build_setup_window(mtm))
}

pub(super) fn build_setup_window(mtm: MainThreadMarker) -> SetupUi {
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(SETUP_W, SETUP_H));
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::FullSizeContentView;
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };

    let panel = slate::SlateView::new(mtm, frame, slate::Paint::Surface, 0.0);

    // ── Heading band ───────────────────────────────────────────────────
    let header_y = SETUP_H - TITLEBAR_H - HEADER_H;
    let heading = slate::label(
        mtm,
        "Setup & permissions",
        TYPE_TITLE,
        WEIGHT_SEMIBOLD,
        theme::text_primary(),
    );
    heading.setFrame(NSRect::new(
        NSPoint::new(CARD_PAD, header_y + HEADER_H / 2.0 - 2.0),
        NSSize::new(SETUP_W - 2.0 * CARD_PAD, TYPE_TITLE + SPACE_XS),
    ));
    panel.addSubview(&heading);

    let subtitle = slate::label(
        mtm,
        "",
        TYPE_META,
        WEIGHT_REGULAR,
        theme::text_secondary(),
    );
    subtitle.setFrame(NSRect::new(
        NSPoint::new(CARD_PAD, header_y + SPACE_XS),
        NSSize::new(SETUP_W - 2.0 * CARD_PAD, TYPE_META + SPACE_XS),
    ));
    panel.addSubview(&subtitle);

    // ── Footer well ────────────────────────────────────────────────────
    let bottom_divider = slate::SlateView::new(
        mtm,
        NSRect::new(
            NSPoint::new(0.0, CONTROL_FOOTER),
            NSSize::new(SETUP_W, HAIRLINE),
        ),
        slate::Paint::Divider,
        0.0,
    );
    panel.addSubview(&bottom_divider);
    let footer = slate::SlateView::new(
        mtm,
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(SETUP_W, CONTROL_FOOTER)),
        slate::Paint::Well,
        0.0,
    );
    let footer_status = slate::label(
        mtm,
        SETUP_HINT,
        TYPE_META,
        WEIGHT_REGULAR,
        theme::text_secondary(),
    );
    slate::place_left(&footer_status, SPACE_MD, CONTROL_FOOTER / 2.0);
    footer.addSubview(&footer_status);
    panel.addSubview(&footer);

    // ── The checklist card ─────────────────────────────────────────────
    let requirements = [
        Requirement::Microphone,
        Requirement::Accessibility,
        Requirement::Models,
    ];
    let card_h = CARD_H;
    debug_assert_eq!(requirements.len() as f64 * ROW_H + 2.0 * SPACE_SM, card_h);
    let card_y = header_y - SPACE_MD - card_h;
    let card = slate::SlateView::new(
        mtm,
        NSRect::new(
            NSPoint::new(CARD_PAD, card_y),
            NSSize::new(SETUP_W - 2.0 * CARD_PAD, card_h),
        ),
        slate::Paint::Card,
        RADIUS_LG,
    );
    let card_w = card.frame().size.width;

    let actions = MenuActions::new();
    let mut rows = Vec::new();
    for (index, requirement) in requirements.iter().enumerate() {
        let row_y = card_h - SPACE_SM - (index as f64 + 1.0) * ROW_H;
        let holder = slate::SlateView::new(
            mtm,
            NSRect::new(NSPoint::new(0.0, row_y), NSSize::new(card_w, ROW_H)),
            slate::Paint::Row,
            RADIUS_MD,
        );
        let mid = ROW_H / 2.0;
        holder.addSubview(&slate::icon_tile(mtm, requirement.symbol(), SPACE_SM, mid));

        let text_x = SPACE_SM + CONTROL_TILE + SPACE_SM;
        // Two lines centred on the row's midline: the title sits just above
        // it, its detail just below, each measured rather than guessed.
        let title = slate::label(
            mtm,
            requirement.title(),
            TYPE_LABEL,
            WEIGHT_MEDIUM,
            theme::text_primary(),
        );
        title.sizeToFit();
        let title_h = title.frame().size.height;
        title.setFrame(NSRect::new(
            NSPoint::new(text_x, (mid + LINE_GAP / 2.0).round()),
            NSSize::new(card_w - text_x - ACTION_COLUMN, title_h),
        ));
        holder.addSubview(&title);

        let detail = slate::label(mtm, "Ag", TYPE_META, WEIGHT_REGULAR, theme::text_secondary());
        detail.sizeToFit();
        let detail_h = detail.frame().size.height;
        detail.setFrame(NSRect::new(
            NSPoint::new(text_x, (mid - LINE_GAP / 2.0 - detail_h).round()),
            NSSize::new(card_w - text_x - ACTION_COLUMN, detail_h),
        ));
        unsafe {
            let _: () = msg_send![&*detail, setLineBreakMode: NSLineBreakMode::ByTruncatingTail];
        }
        holder.addSubview(&detail);

        // The button is the row's action; it is moved and retitled on every
        // refresh, and hidden once the requirement is met.
        let button = slate::click_overlay(
            mtm,
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)),
            &actions,
            sel!(setupRowAction:),
            requirement.tag(),
        );
        holder.addSubview(&button);
        card.addSubview(&holder);
        rows.push(SetupRow {
            requirement: *requirement,
            detail,
            holder,
            button,
            mid_y: mid,
        });
    }
    panel.addSubview(&card);

    unsafe {
        window.setTitle(&NSString::from_str("Setup & permissions"));
        window.setReleasedWhenClosed(false);
        window.setContentView(Some(&*panel));
        slate::apply_panel_chrome(&window);
        window.center();
    }

    // Deliberately NOT applied here: `show_setup_window` and `refresh` read
    // the live state, and the offscreen render proof applies invented rows
    // instead, so a proof never queries this Mac's permissions or models.
    SetupUi {
        window,
        panel: Retained::into_super(panel),
        subtitle,
        rows,
        footer_status,
    }
}

/// Paint invented rows for the offscreen render proof. Never reads the user's
/// permissions, settings or model directory.
pub(super) fn apply_for_render(ui: &SetupUi, states: &[RowState]) {
    apply(ui, states);
}

/// A two-line row is one spacing step taller than a plain `CONTROL_ROW`.
const ROW_H: f64 = CONTROL_ROW + SPACE_MD;
/// Space between the title line and its detail line.
const LINE_GAP: f64 = SPACE_XXS;
/// Width kept clear on the right of each row for the status dot and the chip.
const ACTION_COLUMN: f64 = 150.0;

/// Repaint every row from `states`. Rebuilding the dot and the chip is simpler
/// and less error-prone than mutating two views in step, and it happens once a
/// second at most.
fn apply(ui: &SetupUi, states: &[RowState]) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    ui.subtitle
        .setStringValue(&NSString::from_str(&headline(states)));
    for (row, state) in ui.rows.iter().zip(states) {
        row.detail
            .setStringValue(&NSString::from_str(&state.detail));
        // Drop the previous dot and chip; the tile, title, detail and button
        // are kept.
        for view in row.holder.subviews().iter() {
            if view.tag() == DECORATION_TAG {
                view.removeFromSuperview();
            }
        }
        let width = row.holder.frame().size.width;
        let dot = slate::status_dot(
            mtm,
            state.satisfied,
            width - SPACE_MD - slate::DOT,
            row.mid_y,
        );
        unsafe {
            let _: () = msg_send![&*dot, setTag: DECORATION_TAG];
        }
        row.holder.addSubview(&dot);

        match &state.action {
            Some(text) => {
                let chip = slate::chip(
                    mtm,
                    text,
                    None,
                    width - SPACE_MD - slate::DOT - SPACE_SM,
                    row.mid_y,
                );
                unsafe {
                    let _: () = msg_send![&*chip, setTag: DECORATION_TAG];
                }
                let chip_frame = chip.frame();
                row.holder.addSubview(&chip);
                row.button.setFrame(chip_frame);
                row.button.setHidden(false);
                unsafe {
                    let _: () = msg_send![
                        &*row.button,
                        setAccessibilityLabel: &*NSString::from_str(
                            &format!("{} — {text}", row.requirement.title())
                        )
                    ];
                }
            }
            None => row.button.setHidden(true),
        }
    }
}

/// Tag on the views `apply` rebuilds, so a repaint removes exactly those.
const DECORATION_TAG: isize = 7701;

/// Re-read the live state and repaint. Called from the menu bar's slow tick
/// while the window is on screen.
pub(crate) fn refresh() {
    let Some(ui) = SETUP_UI.get() else {
        return;
    };
    if !ui.window.isVisible() {
        return;
    }
    apply(ui, &current_states());
}

/// Open the window (building it the first time).
pub(super) fn show_setup_window() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let ui = setup_ui(mtm);
    apply(ui, &current_states());
    ui.footer_status
        .setStringValue(&NSString::from_str(SETUP_HINT));
    slate::place_left(&ui.footer_status, SPACE_MD, CONTROL_FOOTER / 2.0);
    ui.window.makeKeyAndOrderFront(None);
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    {
        app.activateIgnoringOtherApps(true)
    };
}

/// True when this launch is not ready to dictate, so the Setup window should
/// open by itself. A user whose app already works never sees it.
pub(crate) fn should_open_on_launch(states: &[RowState]) -> bool {
    states.iter().any(|s| !s.satisfied)
}

/// Handle a click on a row's action chip.
pub(super) fn row_action(tag: isize) {
    let Some(requirement) = Requirement::from_tag(tag) else {
        return;
    };
    // Whatever the row, a chip reading "Relaunch" relaunches: the row already
    // decided that nothing else can clear its state.
    if current_states()
        .get(requirement.tag() as usize)
        .and_then(|r| r.action.as_deref())
        == Some(RELAUNCH)
    {
        super::relaunch_daemon();
        return;
    }
    match requirement {
        Requirement::Microphone => match permissions::microphone_status() {
            // Only "not asked yet" can be answered in-app; once the user has
            // said no, System Settings is the only place that can change it.
            Grant::Undecided => permissions::request_microphone(|grant| {
                eprintln!("[setup] microphone {}", grant.label());
                permissions::refresh_microphone_health();
            }),
            _ => permissions::open_settings_pane(permissions::MICROPHONE_PANE),
        },
        Requirement::Accessibility => {
            permissions::open_settings_pane(permissions::ACCESSIBILITY_PANE)
        }
        Requirement::Models => model_action(),
    }
    refresh();
}

fn model_action() {
    if model_fetch::fetch_running() {
        model_fetch::cancel_fetch();
        return;
    }
    let base = crate::app_paths::models_base_dir();
    let statuses = model_fetch::resolved_status();
    let Some(missing) = statuses.iter().find(|s| s.is_blocking()) else {
        return;
    };
    if missing.entry.is_fetchable() {
        model_fetch::start_fetch(missing.entry, base);
        return;
    }
    choose_model_folder(missing);
}

/// Let the user point at a checkpoint they already have. Uses AppleScript's
/// `choose folder` (the same macOS panel) because `NSOpenPanel` is not in this
/// build's `objc2-app-kit` feature set — the identical reason the dictionary
/// CSV import gives. The dialog runs off the main thread; the link is made on
/// it once a folder is chosen.
fn choose_model_folder(missing: &model_fetch::ModelStatus) {
    let prompt = format!("Choose the folder holding {}", missing.entry.id);
    let script = format!(
        "POSIX path of (choose folder with prompt {})",
        applescript_string(&prompt)
    );
    if let Ok(mut pending) = PENDING_MODEL_FOLDER.lock() {
        *pending = Some(missing.clone());
    }
    super::pick_path_with_osascript(&script, "model folder", link_chosen_model_folder);
}

/// The missing model the open folder picker is for.
static PENDING_MODEL_FOLDER: std::sync::Mutex<Option<model_fetch::ModelStatus>> =
    std::sync::Mutex::new(None);

/// Main thread, after `choose_model_folder`'s picker returned a folder.
fn link_chosen_model_folder(chosen: std::path::PathBuf) {
    let Some(missing) = PENDING_MODEL_FOLDER.lock().ok().and_then(|mut p| p.take()) else {
        return;
    };
    let missing = &missing;
    if !missing.entry.present_at(&chosen) {
        set_footer(&format!(
            "That folder has no {} in it.",
            missing.entry.required_files.join(" and ")
        ));
        return;
    }
    // Point the app at it rather than copying gigabytes: the env override is
    // the documented way to move the model home, and a symlink keeps the fixed
    // layout the loader expects.
    let destination = missing.path.clone();
    if let Some(parent) = destination.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::remove_file(&destination);
    match std::os::unix::fs::symlink(&chosen, &destination) {
        Ok(()) => {
            eprintln!("[setup] linked {} → {}", destination.display(), chosen.display());
            set_footer("Model linked. Relaunch from the menu to load it.");
            crate::model_fetch::ensure_models_on_first_run();
        }
        Err(e) => {
            eprintln!("[setup] could not link the chosen folder: {e}");
            set_footer(&format!("Could not use that folder: {e}"));
        }
    }
}

fn set_footer(text: &str) {
    let Some(ui) = SETUP_UI.get() else {
        return;
    };
    ui.footer_status.setStringValue(&NSString::from_str(text));
    slate::place_left(&ui.footer_status, SPACE_MD, CONTROL_FOOTER / 2.0);
}

/// Quote a string for AppleScript. Only backslash and double quote are special
/// inside an AppleScript string literal.
pub(crate) fn applescript_string(text: &str) -> String {
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn present(entry: model_fetch::ModelEntry, required: bool) -> model_fetch::ModelStatus {
        model_fetch::ModelStatus {
            entry,
            path: std::path::PathBuf::from("/models/x"),
            present: true,
            required,
        }
    }

    fn absent(entry: model_fetch::ModelEntry, required: bool) -> model_fetch::ModelStatus {
        model_fetch::ModelStatus {
            entry,
            path: std::path::PathBuf::from("/models/x"),
            present: false,
            required,
        }
    }

    #[test]
    fn the_microphone_row_offers_asking_only_while_asking_can_still_work() {
        let undecided = microphone_state(Grant::Undecided);
        assert!(!undecided.satisfied);
        assert_eq!(undecided.action.as_deref(), Some("Allow"));
        // Once denied, asking again does nothing: send them to System Settings.
        assert_eq!(
            microphone_state(Grant::Denied).action.as_deref(),
            Some("Open Settings")
        );
        assert_eq!(
            microphone_state(Grant::Restricted).action.as_deref(),
            Some("Open Settings")
        );
        for grant in [Grant::Granted, Grant::Unknown] {
            let state = microphone_state(grant);
            assert!(state.satisfied, "{grant:?}");
            assert!(state.action.is_none(), "{grant:?}");
        }
    }

    #[test]
    fn the_accessibility_row_promises_no_restart() {
        let off = accessibility_state(false, false);
        assert!(!off.satisfied);
        assert_eq!(off.action.as_deref(), Some("Open Settings"));
        assert!(off.detail.contains("no restart"), "{}", off.detail);
        let on = accessibility_state(true, false);
        assert!(on.satisfied && on.action.is_none());
    }

    /// Regression: a granted permission whose tap install failed used to read
    /// "Allowed" with no action, so the row said everything was fine while the
    /// hotkey was dead and nothing on screen could revive it.
    #[test]
    fn a_granted_permission_whose_tap_failed_offers_a_relaunch() {
        let stuck = accessibility_state(true, true);
        assert!(!stuck.satisfied);
        assert_eq!(stuck.action.as_deref(), Some(RELAUNCH));
        assert!(stuck.detail.contains("Relaunch"), "{}", stuck.detail);
        // It must not send the user to a switch that is already on.
        assert!(!stuck.detail.contains("Open Settings"));
    }

    /// Regression: a model that has just downloaded is on disk but not in
    /// memory. The row said "relaunch from the menu" and gave no button.
    #[test]
    fn a_freshly_downloaded_model_offers_the_relaunch_it_asks_for() {
        let ready = [present(model_fetch::ASR, true), present(model_fetch::CLEANUP, true)];
        let after = models_state(&ready, Some(&model_fetch::Phase::Done));
        assert!(after.satisfied);
        assert_eq!(after.action.as_deref(), Some(RELAUNCH));
        // A steady state offers nothing: the button appears only after a fetch.
        assert!(models_state(&ready, None).action.is_none());
    }

    #[test]
    fn the_models_row_offers_a_download_only_for_a_model_with_a_published_source() {
        // Cleanup model missing: the app can get that one.
        let state = models_state(
            &[present(model_fetch::ASR, true), absent(model_fetch::CLEANUP, true)],
            None,
        );
        assert_eq!(state.action.as_deref(), Some("Get model"));
        assert!(state.detail.contains("Superwhisper"), "{}", state.detail);

        // ASR missing: there is no URL, so the row asks for a folder and says
        // what is wrong. It must never offer a download.
        let state = models_state(
            &[absent(model_fetch::ASR, true), present(model_fetch::CLEANUP, true)],
            None,
        );
        assert_eq!(state.action.as_deref(), Some("Choose model folder…"));
        assert!(state.detail.contains("cannot be downloaded"), "{}", state.detail);
    }

    #[test]
    fn a_missing_cleanup_model_with_cleanup_off_is_not_a_problem() {
        // CP-11: the row reads ready, so a stranger is not dead-ended by an
        // optional model.
        let state = models_state(
            &[
                present(model_fetch::ASR, true),
                absent(model_fetch::CLEANUP, false),
            ],
            None,
        );
        assert!(state.satisfied, "{state:?}");
        assert!(state.action.is_none());
    }

    #[test]
    fn a_model_that_just_landed_says_it_needs_a_relaunch_to_be_loaded() {
        // The worker loads its models once at boot. "Ready" alone would leave
        // a user staring at a dead hotkey after a successful download.
        let state = models_state(
            &[
                present(model_fetch::ASR, true),
                present(model_fetch::CLEANUP, true),
            ],
            Some(&model_fetch::Phase::Done),
        );
        assert!(state.satisfied);
        assert!(state.detail.contains("relaunch"), "{}", state.detail);
        // A session that downloaded nothing just says Ready.
        let quiet = models_state(&[present(model_fetch::ASR, true)], None);
        assert_eq!(quiet.detail, "Ready");
    }

    #[test]
    fn a_running_fetch_shows_its_progress_and_offers_cancel() {
        let statuses = [
            present(model_fetch::ASR, true),
            absent(model_fetch::CLEANUP, true),
        ];
        let phase = model_fetch::Phase::Downloading {
            done: 30,
            total: 100,
        };
        let state = models_state(&statuses, Some(&phase));
        assert_eq!(state.action.as_deref(), Some("Cancel"));
        assert!(state.detail.contains("30%"), "{}", state.detail);

        // A failure is not swallowed: the row keeps the reason and offers the
        // retry, rather than looking as if nothing happened.
        let failed = model_fetch::Phase::Failed("no disk space".into());
        let state = models_state(&statuses, Some(&failed));
        assert_eq!(state.action.as_deref(), Some("Get model"));
        assert!(state.detail.contains("no disk space"), "{}", state.detail);
    }

    #[test]
    fn the_window_opens_by_itself_only_when_something_is_missing() {
        let ready = vec![
            microphone_state(Grant::Granted),
            accessibility_state(true, false),
            models_state(&[present(model_fetch::ASR, true)], None),
        ];
        assert!(
            !should_open_on_launch(&ready),
            "a working app must never show Setup"
        );
        assert_eq!(headline(&ready), "Everything is ready. Hold your hotkey and speak.");

        let one_off = vec![
            microphone_state(Grant::Granted),
            accessibility_state(false, false),
            models_state(&[present(model_fetch::ASR, true)], None),
        ];
        assert!(should_open_on_launch(&one_off));
        assert_eq!(headline(&one_off), "One thing left.");

        let fresh = vec![
            microphone_state(Grant::Undecided),
            accessibility_state(false, false),
            models_state(&[absent(model_fetch::ASR, true)], None),
        ];
        assert_eq!(headline(&fresh), "3 things left.");
    }

    #[test]
    fn every_requirement_round_trips_through_its_row_tag() {
        for requirement in [
            Requirement::Microphone,
            Requirement::Accessibility,
            Requirement::Models,
        ] {
            assert_eq!(Requirement::from_tag(requirement.tag()), Some(requirement));
            assert!(!requirement.title().is_empty());
            assert!(!requirement.symbol().is_empty());
        }
        assert_eq!(Requirement::from_tag(99), None);
    }

    #[test]
    fn a_folder_prompt_cannot_break_out_of_its_applescript_string() {
        assert_eq!(applescript_string("plain"), "\"plain\"");
        assert_eq!(
            applescript_string("say \"hi\""),
            "\"say \\\"hi\\\"\""
        );
        assert_eq!(applescript_string("back\\slash"), "\"back\\\\slash\"");
    }
}
