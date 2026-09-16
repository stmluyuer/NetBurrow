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

#[cfg(test)]
mod tests {
    use super::*;

    fn text_shapes(output: &egui::FullOutput) -> Vec<(String, egui::Rect, egui::Rect)> {
        fn collect(
            shape: &egui::Shape,
            clip: egui::Rect,
            result: &mut Vec<(String, egui::Rect, egui::Rect)>,
        ) {
            match shape {
                egui::Shape::Text(text) => result.push((
                    text.galley.text().to_owned(),
                    text.visual_bounding_rect(),
                    clip,
                )),
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        collect(shape, clip, result);
                    }
                }
                _ => {}
            }
        }
        let mut result = Vec::new();
        for shape in &output.shapes {
            collect(&shape.shape, shape.clip_rect, &mut result);
        }
        result
    }

    fn render(app: &mut NetBurrowApp, context: &egui::Context, size: Vec2) -> egui::FullOutput {
        let mut output = egui::FullOutput::default();
        for _ in 0..3 {
            output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                    ..Default::default()
                },
                |ui| app.main_ui(ui),
            );
        }
        output
    }

    #[test]
    fn update_results_render_without_changing_connection_state() {
        use std::cmp::Ordering;
        for comparison in [Ordering::Less, Ordering::Equal, Ordering::Greater] {
            let context = egui::Context::default();
            configure_fonts(&context);
            configure_style(&context);
            let mut app = NetBurrowApp::new(context.clone(), true, None);
            assert!(app.update_check_at.is_none());
            app.notice = Some("原有联机提示".into());
            app.update_check.result = Some(Ok(update_check::CheckedRelease {
                release: update_check::Release {
                    version: "9.0.0".into(),
                    notes: "中文更新说明，保持纯文本。\n".repeat(100),
                },
                comparison,
            }));
            let home = render(&mut app, &context, Vec2::new(520.0, 620.0));
            let has_hint = text_shapes(&home).iter().any(|(text, _, _)| text.contains("查看更新"));
            assert_eq!(has_hint, comparison == Ordering::Greater);
            assert_eq!(app.notice.as_deref(), Some("原有联机提示"));
            app.open_settings();
            app.notice = Some("原有联机提示".into());
            app.view.settings_tab = SettingsTab::About;
            let output = render(&mut app, &context, Vec2::new(520.0, 620.0));
            let texts = text_shapes(&output);
            let expected = match comparison {
                Ordering::Less => "当前版本高于公开发布版本",
                Ordering::Equal => "当前已是最新发布版本",
                Ordering::Greater => "发现新版本",
            };
            assert!(texts.iter().any(|(text, _, _)| text.starts_with(expected)));
            assert_eq!(texts.iter().any(|(text, _, _)| text == "下载新版 ZIP ↗"), comparison == Ordering::Greater);
            assert_eq!(app.notice.as_deref(), Some("原有联机提示"));
            assert!(app.client.is_none());
            app.update_check.result = Some(Err("网络不可达".into()));
            let output = render(&mut app, &context, Vec2::new(520.0, 620.0));
            assert!(text_shapes(&output).iter().any(|(text, _, _)| text == "暂时无法检查更新"));
            assert_eq!(app.notice.as_deref(), Some("原有联机提示"));
        }
    }

    #[test]
    fn compact_layout_keeps_primary_controls_visible_at_supported_sizes() {
        for size in [
            Vec2::new(520.0, 620.0),
            Vec2::new(580.0, 720.0),
            Vec2::new(900.0, 800.0),
        ] {
            let context = egui::Context::default();
            configure_fonts(&context);
            configure_style(&context);
            let mut app = NetBurrowApp::new(context.clone(), true, None);
            app.settings.server = "relay.example.com:24872".into();
            app.settings.display_name = "很长的中文玩家名字用于检查显示名不会挤掉修改按钮".into();
            for editing in [false, true] {
                app.edit_server = editing;
                app.edit_name = editing;
                let output = render(&mut app, &context, size);
                let texts = text_shapes(&output);
                for label in [
                    "启用联机",
                    "日志与诊断",
                    "联机组",
                    "服务器",
                    "显示名",
                    "同组成员",
                ] {
                    let (_, rect, clip) = texts
                        .iter()
                        .find(|(text, _, _)| text.trim() == label)
                        .unwrap_or_else(|| panic!("missing {label} at {size:?}"));
                    assert!(
                        clip.expand(1.0).contains_rect(*rect),
                        "clipped {label}: {rect:?}, clip {clip:?}, size {size:?}"
                    );
                    assert!(
                        rect.right() <= size.x && rect.bottom() <= size.y,
                        "offscreen {label} at {size:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn stopped_connection_hides_previous_members_and_traffic() {
        let context = egui::Context::default();
        configure_fonts(&context);
        configure_style(&context);
        let mut app = NetBurrowApp::new(context.clone(), true, None);
        app.smoke_test = None;
        app.last_snapshot.sent = 12345;
        app.last_snapshot.peers.push(netburrow_core::PeerInfo {
            client_id: 99,
            game_epoch: 1,
            steam_id: 0,
            ready: true,
            is_self: false,
            status: None,
            status_updated: None,
        });
        let output = render(&mut app, &context, Vec2::new(580.0, 720.0));
        let texts = text_shapes(&output);
        assert!(texts.iter().any(|(text, _, _)| text == "0 人"));
        assert!(
            !texts
                .iter()
                .any(|(text, _, _)| text.contains("成员 99") || text.contains("12345"))
        );
    }

    #[test]
    fn page_navigation_preserves_home_draft_and_requires_explicit_game_apply() {
        let context = egui::Context::default();
        let mut app = NetBurrowApp::new(context, true, None);
        app.settings.group = "home-draft".into();
        app.open_settings();
        app.view.game_draft.as_mut().unwrap().game_path = "changed-game-path".into();
        app.leave_page(Page::Home);
        assert_eq!(app.view.page, Page::Settings);
        assert_eq!(app.view.leave_settings, Some(Page::Home));
        assert_eq!(app.settings.group, "home-draft");
        assert_ne!(app.settings.game_path, "changed-game-path");
        app.view.game_draft = Some(app.settings.clone());
        app.leave_page(Page::Home);
        assert_eq!(app.view.page, Page::Home);
        assert!(app.client.is_none());
    }

    #[test]
    fn secondary_pages_keep_navigation_and_actions_inside_window() {
        for size in [Vec2::new(520.0, 620.0), Vec2::new(580.0, 720.0)] {
            for scale in [1.0, 1.25, 1.5] {
                let context = egui::Context::default();
                configure_fonts(&context);
                configure_style(&context);
                let mut app = NetBurrowApp::new(context.clone(), true, None);
                app.open_settings();
                for (page, settings_tab, diagnostic_tab, expected) in [
                    (
                        Page::Settings,
                        SettingsTab::Game,
                        DiagnosticTab::Checks,
                        "游戏位置",
                    ),
                    (
                        Page::Settings,
                        SettingsTab::General,
                        DiagnosticTab::Checks,
                        "登录 Windows 时启动",
                    ),
                    (
                        Page::Settings,
                        SettingsTab::About,
                        DiagnosticTab::Checks,
                        "复制版本信息",
                    ),
                    (
                        Page::Diagnostics,
                        SettingsTab::Game,
                        DiagnosticTab::Checks,
                        "重新检查",
                    ),
                    (
                        Page::Diagnostics,
                        SettingsTab::Game,
                        DiagnosticTab::Logs,
                        "跟随最新",
                    ),
                ] {
                    app.view.page = page;
                    app.view.settings_tab = settings_tab;
                    app.view.diagnostic_tab = diagnostic_tab;
                    let mut output = egui::FullOutput::default();
                    for _ in 0..3 {
                        let mut input = egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                            ..Default::default()
                        };
                        input
                            .viewports
                            .get_mut(&egui::ViewportId::ROOT)
                            .unwrap()
                            .native_pixels_per_point = Some(scale);
                        output = context.run_ui(input, |ui| app.main_ui(ui));
                    }
                    let texts = text_shapes(&output);
                    for label in ["← 返回联机", expected] {
                        let (_, rect, clip) = texts
                            .iter()
                            .find(|(text, _, _)| text == label)
                            .unwrap_or_else(|| panic!("missing {label}"));
                        assert!(
                            clip.expand(1.0).contains_rect(*rect),
                            "clipped {label} at {size:?} scale {scale}: {rect:?} {clip:?}"
                        );
                    }
                    assert!(!texts.iter().any(|(text, _, _)| text == "启用联机"));
                }
            }
        }
    }

    #[test]
    fn recent_keyboard_selection_changes_server_and_group_without_starting() {
        let context = egui::Context::default();
        configure_fonts(&context);
        configure_style(&context);
        let mut app = NetBurrowApp::new(context.clone(), true, None);
        let recent = netburrow_core::RecentConnection {
            server: "other.example:24872".into(),
            group: format!("NB1-{}", "ab".repeat(32)),
        };
        app.settings.recent_connections = vec![recent.clone()];
        app.view.history_open = true;
        render(&mut app, &context, Vec2::new(580.0, 720.0));
        context.memory_mut(|memory| memory.request_focus(egui::Id::new("group-code")));
        for key in [egui::Key::ArrowDown, egui::Key::Enter] {
            let _ = context.run_ui(
                egui::RawInput {
                    events: vec![egui::Event::Key {
                        key,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    }],
                    ..Default::default()
                },
                |ui| app.main_ui(ui),
            );
        }
        assert_eq!(app.settings.server, recent.server);
        assert_eq!(app.settings.group, recent.group);
        assert!(!app.view.history_open);
        assert!(app.client.is_none());
        assert!(app.preflight.is_none());
    }

    #[test]
    fn escape_restores_inline_edit_without_saving() {
        let context = egui::Context::default();
        configure_fonts(&context);
        configure_style(&context);
        let mut app = NetBurrowApp::new(context.clone(), true, None);
        app.settings.server = "original.example:24872".into();
        app.view.server_before = app.settings.server.clone();
        app.edit_server = true;
        render(&mut app, &context, Vec2::new(580.0, 720.0));
        context.memory_mut(|memory| memory.request_focus(egui::Id::new("server-edit")));
        let _ = context.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Text("changed".into())],
                ..Default::default()
            },
            |ui| app.main_ui(ui),
        );
        assert_ne!(app.settings.server, app.view.server_before);
        let _ = context.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| app.main_ui(ui),
        );
        assert_eq!(app.settings.server, app.view.server_before);
        assert!(!app.edit_server);
    }

    #[test]
    fn history_remove_undo_and_filter_do_not_change_the_current_connection() {
        let context = egui::Context::default();
        configure_fonts(&context);
        configure_style(&context);
        let mut app = NetBurrowApp::new(context.clone(), true, None);
        app.settings.group = "current-draft".into();
        app.settings.recent_connections = vec![netburrow_core::RecentConnection {
            server: "history.example:24872".into(),
            group: format!("NB1-{}", "ab".repeat(32)),
        }];
        app.view.history_open = true;
        app.view.history_manage = true;
        for label in ["移除", "撤销移除"] {
            let output = render(&mut app, &context, Vec2::new(580.0, 720.0));
            let texts = text_shapes(&output);
            let (_, rect, _) = texts.iter().find(|(text, _, _)| text == label).unwrap();
            let pos = rect.center();
            for pressed in [true, false] {
                let _ = context.run_ui(
                    egui::RawInput {
                        events: vec![
                            egui::Event::PointerMoved(pos),
                            egui::Event::PointerButton {
                                pos,
                                button: egui::PointerButton::Primary,
                                pressed,
                                modifiers: egui::Modifiers::NONE,
                            },
                        ],
                        ..Default::default()
                    },
                    |ui| app.main_ui(ui),
                );
            }
            assert_eq!(app.settings.group, "current-draft");
            assert_eq!(
                app.settings.recent_connections.len(),
                if label == "移除" { 0 } else { 1 }
            );
        }
        app.view.history_filter = "does-not-match".into();
        let output = render(&mut app, &context, Vec2::new(580.0, 720.0));
        assert!(
            text_shapes(&output)
                .iter()
                .any(|(text, _, _)| text == "没有匹配的最近记录")
        );
        assert!(app.client.is_none());
    }

    #[test]
    fn pending_check_blocks_recent_selection() {
        let mut app = NetBurrowApp::new(egui::Context::default(), true, None);
        let before = app.settings.clone();
        let (_sender, receiver) = std::sync::mpsc::channel();
        app.preflight = Some(PendingPreflight {
            settings: before.clone(),
            receiver,
            start_after: false,
        });
        app.select_recent(&netburrow_core::RecentConnection {
            server: "new.example:24872".into(),
            group: "new".into(),
        });
        assert!(app.settings.same_connection(&before));
        assert!(!app.connection_editable());
    }
}
