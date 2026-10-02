#![cfg_attr(windows, windows_subsystem = "windows")]

mod clipboard;
mod icons;
mod notifications;
mod tray;
mod window_state;
mod startup;
mod connection_ui;
mod layout;
mod preferences_ui;
mod pages;
mod recent_ui;
mod file_picker;
mod preview;
mod diagnostics_bundle;
mod crash_capture;
mod update_check;
use pages::{Page, SettingsTab, DiagnosticTab, ViewState};

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, Vec2};
use netburrow_core::{Client, Phase, Settings, SingleInstance, Snapshot, Transport};

const WINDOW_TITLE: &str = "NetBurrow";
const BACKGROUND: Color32 = Color32::from_rgb(246, 243, 237);
const SURFACE: Color32 = Color32::from_rgb(237, 233, 224);
const BORDER: Color32 = Color32::from_rgb(216, 210, 200);
const TEXT: Color32 = Color32::from_rgb(52, 49, 45);
const MUTED: Color32 = Color32::from_rgb(117, 111, 102);
const ACCENT: Color32 = Color32::from_rgb(92, 114, 90);

fn main() {
    let smoke_test = std::env::args().any(|argument| argument == "--smoke-test");
    if !smoke_test {
        if let Ok(settings) = netburrow_core::load_settings() {
            netburrow_core::i18n::set_language(settings.language);
        }
    }
    let instance = match if smoke_test {
        Ok(None)
    } else {
        SingleInstance::acquire()
    } {
        Ok(None) if !smoke_test => return,
        Ok(instance) => instance,
        Err(error) => {
            show_error(&netburrow_core::text_format!("无法启动 NetBurrow\n\n{error}", "Could not start NetBurrow\n\n{error}"));
            return;
        }
    };

    if !smoke_test {
        netburrow_core::diagnostics::init("client");
    }
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(Vec2::new(580.0, 720.0))
            .with_min_inner_size(Vec2::new(520.0, 620.0))
            .with_title(WINDOW_TITLE).with_icon(icons::window_icon()),
        ..Default::default()
    };

    if let Err(error) = eframe::run_native(
        WINDOW_TITLE,
        native_options,
        Box::new(move |creation_context| {
            configure_style(&creation_context.egui_ctx);
            Ok(Box::new(NetBurrowApp::new(
                creation_context.egui_ctx.clone(),
                smoke_test,
                instance,
            )))
        }),
    ) {
        show_error(&netburrow_core::text_format!("无法打开 NetBurrow\n\n{error}", "Could not open NetBurrow\n\n{error}"));
    }
}

struct NetBurrowApp {
    instance: Option<SingleInstance>,
    settings: Settings,
    saved_settings: Settings,
    active_settings: Option<Settings>,
    preflight: Option<PendingPreflight>,
    preflight_report: Option<(Settings, netburrow_core::PreflightReport)>,
    startup_enabled: bool,
    update_check: update_check::UpdateCheck,
    update_check_at: Option<Instant>,
    client: Option<Client>,
    last_snapshot: Snapshot,
    notice: Option<String>,
    config_error: Option<String>,
    requires_explicit_save: bool,
    view: ViewState,
    edit_server: bool,
    edit_name: bool,
    diagnostic_export: Option<std::path::PathBuf>,
    diagnostic_bundle: Option<std::sync::mpsc::Receiver<Result<diagnostics_bundle::Bundle, String>>>,
    diagnostic_bundle_automatic: bool,
    crash_capture: crash_capture::Capture,
    crash_capture_confirm: bool,
    pending_crash_bundle: bool,
    crash_bundle_export: Option<std::path::PathBuf>,
    notifications: notifications::Notifications,
    restore_window_pending: bool,
    stop_requested: Arc<AtomicBool>,
    quit_requested: Arc<AtomicBool>,
    smoke_test: Option<Instant>,
}

struct PendingPreflight {
    settings: Settings,
    receiver: std::sync::mpsc::Receiver<netburrow_core::PreflightReport>,
    start_after: bool,
}

