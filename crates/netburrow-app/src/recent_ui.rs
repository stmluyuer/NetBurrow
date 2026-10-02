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
            netburrow_core::text_format!("已选择服务器 {}，连接后生效", "Server set to {}. Applies when you connect.", recent.server)
        } else {
            netburrow_core::text!("已选择联机组，点击连接加入", "Group selected. Click Connect to join.").into()
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
                self.notice = Some(netburrow_core::text_format!("无法保存最近记录：{error}", "Could not save recent groups: {error}"));
                false
            }
        }
    }
    pub(super) fn group_ui(&mut self, ui: &mut egui::Ui) {
        let enabled = self.connection_editable();
        ui.horizontal(|ui| {
            ui.label(RichText::new(netburrow_core::text!("联机组", "Group")).font(bold(21.0)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(enabled, egui::Button::new(netburrow_core::text!("＋ 新建组", "＋ New group")).frame(false))
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
                                    .hint_text(netburrow_core::text!("输入或粘贴组码", "Enter or paste group code")),
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
                        if icons::icon_button(ui, icons::Action::Down, netburrow_core::text!("最近使用", "Recent groups")).clicked()
                        {
                            self.view.history_open = !self.view.history_open;
                            self.view.history_filter.clear();
                            self.view.history_selection = None;
                        }
                        if icons::icon_button(ui, icons::Action::Paste, netburrow_core::text!("粘贴组码", "Paste group code")).clicked() {
                            match clipboard::read_text() {
                                Ok(group) => {
                                    self.settings.group = group;
                                    self.view.history_open = false;
                                }
                                Err(error) => self.notice = Some(error),
                            }
                        }
                    });
                    if icons::icon_button(ui, icons::Action::Copy, netburrow_core::text!("复制组码", "Copy group code")).clicked() {
                        ui.ctx().copy_text(self.settings.group.clone());
                        self.notice = Some(netburrow_core::text!("组码已复制", "Group code copied").into());
                    }
                });
            });
        field.response.clone().on_hover_text(if self.preflight.is_some() {
            netburrow_core::text!("检查完成后可编辑", "Wait for the check to finish")
        } else if !enabled {
            netburrow_core::text!("断开后可编辑", "Disconnect to edit")
        } else {
            netburrow_core::text!("与朋友使用相同服务器和组码", "Use the same server and group code as your friends")
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
                ui.label(RichText::new(netburrow_core::text!("最近使用", "Recent groups")).font(bold(15.0)));
                if records.is_empty() {
                    ui.small(if self.settings.recent_connections.is_empty() {
                        netburrow_core::text!("暂无记录", "No recent groups")
                    } else {
                        netburrow_core::text!("无匹配记录", "No matching groups")
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
                                        netburrow_core::text!("当前 · ", "Current · ")
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
                                if self.view.history_manage && ui.small_button(netburrow_core::text!("移除", "Remove")).clicked() {
                                    remove = Some(*index);
                                }
                            });
                        }
                    });
                ui.separator();
                if ui
                    .small_button(if self.view.history_manage {
                        netburrow_core::text!("完成", "Done")
                    } else {
                        netburrow_core::text!("管理", "Manage")
                    })
                    .clicked()
                {
                    self.view.history_manage = !self.view.history_manage;
                }
                if self.view.history_undo.is_some() && ui.small_button(netburrow_core::text!("撤销", "Undo")).clicked() {
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
