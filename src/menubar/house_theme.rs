// GENERATED from design-system/tokens.json 3f0480fd80d7; do not edit
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
    /// Window and panel ground.
    pub surface: Rgba,
    /// Cards, inputs, and rows that sit above the ground.
    pub surface_raised: Rgba,
    /// Tracks and wells that sit below the ground.
    pub surface_sunken: Rgba,
    /// Quiet grouped surfaces painted over any ground.
    pub surface_tint: Rgba,
    /// Body text and primary labels.
    pub text_primary: Rgba,
    /// Supporting text.
    pub text_secondary: Rgba,
    /// Metadata. Still 4.5:1 on surface.
    pub text_tertiary: Rgba,
    /// Text on accent or on the HUD.
    pub text_inverse: Rgba,
    /// The one accent. Primary action only.
    pub accent: Rgba,
    /// Tinted fill behind an accent label.
    pub accent_soft: Rgba,
    /// Status only.
    pub success: Rgba,
    /// Status only.
    pub warning: Rgba,
    /// Status and destructive actions only.
    pub danger: Rgba,
    /// Hairline around panels, cards, fields.
    pub stroke: Rgba,
    /// Selected or focused hairline.
    pub stroke_strong: Rgba,
    /// Highlighted row.
    pub selection_fill: Rgba,
    /// Hover and pressed feedback.
    pub hover_fill: Rgba,
    /// Key-cap background.
    pub key_cap_fill: Rgba,
    /// Key-cap hairline.
    pub key_cap_stroke: Rgba,
    /// Tint over the blur material of a floating panel.
    pub panel_tint: Rgba,
    /// Dark HUD pill or toast, same in both modes.
    pub hud_fill: Rgba,
    /// Text on the HUD.
    pub hud_text: Rgba,
    /// Hairline on the HUD.
    pub hud_stroke: Rgba,
    /// Busy or idle marks on the HUD.
    pub hud_muted: Rgba,
}

pub const LIGHT: Palette = Palette {
    surface: Rgba { r: 0.9686, g: 0.9686, b: 0.9725, a: 1.0 },
    surface_raised: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
    surface_sunken: Rgba { r: 0.9255, g: 0.9255, b: 0.9373, a: 1.0 },
    surface_tint: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.035 },
    text_primary: Rgba { r: 0.0667, g: 0.0667, b: 0.0784, a: 1.0 },
    text_secondary: Rgba { r: 0.3451, g: 0.3451, b: 0.3804, a: 1.0 },
    text_tertiary: Rgba { r: 0.4314, g: 0.4314, b: 0.4627, a: 1.0 },
    text_inverse: Rgba { r: 0.949, g: 0.949, b: 0.9608, a: 1.0 },
    accent: Rgba { r: 0.0314, g: 0.4, b: 0.8392, a: 1.0 },
    accent_soft: Rgba { r: 0.0314, g: 0.4, b: 0.8392, a: 0.12 },
    success: Rgba { r: 0.1176, g: 0.5412, b: 0.3059, a: 1.0 },
    warning: Rgba { r: 0.7216, g: 0.4627, b: 0.0392, a: 1.0 },
    danger: Rgba { r: 0.7529, g: 0.2235, b: 0.1686, a: 1.0 },
    stroke: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.1 },
    stroke_strong: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.18 },
    selection_fill: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.07 },
    hover_fill: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.075 },
    key_cap_fill: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.06 },
    key_cap_stroke: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.1 },
    panel_tint: Rgba { r: 0.9686, g: 0.9686, b: 0.9725, a: 0.78 },
    hud_fill: Rgba { r: 0.0706, g: 0.0706, b: 0.0784, a: 0.95 },
    hud_text: Rgba { r: 0.949, g: 0.949, b: 0.9608, a: 1.0 },
    hud_stroke: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.1 },
    hud_muted: Rgba { r: 0.549, g: 0.5804, b: 0.6196, a: 1.0 },
};

pub const DARK: Palette = Palette {
    surface: Rgba { r: 0.102, g: 0.1059, b: 0.1176, a: 1.0 },
    surface_raised: Rgba { r: 0.149, g: 0.1569, b: 0.1725, a: 1.0 },
    surface_sunken: Rgba { r: 0.0784, g: 0.0824, b: 0.0902, a: 1.0 },
    surface_tint: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.055 },
    text_primary: Rgba { r: 0.9412, g: 0.9412, b: 0.949, a: 1.0 },
    text_secondary: Rgba { r: 0.6588, g: 0.6588, b: 0.6902, a: 1.0 },
    text_tertiary: Rgba { r: 0.549, g: 0.549, b: 0.5804, a: 1.0 },
    text_inverse: Rgba { r: 0.0667, g: 0.0667, b: 0.0784, a: 1.0 },
    accent: Rgba { r: 0.3529, g: 0.6667, b: 1.0, a: 1.0 },
    accent_soft: Rgba { r: 0.3529, g: 0.6667, b: 1.0, a: 0.18 },
    success: Rgba { r: 0.298, g: 0.7647, b: 0.5412, a: 1.0 },
    warning: Rgba { r: 0.9608, g: 0.7216, b: 0.2902, a: 1.0 },
    danger: Rgba { r: 1.0, g: 0.4196, b: 0.3686, a: 1.0 },
    stroke: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.14 },
    stroke_strong: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.22 },
    selection_fill: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.1 },
    hover_fill: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.1 },
    key_cap_fill: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.09 },
    key_cap_stroke: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.12 },
    panel_tint: Rgba { r: 0.1098, g: 0.1216, b: 0.1412, a: 0.82 },
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
pub const TYPE_BODY: f64 = 14.0;
pub const TYPE_BODY_SMALL: f64 = 13.0;
pub const TYPE_LABEL: f64 = 13.0;
pub const TYPE_META: f64 = 12.0;
pub const TYPE_CAPTION: f64 = 11.0;
pub const TYPE_MICRO: f64 = 10.0;
pub const TYPE_KEY_CAP: f64 = 10.0;
pub const TYPE_CODE: f64 = 12.0;

/// Font weights as NSFontWeight values.
pub const WEIGHT_REGULAR: f64 = 0.0;
pub const WEIGHT_MEDIUM: f64 = 0.23;
pub const WEIGHT_SEMIBOLD: f64 = 0.3;
pub const WEIGHT_BOLD: f64 = 0.4;

pub const RADIUS_XS: f64 = 4.0;
pub const RADIUS_SM: f64 = 6.0;
pub const RADIUS_MD: f64 = 8.0;
pub const RADIUS_LG: f64 = 12.0;
pub const RADIUS_XL: f64 = 16.0;
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

pub const CONTROL_COMPACT: f64 = 28.0;
pub const CONTROL_SMALL: f64 = 32.0;
pub const CONTROL_MEDIUM: f64 = 40.0;
pub const CONTROL_LARGE: f64 = 44.0;
pub const CONTROL_XLARGE: f64 = 48.0;
pub const CONTROL_HERO: f64 = 56.0;

pub const MOTION_FAST: f64 = 0.1;
pub const MOTION_NORMAL: f64 = 0.2;
pub const HAIRLINE: f64 = 1.0;
