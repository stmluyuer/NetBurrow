//! Line art produced by scripts/convert-art.ps1: "NBM1", u16 LE width/height, raw DEFLATE alpha.
//! Uploaded once as white + alpha so every state can tint the same texture.
use std::io::Read;

use eframe::egui::{self, Color32, Vec2};

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

fn decode(bytes: &[u8]) -> Result<egui::ColorImage, String> {
    let header = bytes
        .get(..8)
        .filter(|header| header.starts_with(b"NBM1"))
        .ok_or("invalid art header")?;
    let width = u16::from_le_bytes([header[4], header[5]]) as usize;
    let height = u16::from_le_bytes([header[6], header[7]]) as usize;
    let mut alpha = Vec::with_capacity(width * height);
    flate2::read::DeflateDecoder::new(&bytes[8..])
        .take(width as u64 * height as u64 + 1)
        .read_to_end(&mut alpha)
        .map_err(|error| error.to_string())?;
    if alpha.len() != width * height {
        return Err(format!("expected {}x{} alpha, got {} bytes", width, height, alpha.len()));
    }
    Ok(egui::ColorImage::new(
        [width, height],
        alpha.into_iter().map(Color32::from_white_alpha).collect(),
    ))
}

fn texture(context: &egui::Context, art: Art) -> Option<egui::TextureHandle> {
    let id = egui::Id::new(("netburrow-art", art));
    if let Some(cached) = context.data(|data| data.get_temp::<Option<egui::TextureHandle>>(id)) {
        return cached;
    }
    let handle = match decode(art.bytes()) {
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
    };
    context.data_mut(|data| data.insert_temp(id, handle.clone()));
    handle
}

/// Draws `art` fitted and centred in `size`; the space is reserved even if the asset fails to load.
pub fn show(ui: &mut egui::Ui, art: Art, size: Vec2, tint: Color32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
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
