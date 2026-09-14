//! Network-free renderer captures using the existing smoke-test mode. Never reads user settings.
use super::*;

impl NetBurrowApp {
    pub(super) fn preview_tick(&mut self, context: &egui::Context) {
        let Some(started) = self.smoke_test else {
            return;
        };
        if !self.view.preview_requested && started.elapsed() >= Duration::from_millis(600) {
            self.view.preview_requested = true;
            if let Some(page) = std::env::args()
                .find_map(|arg| arg.strip_prefix("--preview-page=").map(str::to_owned))
            {
                self.settings.server = "relay.example.com:24872".into();
                self.settings.display_name = "玩家一".into();
                self.settings.group = format!("NB1-{}", "1234abcd".repeat(8));
                self.settings.recent_connections = vec![netburrow_core::RecentConnection {
                    server: self.settings.server.clone(),
                    group: self.settings.group.clone(),
                }];
                match page.as_str() {
                    "game" | "general" | "about" => {
                        self.open_settings();
                        self.view.settings_tab = match page.as_str() {
                            "general" => SettingsTab::General,
                            "about" => SettingsTab::About,
                            _ => SettingsTab::Game,
                        };
                    }
                    "checks" | "logs" => {
                        self.view.page = Page::Diagnostics;
                        self.view.diagnostic_tab = if page == "logs" {
                            DiagnosticTab::Logs
                        } else {
                            DiagnosticTab::Checks
                        };
                    }
                    "recent" => {
                        self.view.history_open = true;
                    }
                    _ => {}
                }
            }
        }
        if !self.view.capture_requested && started.elapsed() >= Duration::from_millis(1400) {
            self.view.capture_requested = true;
            if std::env::args().any(|arg| arg.starts_with("--preview-out=")) {
                context.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
            }
        }
        let output =
            std::env::args().find_map(|arg| arg.strip_prefix("--preview-out=").map(str::to_owned));
        if let Some(path) = output {
            context.input(|input| {
                for event in &input.events {
                    if let egui::Event::Screenshot { image, .. } = event {
                        if let Err(error) = save_bmp(Path::new(&path), image) {
                            eprintln!("Preview capture failed: {error}");
                        }
                    }
                }
            });
        }
    }
}
fn save_bmp(path: &Path, image: &egui::ColorImage) -> std::io::Result<()> {
    let [width, height] = image.size;
    let stride = (width * 3 + 3) & !3;
    let mut bytes = vec![0u8; 54 + stride * height];
    bytes[0..2].copy_from_slice(b"BM");
    let size = bytes.len() as u32;
    bytes[2..6].copy_from_slice(&size.to_le_bytes());
    bytes[10..14].copy_from_slice(&54u32.to_le_bytes());
    bytes[14..18].copy_from_slice(&40u32.to_le_bytes());
    bytes[18..22].copy_from_slice(&(width as i32).to_le_bytes());
    bytes[22..26].copy_from_slice(&(height as i32).to_le_bytes());
    bytes[26..28].copy_from_slice(&1u16.to_le_bytes());
    bytes[28..30].copy_from_slice(&24u16.to_le_bytes());
    for y in 0..height {
        for x in 0..width {
            let color = image.pixels[y * width + x];
            let offset = 54 + (height - 1 - y) * stride + x * 3;
            bytes[offset..offset + 3].copy_from_slice(&[color.b(), color.g(), color.r()]);
        }
    }
    std::fs::write(path, bytes)
}
