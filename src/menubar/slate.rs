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

use super::house_theme::{Palette, Rgba, DARK, LIGHT};
use super::*;

use objc2::DefinedClass;
use std::cell::Cell;
use std::ptr::NonNull;

mod controls;
pub(crate) use controls::{
    InkToggle,
    WaveformView,
    DOT,
    GLYPH,
    TOGGLE_H,
    TOGGLE_W,
    apply_panel_chrome,
    chip,
    click_overlay,
    glass,
    icon_tile,
    key_cap,
    key_hint,
    label,
    place_left,
    section_label,
    status_dot,
    template_symbol,
};

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
    /// An active processing stage, paired with its visible label.
    DotNeutral,
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
            Paint::DotNeutral => {
                p.text_secondary.ns_color().setFill();
                self.rounded(bounds, bounds.size.height / 2.0).fill();
            }
        }
    }
}
