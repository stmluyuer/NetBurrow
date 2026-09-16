use super::*;

impl NetBurrowApp {
    pub(super) fn main_ui(&mut self, ui: &mut egui::Ui) {
        if self.view.page != Page::Home {
            self.secondary_ui(ui);
            return;
        }
        egui::Panel::bottom("primary-action")
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(BACKGROUND)
                    .inner_margin(egui::Margin::symmetric(24, 12)),
            )
            .show(ui, |ui| {
                let active = self.client.is_some();
                let checking = self.preflight.is_some();
                let label = if checking {
                    "取消自检"
                } else if active {
                    "停止联机"
                } else {
                    "启用联机"
                };
                let fill = if active || checking { SURFACE } else { ACCENT };
                let color = if active || checking {
                    TEXT
                } else {
                    Color32::WHITE
                };
                let response = ui.add_sized(
                    [ui.available_width(), 48.0],
                    egui::Button::new("")
                        .fill(fill)
                        .stroke(egui::Stroke::NONE)
                        .corner_radius(7),
                );
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label)
                });
                let galley = ui.painter().layout_no_wrap(label.into(), bold(18.0), color);
                let start = response.rect.center() - Vec2::new((galley.size().x + 34.0) / 2.0, 0.0);
                icons::draw(
                    ui.painter(),
                    egui::Rect::from_center_size(start + Vec2::new(11.0, 0.0), Vec2::splat(22.0)),
                    icons::Action::Power,
                    color,
                    fill,
                );
                ui.painter().galley(
                    start + Vec2::new(34.0, -galley.size().y / 2.0),
                    galley,
                    color,
                );
                if response.clicked() {
                    if active || checking {
                        self.stop();
                    } else {
                        self.start();
                    }
                }
                ui.add_space(4.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new(if checking {
                            "正在检查连接…"
                        } else if active {
                            "停止后需重开游戏"
                        } else if self.settings.allow_late_hook {
                            "请停在游戏主菜单后启用"
                        } else {
                            "启用后，从 Steam 启动游戏"
                        })
                        .size(12.0)
                        .color(MUTED),
                    );
                });
                ui.add_space(4.0);
                ui.separator();
                if let Some(release) = self.update_check.newer_release() {
                    if ui.link(format!("发现新版本 v{} · 查看更新", release.version)).clicked() {
                        self.open_settings();
                        self.view.settings_tab = SettingsTab::About;
                    }
                }
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("日志与诊断").color(MUTED))
                                .frame(false),
                        )
                        .clicked()
                    {
                        self.view.page = Page::Diagnostics;
                    }
                });
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BACKGROUND).inner_margin(24))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            icons::logo(ui);
                            ui.add_space(4.0);
                            ui.vertical(|ui| {
                                ui.label(RichText::new("NetBurrow").font(bold(27.0)));
                                ui.label(RichText::new("以撒好友联机").size(13.0).color(MUTED));
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if icons::icon_button(ui, icons::Action::Settings, "偏好设置")
                                        .clicked()
                                    {
                                        self.open_settings();
                                    }
                                    let (status, color) = if self.preflight.is_some() {
                                        ("正在自检", MUTED)
                                    } else {
                                        self.status_label()
                                    };
                                    ui.label(
                                        RichText::new(format!("● {status}"))
                                            .size(13.0)
                                            .color(color),
                                    );
                                },
                            );
                        });
                        ui.add_space(22.0);
                        self.group_ui(ui);
                        ui.add_space(16.0);
                        self.profile_rows(ui);
                        ui.add_space(12.0);
                        ui.separator();
                        ui.add_space(12.0);
                        self.activity_ui(ui);
                        ui.add_space(12.0);
                        ui.separator();
                        ui.add_space(8.0);
                        if self
                            .preflight_report
                            .as_ref()
                            .is_some_and(|(_, report)| !report.can_start())
                        {
                            if ui.button("查看检查结果").clicked() {
                                self.view.page = Page::Diagnostics;
                            }
                        }
                        if !Path::new(self.settings.game_path.trim()).is_file() {
                            ui.horizontal(|ui| {
                                ui.small("请先设置游戏路径");
                                if ui.button("前往设置").clicked() {
                                    self.open_settings();
                                }
                            });
                        }
                        if let Some(error) = &self.config_error {
                            ui.label(RichText::new(error).size(12.0));
                        }
                        self.notice_ui(ui);
                    });
            });
    }

    fn profile_rows(&mut self, ui: &mut egui::Ui) {
        for is_server in [true, false] {
            if !is_server {
                ui.separator();
            }
            ui.horizontal(|ui| {
                ui.set_min_height(38.0);
                icons::inline(
                    ui,
                    if is_server {
                        icons::Action::Server
                    } else {
                        icons::Action::User
                    },
                );
                ui.add_space(4.0);
                ui.allocate_ui_with_layout(
                    Vec2::new(70.0, 34.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.label(
                            RichText::new(if is_server { "服务器" } else { "显示名" })
                                .font(bold(15.0)),
                        );
                    },
                );
                let editing = if is_server {
                    self.edit_server
                } else {
                    self.edit_name
                };
                let enabled = self.connection_editable();
                let width = ui.available_width() - 56.0;
                ui.allocate_ui_with_layout(
                    Vec2::new(width, 34.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.set_min_width(width);
                        if editing {
                            let value = if is_server {
                                &mut self.settings.server
                            } else {
                                &mut self.settings.display_name
                            };
                            let editor = egui::TextEdit::singleline(value)
                                .id(egui::Id::new(if is_server {
                                    "server-edit"
                                } else {
                                    "name-edit"
                                }))
                                .desired_width(width);
                            let response = ui.add_enabled(
                                enabled,
                                if is_server {
                                    editor
                                } else {
                                    editor.char_limit(24)
                                },
                            );
                            if (response.has_focus() || response.lost_focus())
                                && ui.input(|i| i.key_pressed(egui::Key::Escape))
                            {
                                if is_server {
                                    self.settings.server = self.view.server_before.clone();
                                    self.edit_server = false;
                                } else {
                                    self.settings.display_name = self.view.name_before.clone();
                                    self.edit_name = false;
                                }
                                response.surrender_focus();
                            } else if response.lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter))
                            {
                                if is_server {
                                    self.edit_server = false;
                                } else {
                                    self.edit_name = false;
                                }
                            }
                        } else {
                            let value = if is_server {
                                &self.settings.server
                            } else {
                                &self.settings.display_name
                            };
                            let value = if value.trim().is_empty() {
                                if is_server {
                                    "未设置"
                                } else {
                                    "默认成员编号"
                                }
                            } else {
                                value
                            };
                            ui.add(egui::Label::new(RichText::new(value).size(15.0)).truncate())
                                .on_hover_text(value);
                        }
                    },
                );
                if ui
                    .add_enabled(
                        enabled,
                        egui::Button::new(
                            RichText::new(if editing { "完成" } else { "编辑" })
                                .size(14.0)
                                .color(ACCENT),
                        )
                        .frame(false),
                    )
                    .on_disabled_hover_text(if self.preflight.is_some() {
                        "检查期间无法编辑"
                    } else {
                        "停止联机后可编辑"
                    })
                    .clicked()
                {
                    if is_server {
                        if !editing {
                            self.view.server_before = self.settings.server.clone();
                            ui.memory_mut(|m| m.request_focus(egui::Id::new("server-edit")));
                        }
                        self.edit_server = !self.edit_server;
                    } else {
                        if !editing {
                            self.view.name_before = self.settings.display_name.clone();
                            ui.memory_mut(|m| m.request_focus(egui::Id::new("name-edit")));
                        }
                        self.edit_name = !self.edit_name;
                    }
                }
            });
        }
    }
}