impl NetBurrowApp {
    fn new(context: egui::Context, smoke_test: bool, instance: Option<SingleInstance>) -> Self {
        let (mut settings, config_error, requires_explicit_save) = match if smoke_test {
            Ok(Settings::default())
        } else {
            netburrow_core::load_settings()
        } {
            Ok(settings) => (settings, None, false),
            Err(error) => (
                Settings::default(),
                Some(netburrow_core::text_format!("无法读取设置：{error}。请核对并重新保存，联机组不会自动更换。", "Could not read settings: {error}. Review and save them again. Your group will not change automatically."
                )),
                true,
            ),
        };
        if smoke_test && std::env::args().any(|arg| arg == "--preview-language=en") {
            settings.language = netburrow_core::i18n::Language::En;
        }
        netburrow_core::i18n::set_language(settings.language);
        let chinese_font_available = configure_fonts(&context);
        let font_notice = if !chinese_font_available
            && settings.language == netburrow_core::i18n::Language::ZhCn
        {
            // Keep the saved preference; only this session falls back to readable text.
            netburrow_core::i18n::set_language(netburrow_core::i18n::Language::En);
            Some("Chinese system fonts are unavailable. Using English for this session; install Simplified Chinese fonts in Windows to use Chinese.".to_owned())
        } else {
            None
        };
        let stop_requested = Arc::new(AtomicBool::new(false));
        let quit_requested = Arc::new(AtomicBool::new(false));
        let update_check_at = (!smoke_test && !cfg!(test) && settings.auto_check_updates)
            .then(|| Instant::now() + Duration::from_millis(500));
        let mut app = Self {
            instance,
            saved_settings: settings.clone(),
            settings,
            active_settings: None,
            preflight: None,
            preflight_report: None,
            startup_enabled: false,
            update_check: update_check::UpdateCheck::default(),
            update_check_at,
            client: None,
            last_snapshot: Snapshot::default(),
            notice: font_notice,
            config_error,
            requires_explicit_save,
            view: ViewState::default(),
            edit_server: false,
            edit_name: false,
            diagnostic_export: None,
            diagnostic_bundle: None,
            diagnostic_bundle_automatic: false,
            crash_capture: crash_capture::Capture::default(),
            crash_capture_confirm: false,
            pending_crash_bundle: false,
            crash_bundle_export: None,
            notifications: notifications::Notifications::default(),
            restore_window_pending: !smoke_test,
            stop_requested,
            quit_requested,
            smoke_test: smoke_test.then(Instant::now),
        };
        if !smoke_test {
            match startup::is_enabled() {
                Ok(enabled) => app.startup_enabled = enabled,
                Err(error) => app.notice = Some(error),
            }
            if let Err(error) = tray::install(
                context,
                Arc::clone(&app.stop_requested),
                Arc::clone(&app.quit_requested),
            ) {
                app.notice = Some(netburrow_core::text_format!("托盘不可用：{error}", "System tray unavailable: {error}"));
            }
        }
        // Explicit, network-free fixture for inspecting member rows in renderer screenshots.
        if smoke_test && std::env::args().any(|arg| arg == "--preview-members") {
            app.notice = Some(netburrow_core::text!("预览 · 模拟数据", "Preview · Sample data").into());
            app.last_snapshot.peers = [
                (netburrow_core::text!("玩家一", "Player one"), 3, 27, 0),
                (netburrow_core::text!("好友", "Friend"), 1, 52, 0),
                (netburrow_core::text!("玩家三", "Player three"), 3, 88, 15),
            ]
            .into_iter()
            .enumerate()
            .map(
                |(index, (name, phase, ping, age))| netburrow_core::PeerInfo {
                    client_id: index as u64 + 1,
                    game_epoch: 1,
                    steam_id: 0,
                    ready: phase == 3,
                    is_self: index == 0,
                    status: Some(netburrow_core::MemberStatus {
                        name: name.into(),
                        phase,
                        ping_ms: Some(ping),
                        transport: if index == 0 { 2 } else { 0 },
                        sent: 1248,
                        received: 1284,
                    }),
                    status_updated: Instant::now().checked_sub(Duration::from_secs(age)),
                },
            )
            .collect();
        }
        app
    }

    fn start(&mut self) {
        self.begin_preflight(true);
    }

