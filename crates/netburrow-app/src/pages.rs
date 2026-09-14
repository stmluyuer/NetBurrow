use super::*;

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Page {
    #[default]
    Home,
    Settings,
    Diagnostics,
}
#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsTab {
    #[default]
    Game,
    General,
    About,
}
#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub(super) enum DiagnosticTab {
    #[default]
    Checks,
    Logs,
}

pub(super) struct ViewState {
    pub page: Page,
    pub settings_tab: SettingsTab,
    pub diagnostic_tab: DiagnosticTab,
    pub game_draft: Option<Settings>,
    pub leave_settings: Option<Page>,
    pub server_before: String,
    pub name_before: String,
    pub history_open: bool,
    pub history_filter: String,
    pub history_selection: Option<usize>,
    pub history_manage: bool,
    pub history_undo: Option<Vec<netburrow_core::RecentConnection>>,
    pub log_query: String,
    pub log_follow: bool,
    pub preview_requested: bool,
    pub capture_requested: bool,
    pub notice_seen: Option<String>,
    pub notice_since: Instant,
}
impl Default for ViewState {
    fn default() -> Self {
        Self {
            page: Page::Home,
            settings_tab: SettingsTab::Game,
            diagnostic_tab: DiagnosticTab::Checks,
            game_draft: None,
            leave_settings: None,
            server_before: String::new(),
            name_before: String::new(),
            history_open: false,
            history_filter: String::new(),
            history_selection: None,
            history_manage: false,
            history_undo: None,
            log_query: String::new(),
            log_follow: true,
            preview_requested: false,
            capture_requested: false,
            notice_seen: None,
            notice_since: Instant::now(),
        }
    }
}

pub(super) fn tab(ui: &mut egui::Ui, selected: bool, title: &str) -> bool {
    let response = ui.add(
        egui::Button::new(RichText::new(title).color(if selected { ACCENT } else { MUTED }))
            .frame(false),
    );
    if selected {
        ui.painter().line_segment(
            [response.rect.left_bottom(), response.rect.right_bottom()],
            egui::Stroke::new(2.0, ACCENT),
        );
    }
    response.clicked()
}

