// GENERATED from design-system/tokens.json d1fa3a8133f6; do not edit
// Source of truth: the design-system repo (DESIGN.md + tokens.json).
// Regenerate with `make rust` there; `make check` verifies this copy.
#![allow(dead_code)]

/// sRGB colour, components in 0.0..=1.0.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgba {
    pub r: f64,
    pub g: f64,
    pub b: f64,
    pub a: f64,
}

#[cfg(target_os = "macos")]
impl Rgba {
    /// AppKit colour for this value (objc2-app-kit).
    pub fn ns_color(&self) -> objc2::rc::Retained<objc2_app_kit::NSColor> {
        objc2_app_kit::NSColor::colorWithSRGBRed_green_blue_alpha(self.r, self.g, self.b, self.a)
    }
}

/// One colour per semantic token.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    /// Opaque window ground (Settings, History).
    pub surface: Rgba,
    /// Raised cards and composers that sit above the ground.
    pub surface_raised: Rgba,
    /// Opaque rails and tracks that sit below the ground.
    pub surface_sunken: Rgba,
    /// Quiet grouped fill painted over any ground (rail, card body).
    pub surface_tint: Rgba,
    /// Sunken footer or bar well painted over a panel.
    pub well: Rgba,
    /// 26 px icon tile behind a row glyph.
    pub tile_fill: Rgba,
    /// Hairline around an icon tile.
    pub tile_stroke: Rgba,
    /// Model, context, and session chips.
    pub chip_fill: Rgba,
    /// Ink. Warm near-black on light, warm near-white on dark.
    pub text_primary: Rgba,
    /// Ink at 60 %. Supporting text; tracks any ground.
    pub text_secondary: Rgba,
    /// Ink at 40 %. Metadata, placeholders, section labels.
    pub text_tertiary: Rgba,
    /// Text on the HUD or on an ink-filled control.
    pub text_inverse: Rgba,
    /// Focus rings and links only. Never chrome, never a button fill.
    pub accent: Rgba,
    /// Tinted fill behind an accent label.
    pub accent_soft: Rgba,
    /// Status dot: ready, on, landed.
    pub success: Rgba,
    /// Status only.
    pub warning: Rgba,
    /// Status dot: off, error, recording. Destructive actions.
    pub danger: Rgba,
    /// Hairline around a panel or card.
    pub stroke: Rgba,
    /// Focused or selected hairline.
    pub stroke_strong: Rgba,
    /// Line between rows and sections. Quieter than the panel stroke.
    pub divider: Rgba,
    /// 1 px inset highlight along the top edge of a panel or raised card.
    pub highlight_top: Rgba,
    /// Selected row fill.
    pub selection_fill: Rgba,
    /// Inset ring on the selected row.
    pub selection_ring: Rgba,
    /// Hover feedback. Half the selection fill.
    pub hover_fill: Rgba,
    /// Key caps are outlined, not filled.
    pub key_cap_fill: Rgba,
    /// 1 px outline of a key cap.
    pub key_cap_stroke: Rgba,
    /// Tint over the blur material of a floating panel or the pill.
    pub panel_tint: Rgba,
    /// Opaque dark HUD for full-screen overlays (Type to Click). Same in both modes.
    pub hud_fill: Rgba,
    /// Text on the HUD.
    pub hud_text: Rgba,
    /// Hairline on the HUD.
    pub hud_stroke: Rgba,
    /// Busy or idle marks on the HUD.
    pub hud_muted: Rgba,
}

pub const LIGHT: Palette = Palette {
    surface: Rgba { r: 0.9686, g: 0.9686, b: 0.9647, a: 1.0 },
    surface_raised: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
    surface_sunken: Rgba { r: 0.9412, g: 0.9412, b: 0.9373, a: 1.0 },
    surface_tint: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.045 },
    well: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.035 },
    tile_fill: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.045 },
    tile_stroke: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.05 },
    chip_fill: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.05 },
    text_primary: Rgba { r: 0.1098, g: 0.1059, b: 0.102, a: 1.0 },
    text_secondary: Rgba { r: 0.1098, g: 0.1059, b: 0.102, a: 0.62 },
    text_tertiary: Rgba { r: 0.1098, g: 0.1059, b: 0.102, a: 0.5 },
    text_inverse: Rgba { r: 0.949, g: 0.949, b: 0.9608, a: 1.0 },
    accent: Rgba { r: 0.2314, g: 0.3569, b: 0.8588, a: 1.0 },
    accent_soft: Rgba { r: 0.2314, g: 0.3569, b: 0.8588, a: 0.12 },
    success: Rgba { r: 0.1804, g: 0.6078, b: 0.4039, a: 1.0 },
    warning: Rgba { r: 0.7216, g: 0.4627, b: 0.0392, a: 1.0 },
    danger: Rgba { r: 0.7529, g: 0.2235, b: 0.1686, a: 1.0 },
    stroke: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.06 },
    stroke_strong: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.14 },
    divider: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.06 },
    highlight_top: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.9 },
    selection_fill: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.055 },
    selection_ring: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.06 },
    hover_fill: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.03 },
    key_cap_fill: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 },
    key_cap_stroke: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.14 },
    panel_tint: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.88 },
    hud_fill: Rgba { r: 0.0706, g: 0.0706, b: 0.0784, a: 0.95 },
    hud_text: Rgba { r: 0.949, g: 0.949, b: 0.9608, a: 1.0 },
    hud_stroke: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.1 },
    hud_muted: Rgba { r: 0.549, g: 0.5804, b: 0.6196, a: 1.0 },
};