    fn activate_checked(&mut self) {
        if self.client.is_some() {
            return;
        }
        if let Err(error) = self.settings.validate() {
            netburrow_core::diagnostics::record("WARN", "settings validation", &error);
            self.notice = Some(netburrow_core::text_format!("请检查设置：{error}", "Check your settings: {error}"));
            return;
        }
        if let Err(error) = netburrow_core::save_settings(&self.settings) {
            self.notice = Some(netburrow_core::text_format!("设置保存失败，未连接：{error}", "Settings could not be saved. Not connected: {error}"));
            return;
        }
        self.config_error = None;
        self.requires_explicit_save = false;
        self.saved_settings = self.settings.clone();
        match Client::start(self.settings.clone()) {
            Ok(client) => {
                self.client = Some(client);
                self.active_settings = Some(self.settings.clone());
                self.last_snapshot = Snapshot::default();
                let previous = self.settings.recent_connections.clone();
                self.settings.remember_connection();
                self.notice = match netburrow_core::save_recent_connections(&self.settings.recent_connections) {
                    Ok(()) => None,
                    Err(error) => {
                        self.settings.recent_connections = previous;
                        Some(netburrow_core::text_format!("正在连接，最近记录保存失败：{error}", "Connecting, but recent group could not be saved: {error}"))
                    }
                };
            }
            Err(error) => {
                netburrow_core::diagnostics::record("ERROR", "enable failed", &error);
                self.notice = Some(netburrow_core::text_format!("无法连接：{error}", "Could not connect: {error}"));
            }
        }
    }

    fn stop(&mut self) {
        self.crash_capture.stop();
        if self.preflight.take().is_some() { self.notice = Some(netburrow_core::text!("检查已取消", "Check canceled").into()); }
        self.active_settings = None;
        self.notifications.update(Phase::Stopped, false, Instant::now());
        if let Some(mut client) = self.client.take() {
            netburrow_core::diagnostics::record(
                "INFO",
                "stop",
                "user stopped networking / application exiting",
            );
            client.stop();
            self.notice = Some(netburrow_core::text!("已断开。再次连接前，请重开正在运行的游戏。", "Disconnected. Restart any running game before reconnecting.").to_owned());
        }
    }

    fn poll_core(&mut self) {
        self.poll_preflight();
        if let Some(client) = &self.client {
            self.last_snapshot = client.snapshot();
        }
        if let Some(notification) = self.notifications.update(
            self.current_phase(), self.settings.notifications_enabled && self.smoke_test.is_none(), Instant::now(),
        ) {
            if !tray::notify(&notification) {
                netburrow_core::diagnostics::record("WARN", "notification", "Tray notification could not be submitted; see application status");
            }
        }
    }

    fn export_diagnostics(&mut self) {
        self.export_diagnostics_at(false);
    }

    fn diagnostic_snapshot(&self) -> Snapshot {
        let mut snapshot = self.client.as_ref().map_or_else(
            || self.last_snapshot.clone(),
            |client| client.snapshot(),
        );
        if self.client.is_none() {
            snapshot.phase = Phase::Stopped;
            snapshot.detail = netburrow_core::text!("未连接；以下可能包含上次记录", "Disconnected. Data below may be from the previous session.").into();
        }
        snapshot
    }

    fn export_diagnostics_at(&mut self, freeze: bool) {
        let snapshot = self.diagnostic_snapshot();
        let report = if freeze {
            netburrow_core::export_freeze_report(&self.settings, &snapshot)
        } else {
            netburrow_core::export_report(&self.settings, &snapshot)
        };
        match report {
            Ok(path) => {
                self.notice = Some(if freeze {
                    netburrow_core::text!("现场已记录，可分享诊断文件以便排查", "Freeze recorded. Share the diagnostics file with support.")
                } else {
                    netburrow_core::text!("诊断已导出", "Diagnostics exported")
                }.into());
                self.diagnostic_export = Some(path);
            }
            Err(error) => self.notice = Some(error),
        }
    }

    fn current_phase(&self) -> Phase {
        self.client
            .as_ref()
            .map_or(Phase::Stopped, |_| self.last_snapshot.phase)
    }

    fn status_label(&self) -> (&'static str, Color32) {
        match self.current_phase() {
            Phase::Stopped => (netburrow_core::text!("未连接", "Disconnected"), Color32::GRAY),
            Phase::Connecting => (netburrow_core::text!("连接中", "Connecting"), Color32::from_rgb(151, 103, 37)),
            Phase::WaitingForGame => (netburrow_core::text!("等待游戏", "Waiting for game"), Color32::from_rgb(59, 108, 139)),
            Phase::Attaching => (netburrow_core::text!("接入中", "Attaching"), Color32::from_rgb(151, 103, 37)),
            Phase::Ready => (netburrow_core::text!("已就绪", "Ready"), Color32::from_rgb(92, 114, 90)),
            Phase::RestartRequired => (netburrow_core::text!("需重开游戏", "Restart game"), Color32::from_rgb(158, 97, 39)),
            Phase::Failed => (netburrow_core::text!("连接失败", "Connection failed"), Color32::from_rgb(174, 65, 60)),
        }
    }

