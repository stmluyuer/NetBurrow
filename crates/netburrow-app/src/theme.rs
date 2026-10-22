//! Design tokens for every theme, the global egui style and the theme fonts.
//! Each theme has a page palette (panels) and a card palette. They only differ when cards are much
//! lighter than the page (Paper); `widgets::Card` switches to the card palette for its contents.
use std::cell::Cell;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};

use eframe::egui::{self, Color32, CornerRadius, Vec2};
pub use netburrow_core::Theme;

pub const PAGE_MARGIN: i8 = 24;

pub struct Palette {
    pub background: Color32,
    /// Sunken controls: segmented tracks, hovered rows, secondary fills.
    pub surface: Color32,
    /// Raised cards that sit on the page background.
    pub card: Color32,
    pub field: Color32,
    pub border: Color32,
    pub divider: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub subtle: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub on_accent: Color32,
    /// Accent-coloured text such as links and text-only buttons; brighter than `accent` on dark pages.
    pub link: Color32,
    /// Text on `card`-coloured controls, which may sit on a darker page.
    pub on_card: Color32,
    /// Popups and tooltips.
    pub window: Color32,
    pub switch_off: Color32,
    /// Foreground and soft tint for Accent, Info, Warn and Danger.
    tones: [(Color32, Color32); 4],
    pub card_radius: CornerRadius,
    pub control_radius: CornerRadius,
    pub stroke: f32,
    /// Inked themes outline every card and button in `border` instead of a tone tint.
    pub ink_outline: bool,
    pub card_shadow: egui::Shadow,
    /// Solid offset shadow under buttons, for the sticker and pixel looks.
    pub button_shadow: Option<(Vec2, Color32)>,
    pub dark: bool,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

const fn corners(nw: u8, ne: u8, sw: u8, se: u8) -> CornerRadius {
    CornerRadius { nw, ne, sw, se }
}

const SOFT_SHADOW: egui::Shadow = egui::Shadow { offset: [0, 1], blur: 6, spread: 0, color: Color32::from_black_alpha(10) };

const CLASSIC: Palette = Palette {
    background: rgb(246, 243, 237),
    surface: rgb(237, 233, 224),
    card: rgb(251, 249, 245),
    field: rgb(252, 250, 247),
    border: rgb(216, 210, 200),
    divider: rgb(232, 227, 218),
    text: rgb(52, 49, 45),
    muted: rgb(117, 111, 102),
    subtle: rgb(163, 156, 145),
    accent: rgb(92, 114, 90),
    accent_hover: rgb(79, 99, 77),
    on_accent: Color32::WHITE,
    link: rgb(92, 114, 90),
    on_card: rgb(52, 49, 45),
    window: rgb(251, 249, 245),
    switch_off: rgb(196, 189, 178),
    tones: [
        (rgb(63, 90, 62), rgb(227, 233, 223)),
        (rgb(59, 108, 139), rgb(226, 234, 240)),
        (rgb(151, 103, 37), rgb(244, 233, 215)),
        (rgb(174, 65, 60), rgb(245, 226, 223)),
    ],
    card_radius: CornerRadius::same(12),
    control_radius: CornerRadius::same(8),
    stroke: 1.0,
    ink_outline: false,
    card_shadow: SOFT_SHADOW,
    button_shadow: None,
    dark: false,
};

/// Paper scraps inked on a dark basement floor; card contents use `PAPER_CARD`.
const PAPER_TONES: [(Color32, Color32); 4] = [
    (rgb(52, 98, 40), rgb(214, 222, 178)),
    (rgb(40, 88, 128), rgb(208, 220, 222)),
    (rgb(140, 86, 10), rgb(240, 214, 162)),
    (rgb(164, 30, 24), rgb(240, 202, 186)),
];
const PAPER_CARD: Palette = Palette {
    background: rgb(42, 29, 23),
    surface: rgb(220, 199, 161),
    card: rgb(233, 215, 182),
    field: rgb(244, 234, 214),
    border: rgb(28, 18, 12),
    divider: rgb(180, 155, 120),
    text: rgb(42, 26, 18),
    muted: rgb(107, 82, 64),
    subtle: rgb(150, 128, 100),
    accent: rgb(179, 38, 30),
    accent_hover: rgb(150, 30, 24),
    on_accent: rgb(250, 238, 216),
    link: rgb(179, 38, 30),
    on_card: rgb(42, 26, 18),
    window: rgb(58, 42, 32),
    switch_off: rgb(196, 176, 142),
    tones: PAPER_TONES,
    card_radius: corners(4, 18, 18, 4),
    control_radius: corners(3, 12, 12, 3),
    stroke: 2.0,
    ink_outline: true,
    card_shadow: egui::Shadow { offset: [2, 3], blur: 0, spread: 0, color: Color32::from_black_alpha(110) },
    button_shadow: None,
    dark: true,
};
const PAPER_PAGE: Palette = Palette {
    surface: rgb(24, 16, 11),
    field: rgb(30, 21, 16),
    divider: rgb(90, 68, 51),
    text: rgb(234, 220, 192),
    muted: rgb(184, 162, 134),
    subtle: rgb(140, 119, 97),
    accent: rgb(226, 98, 76),
    accent_hover: rgb(240, 122, 98),
    link: rgb(232, 112, 88),
    switch_off: rgb(90, 68, 51),
    ..PAPER_CARD
};

const PIXEL: Palette = Palette {
    background: rgb(59, 42, 32),
    surface: rgb(26, 17, 12),
    card: rgb(37, 24, 15),
    field: rgb(26, 17, 12),
    border: rgb(107, 75, 54),
    divider: rgb(64, 46, 34),
    text: rgb(234, 217, 182),
    muted: rgb(170, 146, 114),
    subtle: rgb(146, 124, 98),
    accent: rgb(163, 39, 31),
    accent_hover: rgb(190, 52, 42),
    on_accent: rgb(251, 233, 208),
    link: rgb(236, 110, 92),
    on_card: rgb(234, 217, 182),
    window: rgb(37, 24, 15),
    switch_off: rgb(64, 46, 34),
    tones: [
        (rgb(126, 200, 122), rgb(30, 48, 26)),
        (rgb(127, 182, 216), rgb(22, 36, 48)),
        (rgb(230, 184, 92), rgb(58, 38, 16)),
        (rgb(236, 96, 84), rgb(62, 20, 16)),
    ],
    card_radius: CornerRadius::ZERO,
    control_radius: CornerRadius::ZERO,
    stroke: 2.0,
    ink_outline: true,
    card_shadow: egui::Shadow { offset: [3, 3], blur: 0, spread: 0, color: rgb(18, 11, 7) },
    button_shadow: Some((Vec2::new(0.0, 3.0), rgb(18, 11, 7))),
    dark: true,
};

const CARTOON: Palette = Palette {
    background: rgb(233, 220, 195),
    surface: rgb(226, 211, 184),
    card: rgb(243, 232, 208),
    field: rgb(251, 244, 230),
    border: rgb(30, 22, 17),
    divider: rgb(222, 206, 178),
    text: rgb(30, 22, 17),
    muted: rgb(116, 96, 74),
    subtle: rgb(158, 138, 114),
    accent: rgb(194, 58, 48),
    accent_hover: rgb(170, 46, 38),
    on_accent: rgb(255, 246, 234),
    link: rgb(194, 58, 48),
    on_card: rgb(30, 22, 17),
    window: rgb(243, 232, 208),
    switch_off: rgb(214, 198, 168),
    tones: [
        (rgb(47, 90, 34), rgb(225, 236, 208)),
        (rgb(40, 90, 130), rgb(218, 230, 236)),
        (rgb(122, 76, 10), rgb(246, 226, 188)),
        (rgb(164, 36, 28), rgb(246, 214, 204)),
    ],
    card_radius: corners(20, 16, 16, 20),
    control_radius: CornerRadius::same(12),
    stroke: 2.5,
    ink_outline: true,
    card_shadow: egui::Shadow { offset: [0, 3], blur: 0, spread: 0, color: rgb(30, 22, 17) },
    button_shadow: Some((Vec2::new(0.0, 3.0), rgb(30, 22, 17))),
    dark: false,
};

static THEME: AtomicU8 = AtomicU8::new(0);

thread_local! {
    static CARD_DEPTH: Cell<u32> = const { Cell::new(0) };
}

pub fn current() -> Theme {
    match THEME.load(Ordering::Relaxed) {
        1 => Theme::Paper,
        2 => Theme::Pixel,
        3 => Theme::Cartoon,
        _ => Theme::Classic,
    }
}

/// Game-styled themes swap line icons and empty states for illustrated items.
pub fn illustrated() -> bool {
    current() != Theme::Classic
}

fn palettes() -> (&'static Palette, &'static Palette) {
    match current() {
        Theme::Classic => (&CLASSIC, &CLASSIC),
        Theme::Paper => (&PAPER_PAGE, &PAPER_CARD),
        Theme::Pixel => (&PIXEL, &PIXEL),
        Theme::Cartoon => (&CARTOON, &CARTOON),
    }
}

/// Palette for whatever is being drawn now: card contents get the card palette.
pub fn pal() -> &'static Palette {
    let (page, card) = palettes();
    if CARD_DEPTH.with(Cell::get) > 0 { card } else { page }
}

