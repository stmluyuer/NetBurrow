use eframe::egui::{self, Pos2, Rect, Stroke, Vec2};

pub fn window_icon() -> egui::IconData {
    egui::IconData {
        rgba: include_bytes!("../assets/netburrow.rgba").to_vec(),
        width: 64,
        height: 64,
    }
}

pub fn logo(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(60.0, 54.0), egui::Sense::hover());
    let p = ui.painter();
    let at = |x, y| rect.min + Vec2::new(x, y);
    let ink = super::TEXT;
    let stroke = Stroke::new(1.8, ink);
    // Slightly irregular double arch, stone marks and hatching echo the reference's ink drawing.
    let arch = |left: f32, right: f32, top: f32, floor: f32| {
        let center = (left + right) * 0.5;
        let radius = (right - left) * 0.5;
        let spring = top + radius;
        let mut points = vec![at(left - 1.0, floor), at(left, spring)];
        points.extend((0..=24).map(|i| {
            let angle = std::f32::consts::PI * i as f32 / 24.0;
            at(center - radius * angle.cos(), spring - radius * angle.sin())
        }));
        points.push(at(right + 0.6, floor));
        points
    };
    p.add(egui::Shape::line(arch(10.0, 47.0, 5.0, 45.0), stroke));
    p.add(egui::Shape::line(
        arch(15.0, 43.0, 9.0, 44.0),
        Stroke::new(1.0, ink),
    ));
    p.add(egui::Shape::convex_polygon(
        arch(23.0, 38.0, 20.0, 44.0),
        ink,
        Stroke::NONE,
    ));
    for i in 0..6 {
        let y = 30.0 + i as f32 * 2.2;
        p.line_segment(
            [at(25.0, y + 3.0), at(35.0, y - 3.0)],
            Stroke::new(0.6, super::BACKGROUND),
        );
    }
    for (x, y) in [
        (14.0, 25.0),
        (13.0, 34.0),
        (18.0, 17.0),
        (28.0, 10.0),
        (39.0, 15.0),
        (44.0, 25.0),
        (44.0, 36.0),
    ] {
        p.line_segment([at(x, y), at(x + 2.0, y + 1.2)], Stroke::new(1.3, ink));
    }
    for (a, b) in [
        ((4.0, 44.0), (1.0, 37.0)),
        ((7.0, 45.0), (7.0, 35.0)),
        ((49.0, 44.0), (55.0, 33.0)),
        ((50.0, 44.0), (58.0, 39.0)),
        ((8.0, 47.0), (22.0, 47.0)),
        ((39.0, 47.0), (53.0, 47.0)),
    ] {
        p.line_segment([at(a.0, a.1), at(b.0, b.1)], stroke);
    }
    p.add(egui::Shape::closed_line(
        vec![
            at(26.0, 48.0),
            at(28.0, 44.5),
            at(33.0, 45.0),
            at(36.0, 48.0),
        ],
        stroke,
    ));
}

pub fn members_empty(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(60.0, 36.0), egui::Sense::hover());
    let p = ui.painter();
    let stroke = Stroke::new(1.6, super::MUTED.gamma_multiply(0.65));
    for x in [16.0, 43.0] {
        let head = rect.min + Vec2::new(x, 9.0);
        p.circle_stroke(head, 6.0, stroke);
        p.add(egui::Shape::line(
            (0..=16)
                .map(|i| {
                    let angle = std::f32::consts::PI * i as f32 / 16.0;
                    rect.min + Vec2::new(x - 11.0 * angle.cos(), 32.0 - 12.0 * angle.sin())
                })
                .collect(),
            stroke,
        ));
        p.line_segment(
            [
                rect.min + Vec2::new(x - 11.0, 32.0),
                rect.min + Vec2::new(x + 11.0, 32.0),
            ],
            stroke,
        );
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

pub fn icon_button(ui: &mut egui::Ui, action: Action, label: &str) -> egui::Response {
    let response = ui
        .add_sized([36.0, 36.0], egui::Button::new("").frame(false))
        .on_hover_text(label);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    draw(
        ui.painter(),
        Rect::from_center_size(response.rect.center(), Vec2::splat(21.0)),
        action,
        ui.style().interact(&response).fg_stroke.color,
        super::BACKGROUND,
    );
    response
}

pub fn inline(ui: &mut egui::Ui, action: Action) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(22.0), egui::Sense::hover());
    draw(ui.painter(), rect, action, super::TEXT, super::BACKGROUND);
}

pub fn button(ui: &mut egui::Ui, action: Action, label: &str) -> egui::Response {
    let response = ui.button(format!("     {label}"));
    let rect = Rect::from_min_size(
        Pos2::new(response.rect.min.x + 10.0, response.rect.center().y - 7.0),
        Vec2::splat(14.0),
    );
    draw(
        ui.painter(),
        rect,
        action,
        ui.style().interact(&response).fg_stroke.color,
        ui.style().interact(&response).bg_fill,
    );
    response
}

pub fn draw(
    p: &egui::Painter,
    rect: Rect,
    action: Action,
    color: egui::Color32,
    background: egui::Color32,
) {
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
