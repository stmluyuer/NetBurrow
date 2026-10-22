//! Design tokens: the warm paper palette, semantic tones, radii and the global egui style.
use eframe::egui::{self, Color32, Vec2};

pub const BACKGROUND: Color32 = Color32::from_rgb(246, 243, 237);
/// Sunken controls: segmented tracks, hovered rows, secondary fills.
pub const SURFACE: Color32 = Color32::from_rgb(237, 233, 224);
/// Raised cards that sit on the page background.
pub const CARD: Color32 = Color32::from_rgb(251, 249, 245);
pub const FIELD: Color32 = Color32::from_rgb(252, 250, 247);
pub const BORDER: Color32 = Color32::from_rgb(216, 210, 200);
pub const DIVIDER: Color32 = Color32::from_rgb(232, 227, 218);
pub const TEXT: Color32 = Color32::from_rgb(52, 49, 45);
pub const MUTED: Color32 = Color32::from_rgb(117, 111, 102);
pub const SUBTLE: Color32 = Color32::from_rgb(163, 156, 145);
pub const ACCENT: Color32 = Color32::from_rgb(92, 114, 90);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(79, 99, 77);

pub const RADIUS_CARD: u8 = 12;
pub const RADIUS_CONTROL: u8 = 8;
pub const PAGE_MARGIN: i8 = 24;

/// Semantic colour family; `fg` is for text and icons, `soft` for tinted surfaces behind them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    Neutral,
    Accent,
    Info,
    Warn,
    Danger,
}

impl Tone {
    pub const fn fg(self) -> Color32 {
        match self {
            Tone::Neutral => MUTED,
            Tone::Accent => Color32::from_rgb(63, 90, 62),
            Tone::Info => Color32::from_rgb(59, 108, 139),
            Tone::Warn => Color32::from_rgb(151, 103, 37),
            Tone::Danger => Color32::from_rgb(174, 65, 60),
        }
    }
    pub const fn soft(self) -> Color32 {
        match self {
            Tone::Neutral => SURFACE,
            Tone::Accent => Color32::from_rgb(227, 233, 223),
            Tone::Info => Color32::from_rgb(226, 234, 240),
            Tone::Warn => Color32::from_rgb(244, 233, 215),
            Tone::Danger => Color32::from_rgb(245, 226, 223),
        }
    }
    pub fn border(self) -> Color32 {
        if self == Tone::Neutral { DIVIDER } else { self.soft().lerp_to_gamma(self.fg(), 0.25) }
    }
}

pub fn bold(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name("ui-bold".into()))
}

pub fn configure_style(context: &egui::Context) {
    // Fix the theme as well as its palette: OS light-mode events must not replace half the UI.
    context.set_theme(egui::ThemePreference::Light);
    context.all_styles_mut(|style| {
        let mut visuals = egui::Visuals::light();
        visuals.override_text_color = Some(TEXT);
        visuals.weak_text_color = Some(MUTED);
        visuals.panel_fill = BACKGROUND;
        visuals.window_fill = CARD;
        visuals.extreme_bg_color = BACKGROUND;
        visuals.text_edit_bg_color = Some(FIELD);
        visuals.faint_bg_color = SURFACE;
        visuals.window_corner_radius = egui::CornerRadius::same(RADIUS_CARD);
        visuals.window_stroke = egui::Stroke::new(1.0, BORDER);
        visuals.selection.bg_fill = Tone::Accent.soft().lerp_to_gamma(ACCENT, 0.15);
        visuals.selection.stroke = egui::Stroke::new(1.0, ACCENT);
        visuals.hyperlink_color = ACCENT;
        visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, DIVIDER);
        visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, TEXT);
        for (widget, fill, stroke) in [
            (&mut visuals.widgets.inactive, CARD, BORDER),
            (&mut visuals.widgets.hovered, SURFACE, BORDER),
            (&mut visuals.widgets.active, Tone::Accent.soft(), ACCENT),
            (&mut visuals.widgets.open, SURFACE, BORDER),
        ] {
            widget.bg_fill = fill;
            widget.weak_bg_fill = fill;
            widget.bg_stroke = egui::Stroke::new(1.0, stroke);
            widget.fg_stroke = egui::Stroke::new(1.0, TEXT);
            widget.corner_radius = egui::CornerRadius::same(RADIUS_CONTROL);
            widget.expansion = 0.0;
        }
        style.visuals = visuals;
        style.spacing.item_spacing = Vec2::new(9.0, 6.0);
        style.spacing.button_padding = Vec2::new(12.0, 6.0);
        style.spacing.interact_size = Vec2::new(36.0, 30.0);
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(15.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(13.0));
        style
            .text_styles
            .insert(egui::TextStyle::Small, egui::FontId::proportional(12.0));
    });
}
