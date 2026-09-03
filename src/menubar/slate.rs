//! Slate — the house-design drawing layer.
//!
//! Every pixel of Local Dictation's own chrome is painted here, from the
//! tokens in [`house_theme`] (generated from `design-system/tokens.json`).
//! Nothing in this file types a colour, a radius, a font size or a control
//! height by hand; the only literals are pure geometry that the token file
//! does not name (glyph point sizes, the 6 px status dot, gaps inside a
//! composite control).
//!
//! Two mechanisms keep the whole UI appearance-correct in both light and dark
//! without rebuilding a single view:
//!
//!   * **Dynamic `NSColor`s** ([`dyn_color`]) for anything AppKit owns the
//!     drawing of — text fields, popup titles, placeholder text. macOS
//!     re-resolves them whenever the effective appearance changes.
//!   * **`drawRect:` painting** for everything we own ([`SlateView`],
//!     [`WaveformView`], [`InkToggle`]). These read the palette from
//!     `NSAppearance::currentDrawingAppearance`, so one view tree renders
//!     correctly under either appearance — which is what lets the offscreen
//!     `render-ui` snapshot both from a single process.

#![allow(dead_code)]

use super::house_theme::{Palette, Rgba, DARK, LIGHT};
use super::*;

use objc2::DefinedClass;
use std::cell::Cell;
use std::ptr::NonNull;

// ─── Appearance resolution ───────────────────────────────────────────────

/// True when `appearance` resolves to Dark Aqua (including its
/// high-contrast / vibrant variants, which `bestMatchFromAppearancesWithNames`
/// folds onto the two base names).
pub(crate) fn appearance_is_dark(appearance: &NSAppearance) -> bool {
    let names = NSArray::from_slice(&[
        unsafe { NSAppearanceNameAqua },
        unsafe { NSAppearanceNameDarkAqua },
    ]);
    match appearance.bestMatchFromAppearancesWithNames(&names) {
        Some(best) => best.isEqualToString(unsafe { NSAppearanceNameDarkAqua }),
        None => false,
    }
}

/// The palette for the appearance a `drawRect:` is currently drawing into.
/// Outside a draw this falls back to the application's effective appearance.
pub(crate) fn palette() -> Palette {
    let current = NSAppearance::currentDrawingAppearance();
    if appearance_is_dark(&current) {
        DARK
    } else {
        LIGHT
    }
}

/// The palette for the application's effective appearance (used when building
/// views, before any drawing context exists).
pub(crate) fn app_palette() -> Palette {
    let Some(mtm) = MainThreadMarker::new() else {
        return DARK;
    };
    let app = NSApplication::sharedApplication(mtm);
    let eff = app.effectiveAppearance();
    if appearance_is_dark(&eff) {
        DARK
    } else {
        LIGHT
    }
}

/// True while the offscreen `render-ui` snapshot is running. Blur materials
/// draw nothing in a window that was never ordered in, so the panel tint is
/// painted opaque instead — see `render_ui.rs`.
static OPAQUE_MATERIALS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(crate) fn set_opaque_materials(on: bool) {
    OPAQUE_MATERIALS.store(on, Ordering::SeqCst);
}

pub(crate) fn opaque_materials() -> bool {
    OPAQUE_MATERIALS.load(Ordering::SeqCst)
}

// ─── Colour helpers ──────────────────────────────────────────────────────

/// A `NSColor` that re-resolves itself per appearance. Both concrete colours
/// are captured by the provider block, so nothing is allocated or leaked when
/// macOS asks for the value again.
pub(crate) fn dyn_color(name: &str, light: Rgba, dark: Rgba) -> Retained<NSColor> {
    let l = light.ns_color();
    let d = dark.ns_color();
    let provider = block2::RcBlock::new(move |appearance: NonNull<NSAppearance>| {
        let appearance: &NSAppearance = unsafe { appearance.as_ref() };
        let chosen = if appearance_is_dark(appearance) { &d } else { &l };
        NonNull::from(&**chosen)
    });
    let key = NSString::from_str(name);
    unsafe { NSColor::colorWithName_dynamicProvider(Some(&key), &provider) }
}

/// The same colour with its alpha forced to 1. Used for the panel tint in the
/// offscreen render path, where there is no blur behind it to composite over.
pub(crate) fn opaque(c: Rgba) -> Rgba {
    Rgba { a: 1.0, ..c }
}

/// Blend `over` onto `under` (both straight alpha) and return an opaque colour.
pub(crate) fn flatten(under: Rgba, over: Rgba) -> Rgba {
    let a = over.a;
    Rgba {
        r: over.r * a + under.r * (1.0 - a),
        g: over.g * a + under.g * (1.0 - a),
        b: over.b * a + under.b * (1.0 - a),
        a: 1.0,
    }
}

