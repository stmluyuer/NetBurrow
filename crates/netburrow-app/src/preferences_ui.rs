use super::*;

fn toggle(ui: &mut egui::Ui, value: &mut bool, title: &str, description: &str) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let width = ui.available_width() - 80.0;
        ui.allocate_ui_with_layout(
            Vec2::new(width, 48.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_min_width(width);
                ui.label(RichText::new(title).font(bold(15.0)));
                ui.label(RichText::new(description).size(12.0).color(MUTED));
            },
        );
        let response = ui.add_sized([64.0, 32.0], egui::Button::new("").frame(false));
        let track = egui::Rect::from_center_size(
            response.rect.center() - Vec2::new(11.0, 0.0),
            Vec2::new(38.0, 22.0),
        );
        ui.painter().rect_filled(
            track,
            11,
            if *value {
                ACCENT
            } else {
                Color32::from_rgb(186, 180, 170)
            },
        );
        let center = egui::Pos2::new(
            if *value {
                track.right() - 11.0
            } else {
                track.left() + 11.0
            },
            track.center().y,
        );
        ui.painter().circle_filled(center, 8.0, Color32::WHITE);
        ui.painter().text(
            response.rect.right_center(),
            egui::Align2::RIGHT_CENTER,
            if *value { "开" } else { "关" },
            egui::FontId::proportional(12.0),
            MUTED,
        );
        response.widget_info(|| {
            egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *value, title)
        });
        if response.clicked() {
            *value = !*value;
            changed = true;
        }
    });
    ui.add_space(6.0);
    changed
}

impl NetBurrowApp {
    pub(super) fn game_settings_ui(&mut self, ui: &mut egui::Ui) {
        let enabled = self.connection_editable();
        if !enabled {
            ui.small("停止联机后可修改游戏与网络设置");
        }
        let draft = self
            .view
            .game_draft
            .get_or_insert_with(|| self.settings.clone());
        ui.add_enabled_ui(enabled, |ui| {
            ui.label(RichText::new("游戏").font(bold(18.0)));
            ui.add_space(8.0);
            ui.label("游戏位置");
            ui.add_sized(
                [ui.available_width(), 36.0],
                egui::TextEdit::singleline(&mut draft.game_path).hint_text("选择 isaac-ng.exe"),
            );
            ui.horizontal(|ui| {
                if ui.button("选择文件").clicked() {
                    match file_picker::choose_game() {
                        Ok(Some(path)) => draft.game_path = path,
                        Ok(None) => {}
                        Err(error) => self.notice = Some(error),
                    }
                }
                if icons::button(ui, icons::Action::Search, "自动检测").clicked() {
                    match netburrow_core::autodetect_game() {
                        Some(path) => draft.game_path = path.display().to_string(),
                        None => {
                            self.notice = Some("未找到游戏，请选择实际的 isaac-ng.exe。".into())
                        }
                    }
                }
            });
            // Local path/name feedback, not a compatibility assertion or repeated disk scan.
            let path = Path::new(draft.game_path.trim());
            let valid_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.eq_ignore_ascii_case("isaac-ng.exe"));
            ui.label(
                RichText::new(if draft.game_path.trim().is_empty() {
                    "尚未设置游戏位置"
                } else if !valid_name {
                    "请选择 isaac-ng.exe"
                } else if !path.is_file() {
                    "文件不存在，请重新选择"
                } else {
                    "已找到 isaac-ng.exe；接入兼容性由启用前检查确认"
                })
                .size(12.0)
                .color(MUTED),
            );
            ui.add_space(20.0);
            ui.separator();
            ui.label(RichText::new("网络").font(bold(18.0)));
            ui.add_space(8.0);
            ui.label("传输方式");
            ui.horizontal(|ui| {
                ui.radio_value(&mut draft.transport, Transport::Tcp, "TCP（推荐）");
                ui.radio_value(&mut draft.transport, Transport::Udp, "UDP 优先");
            });
            ui.small("UDP 优先：可靠消息仍通过 TCP 发送。");
            ui.add_space(20.0);
            ui.collapsing("高级选项", |ui| {
                toggle(
                    ui,
                    &mut draft.allow_late_hook,
                    "允许接入已运行的游戏（实验性）",
                    "默认关闭，仅接入启用工具后启动的游戏",
                );
                if draft.allow_late_hook {
                    ui.label(
                        RichText::new(
                            "仅在主菜单、尚未联机时使用；已有回调可能无法接管，失败后需重启游戏。",
                        )
                        .size(12.0)
                        .color(Color32::from_rgb(151, 103, 37)),
                    );
                }
            });
        });
        ui.add_space(14.0);
        ui.small("更改后点击底部“应用更改”；未应用的修改不会保存。");
    }
    pub(super) fn preferences_content(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("启动").font(bold(18.0)));
        ui.small("通用偏好立即保存");
        ui.add_space(10.0);
        if toggle(
            ui,
            &mut self.startup_enabled,
            "登录 Windows 时启动",
            "只打开工具，不自动启用联机",
        ) && self.smoke_test.is_none()
        {
            match startup::set_enabled(self.startup_enabled) {
                Ok(()) => self.notice = Some("启动偏好已保存。".into()),
                Err(error) => {
                    self.startup_enabled = !self.startup_enabled;
                    self.notice = Some(error);
                }
            }
        }
        if toggle(
            ui,
            &mut self.settings.start_minimized,
            "启动后最小化",
            "与登录启动独立，下次启动生效",
        ) && self.smoke_test.is_none()
        {
            match netburrow_core::save_start_minimized(self.settings.start_minimized) {
                Ok(()) => self.notice = Some("已保存，下次启动生效。".into()),
                Err(error) => {
                    self.settings.start_minimized = !self.settings.start_minimized;
                    self.notice = Some(error);
                }
            }
        }
        ui.add_space(14.0);
        ui.separator();
        ui.label(RichText::new("窗口").font(bold(18.0)));
        ui.label("关闭窗口时");
        let before = self.settings.minimize_on_close;
        ui.horizontal(|ui| {
            ui.radio_value(&mut self.settings.minimize_on_close, false, "退出工具");
            ui.radio_value(&mut self.settings.minimize_on_close, true, "最小化");
        });
        ui.small(if self.settings.minimize_on_close {
            "最小化到任务栏，联机继续；托盘菜单可退出。"
        } else {
            "退出工具会停止联机。"
        });
        if before != self.settings.minimize_on_close && self.smoke_test.is_none() {
            match netburrow_core::save_minimize_on_close(self.settings.minimize_on_close) {
                Ok(()) => self.notice = Some("窗口偏好已保存。".into()),
                Err(error) => {
                    self.settings.minimize_on_close = before;
                    self.notice = Some(error);
                }
            }
        }
        ui.add_space(20.0);
        ui.separator();
        ui.label(RichText::new("通知").font(bold(18.0)));
        if toggle(
            ui,
            &mut self.settings.notifications_enabled,
            "重要状态通知",
            "联机就绪、断线或接入失败时通知",
        ) && self.smoke_test.is_none()
        {
            match netburrow_core::save_notifications_enabled(self.settings.notifications_enabled) {
                Ok(()) => {
                    if !self.settings.notifications_enabled {
                        tray::dismiss_notification();
                    }
                    self.notice = Some("通知偏好已保存。".into());
                }
                Err(error) => {
                    self.settings.notifications_enabled = !self.settings.notifications_enabled;
                    self.notice = Some(error);
                }
            }
        }
    }
}