pub const DARK: Palette = Palette {
    surface: Rgba { r: 0.102, g: 0.1059, b: 0.1216, a: 1.0 },
    surface_raised: Rgba { r: 0.1294, g: 0.1333, b: 0.1529, a: 1.0 },
    surface_sunken: Rgba { r: 0.0784, g: 0.0824, b: 0.0941, a: 1.0 },
    surface_tint: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.06 },
    well: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.22 },
    tile_fill: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.06 },
    tile_stroke: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.06 },
    chip_fill: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.06 },
    text_primary: Rgba { r: 0.9294, g: 0.9216, b: 0.9098, a: 1.0 },
    text_secondary: Rgba { r: 0.9294, g: 0.9216, b: 0.9098, a: 0.6 },
    text_tertiary: Rgba { r: 0.9294, g: 0.9216, b: 0.9098, a: 0.4 },
    text_inverse: Rgba { r: 0.0667, g: 0.0667, b: 0.0784, a: 1.0 },
    accent: Rgba { r: 0.6157, g: 0.7059, b: 1.0, a: 1.0 },
    accent_soft: Rgba { r: 0.6157, g: 0.7059, b: 1.0, a: 0.16 },
    success: Rgba { r: 0.498, g: 0.8196, b: 0.651, a: 1.0 },
    warning: Rgba { r: 0.9608, g: 0.7216, b: 0.2902, a: 1.0 },
    danger: Rgba { r: 1.0, g: 0.4196, b: 0.3686, a: 1.0 },
    stroke: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.1 },
    stroke_strong: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.16 },
    divider: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.07 },
    highlight_top: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.05 },
    selection_fill: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.09 },
    selection_ring: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.06 },
    hover_fill: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.045 },
    key_cap_fill: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.0 },
    key_cap_stroke: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.14 },
    panel_tint: Rgba { r: 0.102, g: 0.1059, b: 0.1216, a: 0.9 },
    hud_fill: Rgba { r: 0.0706, g: 0.0706, b: 0.0784, a: 0.95 },
    hud_text: Rgba { r: 0.949, g: 0.949, b: 0.9608, a: 1.0 },
    hud_stroke: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.1 },
    hud_muted: Rgba { r: 0.549, g: 0.5804, b: 0.6196, a: 1.0 },
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub light: Palette,
    pub dark: Palette,
}

/// Both appearances. Pick `.light` or `.dark` per window appearance.
pub const fn theme() -> Theme {
    Theme { light: LIGHT, dark: DARK }
}

/// Type scale: point sizes (SF system font).
pub const TYPE_DISPLAY: f64 = 28.0;
pub const TYPE_TITLE: f64 = 20.0;
pub const TYPE_HEADING: f64 = 16.0;
pub const TYPE_INPUT: f64 = 16.0;
pub const TYPE_BODY: f64 = 14.0;
pub const TYPE_BODY_SMALL: f64 = 13.0;
pub const TYPE_LABEL: f64 = 13.0;
pub const TYPE_META: f64 = 12.0;
pub const TYPE_CAPTION: f64 = 11.0;
pub const TYPE_SECTION: f64 = 10.5;
pub const TYPE_MICRO: f64 = 10.0;
pub const TYPE_KEY_CAP: f64 = 11.0;
pub const TYPE_CODE: f64 = 12.0;

/// Font weights as NSFontWeight values.
pub const WEIGHT_REGULAR: f64 = 0.0;
pub const WEIGHT_MEDIUM: f64 = 0.23;
pub const WEIGHT_SEMIBOLD: f64 = 0.3;
pub const WEIGHT_BOLD: f64 = 0.4;