pub fn card_pal() -> &'static Palette {
    palettes().1
}

/// Runs `draw` with the card palette, so colours picked inside match the card surface.
pub fn in_card<R>(draw: impl FnOnce() -> R) -> R {
    CARD_DEPTH.with(|depth| depth.set(depth.get() + 1));
    let result = draw();
    CARD_DEPTH.with(|depth| depth.set(depth.get() - 1));
    result
}

/// Gives a card's child `Ui` the card palette for egui's own widgets and default text.
pub fn card_visuals(ui: &mut egui::Ui) {
    apply_visuals(ui.visuals_mut(), card_pal());
}

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
    fn pair(self) -> (Color32, Color32) {
        let palette = pal();
        match self {
            Tone::Neutral => (palette.muted, palette.surface),
            Tone::Accent => palette.tones[0],
            Tone::Info => palette.tones[1],
            Tone::Warn => palette.tones[2],
            Tone::Danger => palette.tones[3],
        }
    }
    pub fn fg(self) -> Color32 {
        self.pair().0
    }
    pub fn soft(self) -> Color32 {
        self.pair().1
    }
    pub fn border(self) -> Color32 {
        if self == Tone::Neutral { pal().divider } else { self.soft().lerp_to_gamma(self.fg(), 0.25) }
    }
}

