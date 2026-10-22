//! Themed controls shared by every page. Each reports accessibility info like the egui widget it replaces.
use eframe::egui::{self, Color32, Pos2, Rect, RichText, Vec2};

use super::art::{self, Item};
use super::icons::{self, Action};
use super::theme::{self, *};

const HOVER_SECONDS: f32 = 0.12;
const MOVE_SECONDS: f32 = 0.16;

fn hover(ui: &egui::Ui, response: &egui::Response) -> f32 {
    ui.ctx().animate_bool_with_time(
        response.id.with("hover"),
        response.hovered() && response.enabled(),
        HOVER_SECONDS,
    )
}

fn fade(enabled: bool) -> f32 {
    if enabled { 1.0 } else { 0.45 }
}

/// Eases a colour toward `target`, one channel at a time; egui repaints only while it moves.
pub fn animate_color(context: &egui::Context, id: egui::Id, target: Color32, seconds: f32) -> Color32 {
    let channel = |index: usize, value: u8| {
        context
            .animate_value_with_time(id.with(index), value as f32, seconds)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    let [r, g, b, a] = target.to_array();
    Color32::from_rgba_premultiplied(channel(0, r), channel(1, g), channel(2, b), channel(3, a))
}

/// Rises from 0 to 1 over `seconds` each time `key` changes under `id`, for fade-ins.
pub fn appear(context: &egui::Context, id: egui::Id, key: impl std::hash::Hash + std::fmt::Debug, seconds: f32) -> f32 {
    let key = egui::Id::new(key);
    let now = context.input(|input| input.time);
    let since = context.data_mut(|data| {
        let entry = data.get_temp_mut_or_insert_with(id, || (key, now));
        if entry.0 != key {
            *entry = (key, now);
        }
        entry.1
    });
    let t = ((now - since) as f32 / seconds).clamp(0.0, 1.0);
    if t < 1.0 {
        context.request_repaint();
    }
    // Ease-out so content settles rather than stops abruptly.
    1.0 - (1.0 - t).powi(3)
}

/// Gentle 0..1 breathing value for in-progress states; asks for ~30 fps repaints while used.
pub fn pulse(context: &egui::Context, period: f32) -> f32 {
    context.request_repaint_after(std::time::Duration::from_millis(33));
    let phase = context.input(|input| input.time) as f32 / period;
    0.5 - 0.5 * (phase * std::f32::consts::TAU).cos()
}

/// Raised card; a non-neutral tone tints it for status and confirmation content.
/// Its contents are drawn with the theme's card palette.
pub struct Card {
    frame: egui::Frame,
}

impl Card {
    pub fn fill(mut self, fill: Color32) -> Self {
        self.frame = self.frame.fill(fill);
        self
    }
    /// Inked themes keep their outline colour; the width never drops below the theme's.
    pub fn stroke(mut self, stroke: egui::Stroke) -> Self {
        let palette = card_pal();
        let color = if palette.ink_outline { palette.border } else { stroke.color };
        self.frame = self.frame.stroke(egui::Stroke::new(stroke.width.max(palette.stroke), color));
        self
    }
    pub fn inner_margin(mut self, margin: impl Into<egui::Margin>) -> Self {
        self.frame = self.frame.inner_margin(margin);
        self
    }
    pub fn show<R>(self, ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui) -> R) -> egui::InnerResponse<R> {
        theme::in_card(|| {
            self.frame.show(ui, |ui| {
                theme::card_visuals(ui);
                add_contents(ui)
            })
        })
    }
}

