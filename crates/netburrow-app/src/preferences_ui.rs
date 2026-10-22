use super::*;

impl NetBurrowApp {
    pub(super) fn game_settings_ui(&mut self, ui: &mut egui::Ui) {
        let enabled = self.connection_editable();
        let draft = self
            .view
            .game_draft
            .get_or_insert_with(|| self.settings.clone());
        if !enabled {
            widgets::banner(ui, Tone::Info, netburrow_core::text!("断开后可修改游戏与网络设置", "Disconnect to change game and network settings"), false);
            ui.add_space(12.0);
        }
        ui.add_enabled_ui(enabled, |ui| {
            widgets::section(ui, netburrow_core::text!("游戏", "Game"), |ui| {
                ui.label(RichText::new(netburrow_core::text!("游戏位置", "Game path")).size(13.0).color(pal().muted));
                ui.add_sized(
                    [ui.available_width(), 36.0],
                    egui::TextEdit::singleline(&mut draft.game_path)
                        .vertical_align(egui::Align::Center)
                        .hint_text(netburrow_core::text!("选择 isaac-ng.exe", "Select isaac-ng.exe")),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if widgets::button(ui, widgets::Kind::Secondary, Some(icons::Action::Folder), netburrow_core::text!("选择文件", "Choose file"), Vec2::new(0.0, 32.0)).clicked() {
                        match file_picker::choose_game() {
                            Ok(Some(path)) => draft.game_path = path,
                            Ok(None) => {}
                            Err(error) => self.notice = Some(error),
                        }
                    }
                    if widgets::button(ui, widgets::Kind::Secondary, Some(icons::Action::Search), netburrow_core::text!("自动检测", "Auto-detect"), Vec2::new(0.0, 32.0)).clicked() {
                        match netburrow_core::autodetect_game() {
                            Some(path) => draft.game_path = path.display().to_string(),
                            None => {
                                self.notice = Some(netburrow_core::text!("未找到游戏，请选择 isaac-ng.exe", "Game not found. Select isaac-ng.exe.").into())
                            }
                        }
                    }
                });
                ui.add_space(6.0);
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
                match path_error {
                    Some(error) => widgets::check_row(ui, widgets::Check::Warn, error, ""),
                    None => widgets::check_row(ui, widgets::Check::Pass, netburrow_core::text!("已找到 isaac-ng.exe", "Found isaac-ng.exe"), ""),
                }
            });
            widgets::section(ui, netburrow_core::text!("网络", "Network"), |ui| {
                ui.label(RichText::new(netburrow_core::text!("传输方式", "Transport")).size(13.0).color(pal().muted));
                widgets::segmented(ui, &mut draft.transport, &[
                    (Transport::Tcp, netburrow_core::text!("TCP（推荐）", "TCP (recommended)")),
                    (Transport::Udp, netburrow_core::text!("UDP 优先", "Prefer UDP")),
                ]);
                ui.label(
                    RichText::new(if draft.transport == Transport::Udp {
                        netburrow_core::text!("可靠消息仍使用 TCP", "Reliable messages still use TCP")
                    } else {
                        netburrow_core::text!("兼容性最好，适合大多数网络", "Most compatible; works on most networks")
                    })
                    .size(12.0)
                    .color(pal().muted),
                );
                ui.add_space(8.0);
                egui::CollapsingHeader::new(netburrow_core::text!("高级选项", "Advanced options"))
                    .default_open(self.smoke_test.is_some() && std::env::args().any(|arg| arg == "--preview-page=advanced"))
                    .show(ui, |ui| {
                        widgets::switch_row(
                            ui,
                            &mut draft.allow_late_hook,
                            netburrow_core::text!("接入已运行的游戏（实验性）", "Attach to a running game (experimental)"),
                            netburrow_core::text!("关闭时，仅接入启用联机后启动的游戏", "When off, only games started after going online can be attached"),
                        );
                        if draft.allow_late_hook {
                            widgets::banner(
                                ui,
                                Tone::Warn,
                                netburrow_core::text!("仅限未联机的主菜单；接入失败需重开游戏。", "Use only at the main menu before joining a game. Restart the game if attachment fails."),
                                false,
                            );
                        }
                    });
            });
        }).response.on_disabled_hover_text(if self.preflight.is_some() {
            netburrow_core::text!("检查完成后可编辑", "Wait for the check to finish")
        } else {
            netburrow_core::text!("断开后可编辑", "Disconnect to edit")
        });
    }
    pub(super) fn preferences_content(&mut self, ui: &mut egui::Ui) {
        widgets::section(ui, netburrow_core::text!("外观", "Appearance"), |ui| {
            let previous = self.settings.theme;
            widgets::segmented(ui, &mut self.settings.theme, &[
                (netburrow_core::Theme::Classic, netburrow_core::text!("默认", "Classic")),
                (netburrow_core::Theme::Paper, netburrow_core::text!("纸片涂鸦", "Paper")),
                (netburrow_core::Theme::Pixel, netburrow_core::text!("地下室像素", "Pixel")),
                (netburrow_core::Theme::Cartoon, netburrow_core::text!("扁平卡通", "Cartoon")),
            ]);
            if self.settings.theme != previous {
                let result = if self.smoke_test.is_some() {
                    Ok(())
                } else {
                    netburrow_core::save_theme(self.settings.theme)
                };
                match result {
                    Ok(()) => {
                        self.saved_settings.theme = self.settings.theme;
                        if let Some(draft) = &mut self.view.game_draft {
                            draft.theme = self.settings.theme;
                        }
                        theme::set(ui.ctx(), self.settings.theme);
                        self.notice = None;
                    }
                    Err(error) => {
                        self.settings.theme = previous;
                        self.notice = Some(error);
                    }
                }
            }
            ui.label(RichText::new(netburrow_core::text!("立即生效", "Applies immediately")).size(12.0).color(pal().muted));
        });
        widgets::section(ui, netburrow_core::text!("语言", "Language"), |ui| {
            let previous_language = self.settings.language;
            widgets::segmented(ui, &mut self.settings.language, &[
                (netburrow_core::i18n::Language::ZhCn, "简体中文"),
                (netburrow_core::i18n::Language::En, "English"),
            ]);
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
            ui.label(RichText::new(netburrow_core::text!("重启后生效", "Applies after restart")).size(12.0).color(pal().muted));
        });
        widgets::section(ui, netburrow_core::text!("启动", "Startup"), |ui| {
            if widgets::switch_row(
                ui,
                &mut self.startup_enabled,
                netburrow_core::text!("登录 Windows 时启动", "Launch at Windows sign-in"),
                netburrow_core::text!("启动后需手动启用联机", "Go online manually after launch"),
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
            ui.separator();
            ui.add_space(4.0);
            if widgets::switch_row(
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
        });
        widgets::section(ui, netburrow_core::text!("窗口", "Window"), |ui| {
            ui.label(RichText::new(netburrow_core::text!("关闭窗口时", "When closing the window")).size(13.0).color(pal().muted));
            let before = self.settings.minimize_on_close;
            widgets::segmented(ui, &mut self.settings.minimize_on_close, &[
                (false, netburrow_core::text!("退出并断开", "Quit and disconnect")),
                (true, netburrow_core::text!("最小化并保持连接", "Minimize and stay connected")),
            ]);
            if before != self.settings.minimize_on_close && self.smoke_test.is_none() {
                match netburrow_core::save_minimize_on_close(self.settings.minimize_on_close) {
                    Ok(()) => self.notice = None,
                    Err(error) => {
                        self.settings.minimize_on_close = before;
                        self.notice = Some(error);
                    }
                }
            }
        });
        widgets::section(ui, netburrow_core::text!("通知", "Notifications"), |ui| {
            if widgets::switch_row(
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
        });
    }
}