impl NetBurrowApp {
    pub(super) fn notice_ui(&mut self, ui: &mut egui::Ui) {
        if self.view.notice_seen != self.notice {
            self.view.notice_seen = self.notice.clone();
            self.view.notice_since = Instant::now();
        }
        let Some(message) = self.notice.clone() else {
            return;
        };
        let transient = matches!(
            message.as_str(),
            "完整组码已复制。"
                | "版本信息已复制。"
                | "设置已应用"
                | "诊断已导出"
                | "自检完成"
        );
        if transient && self.view.notice_since.elapsed() > Duration::from_secs(4) {
            self.notice = None;
            return;
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(message).size(12.0));
            if ui.small_button("×").on_hover_text("关闭提示").clicked() {
                self.notice = None;
            }
        });
    }
    pub(super) fn connection_editable(&self) -> bool {
        self.client.is_none() && self.preflight.is_none()
    }
    pub(super) fn open_settings(&mut self) {
        if self.view.game_draft.is_none() {
            self.view.game_draft = Some(self.settings.clone());
        }
        self.view.page = Page::Settings;
        self.view.history_open = false;
        self.notice = None;
    }
    pub(super) fn game_dirty(&self) -> bool {
        self.view.game_draft.as_ref().is_some_and(|draft| {
            draft.game_path != self.settings.game_path
                || draft.transport != self.settings.transport
                || draft.allow_late_hook != self.settings.allow_late_hook
        })
    }
    pub(super) fn leave_page(&mut self, destination: Page) {
        if self.view.page == Page::Settings && self.game_dirty() {
            self.view.leave_settings = Some(destination);
        } else {
            self.view.page = destination;
            self.notice = None;
        }
    }
    pub(super) fn apply_game(&mut self) -> bool {
        if !self.connection_editable() {
            self.notice = Some("停止联机后可应用游戏设置。".into());
            return false;
        }
        let Some(draft) = self.view.game_draft.as_ref() else {
            return true;
        };
        let result = if self.smoke_test.is_some() {
            draft.validate_game()
        } else {
            netburrow_core::save_game_settings(draft)
        };
        match result {
            Ok(()) => {
                self.settings.game_path = draft.game_path.trim().into();
                self.settings.transport = draft.transport;
                self.settings.allow_late_hook = draft.allow_late_hook;
                self.view.game_draft = Some(self.settings.clone());
                self.notice = Some("设置已应用".into());
                true
            }
            Err(error) => {
                self.notice = Some(error);
                false
            }
        }
    }
    pub(super) fn secondary_ui(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("page-header")
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(BACKGROUND)
                    .inner_margin(egui::Margin::symmetric(24, 14)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .add(egui::Button::new("← 返回联机").frame(false))
                        .clicked()
                    {
                        self.leave_page(Page::Home);
                    }
                    ui.label(
                        RichText::new(if self.view.page == Page::Settings {
                            "设置"
                        } else {
                            "日志与诊断"
                        })
                        .font(bold(22.0)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (status, color) = self.status_label();
                        ui.label(RichText::new(format!("● {status}")).size(12.0).color(color));
                    });
                });
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if self.view.page == Page::Settings {
                        for (value, title) in [
                            (SettingsTab::Game, "游戏与网络"),
                            (SettingsTab::General, "通用"),
                            (SettingsTab::About, "关于"),
                        ] {
                            if tab(ui, self.view.settings_tab == value, title) {
                                self.view.settings_tab = value;
                            }
                        }
                    } else {
                        for (value, title) in [
                            (DiagnosticTab::Checks, "连接检查"),
                            (DiagnosticTab::Logs, "运行日志"),
                        ] {
                            if tab(ui, self.view.diagnostic_tab == value, title) {
                                self.view.diagnostic_tab = value;
                            }
                        }
                    }
                });
            });
        egui::Panel::bottom("page-actions")
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(BACKGROUND)
                    .inner_margin(egui::Margin::symmetric(24, 12)),
            )
            .show(ui, |ui| {
                if let Some(destination) = self.view.leave_settings {
                    ui.label("有未保存的更改");
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("应用更改并返回").clicked() && self.apply_game() {
                            self.view.leave_settings = None;
                            self.view.page = destination;
                        }
                        if ui.button("放弃修改并返回").clicked() {
                            self.view.game_draft = Some(self.settings.clone());
                            self.view.leave_settings = None;
                            self.view.page = destination;
                        }
                        if ui.button("继续编辑").clicked() {
                            self.view.leave_settings = None;
                        }
                    });
                } else if self.view.page == Page::Settings && self.game_dirty() {
                    ui.horizontal(|ui| {
                        if ui.button("放弃修改").clicked() {
                            self.view.game_draft = Some(self.settings.clone());
                        }
                        if ui
                            .add_enabled(
                                self.connection_editable(),
                                egui::Button::new(RichText::new("应用更改").color(Color32::WHITE))
                                    .fill(ACCENT),
                            )
                            .clicked()
                        {
                            self.apply_game();
                        }
                    });
                } else if self.view.page == Page::Diagnostics {
                    ui.horizontal_wrapped(|ui| {
                        if self.preflight.is_some() && ui.button("取消检查").clicked() {
                            self.stop();
                        }
                        if ui.button("导出诊断").clicked() {
                            self.export_diagnostics();
                        }
                        if icons::button(ui, icons::Action::Folder, "打开日志文件夹").clicked()
                        {
                            self.open_log_folder();
                        }
                        if self.view.diagnostic_tab == DiagnosticTab::Logs
                            && ui.button("回到最新").clicked()
                        {
                            self.view.log_follow = true;
                        }
                    });
                    if let Some(path) = self.diagnostic_export.clone() {
                        if ui.button("打开诊断所在文件夹").clicked() {
                            self.open_folder(&path, true);
                        }
                    }
                }
                self.notice_ui(ui);
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BACKGROUND).inner_margin(24))
            .show(ui, |ui| {
                if self.view.page == Page::Diagnostics
                    && self.view.diagnostic_tab == DiagnosticTab::Logs
                {
                    self.log_content(ui);
                } else {
                    egui::ScrollArea::vertical()
                        .id_salt((
                            "page-content",
                            self.view.page as u8,
                            self.view.settings_tab as u8,
                        ))
                        .show(ui, |ui| match self.view.page {
                            Page::Settings => match self.view.settings_tab {
                                SettingsTab::Game => self.game_settings_ui(ui),
                                SettingsTab::General => self.preferences_content(ui),
                                SettingsTab::About => self.about_content(ui),
                            },
                            Page::Diagnostics => self.check_content(ui),
                            Page::Home => {}
                        });
                }
            });
    }
    fn open_folder(&mut self, path: &Path, select: bool) {
        let mut command = std::process::Command::new("explorer.exe");
        if select {
            command.arg("/select,");
        }
        if let Err(error) = command.arg(path).spawn() {
            self.notice = Some(format!("无法打开文件夹：{error}"));
        }
    }
    fn open_log_folder(&mut self) {
        let path = netburrow_core::diagnostics::directory();
        match std::fs::create_dir_all(&path) {
            Ok(()) => self.open_folder(&path, false),
            Err(error) => self.notice = Some(error.to_string()),
        }
    }
    fn check_content(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("当前连接").font(bold(18.0)));
        let (status, color) = self.status_label();
        ui.colored_label(color, status);
        if self.client.is_some() {
            ui.label(&self.last_snapshot.detail);
        }
        ui.add_space(12.0);
        if ui
            .add_enabled(self.connection_editable(), egui::Button::new("重新检查"))
            .on_disabled_hover_text("联机或检查期间不能重新自检")
            .clicked()
        {
            self.begin_preflight(false);
        }
        if self.preflight_report.is_none() && self.preflight.is_none() {
            ui.small("尚未检查");
        }
        self.preflight_ui(ui);
        ui.label(if self.current_phase() == Phase::Ready {
            "实际游戏接入：已就绪"
        } else {
            "实际游戏接入：尚未验证"
        });
        ui.add_space(16.0);
        ui.separator();
        ui.label(RichText::new("连接统计").font(bold(18.0)));
        if self.client.is_none() {
            ui.small("以下可能包含上次运行记录");
        }
        let (quality, reason) =
            netburrow_core::connection_quality(&self.last_snapshot, Instant::now());
        ui.label(format!(
            "连接质量：{} · {reason}",
            match quality {
                netburrow_core::Quality::Pending => "待测",
                netburrow_core::Quality::Normal => "正常",
                netburrow_core::Quality::Unstable => "波动",
                netburrow_core::Quality::Abnormal => "异常",
            }
        ));
        ui.label(format!(
            "发送 {} 包 · 接收 {} 包",
            self.last_snapshot.sent, self.last_snapshot.received
        ));
        ui.label(format!(
            "UDP 发送 {} 包 · 接收 {} 包",
            self.last_snapshot.udp_sent, self.last_snapshot.udp_received
        ));
        ui.label(format!(
            "断线 {} 次 · 心跳超时 {} 次",
            self.last_snapshot.disconnects, self.last_snapshot.heartbeat_timeouts
        ));
        if !self.last_snapshot.peers.is_empty() {
            ui.collapsing("成员累计统计", |ui| {
                for peer in &self.last_snapshot.peers {
                    if let Some(status) = &peer.status {
                        let name = if status.name.is_empty() {
                            format!("成员 {}", peer.client_id)
                        } else {
                            status.name.clone()
                        };
                        if peer.status_is_stale() {
                            ui.label(format!("{name} · 数据过期"));
                        } else {
                            ui.label(format!(
                                "{name} · 接收 {} / 发送 {} 包",
                                status.received, status.sent
                            ));
                        }
                    }
                }
            });
        }
    }
    fn log_content(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_sized(
                [ui.available_width() - 120.0, 32.0],
                egui::TextEdit::singleline(&mut self.view.log_query).hint_text("搜索当前记录…"),
            );
            ui.checkbox(&mut self.view.log_follow, "跟随最新");
        });
        if let Some(error) = netburrow_core::diagnostics::last_error() {
            ui.colored_label(Color32::from_rgb(174, 65, 60), error);
        }
        ui.separator();
        let query = self.view.log_query.to_lowercase();
        let lines: Vec<_> = self
            .last_snapshot
            .logs
            .iter()
            .filter(|line| line.to_lowercase().contains(&query))
            .collect();
        if lines.is_empty() {
            ui.label(if query.is_empty() {
                "暂无运行记录"
            } else {
                "没有匹配的记录"
            });
        }
        let output = egui::ScrollArea::both()
            .id_salt("log-body")
            .auto_shrink([false, false])
            .stick_to_bottom(self.view.log_follow)
            .show(ui, |ui| {
                for line in lines {
                    ui.add(
                        egui::Label::new(RichText::new(line).monospace())
                            .selectable(true)
                            .extend(),
                    );
                }
            });
        if output.state.offset.y + output.inner_rect.height() + 4.0 < output.content_size.y {
            self.view.log_follow = false;
        }
    }
    fn about_content(&mut self, ui: &mut egui::Ui) {
        icons::logo(ui);
        ui.label(RichText::new("NetBurrow").font(bold(26.0)));
        let version = format!(
            "NetBurrow {} · {}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::ARCH
        );
        ui.label(&version);
        ui.small("以撒的结合：忏悔+ · 好友联机工具");
        ui.add_space(18.0);
        if ui.button("复制版本信息").clicked() {
            ui.ctx().copy_text(version);
            self.notice = Some("版本信息已复制。".into());
        }
        ui.hyperlink_to(
            "查看发布版本 ↗",
            "https://github.com/stmluyuer/NetBurrow/releases",
        );
        ui.hyperlink_to("项目主页 ↗", "https://github.com/stmluyuer/NetBurrow");
        ui.hyperlink_to(
            "反馈问题 ↗",
            "https://github.com/stmluyuer/NetBurrow/issues/new",
        );
        ui.add_space(18.0);
        if ui.button("附上诊断信息 →").clicked() {
            self.leave_page(Page::Diagnostics);
        }
    }
}