pub fn card(tone: Tone) -> Card {
    theme::in_card(|| {
        let palette = pal();
        let outline = if palette.ink_outline { palette.border } else { tone.border() };
        Card {
            frame: egui::Frame::new()
                .fill(if tone == Tone::Neutral { palette.card } else { tone.soft() })
                .stroke(egui::Stroke::new(palette.stroke, outline))
                .corner_radius(palette.card_radius)
                .inner_margin(egui::Margin::same(14))
                .shadow(palette.card_shadow),
        }
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The one main action of a view.
    Primary,
    Secondary,
    /// Text-only action that gains a fill on hover.
    Ghost,
}

/// Themed button. A zero `size.x` fits the label; buttons 40 px or taller use the large bold label.
pub fn button(
    ui: &mut egui::Ui,
    kind: Kind,
    icon: Option<Action>,
    label: &str,
    size: Vec2,
) -> egui::Response {
    let font = if size.y >= 40.0 { bold(17.0) } else { egui::FontId::proportional(14.0) };
    let icon_size = (font.size + 3.0).round();
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, Color32::PLACEHOLDER);
    let content = galley.size().x + icon.map_or(0.0, |_| icon_size + 8.0);
    let width = if size.x > 0.0 { size.x } else { content + 28.0 };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, size.y), egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let t = hover(ui, &response);
    let palette = pal();
    let inked = palette.ink_outline && kind != Kind::Ghost;
    let (mut fill, stroke, text) = match kind {
        Kind::Primary => (
            palette.accent.lerp_to_gamma(palette.accent_hover, t),
            if inked { palette.border } else { Color32::TRANSPARENT },
            palette.on_accent,
        ),
        Kind::Secondary => (palette.card.lerp_to_gamma(palette.surface, t), palette.border, palette.on_card),
        Kind::Ghost => (Color32::TRANSPARENT.lerp_to_gamma(palette.surface, t), Color32::TRANSPARENT, palette.link),
    };
    let pressed = response.is_pointer_button_down_on();
    if pressed {
        fill = fill.lerp_to_gamma(palette.text, 0.08);
    }
    let alpha = fade(response.enabled());
    let painter = ui.painter();
    let mut rect = rect;
    // Sticker and pixel buttons sit on a solid shadow and sink onto it while pressed.
    if let Some((offset, shadow)) = palette.button_shadow.filter(|_| kind != Kind::Ghost) {
        let offset = offset.min(Vec2::splat(rect.height() / 10.0));
        rect = rect.translate(-offset);
        if pressed {
            rect = rect.translate(offset);
        } else {
            painter.rect_filled(rect.translate(offset), palette.control_radius, shadow.gamma_multiply(alpha));
        }
    }
    painter.rect(
        rect,
        palette.control_radius,
        fill.gamma_multiply(alpha),
        egui::Stroke::new(if inked { palette.stroke } else { 1.0 }, stroke.gamma_multiply(alpha)),
        egui::StrokeKind::Inside,
    );
    let color = text.gamma_multiply(alpha);
    let mut x = rect.center().x - content / 2.0;
    if let Some(action) = icon {
        icons::draw(
            painter,
            Rect::from_min_size(Pos2::new(x, rect.center().y - icon_size / 2.0), Vec2::splat(icon_size)),
            action,
            color,
            fill,
        );
        x += icon_size + 8.0;
    }
    painter.galley(Pos2::new(x, rect.center().y - galley.size().y / 2.0), galley, color);
    response
}

/// Round icon-only button with a soft hover disc; `label` is the tooltip and accessible name.
pub fn icon_button(ui: &mut egui::Ui, action: Action, label: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(34.0), egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    let t = hover(ui, &response);
    let painter = ui.painter();
    painter.circle_filled(rect.center(), 16.0, pal().surface.gamma_multiply(t));
    let color = if response.enabled() { pal().muted.lerp_to_gamma(pal().text, t) } else { pal().subtle };
    icons::draw(
        painter,
        Rect::from_center_size(rect.center(), Vec2::splat(19.0)),
        action,
        color,
        pal().field.lerp_to_gamma(pal().surface, t),
    );
    response.on_hover_text(label)
}

fn paint_switch(ui: &egui::Ui, rect: Rect, response: &egui::Response, on: bool) {
    let t = ui.ctx().animate_bool_with_time(response.id.with("switch"), on, MOVE_SECONDS);
    let alpha = fade(response.enabled());
    let palette = pal();
    let track = palette.switch_off
        .lerp_to_gamma(palette.accent, t)
        .lerp_to_gamma(palette.text, 0.08 * hover(ui, response));
    let painter = ui.painter();
    let square = palette.control_radius == egui::CornerRadius::ZERO;
    let outline = if palette.ink_outline {
        egui::Stroke::new(palette.stroke.min(2.0), palette.border.gamma_multiply(alpha))
    } else {
        egui::Stroke::NONE
    };
    let rounding = if square { 0.0 } else { rect.height() / 2.0 };
    painter.rect(rect, rounding, track.gamma_multiply(alpha), outline, egui::StrokeKind::Inside);
    let radius = rect.height() / 2.0 - if palette.ink_outline { 4.0 } else { 3.0 };
    let x = egui::lerp(rect.left() + radius + 4.0..=rect.right() - radius - 4.0, t);
    let knob = Color32::WHITE.lerp_to_gamma(palette.card, 0.3).gamma_multiply(alpha);
    if square {
        painter.rect_filled(Rect::from_center_size(Pos2::new(x, rect.center().y), Vec2::splat(radius * 2.0)), 0.0, knob);
    } else {
        painter.circle(Pos2::new(x, rect.center().y), radius, knob, outline);
    }
}