    fn activity_ui(&mut self, ui: &mut egui::Ui) {
        let show_members = self.client.is_some()
            || (self.smoke_test.is_some() && !self.last_snapshot.peers.is_empty());
        ui.horizontal(|ui| {
            ui.label(RichText::new(netburrow_core::text!("成员", "Members")).font(bold(20.0)));
            ui.label(RichText::new((if show_members { self.last_snapshot.peers.len() } else { 0 }).to_string()).color(MUTED));
        });
        if !show_members || self.last_snapshot.peers.is_empty() {
            ui.add_space(14.0);
            ui.vertical_centered(|ui| {
                icons::members_empty(ui);
                ui.add_space(6.0);
                ui.label(RichText::new(if self.client.is_some() {
                    netburrow_core::text!("等待成员加入", "Waiting for members")
                } else {
                    netburrow_core::text!("暂无成员", "No members")
                }).color(MUTED).size(12.0));
            });
            ui.add_space(14.0);
        } else {
            ui.add_space(4.0);
            egui::ScrollArea::horizontal().id_salt("member-table-scroll").show(ui, |ui| {
            egui::Grid::new("member-status-table")
                .num_columns(4)
                .min_col_width(0.0)
                .spacing(Vec2::new(8.0, 8.0))
                .striped(true)
                .show(ui, |ui| {
                    for title in [netburrow_core::text!("成员", "Member"), netburrow_core::text!("状态", "Status"), netburrow_core::text!("延迟", "Latency"), netburrow_core::text!("传输", "Transport")] {
                        ui.label(RichText::new(title).size(12.0).color(MUTED));
                    }
                    ui.end_row();
                    for peer in &self.last_snapshot.peers {
                        let report = peer.status.as_ref();
                        let name = report
                            .filter(|r| !r.name.is_empty())
                            .map(|r| r.name.clone())
                            .unwrap_or_else(|| netburrow_core::text_format!("成员 {}", "Member {}", peer.client_id));
                        let display = if peer.is_self {
                            netburrow_core::text_format!("{name} · 本机", "{name} · You")
                        } else {
                            name.clone()
                        };
                        ui.add_sized(
                            [134.0, 22.0],
                            egui::Label::new(RichText::new(display).size(13.0)).truncate(),
                        )
                        .on_hover_text(netburrow_core::text_format!("{name}\n连接编号 #{}", "{name}\nConnection #{}", peer.client_id));
                        let stale = report.is_some() && peer.status_is_stale();
                        let (label, color) = if stale {
                            (netburrow_core::text!("已过期", "Stale"), Color32::from_rgb(151, 103, 37))
                        } else if let Some(report) = report {
                            member_phase(report.phase)
                        } else if peer.ready {
                            (netburrow_core::text!("等待数据", "Pending"), MUTED)
                        } else {
                            (netburrow_core::text!("等待游戏", "Waiting for game"), MUTED)
                        };
                        ui.label(RichText::new(label).size(12.0).color(color));
                        let ping = if stale {
                            "—".to_owned()
                        } else {
                            report
                                .and_then(|r| r.ping_ms)
                                .map_or_else(|| "—".into(), |ms| format!("{ms} ms"))
                        };
                        ui.label(RichText::new(ping).size(13.0))
                            .on_hover_text(netburrow_core::text!("成员到服务器的往返延迟，不代表玩家间延迟", "Round-trip latency from this member to the server, not between players"));
                        let transport = if stale {
                            "—"
                        } else {
                            match report.map(|r| r.transport) {
                                Some(0) => "TCP",
                                Some(1) => netburrow_core::text!("UDP 待连接", "UDP pending"),
                                Some(2) => "UDP + TCP",
                                _ => netburrow_core::text!("等待数据", "Pending"),
                            }
                        };
                        ui.label(RichText::new(transport).size(12.0))
                            .on_hover_text(netburrow_core::text!("不可靠消息使用 UDP，可靠消息使用 TCP", "UDP for unreliable messages; TCP for reliable messages"));
                        ui.end_row();
                    }
                });
            });
        }
        let detail = self.last_snapshot.detail.trim();
        let show_detail = self.client.is_some() && !detail.is_empty();
        let finished = self.client.as_ref().is_some_and(Client::is_finished);
        let show_group_hint = self.client.is_some() && !finished
            && self.last_snapshot.peers.len() <= 1
            && matches!(self.current_phase(), Phase::WaitingForGame | Phase::Ready);
        if show_detail || finished || show_group_hint {
            ui.add_space(8.0);
            if show_detail {
                let text = detail;
                ui.add(egui::Label::new(RichText::new(text).size(12.0).color(TEXT)).wrap());
            }
            if show_group_hint {
                ui.add(egui::Label::new(RichText::new(netburrow_core::text!("未看到朋友？核对服务器和完整组码。", "Missing a friend? Check your server and full group code."))
                    .size(12.0).color(MUTED)).wrap());
            }
            if finished {
                ui.label(RichText::new(netburrow_core::text!("请断开，处理问题后重连", "Disconnect, resolve the issue, then reconnect")).size(12.0).color(Color32::from_rgb(151, 103, 37)));
            }
            if matches!(self.current_phase(), Phase::Connecting | Phase::RestartRequired | Phase::Failed) {
                if icons::button(ui, icons::Action::Log, netburrow_core::text!("查看日志", "View logs")).clicked() {
                    self.view.page = Page::Diagnostics;
                }
            }
        }
    }

}