/// A shadow token resolved for the palette currently being drawn.
pub(crate) fn shadow_ns(sh: &super::house_theme::Shadow, dark: bool) -> Retained<NSShadow> {
    let s = NSShadow::new();
    // AppKit shadow offsets are in the view's coordinate space; a positive
    // token `y` means "downwards", which is negative in a bottom-up view.
    s.setShadowOffset(NSSize::new(0.0, -sh.y));
    s.setShadowBlurRadius(sh.blur);
    let alpha = if dark { sh.dark } else { sh.light };
    let black = Rgba {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: alpha,
    };
    s.setShadowColor(Some(&black.ns_color()));
    s
}

// ─── Fonts ───────────────────────────────────────────────────────────────

pub(crate) fn font(size: f64, weight: f64) -> Retained<NSFont> {
    NSFont::systemFontOfSize_weight(size, weight)
}

pub(crate) fn tabular_font(size: f64, weight: f64) -> Retained<NSFont> {
    NSFont::monospacedDigitSystemFontOfSize_weight(size, weight)
}

// ─── The painted view ────────────────────────────────────────────────────

/// What a [`SlateView`] paints. One class covers every Slate surface so the
/// runtime carries one subclass instead of a dozen near-identical ones.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Paint {
    /// Nothing — a plain container.
    None,
    /// Floating glass: `panel_tint` over the blur, `stroke` hairline,
    /// `highlight_top`. Opaque tint while snapshotting.
    Panel,
    /// Opaque window ground (`surface`).
    Surface,
    /// Opaque rail / track (`surface_sunken`).
    Sunken,
    /// Raised card: `surface_raised`, `stroke`, `highlight_top`.
    Card,
    /// Sunken well (`well`) — footers, the waveform track.
    Well,
    /// 26 px icon tile: `tile_fill` + `tile_stroke`.
    Tile,
    /// Chip: `chip_fill` + `stroke`.
    Chip,
    /// Outlined key cap: no fill, `key_cap_stroke`.
    KeyCap,
    /// A list row: `hover_fill` when hovered, `selection_fill` +
    /// `selection_ring` + `SHADOW_SELECTION` when selected.
    Row,
    /// A 1 px `divider` line filling the view.
    Divider,
    /// A status dot in `success`.
    DotSuccess,
    /// A status dot in `danger`.
    DotDanger,
}

pub(crate) struct SlateIvars {
    pub(crate) paint: Cell<Paint>,
    pub(crate) radius: Cell<f64>,
    pub(crate) selected: Cell<bool>,
    pub(crate) hovered: Cell<bool>,
    pub(crate) hover_tracking: Cell<bool>,
    pub(crate) flipped: bool,
    /// Danger-tinted panel stroke (the error pill).
    pub(crate) danger_stroke: Cell<bool>,
    /// `NSView`'s own `tag` is read-only, so a row carries its index here —
    /// the same index the click overlay sends and `HISTORY_ENTRIES` is keyed by.
    pub(crate) tag: Cell<isize>,
}

impl SlateIvars {
    fn new(paint: Paint, radius: f64, flipped: bool) -> Self {
        Self {
            paint: Cell::new(paint),
            radius: Cell::new(radius),
            selected: Cell::new(false),
            hovered: Cell::new(false),
            hover_tracking: Cell::new(false),
            flipped,
            danger_stroke: Cell::new(false),
            tag: Cell::new(-1),
        }
    }
}

define_class!(
    #[unsafe(super(NSView))]
    #[name = "LDSlateView"]
    #[ivars = SlateIvars]
    pub(crate) struct SlateView;

    impl SlateView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            self.ivars().flipped
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.paint_self();
        }

        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            unsafe {
                let _: () = msg_send![super(self), updateTrackingAreas];
            }
            if !self.ivars().hover_tracking.get() {
                return;
            }
            // Replace the single tracking area on every layout pass; the row
            // frames move whenever the list is rebuilt.
            unsafe {
                let existing: Retained<NSArray<NSTrackingArea>> =
                    msg_send![self, trackingAreas];
                for area in existing.iter() {
                    let _: () = msg_send![self, removeTrackingArea: &*area];
                }
                // MouseEnteredAndExited (0x01) | ActiveInKeyWindow (0x20)
                // | InVisibleRect (0x200). A tracking area with no "active"
                // option raises, so the key-window bit is not optional.
                let opts: usize = 0x01 | 0x20 | 0x200;
                let area: Retained<NSTrackingArea> = msg_send![
                    NSTrackingArea::alloc(),
                    initWithRect: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)),
                    options: opts,
                    owner: self,
                    userInfo: std::ptr::null_mut::<AnyObject>()
                ];
                let _: () = msg_send![self, addTrackingArea: &*area];
            }
        }

        #[unsafe(method(tag))]
        fn tag(&self) -> isize {
            self.ivars().tag.get()
        }

        #[unsafe(method(setTag:))]
        fn set_tag_objc(&self, v: isize) {
            self.ivars().tag.set(v);
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _e: *mut AnyObject) {
            self.ivars().hovered.set(true);
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _e: *mut AnyObject) {
            self.ivars().hovered.set(false);
            self.setNeedsDisplay(true);
        }
    }
);