/// Animated on/off switch.
pub fn switch(ui: &mut egui::Ui, value: &mut bool, label: &str) -> egui::Response {
    let (rect, mut response) = ui.allocate_exact_size(Vec2::new(40.0, 24.0), egui::Sense::click());
    if response.clicked() {
        *value = !*value;
        response.mark_changed();
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *value, label)
    });
    paint_switch(ui, rect, &response, *value);
    response
}

/// Switch followed by its label; the whole row toggles.
pub fn toggle(ui: &mut egui::Ui, value: &mut bool, label: &str) -> egui::Response {
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), egui::FontId::proportional(14.0), Color32::PLACEHOLDER);
    let size = Vec2::new(40.0 + 8.0 + galley.size().x, 28.0);
    let (rect, mut response) = ui.allocate_exact_size(size, egui::Sense::click());
    if response.clicked() {
        *value = !*value;
        response.mark_changed();
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *value, label)
    });
    let track = Rect::from_min_size(Pos2::new(rect.left(), rect.center().y - 12.0), Vec2::new(40.0, 24.0));
    paint_switch(ui, track, &response, *value);
    ui.painter().galley(
        Pos2::new(track.right() + 8.0, rect.center().y - galley.size().y / 2.0),
        galley,
        pal().text.gamma_multiply(fade(response.enabled())),
    );
    response
}

/// Settings row: bold title with an optional muted description, switch on the right.
pub fn switch_row(ui: &mut egui::Ui, value: &mut bool, title: &str, description: &str) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let width = (ui.available_width() - 56.0).max(0.0);
        ui.allocate_ui_with_layout(
            Vec2::new(width, 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_min_width(width);
                ui.label(RichText::new(title).font(bold(15.0)));
                if !description.is_empty() {
                    ui.add(egui::Label::new(RichText::new(description).size(12.0).color(pal().muted)).wrap());
                }
            },
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            changed = switch(ui, value, title).changed();
        });
    });
    ui.add_space(8.0);
    changed
}

/// Pill-style single choice; the selection slides between options. Returns true when it changed.
pub fn segmented<T: Copy + PartialEq>(ui: &mut egui::Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    const INSET: f32 = 3.0;
    const PAD: f32 = 14.0;
    let galleys: Vec<_> = options
        .iter()
        .map(|(_, label)| {
            ui.painter()
                .layout_no_wrap((*label).to_owned(), egui::FontId::proportional(14.0), Color32::PLACEHOLDER)
        })
        .collect();
    let widths: Vec<f32> = galleys.iter().map(|galley| galley.size().x + PAD * 2.0).collect();
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(widths.iter().sum::<f32>() + INSET * 2.0, 34.0),
        egui::Sense::hover(),
    );
    let id = ui.id().with(("segmented", options.iter().map(|(_, label)| *label).collect::<Vec<_>>()));
    let enabled = ui.is_enabled();
    let painter = ui.painter().clone();
    painter.rect_filled(rect, pal().control_radius, pal().surface);

    let mut segments = Vec::with_capacity(options.len());
    let mut x = rect.left() + INSET;
    for width in &widths {
        segments.push(Rect::from_min_size(
            Pos2::new(x, rect.top() + INSET),
            Vec2::new(*width, rect.height() - INSET * 2.0),
        ));
        x += width;
    }
    if let Some(selected) = options.iter().position(|(option, _)| option == value) {
        let target = segments[selected];
        let left = ui.ctx().animate_value_with_time(id.with("left"), target.left(), MOVE_SECONDS);
        let width = ui.ctx().animate_value_with_time(id.with("width"), target.width(), MOVE_SECONDS);
        let pill = Rect::from_min_size(Pos2::new(left, target.top()), Vec2::new(width, target.height()));
        painter.add(
            egui::Shadow { offset: [0, 1], blur: 3, spread: 0, color: Color32::from_black_alpha(18) }
                .as_shape(pill, pal().control_radius),
        );
        painter.rect_filled(pill, pal().control_radius, pal().card);
        if pal().ink_outline {
            painter.rect_stroke(pill, pal().control_radius, egui::Stroke::new(1.5, pal().border), egui::StrokeKind::Inside);
        }
    }

    let mut changed = false;
    for (index, ((option, label), galley)) in options.iter().zip(galleys).enumerate() {
        let segment = segments[index];
        let response = ui.interact(segment, id.with(index), egui::Sense::click());
        let selected = option == value;
        response.widget_info(|| {
            egui::WidgetInfo::selected(egui::WidgetType::RadioButton, enabled, selected, *label)
        });
        if response.clicked() && !selected {
            *value = *option;
            changed = true;
        }
        let color = if selected { pal().on_card } else { pal().muted.lerp_to_gamma(pal().text, hover(ui, &response)) };
        painter.galley(
            segment.center() - galley.size() / 2.0,
            galley,
            color.gamma_multiply(fade(enabled)),
        );
    }
    changed
}

