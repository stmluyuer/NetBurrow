use super::*;

fn toggle(ui: &mut egui::Ui, value: &mut bool, title: &str, description: &str) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let width = ui.available_width() - 80.0;
        ui.allocate_ui_with_layout(
            Vec2::new(width, 32.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_min_width(width);
                ui.label(RichText::new(title).font(bold(15.0)))
                    .on_hover_text(description);
            },
        );
        let response = ui.add_sized([64.0, 32.0], egui::Button::new("").frame(false))
            .on_hover_text(description);
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
            if *value { netburrow_core::text!("开", "On") } else { netburrow_core::text!("关", "Off") },
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
        let draft = self
            .view
            .game_draft
            .get_or_insert_with(|| self.settings.clone());
        ui.add_enabled_ui(enabled, |ui| {
            ui.label(RichText::new(netburrow_core::text!("游戏", "Game")).font(bold(18.0)));
            ui.add_space(8.0);
            ui.label(netburrow_core::text!("游戏位置", "Game path"));
            ui.add_sized(
                [ui.available_width(), 36.0],
                egui::TextEdit::singleline(&mut draft.game_path).hint_text(netburrow_core::text!("选择 isaac-ng.exe", "Select isaac-ng.exe")),
            );
            ui.horizontal(|ui| {
                if ui.button(netburrow_core::text!("浏览…", "Browse…")).clicked() {
                    match file_picker::choose_game() {
                        Ok(Some(path)) => draft.game_path = path,
                        Ok(None) => {}
                        Err(error) => self.notice = Some(error),
                    }
                }
                if icons::button(ui, icons::Action::Search, netburrow_core::text!("自动检测", "Auto-detect")).clicked() {
                    match netburrow_core::autodetect_game() {
                        Some(path) => draft.game_path = path.display().to_string(),
                        None => {
                            self.notice = Some(netburrow_core::text!("未找到游戏，请选择 isaac-ng.exe", "Game not found. Select isaac-ng.exe.").into())
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
            let path_error = if draft.game_path.trim().is_empty() {
                Some(netburrow_core::text!("请选择游戏文件", "Select the game file"))
            } else if !valid_name {
                Some(netburrow_core::text!("请选择 isaac-ng.exe", "Select isaac-ng.exe"))
            } else if !path.is_file() {
                Some(netburrow_core::text!("文件不存在，请重新选择", "File not found. Select another file."))
            } else {
                None
            };
            if let Some(error) = path_error {
                ui.label(RichText::new(error).size(12.0).color(MUTED));
            }
            ui.add_space(20.0);
            ui.separator();
            ui.label(RichText::new(netburrow_core::text!("网络", "Network")).font(bold(18.0)));
            ui.add_space(8.0);
            ui.label(netburrow_core::text!("传输方式", "Transport"));
            ui.horizontal(|ui| {
                ui.radio_value(&mut draft.transport, Transport::Tcp, netburrow_core::text!("TCP（推荐）", "TCP (recommended)"));
                ui.radio_value(&mut draft.transport, Transport::Udp, netburrow_core::text!("UDP 优先", "Prefer UDP"))
                    .on_hover_text(netburrow_core::text!("可靠消息仍使用 TCP", "Reliable messages still use TCP"));
            });
            ui.add_space(20.0);
            egui::CollapsingHeader::new(netburrow_core::text!("高级", "Advanced"))
                .default_open(self.smoke_test.is_some() && std::env::args().any(|arg| arg == "--preview-page=advanced"))
                .show(ui, |ui| {
                toggle(
                    ui,
                    &mut draft.allow_late_hook,
                    netburrow_core::text!("接入已运行的游戏（实验性）", "Attach to a running game (experimental)"),
                    netburrow_core::text!("关闭时，仅接入连接后启动的游戏", "When off, only games started after connecting can be attached"),
                );
                if draft.allow_late_hook {
                    ui.label(
                        RichText::new(
                            netburrow_core::text!("仅限未联机的主菜单；接入失败需重开游戏。", "Use only at the main menu before joining a game. Restart the game if attachment fails."),
                        )
                        .size(12.0)
                        .color(Color32::from_rgb(151, 103, 37)),
                    );
                }
            });
        }).response.on_disabled_hover_text(if self.preflight.is_some() {
            netburrow_core::text!("检查完成后可编辑", "Wait for the check to finish")
        } else {
            netburrow_core::text!("断开后可编辑", "Disconnect to edit")
        });
    }
    pub(super) fn preferences_content(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new(netburrow_core::text!("语言", "Language")).font(bold(18.0)));
        let previous_language = self.settings.language;
        ui.horizontal_wrapped(|ui| {
            ui.radio_value(&mut self.settings.language, netburrow_core::i18n::Language::ZhCn, "简体中文");
            ui.radio_value(&mut self.settings.language, netburrow_core::i18n::Language::En, "English");
        });
        if self.settings.language != previous_language {
            let result = if self.smoke_test.is_some() {
                Ok(())
            } else {
                netburrow_core::save_language(self.settings.language)
            };
            match result {
                Ok(()) => {
                    self.saved_settings.language = self.settings.language;
                    if let Some(draft) = &mut self.view.game_draft {
                        draft.language = self.settings.language;
                    }
                    self.notice = None;
                }
                Err(error) => {
                    self.settings.language = previous_language;
                    self.notice = Some(error);
                }
            }
        }
        ui.small(netburrow_core::text!("重启后生效", "Applies after restart"));
        ui.add_space(14.0);
        ui.separator();
        ui.label(RichText::new(netburrow_core::text!("启动", "Startup")).font(bold(18.0)));
        ui.add_space(10.0);
        if toggle(
            ui,
            &mut self.startup_enabled,
            netburrow_core::text!("登录 Windows 时启动", "Launch at Windows sign-in"),
            netburrow_core::text!("启动后需手动连接", "Connection must be started manually"),
        ) && self.smoke_test.is_none()
        {
            match startup::set_enabled(self.startup_enabled) {
                Ok(()) => self.notice = None,
                Err(error) => {
                    self.startup_enabled = !self.startup_enabled;
                    self.notice = Some(error);
                }
            }
        }
        if toggle(
            ui,
            &mut self.settings.start_minimized,
            netburrow_core::text!("启动后最小化", "Start minimized"),
            netburrow_core::text!("下次启动生效", "Applies next launch"),
        ) && self.smoke_test.is_none()
        {
            match netburrow_core::save_start_minimized(self.settings.start_minimized) {
                Ok(()) => self.notice = None,
                Err(error) => {
                    self.settings.start_minimized = !self.settings.start_minimized;
                    self.notice = Some(error);
                }
            }
        }
        ui.add_space(14.0);
        ui.separator();
        ui.label(RichText::new(netburrow_core::text!("窗口", "Window")).font(bold(18.0)));
        ui.label(netburrow_core::text!("关闭窗口时", "When closing the window"));
        let before = self.settings.minimize_on_close;
        ui.horizontal_wrapped(|ui| {
            ui.radio_value(&mut self.settings.minimize_on_close, false, netburrow_core::text!("退出并断开", "Quit and disconnect"));
            ui.radio_value(&mut self.settings.minimize_on_close, true, netburrow_core::text!("最小化并保持连接", "Minimize and stay connected"));
        });
        if before != self.settings.minimize_on_close && self.smoke_test.is_none() {
            match netburrow_core::save_minimize_on_close(self.settings.minimize_on_close) {
                Ok(()) => self.notice = None,
                Err(error) => {
                    self.settings.minimize_on_close = before;
                    self.notice = Some(error);
                }
            }
        }
        ui.add_space(20.0);
        ui.separator();
        ui.label(RichText::new(netburrow_core::text!("通知", "Notifications")).font(bold(18.0)));
        if toggle(
            ui,
            &mut self.settings.notifications_enabled,
            netburrow_core::text!("连接状态通知", "Connection notifications"),
            netburrow_core::text!("就绪、断线或接入失败时通知", "Notify when ready, disconnected, or attachment fails"),
        ) && self.smoke_test.is_none()
        {
            match netburrow_core::save_notifications_enabled(self.settings.notifications_enabled) {
                Ok(()) => {
                    if !self.settings.notifications_enabled {
                        tray::dismiss_notification();
                    }
                    self.notice = None;
                }
                Err(error) => {
                    self.settings.notifications_enabled = !self.settings.notifications_enabled;
                    self.notice = Some(error);
                }
            }
        }
    }
}
