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
    /// Smoke-test captures only: shows a phase without starting a client.
    pub preview_phase: Option<Phase>,
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
            preview_phase: None,
            notice_seen: None,
            notice_since: Instant::now(),
        }
    }
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
        let transient = [netburrow_core::text!("组码已复制", "Group code copied"), netburrow_core::text!("版本信息已复制", "Version info copied"), netburrow_core::text!("设置已应用", "Settings applied"), netburrow_core::text!("诊断已导出", "Diagnostics exported"), netburrow_core::text!("检查完成", "Check complete")].contains(&message.as_str());
        if transient && self.view.notice_since.elapsed() > Duration::from_secs(4) {
            self.notice = None;
            return;
        }
        ui.add_space(8.0);
        let dismissed = ui
            .scope(|ui| {
                ui.multiply_opacity(widgets::appear(ui.ctx(), egui::Id::new("notice-fade"), &message, 0.25));
                widgets::banner(ui, Tone::Neutral, &message, true)
            })
            .inner;
        if dismissed {
            self.notice = None;
        }
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
            self.notice = Some(netburrow_core::text!("断开后可应用游戏设置", "Disconnect to apply game settings").into());
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
                self.notice = Some(netburrow_core::text!("设置已应用", "Settings applied").into());
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
                    .fill(pal().background)
                    .inner_margin(egui::Margin::symmetric(PAGE_MARGIN, 14)),
            )
            .show(ui, |ui| {
                art::backdrop(ui);
                ui.horizontal(|ui| {
                    if widgets::button(ui, widgets::Kind::Ghost, None, netburrow_core::text!("← 返回联机", "← Back"), Vec2::new(0.0, 32.0))
                        .clicked()
                    {
                        self.leave_page(Page::Home);
                    }
                    ui.label(
                        RichText::new(if self.view.page == Page::Settings {
                            netburrow_core::text!("设置", "Settings")
                        } else {
                            netburrow_core::text!("日志与诊断", "Diagnostics")
                        })
                        .font(bold(22.0)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (status, tone) = self.status_label();
                        widgets::status_chip(ui, status, tone);
                    });
                });
                ui.add_space(12.0);
                if self.view.page == Page::Settings {
                    widgets::segmented(ui, &mut self.view.settings_tab, &[
                        (SettingsTab::Game, netburrow_core::text!("游戏与网络", "Game & network")),
                        (SettingsTab::General, netburrow_core::text!("通用", "General")),
                        (SettingsTab::About, netburrow_core::text!("关于", "About")),
                    ]);
                } else {
                    widgets::segmented(ui, &mut self.view.diagnostic_tab, &[
                        (DiagnosticTab::Checks, netburrow_core::text!("连接检查", "Checks")),
                        (DiagnosticTab::Logs, netburrow_core::text!("运行日志", "Logs")),
                    ]);
                }
            });
        egui::Panel::bottom("page-actions")
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(pal().background)
                    .inner_margin(egui::Margin::symmetric(PAGE_MARGIN, 12)),
            )
            .show(ui, |ui| {
                art::backdrop(ui);
                if let Some(destination) = self.view.leave_settings {
                    widgets::banner(ui, Tone::Warn, netburrow_core::text!("有未应用的更改", "Unapplied changes"), false);
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        if widgets::button(ui, widgets::Kind::Primary, None, netburrow_core::text!("应用并返回", "Apply and return"), Vec2::new(0.0, 32.0)).clicked() && self.apply_game() {
                            self.view.leave_settings = None;
                            self.view.page = destination;
                        }
                        if widgets::button(ui, widgets::Kind::Secondary, None, netburrow_core::text!("放弃并返回", "Discard and return"), Vec2::new(0.0, 32.0)).clicked() {
                            self.view.game_draft = Some(self.settings.clone());
                            self.view.leave_settings = None;
                            self.view.page = destination;
                        }
                        if widgets::button(ui, widgets::Kind::Ghost, None, netburrow_core::text!("继续编辑", "Keep editing"), Vec2::new(0.0, 32.0)).clicked() {
                            self.view.leave_settings = None;
                        }
                    });
                } else if self.view.page == Page::Settings && self.game_dirty() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(netburrow_core::text!("有未应用的更改", "Unapplied changes")).size(13.0).color(pal().muted));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let editable = self.connection_editable();
                        if ui
                            .add_enabled_ui(editable, |ui| {
                                widgets::button(ui, widgets::Kind::Primary, None, netburrow_core::text!("应用", "Apply"), Vec2::new(0.0, 34.0))
                            })
                            .inner
                            .clicked()
                        {
                            self.apply_game();
                        }
                        if widgets::button(ui, widgets::Kind::Secondary, None, netburrow_core::text!("放弃", "Discard"), Vec2::new(0.0, 34.0)).clicked() {
                            self.view.game_draft = Some(self.settings.clone());
                        }
                        });
                    });
                } else if self.view.page == Page::Diagnostics {
                    ui.horizontal_wrapped(|ui| {
                        if self.preflight.is_some() && widgets::button(ui, widgets::Kind::Secondary, None, netburrow_core::text!("取消检查", "Cancel check"), Vec2::new(0.0, 32.0)).clicked() {
                            self.stop();
                        }
                        let packing = self.diagnostic_bundle.is_some();
                        if ui
                            .add_enabled_ui(!packing, |ui| {
                                widgets::button(
                                    ui,
                                    widgets::Kind::Primary,
                                    Some(icons::Action::Log),
                                    if packing { netburrow_core::text!("正在打包…", "Exporting…") } else { netburrow_core::text!("一键打包日志", "Export log bundle") },
                                    Vec2::new(0.0, 32.0),
                                )
                            })
                            .inner
                            .on_hover_text(netburrow_core::text!("导出日志、诊断和最近完整崩溃记录为 ZIP；包含转储时可能较大", "Export logs, diagnostics, and the latest complete crash capture as ZIP. Dumps may make the file large."))
                            .clicked()
                        {
                            self.begin_diagnostic_bundle(ui.ctx());
                        }
                        if widgets::button(ui, widgets::Kind::Secondary, None, netburrow_core::text!("导出诊断", "Export diagnostics"), Vec2::new(0.0, 32.0)).clicked() {
                            self.export_diagnostics();
                        }
                        if widgets::button(ui, widgets::Kind::Secondary, None, netburrow_core::text!("记录卡住现场", "Capture freeze"), Vec2::new(0.0, 32.0))
                            .on_hover_text(netburrow_core::text!("游戏卡住时，记录时间并导出状态和近期日志", "When the game freezes, record the time and export current status and recent logs"))
                            .clicked()
                        {
                            self.export_diagnostics_at(true);
                        }
                        if widgets::button(ui, widgets::Kind::Secondary, Some(icons::Action::Folder), netburrow_core::text!("打开日志文件夹", "Open log folder"), Vec2::new(0.0, 32.0)).clicked()
                        {
                            self.open_log_folder();
                        }
                        if self.view.diagnostic_tab == DiagnosticTab::Logs
                            && widgets::button(ui, widgets::Kind::Ghost, None, netburrow_core::text!("回到最新", "Jump to latest"), Vec2::new(0.0, 32.0)).clicked()
                        {
                            self.view.log_follow = true;
                        }
                    });
                    self.crash_capture_ui(ui);
                    if let Some(path) = self.diagnostic_export.clone() {
                        if widgets::button(ui, widgets::Kind::Ghost, Some(icons::Action::Folder), netburrow_core::text!("打开导出位置", "Show exported file"), Vec2::new(0.0, 30.0)).clicked() {
                            self.open_folder(&path, true);
                        }
                    }
                }
                self.notice_ui(ui);
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(pal().background).inner_margin(PAGE_MARGIN))
            .show(ui, |ui| {
                art::backdrop(ui);
                // Only the content fades on tab switches; the header and tabs stay put.
                let tab = (self.view.page as u8, self.view.settings_tab as u8, self.view.diagnostic_tab as u8);
                ui.multiply_opacity(widgets::appear(ui.ctx(), egui::Id::new("tab-fade"), tab, 0.18));
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
    fn begin_diagnostic_bundle(&mut self, context: &egui::Context) {
        if self.diagnostic_bundle.is_some() {
            return;
        }
        let settings = self.settings.clone();
        let snapshot = self.diagnostic_snapshot();
        let context = context.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        match std::thread::Builder::new()
            .name("diagnostics-bundle".into())
            .spawn(move || {
                let _ = sender.send(diagnostics_bundle::export(&settings, &snapshot));
                context.request_repaint();
            })
        {
            Ok(_) => {
                self.diagnostic_bundle = Some(receiver);
                self.diagnostic_bundle_automatic = false;
                self.notice = Some(netburrow_core::text!("正在导出日志包…", "Exporting log bundle…").into());
            }
            Err(error) => self.notice = Some(netburrow_core::text_format!("无法导出日志包：{error}", "Could not export log bundle: {error}")),
        }
    }

    pub(super) fn poll_diagnostic_bundle(&mut self) {
        let Some(receiver) = &self.diagnostic_bundle else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err(netburrow_core::text!("日志导出中断，请重试", "Log export interrupted. Try again.").into())
            }
        };
        self.diagnostic_bundle = None;
        let automatic = std::mem::take(&mut self.diagnostic_bundle_automatic);
        match result {
            Ok(bundle) => {
                let mut notice = if bundle.crash_included {
                    self.crash_bundle_export = Some(bundle.path.clone());
                    netburrow_core::text!("排查包已导出，含最近崩溃记录和日志。转储含游戏内存，仅与可信人员分享。", "Bundle exported with recent crash capture and logs. Dumps contain game memory; share only with trusted support.").to_owned()
                } else { netburrow_core::text!("日志包已导出", "Log bundle exported").to_owned() };
                if !bundle.missing_logs.is_empty() {
                    notice.push_str(&netburrow_core::text_format!("；未生成的日志：{}", "; Logs not yet available: {}", bundle.missing_logs.join(netburrow_core::text!("、", ", "))));
                }
                if let Some(note) = bundle.crash_note { notice.push_str(&netburrow_core::text_format!("；{note}", "; {note}")); }
                self.notice = Some(notice);
                self.diagnostic_export = Some(bundle.path.clone());
                if !automatic { self.open_folder(&bundle.path, true); }
                else if self.settings.notifications_enabled && self.smoke_test.is_none() && bundle.crash_included {
                    let _ = tray::notify(&notifications::Notification {
                        title: netburrow_core::text!("NetBurrow 排查包已导出", "NetBurrow bundle exported"),
                        body: netburrow_core::text!("崩溃记录和日志已保存在本机，可在“日志与诊断”中打开。", "Crash capture and logs saved locally. Open the bundle from Diagnostics."),
                        warning: false,
                    });
                }
            }
            Err(error) => self.notice = Some(error),
        }
    }

    pub(super) fn poll_crash_capture(&mut self, context: &egui::Context) {
        let wanted = if self.smoke_test.is_none() && !cfg!(test)
            && self.settings.auto_crash_capture && self.settings.crash_capture_consent
            && self.client.is_some()
        {
            self.active_settings.as_ref().map(|settings| settings.game_path.as_str())
        } else { None };
        if self.crash_capture.sync(wanted).is_some() {
            self.pending_crash_bundle = true;
            self.crash_bundle_export = None;
        }
        if self.pending_crash_bundle && self.diagnostic_bundle.is_none() {
            self.pending_crash_bundle = false;
            self.begin_diagnostic_bundle(context);
            self.diagnostic_bundle_automatic = self.diagnostic_bundle.is_some();
        }
    }

    fn set_crash_capture(&mut self, enabled: bool, consent: bool) {
        if self.smoke_test.is_none() && !cfg!(test) {
            if let Err(error) = netburrow_core::save_crash_capture(enabled, consent) {
                self.notice = Some(netburrow_core::text_format!("无法保存自动记录设置：{error}", "Could not save crash capture settings: {error}"));
                return;
            }
        }
        self.settings.auto_crash_capture = enabled;
        self.settings.crash_capture_consent = consent;
        self.saved_settings.auto_crash_capture = enabled;
        self.saved_settings.crash_capture_consent = consent;
        self.crash_capture_confirm = false;
        if !enabled { self.crash_capture.stop(); }
    }

    fn crash_capture_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            let mut enabled = self.settings.auto_crash_capture;
            if widgets::toggle(ui, &mut enabled, netburrow_core::text!("自动记录闪退", "Capture crashes automatically")).changed() {
                if enabled && !self.settings.crash_capture_consent {
                    self.crash_capture_confirm = true;
                } else { self.set_crash_capture(enabled, self.settings.crash_capture_consent); }
            }
            if !(self.settings.auto_crash_capture || self.crash_capture.is_running()) {
                ui.label(RichText::new(netburrow_core::text!("启用联机后自动开始记录", "Starts when you go online")).size(12.0).color(pal().muted));
            } else {
                ui.small(self.crash_capture.status.display_message())
                    .on_hover_text(&self.crash_capture.status.message);
                if self.settings.auto_crash_capture && self.crash_capture.status.state == crash_capture::State::Failed
                    && ui.button(netburrow_core::text!("重试", "Retry")).clicked()
                { self.crash_capture.retry(); }
            }
            if let Some(path) = self.crash_bundle_export.clone() {
                if ui.button(netburrow_core::text!("打开排查包", "Open bundle")).clicked() { self.open_folder(&path, true); }
            }
        });
        if self.crash_capture_confirm {
            ui.add_space(6.0);
            widgets::card(Tone::Warn).show(ui, |ui| {
                ui.label(netburrow_core::text!("连接期间自动监视游戏，闪退后生成排查 ZIP。仅保存在本机，不自动上传。", "Monitor the game while connected and create a ZIP after a crash. Files stay on this PC and are not uploaded automatically."));
                ui.small(netburrow_core::text!("首次使用会下载并校验微软 ProcDump。采集可能短暂停顿游戏；转储可能占用数 GB，并含个人信息。导出日志包时会包含最近一次完整记录。", "First use downloads and verifies Microsoft ProcDump. Capture may briefly pause the game. Dumps may use several GB and contain personal information. Log bundles include the latest complete capture."));
                ui.hyperlink_to(netburrow_core::text!("微软 Sysinternals 许可", "Microsoft Sysinternals license"), "https://learn.microsoft.com/en-us/sysinternals/license-terms");
                ui.horizontal(|ui| {
                    if ui.button(netburrow_core::text!("同意许可并开启", "Accept license and enable")).clicked() { self.set_crash_capture(true, true); }
                    if ui.button(netburrow_core::text!("取消", "Cancel")).clicked() { self.crash_capture_confirm = false; }
                });
            });
        }
    }

    fn open_folder(&mut self, path: &Path, select: bool) {
        let mut command = std::process::Command::new("explorer.exe");
        if select {
            command.arg("/select,");
        }
        if let Err(error) = command.arg(path).spawn() {
            self.notice = Some(netburrow_core::text_format!("无法打开文件夹：{error}", "Could not open folder: {error}"));
        }
    }
    /// Writes the embedded license texts next to the settings and opens them in Notepad.
    fn open_licenses(&mut self) {
        let directory = netburrow_core::config_directory();
        let path = directory.join("licenses.txt");
        let result = (|| -> std::io::Result<()> {
            let mut text = Vec::new();
            std::io::Read::read_to_end(
                &mut flate2::read::DeflateDecoder::new(&include_bytes!(concat!(env!("OUT_DIR"), "/licenses.deflate"))[..]),
                &mut text,
            )?;
            std::fs::create_dir_all(&directory)?;
            std::fs::write(&path, text)?;
            std::process::Command::new("notepad.exe").arg(&path).spawn()?;
            Ok(())
        })();
        if let Err(error) = result {
            self.notice = Some(netburrow_core::text_format!("无法打开许可文本：{error}", "Could not open license texts: {error}"));
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
        widgets::section(ui, netburrow_core::text!("当前连接", "Connection"), |ui| {
            ui.horizontal(|ui| {
                let (status, tone) = self.status_label();
                widgets::status_chip(ui, status, tone);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let editable = self.connection_editable();
                    if ui
                        .add_enabled_ui(editable, |ui| {
                            widgets::button(ui, widgets::Kind::Secondary, Some(icons::Action::Search), netburrow_core::text!("重新检查", "Run check"), Vec2::new(0.0, 30.0))
                        })
                        .inner
                        .on_disabled_hover_text(netburrow_core::text!("断开且当前检查结束后可重试", "Disconnect and wait for any active check to finish"))
                        .clicked()
                    {
                        self.begin_preflight(false);
                    }
                });
            });
            let detail = self.last_snapshot.detail.trim();
            if self.client.is_some() && !detail.is_empty() {
                ui.add(egui::Label::new(RichText::new(detail).size(13.0).color(pal().muted)).wrap());
            }
            ui.add_space(8.0);
            if self.preflight_report.is_none() && self.preflight.is_none() {
                ui.horizontal(|ui| {
                    art::show(ui, art::Art::EmptyCheck, Vec2::new(72.0, 40.0), pal().subtle);
                    ui.label(RichText::new(netburrow_core::text!("尚未检查", "Not checked yet")).size(13.0).color(pal().muted));
                });
                ui.add_space(4.0);
            }
            self.preflight_ui(ui);
            let ready = self.current_phase() == Phase::Ready;
            widgets::check_row(
                ui,
                if ready { widgets::Check::Pass } else { widgets::Check::Info },
                if ready {
                    netburrow_core::text!("实际游戏接入：已就绪", "Game attachment: ready")
                } else {
                    netburrow_core::text!("实际游戏接入：尚未验证", "Game attachment: not verified")
                },
                "",
            );
        });
        widgets::section(ui, netburrow_core::text!("连接统计", "Statistics"), |ui| {
            if self.client.is_none() {
                ui.label(RichText::new(netburrow_core::text!("以下可能包含上次运行记录", "May include data from the previous session")).size(12.0).color(pal().muted));
                ui.add_space(4.0);
            }
            let (quality, reason) =
                netburrow_core::connection_quality(&self.last_snapshot, Instant::now());
            let (label, tone) = match quality {
                netburrow_core::Quality::Pending => (netburrow_core::text!("待测", "Pending"), Tone::Neutral),
                netburrow_core::Quality::Normal => (netburrow_core::text!("正常", "Normal"), Tone::Accent),
                netburrow_core::Quality::Unstable => (netburrow_core::text!("波动", "Unstable"), Tone::Warn),
                netburrow_core::Quality::Abnormal => (netburrow_core::text!("异常", "Abnormal"), Tone::Danger),
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new(netburrow_core::text!("连接质量", "Quality")).size(14.0));
                widgets::status_chip(ui, label, tone);
            });
            ui.add(egui::Label::new(RichText::new(reason).size(12.0).color(pal().muted)).wrap());
            ui.add_space(8.0);
            let snapshot = &self.last_snapshot;
            widgets::metrics(ui, &[
                (art::Item::Coin, netburrow_core::text!("发送 · 接收", "Sent · Received"), netburrow_core::text_format!("{} · {} 包", "{} · {} packets", snapshot.sent, snapshot.received)),
                (art::Item::SoulHeart, netburrow_core::text!("UDP 发送 · 接收", "UDP sent · Received"), netburrow_core::text_format!("{} · {} 包", "{} · {} packets", snapshot.udp_sent, snapshot.udp_received)),
                (art::Item::Bomb, netburrow_core::text!("断线", "Disconnects"), netburrow_core::text_format!("{} 次", "{}", snapshot.disconnects)),
                (art::Item::EmptyHeart, netburrow_core::text!("心跳超时", "Heartbeat timeouts"), netburrow_core::text_format!("{} 次", "{}", snapshot.heartbeat_timeouts)),
                (art::Item::Key, netburrow_core::text!("原 Relay 会话恢复", "Relay session recoveries"), netburrow_core::text_format!("{} 次", "{}", snapshot.relay_recoveries)),
                (art::Item::HalfHeart, netburrow_core::text!("本地游戏连接恢复", "Game connection recoveries"), netburrow_core::text_format!("{} 次", "{}", snapshot.ipc_recoveries)),
            ]);
            self.member_diagnostics_ui(ui);
        });
    }
    fn member_diagnostics_ui(&self, ui: &mut egui::Ui) {
        if !self.last_snapshot.peers.is_empty() {
            ui.collapsing(netburrow_core::text!("成员链路诊断", "Member connection diagnostics"), |ui| {
                let diagnostics = &self.last_snapshot.path_diagnostics;
                if !diagnostics.relay_supported {
                    ui.small(netburrow_core::text!("服务器尚未确认诊断支持，旧版服务器不支持此功能。", "Server diagnostics support is unconfirmed. Older servers do not support this feature."));
                }
                ui.small(netburrow_core::text!("探测经 TCP 到达成员客户端；成功不代表游戏画面正常更新。", "Probes reach members over TCP. Success does not confirm that the game is advancing."));
                for p in &diagnostics.peers {
                    let peer = self.last_snapshot.peers.iter().find(|peer| peer.client_id == p.member);
                    let name = peer.and_then(|peer| peer.status.as_ref()).filter(|s| !s.name.is_empty()).map(|s| s.name.clone()).unwrap_or_else(|| netburrow_core::text_format!("成员 {}", "Member {}", p.member));
                    let path = if !p.supported { netburrow_core::text!("不支持诊断", "Diagnostics unsupported").to_owned() }
                        else if p.probe_timed_out { netburrow_core::text!("探测超时", "Probe timed out").to_owned() }
                        else if let Some(rtt) = p.rtt_ms { netburrow_core::text_format!("往返 {rtt} ms · {} ms 前成功", "Round trip {rtt} ms · Last success {} ms ago", p.success_age_ms.unwrap_or_default()) }
                        else { netburrow_core::text!("等待回复", "Awaiting reply").to_owned() };
                    ui.label(netburrow_core::text_format!("{name} · {path} · 超时 {} 次", "{name} · {path} · Timeouts {}", p.probe_timeouts));
                    for (i, f) in p.flows.iter().enumerate() {
                        ui.small(netburrow_core::text_format!("{}：编号 {} · 接收 {} · 窗口缺号 {} · 乱序 {} · 重复 {}", "{}: assigned {} · received {} · missing in window {} · reordered {} · duplicates {}",
                            if i == 1 { netburrow_core::text!("可靠数据", "Reliable data") } else { netburrow_core::text!("不可靠数据", "Unreliable data") }, f.assigned, f.received, f.missing_window, f.reordered, f.duplicates));
                    }
                }
                if diagnostics.omitted > 0 { ui.small(netburrow_core::text_format!("容量限制，省略 {} 位成员", "Capacity limit: {} members omitted", diagnostics.omitted)); }
            });
            ui.collapsing(netburrow_core::text!("成员统计", "Member totals"), |ui| {
                for peer in &self.last_snapshot.peers {
                    if let Some(status) = &peer.status {
                        let name = if status.name.is_empty() {
                            netburrow_core::text_format!("成员 {}", "Member {}", peer.client_id)
                        } else {
                            status.name.clone()
                        };
                        if peer.status_is_stale() {
                            ui.label(netburrow_core::text_format!("{name} · 已过期", "{name} · Stale"));
                        } else {
                            ui.label(netburrow_core::text_format!("{name} · 接收 {} / 发送 {} 包", "{name} · Received {} / Sent {} packets",
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
                egui::TextEdit::singleline(&mut self.view.log_query).hint_text(netburrow_core::text!("搜索日志…", "Search logs…")),
            );
            widgets::toggle(ui, &mut self.view.log_follow, netburrow_core::text!("自动滚动", "Auto-scroll"));
        });
        if let Some(error) = netburrow_core::diagnostics::last_error() {
            ui.colored_label(Tone::Danger.fg(), netburrow_core::text_format!("无法写入日志：{error}", "Cannot write logs: {error}"));
        }
        ui.add_space(6.0);
        let query = self.view.log_query.to_lowercase();
        let lines: Vec<_> = self
            .last_snapshot
            .logs
            .iter()
            .filter(|line| line.to_lowercase().contains(&query))
            .collect();
        if lines.is_empty() {
            ui.label(if query.is_empty() {
                netburrow_core::text!("暂无日志", "No logs yet")
            } else {
                netburrow_core::text!("无匹配记录", "No matching entries")
            });
        }
        let output = egui::Frame::new()
            .fill(pal().field)
            .stroke(egui::Stroke::new(1.0, pal().divider))
            .corner_radius(pal().card_radius)
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                egui::ScrollArea::both()
                    .id_salt("log-body")
                    .auto_shrink([false, false])
                    .stick_to_bottom(self.view.log_follow)
                    .show(ui, |ui| {
                        for line in lines {
                            ui.add(
                                egui::Label::new(RichText::new(line).monospace().size(12.5))
                                    .selectable(true)
                                    .extend(),
                            );
                        }
                    })
            })
            .inner;
        if output.state.offset.y + output.inner_rect.height() + 4.0 < output.content_size.y {
            self.view.log_follow = false;
        }
    }
    fn about_content(&mut self, ui: &mut egui::Ui) {
        let version = format!(
            "NetBurrow {} · {}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::ARCH
        );
        widgets::card(Tone::Neutral).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                art::show(ui, art::Art::Logo, Vec2::new(72.0, 58.0), pal().text);
                ui.add_space(4.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new("NetBurrow").font(bold(22.0)));
                    ui.label(RichText::new(&version).size(13.0).color(pal().muted));
                    ui.label(
                        RichText::new(netburrow_core::text!("以撒的结合：忏悔+ 联机工具", "Multiplayer for The Binding of Isaac: Repentance+"))
                            .size(12.0)
                            .color(pal().muted),
                    );
                });
            });
            ui.add_space(6.0);
            if widgets::button(ui, widgets::Kind::Secondary, Some(icons::Action::Copy), netburrow_core::text!("复制版本信息", "Copy version info"), Vec2::new(0.0, 30.0)).clicked() {
                ui.ctx().copy_text(version.clone());
                self.notice = Some(netburrow_core::text!("版本信息已复制", "Version info copied").into());
            }
        });
        ui.add_space(18.0);
        widgets::section(ui, netburrow_core::text!("更新", "Updates"), |ui| {
            let live = self.smoke_test.is_none() && !cfg!(test);
            let mut auto_check = self.settings.auto_check_updates;
            if widgets::switch_row(
                ui,
                &mut auto_check,
                netburrow_core::text!("启动时检查更新", "Check for updates at startup"),
                netburrow_core::text!("开启时立即检查，此后每次启动检查。", "Check now when enabled, then at each launch."),
            ) && live
            {
                match netburrow_core::save_auto_check_updates(auto_check) {
                    Ok(()) => {
                        self.settings.auto_check_updates = auto_check;
                        self.saved_settings.auto_check_updates = auto_check;
                        self.update_check_at = None;
                        if auto_check { self.update_check.begin(); }
                    }
                    Err(error) => self.notice = Some(error),
                }
            }
            ui.horizontal(|ui| {
                let idle = !self.update_check.is_checking() && live;
                if ui
                    .add_enabled_ui(idle, |ui| {
                        widgets::button(ui, widgets::Kind::Secondary, None, netburrow_core::text!("检查更新", "Check for updates"), Vec2::new(0.0, 30.0))
                    })
                    .inner
                    .clicked()
                {
                    self.update_check_at = None;
                    self.update_check.begin();
                }
                if self.update_check.is_checking() {
                    ui.spinner();
                    ui.label(RichText::new(netburrow_core::text!("正在检查…", "Checking…")).size(13.0).color(pal().muted));
                }
            });
            if let Some(result) = &self.update_check.result {
                ui.add_space(6.0);
                match result {
                    Ok(checked) => {
                        let (text, check) = match checked.comparison {
                            std::cmp::Ordering::Equal => (netburrow_core::text!("已是最新版本", "You're up to date").to_owned(), widgets::Check::Pass),
                            std::cmp::Ordering::Less => (netburrow_core::text_format!("当前版本高于公开版本 v{}", "This version is newer than public release v{}", checked.release.version), widgets::Check::Info),
                            std::cmp::Ordering::Greater => (netburrow_core::text_format!("新版本 v{} 可用", "Update v{} available", checked.release.version), widgets::Check::Warn),
                        };
                        widgets::check_row(ui, check, &text, "");
                        if checked.comparison == std::cmp::Ordering::Greater {
                            ui.horizontal(|ui| {
                                ui.hyperlink_to(netburrow_core::text!("下载新版 ↗", "Download update ↗"), checked.release.download_url());
                                ui.hyperlink_to(netburrow_core::text!("发行说明 ↗", "Release notes ↗"), checked.release.page_url());
                            });
                            egui::ScrollArea::vertical().id_salt("update-notes").max_height(140.0).show(ui, |ui| {
                                ui.add(egui::Label::new(&checked.release.notes).wrap());
                            });
                            ui.add(egui::Label::new(RichText::new(netburrow_core::text!("退出游戏和 NetBurrow，将完整 ZIP 解压到新文件夹后启动。现有设置会保留。", "Close the game and NetBurrow, extract the full ZIP into a new folder, then launch it. Your settings are kept.")).small().color(pal().muted)).wrap());
                        }
                    }
                    Err(error) => widgets::check_row(ui, widgets::Check::Fail, netburrow_core::text!("无法检查更新", "Could not check for updates"), error),
                }
            }
        });
        widgets::section(ui, netburrow_core::text!("开源许可", "Open source licenses"), |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(netburrow_core::text!(
                        "NetBurrow 采用 MIT 许可。第三方依赖与内置字体的版权声明和许可原文随程序附带。",
                        "NetBurrow is MIT licensed. Copyright notices and license texts for third-party dependencies and bundled fonts ship with the app."
                    ))
                    .size(13.0)
                    .color(pal().muted),
                )
                .wrap(),
            );
            ui.add_space(6.0);
            if widgets::button(ui, widgets::Kind::Secondary, Some(icons::Action::Log), netburrow_core::text!("查看许可文本", "View license texts"), Vec2::new(0.0, 30.0)).clicked() {
                self.open_licenses();
            }
        });
        widgets::section(ui, netburrow_core::text!("链接", "Links"), |ui| {
            ui.hyperlink_to(netburrow_core::text!("所有版本 ↗", "All releases ↗"), update_check::RELEASES);
            ui.hyperlink_to(netburrow_core::text!("项目主页 ↗", "Project website ↗"), update_check::REPOSITORY);
            ui.hyperlink_to(netburrow_core::text!("反馈问题 ↗", "Report an issue ↗"), update_check::ISSUES);
        });
        if widgets::button(ui, widgets::Kind::Ghost, Some(icons::Action::Log), netburrow_core::text!("打开日志与诊断", "Open diagnostics"), Vec2::new(0.0, 32.0)).clicked() {
            self.leave_page(Page::Diagnostics);
        }
    }
}