/// Titled settings/diagnostics group: a heading above a full-width neutral card.
pub fn section<R>(ui: &mut egui::Ui, title: &str, add_contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.label(RichText::new(title).font(bold(16.0)));
    ui.add_space(6.0);
    let inner = card(Tone::Neutral)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add_contents(ui)
        })
        .inner;
    ui.add_space(18.0);
    inner
}

/// Grid of number tiles (muted label over a bold value), two per row; illustrated themes add the item.
pub fn metrics(ui: &mut egui::Ui, items: &[(Item, &str, String)]) {
    const GAP: f32 = 8.0;
    let width = ((ui.available_width() - GAP) / 2.0).floor();
    for row in items.chunks(2) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            for (item, label, value) in row {
                egui::Frame::new()
                    .fill(pal().surface)
                    .corner_radius(pal().control_radius)
                    .inner_margin(egui::Margin::symmetric(12, 8))
                    .show(ui, |ui| {
                        ui.set_width(width - 24.0);
                        ui.horizontal(|ui| {
                            if theme::illustrated() {
                                art::item(ui, *item, 30.0);
                            }
                            ui.vertical(|ui| {
                                ui.label(RichText::new(*label).size(12.0).color(pal().muted));
                                ui.label(RichText::new(value).font(bold(17.0)));
                            });
                        });
                    });
            }
        });
        ui.add_space(GAP - ui.spacing().item_spacing.y);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Check {
    Pass,
    Info,
    Warn,
    Fail,
}

/// Round status glyph for check lists, drawn with strokes so it needs no symbol font.
pub fn check_icon(ui: &mut egui::Ui, check: Check) -> egui::Response {
    let tone = match check {
        Check::Pass => Tone::Accent,
        Check::Info => Tone::Neutral,
        Check::Warn => Tone::Warn,
        Check::Fail => Tone::Danger,
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(20.0), egui::Sense::hover());
    let painter = ui.painter();
    if theme::illustrated() {
        let item = match check {
            Check::Pass => Item::Heart,
            Check::Info => Item::SoulHeart,
            Check::Warn => Item::HalfHeart,
            Check::Fail => Item::Splat,
        };
        art::paint_item(painter, rect.expand(2.0), item, 1.0);
        return response;
    }
    let c = rect.center();
    painter.circle_filled(c, 10.0, tone.soft());
    let stroke = egui::Stroke::new(1.8, tone.fg());
    let at = |x: f32, y: f32| c + Vec2::new(x, y);
    match check {
        Check::Pass => {
            painter.add(egui::Shape::line(vec![at(-4.5, 0.0), at(-1.5, 3.0), at(4.5, -3.5)], stroke));
        }
        Check::Fail => {
            painter.line_segment([at(-3.5, -3.5), at(3.5, 3.5)], stroke);
            painter.line_segment([at(3.5, -3.5), at(-3.5, 3.5)], stroke);
        }
        Check::Warn => {
            painter.line_segment([at(0.0, -5.0), at(0.0, 1.5)], stroke);
            painter.circle_filled(at(0.0, 4.5), 1.1, tone.fg());
        }
        Check::Info => {
            painter.circle_filled(at(0.0, -4.5), 1.1, tone.fg());
            painter.line_segment([at(0.0, -1.5), at(0.0, 5.0)], stroke);
        }
    }
    response
}

/// Check-list row: status glyph, bold title and an optional wrapped muted detail.
pub fn check_row(ui: &mut egui::Ui, check: Check, title: &str, detail: &str) {
    ui.horizontal_top(|ui| {
        check_icon(ui, check);
        ui.vertical(|ui| {
            ui.label(RichText::new(title).size(14.0));
            if !detail.is_empty() {
                ui.add(egui::Label::new(RichText::new(detail).size(12.0).color(pal().muted)).wrap());
            }
        });
    });
    ui.add_space(4.0);
}