pub fn bold(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name("ui-bold".into()))
}

fn apply_visuals(visuals: &mut egui::Visuals, palette: &Palette) {
    visuals.override_text_color = Some(palette.text);
    visuals.weak_text_color = Some(palette.muted);
    visuals.panel_fill = palette.background;
    visuals.window_fill = palette.window;
    visuals.extreme_bg_color = palette.background;
    visuals.text_edit_bg_color = Some(palette.field);
    visuals.faint_bg_color = palette.surface;
    visuals.window_corner_radius = palette.card_radius;
    visuals.window_stroke = egui::Stroke::new(1.0, palette.border);
    visuals.selection.bg_fill = palette.tones[0].1.lerp_to_gamma(palette.accent, 0.15);
    visuals.selection.stroke = egui::Stroke::new(1.0, palette.accent);
    visuals.hyperlink_color = palette.link;
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, palette.divider);
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, palette.text);
    for (widget, fill, stroke) in [
        (&mut visuals.widgets.inactive, palette.card, palette.border),
        (&mut visuals.widgets.hovered, palette.surface, palette.border),
        (&mut visuals.widgets.active, palette.tones[0].1, palette.accent),
        (&mut visuals.widgets.open, palette.surface, palette.border),
    ] {
        widget.bg_fill = fill;
        widget.weak_bg_fill = fill;
        widget.bg_stroke = egui::Stroke::new(1.0, stroke);
        widget.fg_stroke = egui::Stroke::new(1.0, palette.on_card);
        widget.corner_radius = palette.control_radius;
        widget.expansion = 0.0;
    }
}

