//! Line art produced by scripts/convert-art.ps1: "NBM1", u16 LE width/height, raw DEFLATE alpha.
//! Uploaded once as white + alpha so every state can tint the same texture.
//! Illustrated themes use colour atlases from scripts/convert-theme-art.ps1 instead (see `decode_palette`).
use std::io::Read;

use eframe::egui::{self, Color32, Pos2, Rect, Vec2};

use super::theme::{self, Theme};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Art {
    Logo,
    EmptyMembers,
    EmptyCheck,
    Idle,
    Connecting,
    Waiting,
    Connected,
    Error,
}

impl Art {
    fn bytes(self) -> &'static [u8] {
        match self {
            Art::Logo => include_bytes!("../assets/art/logo.mask"),
            Art::EmptyMembers => include_bytes!("../assets/art/empty-members.mask"),
            Art::EmptyCheck => include_bytes!("../assets/art/empty-check.mask"),
            Art::Idle => include_bytes!("../assets/art/state-idle.mask"),
            Art::Connecting => include_bytes!("../assets/art/state-connecting.mask"),
            Art::Waiting => include_bytes!("../assets/art/state-waiting.mask"),
            Art::Connected => include_bytes!("../assets/art/state-connected.mask"),
            Art::Error => include_bytes!("../assets/art/state-error.mask"),
        }
    }
}

/// Game pickups in the order of each theme's 6x4 items atlas.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Item {
    Heart,
    HalfHeart,
    EmptyHeart,
    SoulHeart,
    Drop,
    Splat,
    PillRed,
    PillBlue,
    PillGreen,
    CardBack,
    CardFace,
    Rune,
    Key,
    Bomb,
    Coin,
    Chest,
    Pedestal,
    Nail,
    Tooth,
    Pin,
    Gear,
    Lens,
    Scroll,
    Power,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Sheet {
    States,
    Items,
    Tile,
}

impl Sheet {
    fn grid(self) -> (usize, usize) {
        match self {
            Sheet::States => (3, 2),
            Sheet::Items => (6, 4),
            Sheet::Tile => (1, 1),
        }
    }
}

fn sheet_bytes(theme: Theme, sheet: Sheet) -> Option<&'static [u8]> {
    Some(match (theme, sheet) {
        (Theme::Paper, Sheet::States) => include_bytes!("../assets/themes/paper/states.nbp"),
        (Theme::Paper, Sheet::Items) => include_bytes!("../assets/themes/paper/items.nbp"),
        (Theme::Paper, Sheet::Tile) => include_bytes!("../assets/themes/paper/tile.nbp"),
        (Theme::Pixel, Sheet::States) => include_bytes!("../assets/themes/pixel/states.nbp"),
        (Theme::Pixel, Sheet::Items) => include_bytes!("../assets/themes/pixel/items.nbp"),
        (Theme::Pixel, Sheet::Tile) => include_bytes!("../assets/themes/pixel/tile.nbp"),
        (Theme::Cartoon, Sheet::States) => include_bytes!("../assets/themes/cartoon/states.nbp"),
        (Theme::Cartoon, Sheet::Items) => include_bytes!("../assets/themes/cartoon/items.nbp"),
        _ => return None,
    })
}

fn inflate(bytes: &[u8], expected: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(expected);
    flate2::read::DeflateDecoder::new(bytes)
        .take(expected as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|error| error.to_string())?;
    if out.len() != expected {
        return Err(format!("expected {expected} bytes, got {}", out.len()));
    }
    Ok(out)
}

fn decode(bytes: &[u8]) -> Result<egui::ColorImage, String> {
    let header = bytes
        .get(..8)
        .filter(|header| header.starts_with(b"NBM1"))
        .ok_or("invalid art header")?;
    let width = u16::from_le_bytes([header[4], header[5]]) as usize;
    let height = u16::from_le_bytes([header[6], header[7]]) as usize;
    let alpha = inflate(&bytes[8..], width * height)?;
    Ok(egui::ColorImage::new(
        [width, height],
        alpha.into_iter().map(Color32::from_white_alpha).collect(),
    ))
}

/// "NBP1", u16 LE width/height/palette size, RGBA palette, raw DEFLATE of one index byte per pixel.
fn decode_palette(bytes: &[u8]) -> Result<egui::ColorImage, String> {
    let header = bytes
        .get(..10)
        .filter(|header| header.starts_with(b"NBP1"))
        .ok_or("invalid theme art header")?;
    let width = u16::from_le_bytes([header[4], header[5]]) as usize;
    let height = u16::from_le_bytes([header[6], header[7]]) as usize;
    let entries = u16::from_le_bytes([header[8], header[9]]) as usize;
    let palette = bytes.get(10..10 + entries * 4).ok_or("truncated palette")?;
    let indices = inflate(&bytes[10 + entries * 4..], width * height)?;
    let mut rgba = Vec::with_capacity(width * height * 4);
    for index in indices {
        let entry = palette.get(index as usize * 4..index as usize * 4 + 4).ok_or("palette index out of range")?;
        rgba.extend_from_slice(if index == 0 { &[0, 0, 0, 0] } else { entry });
    }
    Ok(egui::ColorImage::from_rgba_unmultiplied([width, height], &rgba))
}

fn cached(context: &egui::Context, id: egui::Id, load: impl FnOnce() -> Option<egui::TextureHandle>) -> Option<egui::TextureHandle> {
    if let Some(cached) = context.data(|data| data.get_temp::<Option<egui::TextureHandle>>(id)) {
        return cached;
    }
    let handle = load();
    context.data_mut(|data| data.insert_temp(id, handle.clone()));
    handle
}