impl SlateView {
    pub(crate) fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        paint: Paint,
        radius: f64,
    ) -> Retained<Self> {
        Self::with_flip(mtm, frame, paint, radius, false)
    }

    /// A painted view whose subview coordinates run top-down.
    pub(crate) fn flipped(
        mtm: MainThreadMarker,
        frame: NSRect,
        paint: Paint,
        radius: f64,
    ) -> Retained<Self> {
        Self::with_flip(mtm, frame, paint, radius, true)
    }

    fn with_flip(
        mtm: MainThreadMarker,
        frame: NSRect,
        paint: Paint,
        radius: f64,
        flip: bool,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(SlateIvars::new(paint, radius, flip));
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    pub(crate) fn set_paint(&self, paint: Paint) {
        self.ivars().paint.set(paint);
        self.setNeedsDisplay(true);
    }

    pub(crate) fn set_selected(&self, on: bool) {
        if self.ivars().selected.get() != on {
            self.ivars().selected.set(on);
            self.setNeedsDisplay(true);
        }
    }

    pub(crate) fn selected(&self) -> bool {
        self.ivars().selected.get()
    }

    pub(crate) fn set_danger_stroke(&self, on: bool) {
        self.ivars().danger_stroke.set(on);
        self.setNeedsDisplay(true);
    }

    /// Track the pointer so the row can paint `hover_fill`.
    pub(crate) fn enable_hover(&self) {
        self.ivars().hover_tracking.set(true);
        unsafe {
            let _: () = msg_send![self, updateTrackingAreas];
        }
    }

    fn rounded(&self, rect: NSRect, radius: f64) -> Retained<NSBezierPath> {
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, radius, radius)
    }

    /// Inset by half a hairline so a 1 pt stroke lands *inside* the shape
    /// rather than straddling its edge.
    fn stroke_rect(&self, bounds: NSRect) -> NSRect {
        let h = HAIRLINE / 2.0;
        NSRect::new(
            NSPoint::new(bounds.origin.x + h, bounds.origin.y + h),
            NSSize::new(
                (bounds.size.width - HAIRLINE).max(0.0),
                (bounds.size.height - HAIRLINE).max(0.0),
            ),
        )
    }

    /// The 1 px inset highlight along the top edge of a panel or card.
    fn draw_top_highlight(&self, bounds: NSRect, radius: f64, p: &Palette) {
        let inset = radius.min(bounds.size.width / 2.0) * 0.55;
        let y = if self.ivars().flipped {
            bounds.origin.y + HAIRLINE / 2.0
        } else {
            bounds.origin.y + bounds.size.height - HAIRLINE / 2.0
        };
        let path = NSBezierPath::new();
        path.moveToPoint(NSPoint::new(bounds.origin.x + inset, y));
        path.lineToPoint(NSPoint::new(
            bounds.origin.x + bounds.size.width - inset,
            y,
        ));
        path.setLineWidth(HAIRLINE);
        p.highlight_top.ns_color().setStroke();
        path.stroke();
    }

    fn paint_self(&self) {
        let p = palette();
        let dark = p == DARK;
        let bounds = self.bounds();
        let radius = self.ivars().radius.get();
        match self.ivars().paint.get() {
            Paint::None => {}
            Paint::Panel => {
                let tint = if opaque_materials() {
                    opaque(p.panel_tint)
                } else {
                    p.panel_tint
                };
                tint.ns_color().setFill();
                self.rounded(bounds, radius).fill();
                let stroke = if self.ivars().danger_stroke.get() {
                    Rgba { a: 0.4, ..p.danger }
                } else {
                    p.stroke
                };
                stroke.ns_color().setStroke();
                let ring = self.rounded(self.stroke_rect(bounds), radius);
                ring.setLineWidth(HAIRLINE);
                ring.stroke();
                self.draw_top_highlight(bounds, radius, &p);
            }
            Paint::Surface => {
                p.surface.ns_color().setFill();
                self.rounded(bounds, radius).fill();
            }
            Paint::Sunken => {
                p.surface_sunken.ns_color().setFill();
                self.rounded(bounds, radius).fill();
            }
            Paint::Card => {
                p.surface_raised.ns_color().setFill();
                self.rounded(bounds, radius).fill();
                p.stroke.ns_color().setStroke();
                let ring = self.rounded(self.stroke_rect(bounds), radius);
                ring.setLineWidth(HAIRLINE);
                ring.stroke();
                self.draw_top_highlight(bounds, radius, &p);
            }
            Paint::Well => {
                p.well.ns_color().setFill();
                self.rounded(bounds, radius).fill();
            }
            Paint::Tile => {
                p.tile_fill.ns_color().setFill();
                self.rounded(bounds, radius).fill();
                p.tile_stroke.ns_color().setStroke();
                let ring = self.rounded(self.stroke_rect(bounds), radius);
                ring.setLineWidth(HAIRLINE);
                ring.stroke();
            }
            Paint::Chip => {
                p.chip_fill.ns_color().setFill();
                self.rounded(bounds, radius).fill();
                p.stroke.ns_color().setStroke();
                let ring = self.rounded(self.stroke_rect(bounds), radius);
                ring.setLineWidth(HAIRLINE);
                ring.stroke();
            }
            Paint::KeyCap => {
                // Key caps are outlined; `key_cap_fill` is deliberately clear.
                p.key_cap_fill.ns_color().setFill();
                self.rounded(bounds, radius).fill();
                p.key_cap_stroke.ns_color().setStroke();
                let ring = self.rounded(self.stroke_rect(bounds), radius);
                ring.setLineWidth(HAIRLINE);
                ring.stroke();
            }
            Paint::Row => {
                let selected = self.ivars().selected.get();
                let hovered = self.ivars().hovered.get();
                if selected {
                    {
                        let ctx = NSGraphicsContext::currentContext();
                        if let Some(ctx) = &ctx {
                            ctx.saveGraphicsState();
                        }
                        shadow_ns(&SHADOW_SELECTION, dark).set();
                        p.selection_fill.ns_color().setFill();
                        self.rounded(bounds, radius).fill();
                        if let Some(ctx) = &ctx {
                            ctx.restoreGraphicsState();
                        }
                    }
                    p.selection_ring.ns_color().setStroke();
                    let ring = self.rounded(self.stroke_rect(bounds), radius);
                    ring.setLineWidth(HAIRLINE);
                    ring.stroke();
                } else if hovered {
                    p.hover_fill.ns_color().setFill();
                    self.rounded(bounds, radius).fill();
                }
            }
            Paint::Divider => {
                p.divider.ns_color().setFill();
                NSBezierPath::fillRect(bounds);
            }
            Paint::DotSuccess => {
                p.success.ns_color().setFill();
                self.rounded(bounds, bounds.size.height / 2.0).fill();
            }
            Paint::DotDanger => {
                p.danger.ns_color().setFill();
                self.rounded(bounds, bounds.size.height / 2.0).fill();
            }
        }
    }
}