impl eframe::App for NetBurrowApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.smoke_test.is_none() {
            if let Some(placement) = window_state::capture() {
                if let Err(error) = netburrow_core::save_window_placement(placement) {
                    netburrow_core::diagnostics::record("WARN", "window placement", &error);
                }
            }
            tray::shutdown();
        }
    }
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        if std::mem::take(&mut self.restore_window_pending) {
            netburrow_core::diagnostics::record("INFO", "window", "restoring saved placement");
            if let Some(placement) = &self.settings.window_placement {
                if let Err(error) = window_state::restore(placement) {
                    netburrow_core::diagnostics::record("WARN", "window restore", &error);
                    self.notice = Some(error);
                }
            }
            netburrow_core::diagnostics::record("INFO", "window", "placement initialization complete");
            if self.settings.start_minimized { context.send_viewport_cmd(egui::ViewportCommand::Minimized(true)); }
        }
        if self.instance.as_ref().is_some_and(SingleInstance::activation_requested) {
            context.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            context.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            context.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        if self.stop_requested.swap(false, Ordering::AcqRel) {
            self.stop();
        }
        if self.quit_requested.load(Ordering::Acquire) {
            self.stop();
        }
        self.poll_core();
        self.poll_diagnostic_bundle();
        self.poll_crash_capture(context);
        if self.update_check_at.is_some_and(|at| Instant::now() >= at) {
            self.update_check_at = None;
            self.update_check.begin();
        }
        self.update_check.poll();
        self.preview_tick(context);

        if self
            .smoke_test
            .is_some_and(|started| started.elapsed() >= Duration::from_secs(3))
        {
            self.quit_requested.store(true, Ordering::Release);
            context.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if context.input(|input| input.viewport().close_requested())
            && self.settings.minimize_on_close
            && !self.quit_requested.load(Ordering::Acquire)
            && self.smoke_test.is_none()
        {
            context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            context.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        context.request_repaint_after(Duration::from_millis(400));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.main_ui(ui);
    }
}

impl Drop for NetBurrowApp {
    fn drop(&mut self) {
        self.stop();
    }
}

fn member_phase(phase: u8) -> (&'static str, Color32) {
    match phase {
        0 => (netburrow_core::text!("连接中", "Connecting"), MUTED),
        1 => (netburrow_core::text!("等待游戏", "Waiting for game"), Color32::from_rgb(59, 108, 139)),
        2 => (netburrow_core::text!("接入中", "Attaching"), Color32::from_rgb(151, 103, 37)),
        3 => (netburrow_core::text!("已就绪", "Ready"), ACCENT),
        4 => (netburrow_core::text!("需重开", "Restart needed"), Color32::from_rgb(151, 103, 37)),
        5 => (netburrow_core::text!("连接失败", "Connection failed"), Color32::from_rgb(174, 65, 60)),
        _ => (netburrow_core::text!("未连接", "Disconnected"), MUTED),
    }
}
fn configure_style(context: &egui::Context) {
    // Fix the theme as well as its palette: OS light-mode events must not replace half the UI.
    context.set_theme(egui::ThemePreference::Light);
    context.all_styles_mut(|style| {
        let mut visuals = egui::Visuals::light();
        visuals.override_text_color = Some(TEXT);
        visuals.weak_text_color = Some(MUTED);
        visuals.panel_fill = BACKGROUND;
        visuals.window_fill = SURFACE;
        visuals.extreme_bg_color = BACKGROUND;
        visuals.text_edit_bg_color = Some(Color32::from_rgb(252, 250, 246));
        visuals.faint_bg_color = SURFACE;
        visuals.window_corner_radius = egui::CornerRadius::same(12);
        visuals.window_stroke = egui::Stroke::new(1.0, BORDER);
        visuals.selection.bg_fill = Color32::from_rgb(217, 227, 211);
        visuals.selection.stroke = egui::Stroke::new(1.0, ACCENT);
        visuals.hyperlink_color = ACCENT;
        visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, BORDER);
        visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, TEXT);
        for (widget, fill) in [
            (&mut visuals.widgets.inactive, Color32::from_rgb(237, 233, 224)),
            (&mut visuals.widgets.hovered, Color32::from_rgb(227, 223, 213)),
            (&mut visuals.widgets.active, Color32::from_rgb(213, 221, 207)),
            (&mut visuals.widgets.open, Color32::from_rgb(227, 230, 219)),
        ] {
            widget.bg_fill = fill;
            widget.weak_bg_fill = fill;
            widget.bg_stroke = egui::Stroke::new(1.0, BORDER);
            widget.fg_stroke = egui::Stroke::new(1.0, TEXT);
            widget.corner_radius = egui::CornerRadius::same(7);
        }
        style.visuals = visuals;
        style.spacing.item_spacing = Vec2::new(9.0, 6.0);
        style.spacing.button_padding = Vec2::new(12.0, 6.0);
        style.spacing.interact_size = Vec2::new(36.0, 30.0);
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(15.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(13.0));
        style
            .text_styles
            .insert(egui::TextStyle::Small, egui::FontId::proportional(12.0));
    });
}
fn bold(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name("ui-bold".into()))
}