/// Small tinted status pill with a leading dot.
pub fn status_chip(ui: &mut egui::Ui, text: &str, tone: Tone) -> egui::Response {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), egui::FontId::proportional(12.5), tone.fg());
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(galley.size().x + 30.0, 24.0),
        egui::Sense::hover(),
    );
    let painter = ui.painter();
    let square = pal().control_radius == egui::CornerRadius::ZERO;
    let outline = if pal().ink_outline { egui::Stroke::new(1.5, tone.fg()) } else { egui::Stroke::NONE };
    painter.rect(rect, if square { 0 } else { 12 }, tone.soft(), outline, egui::StrokeKind::Inside);
    painter.circle_filled(Pos2::new(rect.left() + 12.0, rect.center().y), 3.5, tone.fg());
    painter.galley(
        Pos2::new(rect.left() + 20.0, rect.center().y - galley.size().y / 2.0),
        galley,
        tone.fg(),
    );
    response
}

/// Member initial in a tinted circle, or a trinket in illustrated themes; `seed` keeps each choice stable.
pub fn avatar(ui: &mut egui::Ui, name: &str, seed: u64) -> egui::Response {
    const TRINKETS: [Item; 6] = [Item::Tooth, Item::Nail, Item::Pin, Item::Key, Item::Coin, Item::Rune];
    if theme::illustrated() {
        return art::item(ui, TRINKETS[(seed % TRINKETS.len() as u64) as usize], 32.0);
    }
    const TONES: [Tone; 4] = [Tone::Accent, Tone::Warn, Tone::Info, Tone::Neutral];
    let tone = TONES[(seed % TONES.len() as u64) as usize];
    let initial: String = name.chars().next().into_iter().flat_map(char::to_uppercase).collect();
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(32.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.circle_filled(rect.center(), 16.0, tone.soft());
    let ink = if tone == Tone::Neutral { pal().text } else { tone.fg() };
    painter.text(rect.center(), egui::Align2::CENTER_CENTER, initial, bold(14.0), ink);
    response
}

/// Trend line of recent samples; a flat series sits mid-height instead of hugging an edge.
pub fn sparkline(ui: &mut egui::Ui, samples: &[u64], size: Vec2, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    if samples.len() < 2 {
        return;
    }
    let min = samples.iter().copied().min().unwrap_or_default() as f32;
    let max = samples.iter().copied().max().unwrap_or_default() as f32;
    let range = (max - min).max(10.0);
    let middle = (max + min) / 2.0;
    let last = (samples.len() - 1) as f32;
    let points = samples
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let level = 0.5 + (*value as f32 - middle) / range * 0.8;
            Pos2::new(
                rect.left() + rect.width() * index as f32 / last,
                rect.bottom() - rect.height() * level,
            )
        })
        .collect();
    ui.painter().add(egui::Shape::line(points, egui::Stroke::new(1.5, color)));
}

/// Inline message on a tinted surface. Returns true when its dismiss button was clicked.
pub fn banner(ui: &mut egui::Ui, tone: Tone, text: &str, dismissible: bool) -> bool {
    let mut dismissed = false;
    let color = if tone == Tone::Neutral { pal().text } else { tone.fg() };
    egui::Frame::new()
        .fill(tone.soft())
        .corner_radius(pal().control_radius)
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let message = egui::Label::new(RichText::new(text).size(13.0).color(color)).wrap();
            if !dismissible {
                ui.add(message);
                return;
            }
            // Top-aligned row: a centred row would grow the message downward from the midline.
            ui.horizontal_top(|ui| {
                let width = (ui.available_width() - 28.0).max(0.0);
                ui.allocate_ui_with_layout(
                    Vec2::new(width, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_min_width(width);
                        ui.add_space(2.0);
                        ui.add(message);
                    },
                );
                dismissed = ui
                    .add(egui::Button::new(RichText::new("×").size(15.0).color(pal().muted)).frame(false))
                    .on_hover_text(netburrow_core::text!("关闭提示", "Dismiss"))
                    .clicked();
            });
        });
    dismissed
}

/// Connection quality as three hearts, for illustrated themes.
pub fn hearts(ui: &mut egui::Ui, quality: netburrow_core::Quality) {
    use netburrow_core::Quality;
    let row = match quality {
        Quality::Pending => [Item::EmptyHeart; 3],
        Quality::Normal => [Item::Heart; 3],
        Quality::Unstable => [Item::Heart, Item::HalfHeart, Item::EmptyHeart],
        Quality::Abnormal => [Item::HalfHeart, Item::EmptyHeart, Item::EmptyHeart],
    };
    ui.spacing_mut().item_spacing.x = 1.0;
    for item in row {
        art::item(ui, item, 18.0);
    }
}