// ─── Waveform ────────────────────────────────────────────────────────────

/// The 14 ink bars. One view draws the whole row so the bar colour follows the
/// appearance for free, and so a redraw is one `setNeedsDisplay` rather than
/// fourteen frame changes.
pub(crate) struct WaveIvars {
    pub(crate) heights: Cell<[f64; BAR_COUNT]>,
    /// Ink at 40 % while the pipeline is working.
    pub(crate) muted: Cell<bool>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[name = "LDWaveformView"]
    #[ivars = WaveIvars]
    pub(crate) struct WaveformView;

    impl WaveformView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let p = palette();
            let ink = if self.ivars().muted.get() {
                Rgba {
                    a: 0.4,
                    ..p.text_primary
                }
            } else {
                p.text_primary
            };
            ink.ns_color().setFill();
            let bounds = self.bounds();
            let heights = self.ivars().heights.get();
            for (i, h) in heights.iter().enumerate() {
                let h = h.clamp(BAR_MIN_H, BAR_MAX_H);
                let x = bounds.origin.x + i as f64 * (BAR_W + BAR_GAP);
                let y = bounds.origin.y + (bounds.size.height - h) / 2.0;
                let r = NSRect::new(NSPoint::new(x, y), NSSize::new(BAR_W, h));
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                    r,
                    BAR_W / 2.0,
                    BAR_W / 2.0,
                )
                .fill();
            }
        }
    }
);