#[cfg(windows)]
fn system_font_directory() -> Option<std::path::PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;

    let mut buffer = vec![0u16; 260];
    let mut length = unsafe { GetSystemWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    if length as usize >= buffer.len() {
        buffer.resize(length as usize, 0);
        length = unsafe { GetSystemWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    }
    if length == 0 || length as usize >= buffer.len() {
        return None;
    }
    Some(std::path::PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length as usize])).join("Fonts"))
}

#[cfg(not(windows))]
fn system_font_directory() -> Option<std::path::PathBuf> {
    None
}

/// Returns whether a Chinese font was loaded, so startup can avoid missing glyphs.
fn configure_fonts(context: &egui::Context) -> bool {
    let mut fonts = egui::FontDefinitions::default();
    let mut regular = Vec::new();
    let mut heavy = Vec::new();
    let directory = system_font_directory();
    let mut chinese_font_available = false;
    for (name, file, is_bold) in [
        ("latin", "segoeui.ttf", false),
        ("cjk", "msyh.ttc", false),
        ("cjk-fallback", "simsun.ttc", false),
        ("latin-bold", "segoeuib.ttf", true),
        ("cjk-bold", "msyhbd.ttc", true),
    ] {
        let Some(directory) = &directory else { break };
        if name == "cjk-fallback" && chinese_font_available { continue; }
        if let Ok(bytes) = std::fs::read(directory.join(file)) {
            chinese_font_available |= name == "cjk" || name == "cjk-fallback";
            fonts.font_data.insert(name.into(), Arc::new(egui::FontData::from_owned(bytes)));
            if is_bold { heavy.push(name.to_owned()); } else { regular.push(name.to_owned()); }
        }
    }
    let fallback = fonts.families[&egui::FontFamily::Proportional].clone();
    regular.extend(fallback);
    heavy.extend(regular.clone());
    fonts.families.entry(egui::FontFamily::Monospace).or_default().extend(regular.clone());
    fonts.families.insert(egui::FontFamily::Proportional, regular);
    fonts.families.insert(egui::FontFamily::Name("ui-bold".into()), heavy);
    context.set_fonts(fonts);
    chinese_font_available
}

#[cfg(windows)]
fn show_error(message: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    let title: Vec<u16> = WINDOW_TITLE.encode_utf16().chain(Some(0)).collect();
    let body: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let _ = MessageBoxW(
            std::ptr::null_mut(),
            body.as_ptr(),
            title.as_ptr(),
            MB_ICONERROR | MB_OK,
        );
    }
}

#[cfg(not(windows))]
fn show_error(message: &str) {
    eprintln!("{message}");
}