pub fn configure_style(context: &egui::Context) {
    let palette = palettes().0;
    // Fix the theme as well as its palette: OS light-mode events must not replace half the UI.
    context.set_theme(if palette.dark { egui::ThemePreference::Dark } else { egui::ThemePreference::Light });
    context.all_styles_mut(|style| {
        let mut visuals = if palette.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
        apply_visuals(&mut visuals, palette);
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

/// Switches the whole UI; called at startup and whenever the appearance setting changes.
pub fn set(context: &egui::Context, theme: Theme) {
    THEME.store(theme as u8, Ordering::Relaxed);
    configure_style(context);
    apply_fonts(context);
    context.request_repaint();
}

struct SystemFonts {
    data: Vec<(&'static str, Arc<egui::FontData>)>,
    regular: Vec<String>,
    heavy: Vec<String>,
}

static SYSTEM_FONTS: OnceLock<SystemFonts> = OnceLock::new();

/// Loads Windows fonts once; returns whether a Chinese font was found, so startup can avoid missing glyphs.
pub fn init_fonts() -> bool {
    let fonts = SYSTEM_FONTS.get_or_init(|| {
        let mut fonts = SystemFonts { data: Vec::new(), regular: Vec::new(), heavy: Vec::new() };
        let directory = super::system_font_directory();
        let mut chinese = false;
        for (name, file, is_bold) in [
            ("latin", "segoeui.ttf", false),
            ("cjk", "msyh.ttc", false),
            ("cjk-fallback", "simsun.ttc", false),
            ("latin-bold", "segoeuib.ttf", true),
            ("cjk-bold", "msyhbd.ttc", true),
        ] {
            let Some(directory) = &directory else { break };
            if name == "cjk-fallback" && chinese { continue; }
            if let Ok(bytes) = std::fs::read(directory.join(file)) {
                chinese |= name == "cjk" || name == "cjk-fallback";
                fonts.data.push((name, Arc::new(egui::FontData::from_owned(bytes))));
                if is_bold { fonts.heavy.push(name.into()) } else { fonts.regular.push(name.into()) }
            }
        }
        fonts
    });
    fonts.data.iter().any(|(name, _)| name.starts_with("cjk") && !name.ends_with("bold"))
}

/// Theme fonts are subsets of the UI's own strings (scripts/subset-theme-fonts.py); other text falls
/// through to the system fonts behind them.
fn apply_fonts(context: &egui::Context) {
    let Some(system) = SYSTEM_FONTS.get() else { return };
    let mut fonts = egui::FontDefinitions::default();
    for (name, data) in &system.data {
        fonts.font_data.insert((*name).into(), Arc::clone(data));
    }
    fonts.font_data.insert(
        "kuaile".into(),
        Arc::new(egui::FontData::from_static(include_bytes!("../assets/fonts/kuaile.ttf"))),
    );
    fonts.font_data.insert(
        "pixel".into(),
        Arc::new(egui::FontData::from_static(include_bytes!("../assets/fonts/fusion-pixel.ttf"))),
    );
    let mut regular = system.regular.clone();
    regular.extend(fonts.families[&egui::FontFamily::Proportional].clone());
    let mut heavy = system.heavy.clone();
    heavy.extend(regular.clone());
    fonts.families.entry(egui::FontFamily::Monospace).or_default().extend(regular.clone());
    let (body, title) = match current() {
        Theme::Classic => (None, None),
        Theme::Paper => (Some("kuaile"), Some("kuaile")),
        Theme::Pixel => (Some("pixel"), Some("pixel")),
        Theme::Cartoon => (None, Some("kuaile")),
    };
    if let Some(font) = body {
        regular.insert(0, font.into());
    }
    if let Some(font) = title {
        heavy.insert(0, font.into());
    }
    fonts.families.insert(egui::FontFamily::Proportional, regular);
    fonts.families.insert(egui::FontFamily::Name("ui-bold".into()), heavy);
    context.set_fonts(fonts);
}