impl WaveformView {
    pub(crate) fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(WaveIvars {
            heights: Cell::new([BAR_MIN_H; BAR_COUNT]),
            muted: Cell::new(false),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    pub(crate) fn set_heights(&self, h: [f64; BAR_COUNT]) {
        self.ivars().heights.set(h);
        self.setNeedsDisplay(true);
    }

    pub(crate) fn heights(&self) -> [f64; BAR_COUNT] {
        self.ivars().heights.get()
    }

    pub(crate) fn set_muted(&self, muted: bool) {
        if self.ivars().muted.get() != muted {
            self.ivars().muted.set(muted);
            self.setNeedsDisplay(true);
        }
    }

    /// Total width of the bar block, from the token geometry.
    pub(crate) fn block_width() -> f64 {
        BAR_COUNT as f64 * BAR_W + (BAR_COUNT - 1) as f64 * BAR_GAP
    }
}

// ─── Ink toggle ──────────────────────────────────────────────────────────

/// The house toggle. `NSSwitch` paints itself in the system accent colour and
/// offers no tint, and the rule forbids chrome colour — so the control is ours:
/// ink track with an inverse knob when on, an outlined track with a secondary
/// knob when off. It keeps `NSControl`'s target/action and a `state` /
/// `setState:` pair, so every call site reads exactly like the switch it
/// replaced.
pub(crate) struct ToggleIvars {
    on: Cell<bool>,
    enabled: Cell<bool>,
}

define_class!(
    #[unsafe(super(NSControl))]
    #[name = "LDInkToggle"]
    #[ivars = ToggleIvars]
    pub(crate) struct InkToggle;

    impl InkToggle {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let p = palette();
            let b = self.bounds();
            let track_h = b.size.height;
            let radius = track_h / 2.0;
            let knob = track_h - 2.0 * TOGGLE_KNOB_INSET;
            let dim = if self.ivars().enabled.get() { 1.0 } else { 0.4 };
            if self.ivars().on.get() {
                Rgba {
                    a: p.text_primary.a * dim,
                    ..p.text_primary
                }
                .ns_color()
                .setFill();
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(b, radius, radius).fill();
                Rgba {
                    a: dim,
                    ..p.text_inverse
                }
                .ns_color()
                .setFill();
                let kx = b.origin.x + b.size.width - TOGGLE_KNOB_INSET - knob;
                let kr = NSRect::new(
                    NSPoint::new(kx, b.origin.y + TOGGLE_KNOB_INSET),
                    NSSize::new(knob, knob),
                );
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                    kr,
                    knob / 2.0,
                    knob / 2.0,
                )
                .fill();
            } else {
                Rgba {
                    a: p.key_cap_stroke.a * dim,
                    ..p.key_cap_stroke
                }
                .ns_color()
                .setStroke();
                let inset = NSRect::new(
                    NSPoint::new(b.origin.x + HAIRLINE / 2.0, b.origin.y + HAIRLINE / 2.0),
                    NSSize::new(b.size.width - HAIRLINE, b.size.height - HAIRLINE),
                );
                let ring = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                    inset, radius, radius,
                );
                ring.setLineWidth(HAIRLINE);
                ring.stroke();
                Rgba {
                    a: 0.5 * dim,
                    ..p.text_primary
                }
                .ns_color()
                .setFill();
                let kr = NSRect::new(
                    NSPoint::new(
                        b.origin.x + TOGGLE_KNOB_INSET,
                        b.origin.y + TOGGLE_KNOB_INSET,
                    ),
                    NSSize::new(knob, knob),
                );
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                    kr,
                    knob / 2.0,
                    knob / 2.0,
                )
                .fill();
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _e: *mut AnyObject) {
            if !self.ivars().enabled.get() {
                return;
            }
            self.ivars().on.set(!self.ivars().on.get());
            self.redraw();
            unsafe {
                let action: Option<objc2::runtime::Sel> = msg_send![self, action];
                let target: *mut AnyObject = msg_send![self, target];
                if let Some(sel) = action {
                    let _: bool = msg_send![self, sendAction: sel, to: target];
                }
            }
        }

        #[unsafe(method(state))]
        fn state(&self) -> isize {
            if self.ivars().on.get() {
                1
            } else {
                0
            }
        }

        #[unsafe(method(setState:))]
        fn set_state_objc(&self, v: isize) {
            self.ivars().on.set(v != 0);
            self.redraw();
        }

        #[unsafe(method(isEnabled))]
        fn is_enabled(&self) -> bool {
            self.ivars().enabled.get()
        }

