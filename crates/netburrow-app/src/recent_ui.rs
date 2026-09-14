use super::*;

impl NetBurrowApp {
    pub(super) fn select_recent(&mut self, recent: &netburrow_core::RecentConnection) {
        if !self.connection_editable() {
            return;
        }
        let changed_server = self.settings.server != recent.server;
        self.settings.server = recent.server.clone();
        self.settings.group = recent.group.clone();
        self.view.history_open = false;
        self.notice = Some(if changed_server {
            format!("服务器已切换为 {}，启用联机后生效。", recent.server)
        } else {
            "已选择联机组，点击启用联机加入。".into()
        });
    }
    fn store_history(&mut self, recent: Vec<netburrow_core::RecentConnection>) -> bool {
        let result = if self.smoke_test.is_some() {
            Ok(())
        } else {
            netburrow_core::save_recent_connections(&recent)
        };
        match result {
            Ok(()) => {
                self.settings.recent_connections = recent;
                true
            }
            Err(error) => {
                self.notice = Some(format!("无法保存最近记录：{error}"));
                false
            }
        }
    }
    pub(super) fn group_ui(&mut self, ui: &mut egui::Ui) {
        let enabled = self.connection_editable();
        ui.horizontal(|ui| {
            ui.label(RichText::new("联机组").font(bold(21.0)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(enabled, egui::Button::new("＋ 创建联机组").frame(false))
                    .clicked()
                {
                    match netburrow_core::new_group() {
                        Ok(group) => {
                            self.settings.group = group;
                            self.notice = None;
                        }
                        Err(error) => self.notice = Some(error),
                    }
                }
            });
        });
        ui.add_space(6.0);
        let field = egui::Frame::new()
            .fill(Color32::from_rgb(252, 250, 247))
            .stroke(egui::Stroke::new(1.0, BORDER))
            .corner_radius(6)
            .inner_margin(egui::Margin::symmetric(12, 7))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let response = ui
                        .add_enabled_ui(enabled, |ui| {
                            ui.add_sized(
                                [ui.available_width() - 144.0, 36.0],
                                egui::TextEdit::singleline(&mut self.settings.group)
                                    .id(egui::Id::new("group-code"))
                                    .font(egui::FontId::proportional(16.0))
                                    .frame(egui::Frame::NONE)
                                    .hint_text("输入或粘贴完整组码"),
                            )
                        })
                        .inner;
                    if response.clicked() {
                        self.view.history_open = true;
                        self.view.history_filter.clear();
                        self.view.history_selection = None;
                    }
                    if response.changed() {
                        self.view.history_filter = self.settings.group.clone();
                        self.view.history_open = true;
                        self.view.history_selection = None;
                    }
                    ui.add_enabled_ui(enabled, |ui| {
                        if icons::icon_button(ui, icons::Action::Down, "最近联机组").clicked()
                        {
                            self.view.history_open = !self.view.history_open;
                            self.view.history_filter.clear();
                            self.view.history_selection = None;
                        }
                        if icons::icon_button(ui, icons::Action::Paste, "粘贴组码").clicked() {
                            match clipboard::read_text() {
                                Ok(group) => {
                                    self.settings.group = group;
                                    self.view.history_open = false;
                                }
                                Err(error) => self.notice = Some(error),
                            }
                        }
                    });
                    if icons::icon_button(ui, icons::Action::Copy, "复制完整组码").clicked() {
                        ui.ctx().copy_text(self.settings.group.clone());
                        self.notice = Some("完整组码已复制。".into());
                    }
                });
            });
        field.response.clone().on_hover_text(if self.preflight.is_some() {
            "检查期间无法编辑"
        } else if !enabled {
            "停止联机后可编辑"
        } else {
            "与朋友使用相同的服务器和组码"
        });
        if !enabled {
            self.view.history_open = false;
        }
        if !self.view.history_open {
            return;
        }
        let filter = self.view.history_filter.trim().to_lowercase();
        let records: Vec<_> = self
            .settings
            .recent_connections
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                r.group.to_lowercase().contains(&filter)
                    || r.server.to_lowercase().contains(&filter)
            })
            .map(|(i, r)| (i, r.clone()))
            .collect();
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.view.history_open = false;
            return;
        }
        if !records.is_empty() {
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)) {
                self.view.history_selection = Some(
                    self.view
                        .history_selection
                        .map_or(0, |i| (i + 1) % records.len()),
                );
            }
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)) {
                self.view.history_selection =
                    Some(self.view.history_selection.map_or(records.len() - 1, |i| {
                        (i + records.len() - 1) % records.len()
                    }));
            }
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
                if let Some((_, recent)) = self.view.history_selection.and_then(|i| records.get(i))
                {
                    self.select_recent(recent);
                    return;
                }
            }
        }
        let mut open = self.view.history_open;
        let mut chosen = None;
        let mut remove = None;
        let mut undo = false;
        egui::Popup::from_response(&field.response)
            .open_bool(&mut open)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .width(field.response.rect.width())
            .show(|ui| {
                ui.set_width(field.response.rect.width() - 16.0);
                ui.label(RichText::new("最近联机组").font(bold(15.0)));
                if records.is_empty() {
                    ui.small(if self.settings.recent_connections.is_empty() {
                        "暂无最近联机组"
                    } else {
                        "没有匹配的最近记录"
                    });
                }
                egui::ScrollArea::vertical()
                    .max_height(250.0)
                    .show(ui, |ui| {
                        for (position, (index, recent)) in records.iter().enumerate() {
                            ui.push_id(index, |ui| {
                                let selected = self.view.history_selection == Some(position);
                                let label = format!(
                                    "{}{}\n{}",
                                    if recent.group == self.settings.group
                                        && recent.server == self.settings.server
                                    {
                                        "当前 · "
                                    } else {
                                        ""
                                    },
                                    recent.group,
                                    recent.server
                                );
                                let response = ui.add_sized(
                                    [ui.available_width(), 48.0],
                                    egui::Button::new(RichText::new(label).size(12.0))
                                        .selected(selected)
                                        .wrap(),
                                );
                                if selected {
                                    response.scroll_to_me(Some(egui::Align::Center));
                                }
                                if response.clicked() {
                                    chosen = Some(recent.clone());
                                }
                                if self.view.history_manage && ui.small_button("移除").clicked() {
                                    remove = Some(*index);
                                }
                            });
                        }
                    });
                ui.separator();
                if ui
                    .small_button(if self.view.history_manage {
                        "完成管理"
                    } else {
                        "管理记录"
                    })
                    .clicked()
                {
                    self.view.history_manage = !self.view.history_manage;
                }
                if self.view.history_undo.is_some() && ui.small_button("撤销移除").clicked() {
                    undo = true;
                }
            });
        self.view.history_open = open;
        if let Some(recent) = chosen {
            self.select_recent(&recent);
        }
        if let Some(index) = remove {
            let previous = self.settings.recent_connections.clone();
            let mut next = previous.clone();
            next.remove(index);
            if self.store_history(next) {
                self.view.history_undo = Some(previous);
                self.view.history_selection = None;
            }
        }
        if undo {
            if let Some(previous) = self.view.history_undo.clone() {
                if self.store_history(previous) {
                    self.view.history_undo = None;
                }
            }
        }
    }
}
