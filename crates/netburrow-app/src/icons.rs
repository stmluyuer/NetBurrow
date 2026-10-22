use eframe::egui::{self, Rect, Stroke, Vec2};

pub fn window_icon() -> egui::IconData {
    egui::IconData {
        rgba: include_bytes!("../assets/netburrow.rgba").to_vec(),
        width: 64,
        height: 64,
    }
}

#[derive(Clone, Copy)]
pub enum Action {
    Copy,
    Paste,
    Search,
    Log,
    Folder,
    Settings,
    Power,
    Server,
    User,
    Down,
}

pub fn inline(ui: &mut egui::Ui, action: Action) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(22.0), egui::Sense::hover());
    draw(ui.painter(), rect, action, super::pal().text, super::pal().background);
}

pub fn draw(
    p: &egui::Painter,
    rect: Rect,
    action: Action,
    color: egui::Color32,
    background: egui::Color32,
) {
    // Illustrated themes draw the matching game item; copy, paste and arrows stay as line icons.
    let item = match action {
        Action::Settings => Some(super::art::Item::Gear),
        Action::Power => Some(super::art::Item::Power),
        Action::Search => Some(super::art::Item::Lens),
        Action::Log => Some(super::art::Item::Scroll),
        Action::Folder => Some(super::art::Item::Chest),
        Action::Server => Some(super::art::Item::Rune),
        Action::User => Some(super::art::Item::CardFace),
        Action::Copy | Action::Paste | Action::Down => None,
    };
    if let Some(item) = item.filter(|_| super::theme::illustrated()) {
        super::art::paint_item(p, rect.expand(rect.width() * 0.15), item, color.a() as f32 / 255.0);
        return;
    }
    let scale = rect.width() / 14.0;
    let stroke = Stroke::new(1.25 * scale, color);
    let at = |x, y| rect.min + Vec2::new(x, y) * scale;
    let line = |a: (f32, f32), b: (f32, f32)| {
        p.line_segment([at(a.0, a.1), at(b.0, b.1)], stroke);
    };
    let box_at = |x, y, w, h| {
        p.rect_stroke(
            Rect::from_min_size(at(x, y), Vec2::new(w, h) * scale),
            1,
            stroke,
            egui::StrokeKind::Inside,
        );
    };
    match action {
        Action::Copy => {
            box_at(1.0, 1.0, 8.0, 9.0);
            p.rect_filled(
                Rect::from_min_size(at(5.0, 4.0), Vec2::new(8.0, 9.0) * scale),
                1,
                background,
            );
            box_at(5.0, 4.0, 8.0, 9.0);
        }
        Action::Paste => {
            box_at(2.0, 3.0, 10.0, 10.0);
            box_at(5.0, 1.0, 4.0, 4.0);
        }
        Action::Search => {
            p.circle_stroke(at(6.0, 6.0), 4.0 * scale, stroke);
            line((9.0, 9.0), (13.0, 13.0));
        }
        Action::Log => {
            box_at(2.0, 1.0, 10.0, 12.0);
            for y in [4.0, 7.0, 10.0] {
                line((5.0, y), (9.0, y));
            }
        }
        Action::Folder => {
            p.add(egui::Shape::closed_line(
                vec![
                    at(1.0, 12.0),
                    at(1.0, 2.0),
                    at(6.0, 2.0),
                    at(8.0, 4.0),
                    at(13.0, 4.0),
                    at(13.0, 12.0),
                ],
                stroke,
            ));
        }
        Action::Settings => {
            p.add(egui::Shape::closed_line(
                (0..48)
                    .map(|i| {
                        let angle = i as f32 * std::f32::consts::TAU / 48.0;
                        let radius = if i % 6 < 3 { 6.2 } else { 4.9 };
                        at(7.0 + radius * angle.cos(), 7.0 + radius * angle.sin())
                    })
                    .collect(),
                stroke,
            ));
            p.circle_stroke(at(7.0, 7.0), 2.1 * scale, stroke);
        }
        Action::Power => {
            line((7.0, 0.5), (7.0, 7.0));
            p.add(egui::Shape::line(
                (0..=28)
                    .map(|i| {
                        let angle = -0.95 + i as f32 * (std::f32::consts::TAU - 1.24) / 28.0;
                        at(7.0 + 5.5 * angle.cos(), 7.5 + 5.5 * angle.sin())
                    })
                    .collect(),
                stroke,
            ));
        }
        Action::Server => {
            for y in [1.0, 5.5, 10.0] {
                box_at(1.5, y, 11.0, 3.0);
                p.circle_filled(at(10.5, y + 1.5), 0.55 * scale, color);
            }
        }
        Action::User => {
            p.circle_stroke(at(7.0, 3.0), 2.5 * scale, stroke);
            p.add(egui::Shape::closed_line(
                vec![
                    at(2.0, 13.0),
                    at(2.5, 9.0),
                    at(5.0, 7.5),
                    at(9.0, 7.5),
                    at(11.5, 9.0),
                    at(12.0, 13.0),
                ],
                stroke,
            ));
        }
        Action::Down => {
            line((2.0, 5.0), (7.0, 10.0));
            line((7.0, 10.0), (12.0, 5.0));
        }
    }
}