        #[unsafe(method(setEnabled:))]
        fn set_enabled_objc(&self, on: bool) {
            self.ivars().enabled.set(on);
            self.redraw();
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            self.ivars().enabled.get()
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: *mut AnyObject) {
            // Space toggles, so the control is reachable without a pointer.
            let is_space = unsafe {
                let chars: Retained<NSString> = msg_send![event, charactersIgnoringModifiers];
                chars.to_string() == " "
            };
            if is_space {
                unsafe {
                    let _: () = msg_send![self, mouseDown: std::ptr::null_mut::<AnyObject>()];
                }
            }
        }
    }
);

/// Gap between the toggle track and its knob.
const TOGGLE_KNOB_INSET: f64 = 2.0;
/// Track size. 30 × 18 is the mockup's toggle; both numbers are pure geometry
/// of a composite control, not a token the design system names.
pub(crate) const TOGGLE_W: f64 = 30.0;
pub(crate) const TOGGLE_H: f64 = 18.0;

impl InkToggle {
    pub(crate) fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ToggleIvars {
            on: Cell::new(false),
            enabled: Cell::new(true),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// `NSControl` shadows `setNeedsDisplay` with a no-argument version, so
    /// the redraw goes through the `NSView` selector explicitly.
    fn redraw(&self) {
        unsafe {
            let _: () = msg_send![self, setNeedsDisplay: true];
        }
    }

    pub(crate) fn set_on(&self, on: bool) {
        self.ivars().on.set(on);
        self.redraw();
    }

    pub(crate) fn is_on(&self) -> bool {
        self.ivars().on.get()
    }

    pub(crate) fn set_active(&self, on: bool) {
        self.ivars().enabled.set(on);
        self.redraw();
    }
}

// ─── Composite builders ──────────────────────────────────────────────────

/// A non-editable label in the house type scale.
pub(crate) fn label(
    mtm: MainThreadMarker,
    text: &str,
    size: f64,
    weight: f64,
    color: Retained<NSColor>,
) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    l.setFont(Some(&font(size, weight)));
    unsafe {
        let _: () = msg_send![&*l, setTextColor: &*color];
        let _: () = msg_send![&*l, setLineBreakMode: NSLineBreakMode::ByTruncatingTail];
    }
    l
}

/// Size a label to its text and place its left edge at `x`, vertically centred
/// on `mid_y`. Returns the width consumed.
pub(crate) fn place_left(field: &NSTextField, x: f64, mid_y: f64) -> f64 {
    field.sizeToFit();
    let f = field.frame();
    field.setFrame(NSRect::new(
        NSPoint::new(x, (mid_y - f.size.height / 2.0).round()),
        f.size,
    ));
    f.size.width
}

/// An uppercase section label ("TODAY", "INPUT") with the house tracking.
pub(crate) fn section_label(mtm: MainThreadMarker, text: &str) -> Retained<NSTextField> {
    let l = label(
        mtm,
        &text.to_uppercase(),
        TYPE_SECTION,
        WEIGHT_SEMIBOLD,
        theme::text_tertiary(),
    );
    unsafe {
        let attrs = NSMutableAttributedString::from_nsstring(&NSString::from_str(
            &text.to_uppercase(),
        ));
        let range = NSRange::new(0, attrs.length());
        attrs.addAttribute_value_range(
            NSFontAttributeName,
            &*font(TYPE_SECTION, WEIGHT_SEMIBOLD),
            range,
        );
        attrs.addAttribute_value_range(
            NSForegroundColorAttributeName,
            &*theme::text_tertiary(),
            range,
        );
        attrs.addAttribute_value_range(
            NSKernAttributeName,
            &*NSNumber::new_f64(TRACKING_SECTION),
            range,
        );
        let _: () = msg_send![&*l, setAttributedStringValue: &*attrs];
    }
    l
}

/// An outlined key cap: `esc`, `⌘`, `↩`. 20 px tall, `Radius.xs`, 11 pt SF.
pub(crate) fn key_cap(mtm: MainThreadMarker, text: &str, x: f64, mid_y: f64) -> Retained<SlateView> {
    let text_field = label(
        mtm,
        text,
        TYPE_KEY_CAP,
        WEIGHT_MEDIUM,
        theme::text_secondary(),
    );
    text_field.sizeToFit();
    let tw = text_field.frame().size.width;
    let w = (tw + 2.0 * KEY_CAP_PAD).max(CONTROL_KEYCAP);
    let cap = SlateView::new(
        mtm,
        NSRect::new(
            NSPoint::new(x, (mid_y - CONTROL_KEYCAP / 2.0).round()),
            NSSize::new(w.round(), CONTROL_KEYCAP),
        ),
        Paint::KeyCap,
        RADIUS_XS,
    );
    let th = text_field.frame().size.height;
    text_field.setFrame(NSRect::new(
        NSPoint::new(((w - tw) / 2.0).round(), ((CONTROL_KEYCAP - th) / 2.0).round()),
        NSSize::new(tw, th),
    ));
    cap.addSubview(&text_field);
    cap
}

