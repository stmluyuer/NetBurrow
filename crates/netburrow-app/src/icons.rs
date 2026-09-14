use eframe::egui::{self, Pos2, Rect, Stroke, Vec2};

pub fn window_icon() -> egui::IconData {
    egui::IconData {
        rgba: include_bytes!("../assets/netburrow.rgba").to_vec(),
        width: 64,
        height: 64,
    }
}

pub fn logo(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(48.0), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 12, super::ACCENT);
    let point = |x, y| rect.min + Vec2::new(x, y);
    let stroke = Stroke::new(5.0, super::BACKGROUND);
    p.line_segment([point(14.0, 33.0), point(14.0, 23.0)], stroke);
    p.add(egui::Shape::line(
        (0..=24)
            .map(|i| {
                let angle = std::f32::consts::PI * i as f32 / 24.0;
                point(24.0 - 10.0 * angle.cos(), 23.0 - 10.0 * angle.sin())
            })
            .collect(),
        stroke,
    ));
    p.line_segment([point(34.0, 23.0), point(34.0, 33.0)], stroke);
    p.circle_filled(point(14.0, 34.0), 4.0, super::BACKGROUND);
    p.circle_filled(point(34.0, 34.0), 4.0, super::BACKGROUND);
}

#[derive(Clone, Copy)]
pub enum Action {
    Add,
    Copy,
    Paste,
    Search,
    Save,
    Log,
    Folder,
}

pub fn button(ui: &mut egui::Ui, action: Action, label: &str) -> egui::Response {
    let response = ui.button(format!("     {label}"));
    let rect = Rect::from_min_size(
        Pos2::new(response.rect.min.x + 10.0, response.rect.center().y - 7.0),
        Vec2::splat(14.0),
    );
    let p = ui.painter();
    let stroke = Stroke::new(1.4, ui.style().interact(&response).fg_stroke.color);
    let at = |x, y| rect.min + Vec2::new(x, y);
    let line = |a: (f32, f32), b: (f32, f32)| {
        p.line_segment([at(a.0, a.1), at(b.0, b.1)], stroke);
    };
    let box_at = |x, y, w, h| {
        p.rect_stroke(
            Rect::from_min_size(at(x, y), Vec2::new(w, h)),
            1,
            stroke,
            egui::StrokeKind::Inside,
        );
    };
    match action {
        Action::Add => {
            line((7.0, 2.0), (7.0, 12.0));
            line((2.0, 7.0), (12.0, 7.0));
        }
        Action::Copy => {
            box_at(1.0, 1.0, 8.0, 9.0);
            p.rect_filled(
                Rect::from_min_size(at(5.0, 4.0), Vec2::new(8.0, 9.0)),
                1,
                ui.style().interact(&response).bg_fill,
            );
            box_at(5.0, 4.0, 8.0, 9.0);
        }
        Action::Paste => {
            box_at(2.0, 3.0, 10.0, 10.0);
            box_at(5.0, 1.0, 4.0, 4.0);
        }
        Action::Search => {
            p.circle_stroke(at(6.0, 6.0), 4.0, stroke);
            line((9.0, 9.0), (13.0, 13.0));
        }
        Action::Save => {
            box_at(1.0, 1.0, 12.0, 12.0);
            box_at(4.0, 1.0, 6.0, 4.0);
            box_at(4.0, 8.0, 6.0, 5.0);
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
    }
    response
}