fn texture(context: &egui::Context, art: Art) -> Option<egui::TextureHandle> {
    cached(context, egui::Id::new(("netburrow-art", art)), || match decode(art.bytes()) {
        Ok(image) => Some(context.load_texture(
            format!("art-{art:?}"),
            image,
            // Mipmaps keep thin ink lines smooth when the art is drawn well below its source size.
            egui::TextureOptions {
                mipmap_mode: Some(egui::TextureFilter::Linear),
                ..egui::TextureOptions::LINEAR
            },
        )),
        Err(error) => {
            netburrow_core::diagnostics::record("WARN", "art", &format!("{art:?}: {error}"));
            None
        }
    })
}

fn sheet(context: &egui::Context, sheet: Sheet) -> Option<egui::TextureHandle> {
    let theme = theme::current();
    let bytes = sheet_bytes(theme, sheet)?;
    cached(context, egui::Id::new(("netburrow-theme-art", theme as u8, sheet)), || match decode_palette(bytes) {
        Ok(image) => {
            let wrap_mode = if sheet == Sheet::Tile { egui::TextureWrapMode::Repeat } else { egui::TextureWrapMode::ClampToEdge };
            // Pixel art stays crisp; painted themes need mipmaps to shrink smoothly.
            let options = if theme == Theme::Pixel {
                egui::TextureOptions { wrap_mode, ..egui::TextureOptions::NEAREST }
            } else {
                egui::TextureOptions { wrap_mode, mipmap_mode: Some(egui::TextureFilter::Linear), ..egui::TextureOptions::LINEAR }
            };
            Some(context.load_texture(format!("theme-{theme:?}-{sheet:?}"), image, options))
        }
        Err(error) => {
            netburrow_core::diagnostics::record("WARN", "art", &format!("{theme:?} {sheet:?}: {error}"));
            None
        }
    })
}

fn cell_uv(sheet: Sheet, index: usize) -> Rect {
    let (columns, rows) = sheet.grid();
    let (column, row) = ((index % columns) as f32, (index / columns) as f32);
    let (w, h) = (1.0 / columns as f32, 1.0 / rows as f32);
    Rect::from_min_size(Pos2::new(column * w, row * h), Vec2::new(w, h))
}

fn paint_cell(painter: &egui::Painter, sheet_kind: Sheet, index: usize, rect: Rect, alpha: f32) {
    if let Some(texture) = sheet(painter.ctx(), sheet_kind) {
        painter.image(texture.id(), rect, cell_uv(sheet_kind, index), Color32::WHITE.gamma_multiply(alpha));
    }
}

/// Paints a game item square in `rect`; `alpha` fades it like disabled text.
pub fn paint_item(painter: &egui::Painter, rect: Rect, item: Item, alpha: f32) {
    let side = rect.width().min(rect.height());
    paint_cell(painter, Sheet::Items, item as usize, Rect::from_center_size(rect.center(), Vec2::splat(side)), alpha);
}

pub fn item(ui: &mut egui::Ui, item: Item, size: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        paint_item(ui.painter(), rect, item, if ui.is_enabled() { 1.0 } else { 0.45 });
    }
    response
}

/// Draws `art` fitted and centred in `size`; the space is reserved even if the asset fails to load.
/// Illustrated themes draw their colour picture and only use the tint's opacity.
pub fn show(ui: &mut egui::Ui, art: Art, size: Vec2, tint: Color32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    if theme::illustrated() {
        let side = size.x.min(size.y);
        let square = Rect::from_center_size(rect.center(), Vec2::splat(side));
        let alpha = tint.a() as f32 / 255.0;
        match art {
            Art::Idle => paint_cell(ui.painter(), Sheet::States, 0, square, alpha),
            Art::Connecting => paint_cell(ui.painter(), Sheet::States, 1, square, alpha),
            Art::Waiting => paint_cell(ui.painter(), Sheet::States, 2, square, alpha),
            Art::Connected => paint_cell(ui.painter(), Sheet::States, 3, square, alpha),
            Art::Error => paint_cell(ui.painter(), Sheet::States, 4, square, alpha),
            Art::Logo => paint_cell(ui.painter(), Sheet::States, 5, square, alpha),
            Art::EmptyMembers => paint_item(ui.painter(), square, Item::Pedestal, alpha),
            Art::EmptyCheck => paint_item(ui.painter(), square, Item::Lens, alpha),
        }
        return response;
    }
    if let Some(texture) = texture(ui.ctx(), art) {
        let source = texture.size_vec2();
        let scale = (size.x / source.x).min(size.y / source.y);
        ui.painter().image(
            texture.id(),
            egui::Rect::from_center_size(rect.center(), source * scale),
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            tint,
        );
    }
    response
}

/// Tiles the theme's floor texture behind a panel. Anchored to the window, so panels line up.
pub fn backdrop(ui: &egui::Ui) {
    let Some(texture) = sheet(ui.ctx(), Sheet::Tile) else { return };
    let rect = ui.clip_rect();
    // Pixel tiles are shown at twice their native size so each art pixel stays a crisp block.
    let span = texture.size_vec2() * if theme::current() == Theme::Pixel { 2.0 } else { 1.0 };
    let uv = Rect::from_min_max(
        Pos2::new(rect.left() / span.x, rect.top() / span.y),
        Pos2::new(rect.right() / span.x, rect.bottom() / span.y),
    );
    ui.painter().image(texture.id(), rect, uv, Color32::WHITE);
}