/// Horizontal padding inside a key cap.
const KEY_CAP_PAD: f64 = 5.0;

/// A 6 px status dot. `success` for ready / landed, `danger` for error.
pub(crate) fn status_dot(mtm: MainThreadMarker, ok: bool, x: f64, mid_y: f64) -> Retained<SlateView> {
    SlateView::new(
        mtm,
        NSRect::new(
            NSPoint::new(x, (mid_y - DOT / 2.0).round()),
            NSSize::new(DOT, DOT),
        ),
        if ok {
            Paint::DotSuccess
        } else {
            Paint::DotDanger
        },
        DOT / 2.0,
    )
}

/// Status-dot diameter. The rule names it in prose ("a 6 px dot"), not as a
/// token, so it lives here.
pub(crate) const DOT: f64 = 6.0;

/// An SF Symbol in a 26 px tile — the row glyph that keeps rows aligned
/// whatever the symbol's own width.
pub(crate) fn icon_tile(
    mtm: MainThreadMarker,
    symbol: &str,
    x: f64,
    mid_y: f64,
) -> Retained<SlateView> {
    let tile = SlateView::new(
        mtm,
        NSRect::new(
            NSPoint::new(x, (mid_y - CONTROL_TILE / 2.0).round()),
            NSSize::new(CONTROL_TILE, CONTROL_TILE),
        ),
        Paint::Tile,
        RADIUS_TILE,
    );
    if let Some(img) = template_symbol(symbol) {
        let iv = NSImageView::new(mtm);
        iv.setImage(Some(&img));
        unsafe {
            let _: () = msg_send![&*iv, setContentTintColor: &*theme::text_secondary()];
        }
        let side = GLYPH;
        iv.setFrame(NSRect::new(
            NSPoint::new(
                ((CONTROL_TILE - side) / 2.0).round(),
                ((CONTROL_TILE - side) / 2.0).round(),
            ),
            NSSize::new(side, side),
        ));
        tile.addSubview(&iv);
    }
    tile
}

/// Glyph box inside a 26 px tile.
pub(crate) const GLYPH: f64 = 14.0;

/// An SF Symbol as a template image (adopts `contentTintColor`).
pub(crate) fn template_symbol(name: &str) -> Option<Retained<NSImage>> {
    let ns = NSString::from_str(name);
    let img = NSImage::imageWithSystemSymbolName_accessibilityDescription(&ns, None)?;
    img.setTemplate(true);
    Some(img)
}

/// A chip: 28 px, `Radius.sm`, `chip_fill`, `meta` text, with an optional
/// leading glyph.
pub(crate) fn chip(
    mtm: MainThreadMarker,
    text: &str,
    symbol: Option<&str>,
    x_right: f64,
    mid_y: f64,
) -> Retained<SlateView> {
    let l = label(mtm, text, TYPE_CAPTION, WEIGHT_REGULAR, theme::text_secondary());
    l.sizeToFit();
    let tw = l.frame().size.width;
    let th = l.frame().size.height;
    let glyph_w = if symbol.is_some() {
        CHIP_GLYPH + CHIP_GAP
    } else {
        0.0
    };
    let w = (tw + glyph_w + 2.0 * CHIP_PAD).round();
    let chip = SlateView::new(
        mtm,
        NSRect::new(
            NSPoint::new((x_right - w).round(), (mid_y - CONTROL_CHIP / 2.0).round()),
            NSSize::new(w, CONTROL_CHIP),
        ),
        Paint::Chip,
        RADIUS_MD,
    );
    let mut cx = CHIP_PAD;
    if let Some(sym) = symbol {
        if let Some(img) = template_symbol(sym) {
            let iv = NSImageView::new(mtm);
            iv.setImage(Some(&img));
            unsafe {
                let _: () = msg_send![&*iv, setContentTintColor: &*theme::text_secondary()];
            }
            iv.setFrame(NSRect::new(
                NSPoint::new(cx, ((CONTROL_CHIP - CHIP_GLYPH) / 2.0).round()),
                NSSize::new(CHIP_GLYPH, CHIP_GLYPH),
            ));
            chip.addSubview(&iv);
        }
        cx += CHIP_GLYPH + CHIP_GAP;
    }
    l.setFrame(NSRect::new(
        NSPoint::new(cx, ((CONTROL_CHIP - th) / 2.0).round()),
        NSSize::new(tw, th),
    ));
    chip.addSubview(&l);
    chip
}