pub const RADIUS_XS: f64 = 5.0;
pub const RADIUS_SM: f64 = 6.0;
pub const RADIUS_TILE: f64 = 7.0;
pub const RADIUS_MD: f64 = 8.0;
pub const RADIUS_ROW: f64 = 10.0;
pub const RADIUS_LG: f64 = 12.0;
pub const RADIUS_XL: f64 = 16.0;
pub const RADIUS_PILL: f64 = 18.0;
pub const RADIUS_XXL: f64 = 24.0;

pub const SPACE_XXS: f64 = 4.0;
pub const SPACE_XS: f64 = 8.0;
pub const SPACE_SM: f64 = 12.0;
pub const SPACE_MD: f64 = 16.0;
pub const SPACE_LG: f64 = 20.0;
pub const SPACE_XL: f64 = 24.0;
pub const SPACE_XXL: f64 = 32.0;
pub const SPACE_XXXL: f64 = 48.0;
pub const SPACE_XXXXL: f64 = 64.0;

pub const CONTROL_STATUSGLYPH: f64 = 15.0;
pub const CONTROL_KEYCAP: f64 = 20.0;
pub const CONTROL_TILE: f64 = 26.0;
pub const CONTROL_COMPACT: f64 = 28.0;
pub const CONTROL_CHIP: f64 = 28.0;
pub const CONTROL_SMALL: f64 = 32.0;
pub const CONTROL_RAILROW: f64 = 36.0;
pub const CONTROL_PILL: f64 = 36.0;
pub const CONTROL_MEDIUM: f64 = 40.0;
pub const CONTROL_ROW: f64 = 40.0;
pub const CONTROL_FOOTER: f64 = 40.0;
pub const CONTROL_LARGE: f64 = 44.0;
pub const CONTROL_SESSIONROW: f64 = 44.0;
pub const CONTROL_XLARGE: f64 = 48.0;
pub const CONTROL_COMPOSER: f64 = 52.0;
pub const CONTROL_HERO: f64 = 56.0;
pub const CONTROL_INPUT: f64 = 58.0;

pub const MOTION_HOVER: f64 = 0.1;
pub const MOTION_SELECT: f64 = 0.14;
pub const MOTION_OPEN: f64 = 0.12;
pub const MOTION_FAST: f64 = 0.1;
pub const MOTION_NORMAL: f64 = 0.2;
pub const HAIRLINE: f64 = 1.0;

pub const LINE_HEIGHT_BODY: f64 = 1.55;
pub const LINE_HEIGHT_BODY_SMALL: f64 = 1.5;
/// Letter spacing in points for the section style.
pub const TRACKING_SECTION: f64 = 0.84;

/// Blur material behind `panel_tint` on floating panels and the pill.
pub const MATERIAL_BLUR: f64 = 48.0;
pub const MATERIAL_SATURATE: f64 = 1.3;

/// A soft black shadow: offset, blur radius, and opacity per appearance.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shadow {
    pub y: f64,
    pub blur: f64,
    pub light: f64,
    pub dark: f64,
}

/// Inner of the two panel shadows.
pub const SHADOW_PANEL_NEAR: Shadow = Shadow { y: 8.0, blur: 24.0, light: 0.08, dark: 0.3 };
/// Outer of the two panel shadows.
pub const SHADOW_PANEL_FAR: Shadow = Shadow { y: 40.0, blur: 80.0, light: 0.16, dark: 0.5 };
/// Drop under the selected row.
pub const SHADOW_SELECTION: Shadow = Shadow { y: 1.0, blur: 2.0, light: 0.08, dark: 0.2 };
/// Under a raised card or composer.
pub const SHADOW_CARD: Shadow = Shadow { y: 2.0, blur: 8.0, light: 0.06, dark: 0.24 };

/// Fixed layout widths and heights shared by the apps.
pub const LAYOUT_PANEL_WIDTH: f64 = 750.0;
pub const LAYOUT_ANSWER_PANEL_WIDTH: f64 = 750.0;
pub const LAYOUT_ANSWER_MAX_WIDTH: f64 = 620.0;
pub const LAYOUT_QUICK_A_I_HEIGHT: f64 = 475.0;
pub const LAYOUT_QUICK_A_I_ANSWER_MAX_WIDTH: f64 = 690.0;
pub const LAYOUT_SETTINGS_RAIL: f64 = 220.0;
pub const LAYOUT_SETTINGS_WIDTH: f64 = 860.0;
pub const LAYOUT_SETTINGS_HEIGHT: f64 = 620.0;
pub const LAYOUT_CHAT_WIDTH: f64 = 860.0;
pub const LAYOUT_CHAT_HEIGHT: f64 = 620.0;
pub const LAYOUT_CHAT_RAIL: f64 = 220.0;
pub const LAYOUT_CHAT_MIN_WIDTH: f64 = 720.0;
pub const LAYOUT_CHAT_MIN_HEIGHT: f64 = 480.0;