const CHIP_PAD: f64 = 9.0;
const CHIP_GLYPH: f64 = 12.0;
const CHIP_GAP: f64 = 6.0;

/// A transparent button laid over a painted row so the row keeps its Slate
/// paint job while the click still travels through AppKit's target/action.
pub(crate) fn click_overlay(
    mtm: MainThreadMarker,
    frame: NSRect,
    target: &AnyObject,
    action: objc2::runtime::Sel,
    tag: isize,
) -> Retained<NSButton> {
    let btn: Retained<NSButton> = unsafe { msg_send![NSButton::alloc(mtm), initWithFrame: frame] };
    btn.setTitle(&NSString::from_str(""));
    btn.setBordered(false);
    unsafe {
        let _: () = msg_send![&*btn, setTransparent: true];
        let _: () = msg_send![&*btn, setTarget: target];
        let _: () = msg_send![&*btn, setAction: action];
        let _: () = msg_send![&*btn, setTag: tag];
    }
    btn
}

/// A right-aligned "label + key caps" cluster, as in the footer well
/// ("Copy ↩", "Actions ⌘ K"). Returns the views and the total width.
pub(crate) fn key_hint(
    mtm: MainThreadMarker,
    text: &str,
    caps: &[&str],
    x: f64,
    mid_y: f64,
) -> (Vec<Retained<NSView>>, f64) {
    let mut out: Vec<Retained<NSView>> = Vec::new();
    let mut cx = x;
    let l = label(mtm, text, TYPE_META, WEIGHT_REGULAR, theme::text_secondary());
    let w = place_left(&l, cx, mid_y);
    cx += w + SPACE_XS;
    out.push(Retained::into_super(Retained::into_super(l)));
    for cap in caps {
        let c = key_cap(mtm, cap, cx, mid_y);
        cx += c.frame().size.width + SPACE_XXS;
        out.push(Retained::into_super(c));
    }
    if !caps.is_empty() {
        cx -= SPACE_XXS;
    }
    (out, cx - x)
}

// ─── Window chrome ───────────────────────────────────────────────────────

/// Give a titled window the house look: no titlebar chrome, content running
/// edge to edge under the traffic lights, and the window ground painted from
/// the palette. The appearance is deliberately *not* pinned — every window
/// follows the application appearance.
pub(crate) fn apply_panel_chrome(window: &NSWindow) {
    window.setTitlebarAppearsTransparent(true);
    window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
    window.setBackgroundColor(Some(&theme::surface()));
    window.setMovableByWindowBackground(true);
}

/// The glass behind a floating panel or the pill: an `NSVisualEffectView`
/// filling `frame`, with the panel paint (tint + stroke + highlight) drawn on
/// top of it. Returns the container to add content to, and the tint view.
///
/// A blur material renders nothing useful in a window that was never ordered
/// in — it falls back to an opaque light fill that shows through the panel's
/// rounded corners. So while `render-ui` is snapshotting, the container is a
/// plain view and `Paint::Panel` paints the tint at full alpha instead.
pub(crate) fn glass(
    mtm: MainThreadMarker,
    frame: NSRect,
    radius: f64,
) -> (Retained<NSView>, Retained<SlateView>) {
    let container: Retained<NSView> = if opaque_materials() {
        NSView::initWithFrame(NSView::alloc(mtm), frame)
    } else {
        let ve: Retained<NSVisualEffectView> =
            unsafe { msg_send![NSVisualEffectView::alloc(mtm), initWithFrame: frame] };
        {
            ve.setMaterial(NSVisualEffectMaterial::HUDWindow);
            ve.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
            ve.setState(NSVisualEffectState::Active);
        }
        Retained::into_super(ve)
    };
    unsafe {
        container.setWantsLayer(true);
        let layer: *mut AnyObject = msg_send![&*container, layer];
        let _: () = msg_send![layer, setCornerRadius: radius];
        let _: () = msg_send![layer, setMasksToBounds: true];
    }
    let tint = SlateView::new(
        mtm,
        NSRect::new(NSPoint::new(0.0, 0.0), frame.size),
        Paint::Panel,
        radius,
    );
    unsafe {
        // Width + height sizable, so the tint tracks the pill's width.
        let _: () = msg_send![&*tint, setAutoresizingMask: 2usize | 16usize];
    }
    container.addSubview(&tint);
    (container, tint)
}
