#![cfg_attr(windows, windows_subsystem = "windows")]

mod clipboard;
mod tray;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, Vec2};
use netburrow_core::{Client, Phase, Settings, SingleInstance, Snapshot, Transport};

const WINDOW_TITLE: &str = "NetBurrow";
const BACKGROUND: Color32 = Color32::from_rgb(20, 26, 35);
const SURFACE: Color32 = Color32::from_rgb(29, 38, 50);
const BORDER: Color32 = Color32::from_rgb(49, 63, 80);
const TEXT: Color32 = Color32::from_rgb(226, 233, 242);
const MUTED: Color32 = Color32::from_rgb(157, 174, 194);
const ACCENT: Color32 = Color32::from_rgb(114, 215, 190);

fn main() {
    let smoke_test = std::env::args().any(|argument| argument == "--smoke-test");
    let _instance = match if smoke_test {
        Ok(None)
    } else {
        SingleInstance::acquire().map(Some)
    } {
        Ok(instance) => instance,
        Err(error) => {
            show_error(&format!("NetBurrow 已在运行。\n\n{error}"));
            return;
        }
    };

    if !smoke_test {
        netburrow_core::diagnostics::init("client");
    }
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(Vec2::new(640.0, 820.0))
            .with_min_inner_size(Vec2::new(580.0, 720.0))
            .with_title(WINDOW_TITLE),
        ..Default::default()
    };

    if let Err(error) = eframe::run_native(
        WINDOW_TITLE,
        native_options,
        Box::new(move |creation_context| {
            configure_fonts(&creation_context.egui_ctx);
            configure_style(&creation_context.egui_ctx);
            Ok(Box::new(NetBurrowApp::new(
                creation_context.egui_ctx.clone(),
                smoke_test,
            )))
        }),
    ) {
        show_error(&format!("无法启动 NetBurrow 窗口。\n\n{error}"));
    }
}

struct NetBurrowApp {
    settings: Settings,
    client: Option<Client>,
    last_snapshot: Snapshot,
    notice: Option<String>,
    config_error: Option<String>,
    requires_explicit_save: bool,
    show_logs: bool,
    stop_requested: Arc<AtomicBool>,
    quit_requested: Arc<AtomicBool>,
    smoke_test: Option<Instant>,
}

impl NetBurrowApp {
    fn new(context: egui::Context, smoke_test: bool) -> Self {
        let (settings, config_error, requires_explicit_save) = match if smoke_test {
            Ok(Settings::default())
        } else {
            netburrow_core::load_settings()
        } {
            Ok(settings) => (settings, None, false),
            Err(error) => (
                Settings::default(),
                Some(format!(
                    "本机配置无法读取：{error}。请核对后重新填写并保存；不会自动更换联机组。"
                )),
                true,
            ),
        };
        let stop_requested = Arc::new(AtomicBool::new(false));
        let quit_requested = Arc::new(AtomicBool::new(false));
        let mut app = Self {
            settings,
            client: None,
            last_snapshot: Snapshot::default(),
            notice: None,
            config_error,
            requires_explicit_save,
            show_logs: false,
            stop_requested,
            quit_requested,
            smoke_test: smoke_test.then(Instant::now),
        };
        if !smoke_test {
            if let Err(error) = tray::install(
                context,
                Arc::clone(&app.stop_requested),
                Arc::clone(&app.quit_requested),
            ) {
                app.notice = Some(format!("托盘不可用：{error}"));
            }
        }
        // Explicit, network-free fixture for inspecting member rows in renderer screenshots.
        if smoke_test && std::env::args().any(|arg| arg == "--preview-members") {
            app.notice = Some("界面预览：以下成员及数值均为模拟数据".into());
            app.last_snapshot.peers = [
                ("本机示例", 3, 27, 0),
                ("好友示例", 1, 52, 0),
                ("过期示例", 3, 88, 15),
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
        if self.client.is_some() {
            return;
        }
        if let Err(error) = self.settings.validate() {
            netburrow_core::diagnostics::record("WARN", "settings validation", &error);
            self.notice = Some(format!("请先修正设置：{error}"));
            return;
        }
        if let Err(error) = netburrow_core::save_settings(&self.settings) {
            self.notice = Some(format!("无法保存设置，未启用联机：{error}"));
            return;
        }
        self.config_error = None;
        self.requires_explicit_save = false;
        match Client::start(self.settings.clone()) {
            Ok(client) => {
                self.client = Some(client);
                self.notice = None;
            }
            Err(error) => {
                netburrow_core::diagnostics::record("ERROR", "enable failed", &error);
                self.notice = Some(format!("无法启用联机：{error}"));
            }
        }
    }

    fn stop(&mut self) {
        if let Some(mut client) = self.client.take() {
            netburrow_core::diagnostics::record(
                "INFO",
                "stop",
                "user stopped networking / application exiting",
            );
            client.stop();
            self.notice = Some("联机已停止。若游戏已在运行，请重开游戏后再启用。".to_owned());
        }
    }

    fn save_only(&mut self) {
        if let Err(error) = self.settings.validate() {
            self.notice = Some(format!("请先修正设置：{error}"));
            return;
        }
        match netburrow_core::save_settings(&self.settings) {
            Ok(()) => {
                self.config_error = None;
                self.requires_explicit_save = false;
                self.notice = Some("设置已保存在本机。".to_owned());
            }
            Err(error) => self.notice = Some(format!("无法保存设置：{error}")),
        }
    }

    fn poll_core(&mut self) {
        if let Some(client) = &self.client {
            self.last_snapshot = client.snapshot();
            if client.is_finished() {
                self.notice = Some("联机服务已结束，请查看状态或重新启用。".to_owned());
            }
        }
    }

    fn current_phase(&self) -> Phase {
        self.client
            .as_ref()
            .map_or(Phase::Stopped, |_| self.last_snapshot.phase)
    }

    fn status_label(&self) -> (&'static str, Color32) {
        match self.current_phase() {
            Phase::Stopped => ("未启用", Color32::GRAY),
            Phase::Connecting => ("正在连接服务端", Color32::from_rgb(236, 184, 64)),
            Phase::WaitingForGame => ("等待游戏启动", Color32::from_rgb(103, 181, 226)),
            Phase::Attaching => ("正在接入游戏", Color32::from_rgb(236, 184, 64)),
            Phase::Ready => ("联机已就绪", Color32::from_rgb(104, 201, 128)),
            Phase::RestartRequired => ("需要重开游戏", Color32::from_rgb(238, 150, 83)),
            Phase::Failed => ("连接失败", Color32::from_rgb(234, 101, 101)),
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("连接设置").strong().size(17.0));
        ui.add_space(10.0);
        ui.label("服务器地址");
        ui.add_sized(
            [ui.available_width(), 34.0],
            egui::TextEdit::singleline(&mut self.settings.server)
                .hint_text("relay.example.com:24872"),
        );
        ui.add_space(8.0);
        ui.label("联机组");
        ui.add_sized(
            [ui.available_width(), 34.0],
            egui::TextEdit::singleline(&mut self.settings.group)
                .hint_text("粘贴朋友的组码，或在下方创建一个"),
        );
        ui.horizontal(|ui| {
            if ui.button("创建联机组").clicked() {
                match netburrow_core::new_group() {
                    Ok(group) => {
                        self.settings.group = group;
                        self.notice = Some("已创建新的联机组；请保存后再分享。".to_owned());
                    }
                    Err(error) => self.notice = Some(format!("无法创建联机组：{error}")),
                }
            }
            if ui.button("复制").clicked() {
                ui.ctx().copy_text(self.settings.group.clone());
                self.notice = Some("联机组已复制到剪贴板。".to_owned());
            }
            if ui.button("粘贴").clicked() {
                match clipboard::read_text() {
                    Ok(group) if group.is_empty() => {
                        self.notice = Some("剪贴板中的联机组为空。".to_owned())
                    }
                    Ok(group) => {
                        self.settings.group = group;
                        self.notice = Some("已粘贴联机组，保存后生效。".to_owned());
                    }
                    Err(error) => self.notice = Some(error),
                }
            }
        });
        ui.add_space(8.0);
        ui.collapsing("游戏与传输设置", |ui| {
            ui.label("显示名（可选，同组成员可见）");
            ui.add_enabled(self.client.is_none(), egui::TextEdit::singleline(&mut self.settings.display_name)
                .hint_text("留空时显示成员编号").char_limit(24).desired_width(f32::INFINITY));
            ui.add_space(8.0);
            ui.add_enabled(self.client.is_none(), egui::Checkbox::new(
                &mut self.settings.allow_late_hook, "允许中途 Hook（实验性）"));
            if self.settings.allow_late_hook {
                ui.label(RichText::new("仅在主菜单、尚未联机时启用。工具无法判断是否已进入对局；已有回调可能无法接管，失败后需重启游戏。")
                    .size(12.0).color(Color32::from_rgb(238, 184, 120)));
            }
            ui.label(RichText::new("启用联机前设置；关闭时只接入之后启动的游戏。").size(12.0).color(MUTED));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label("游戏路径");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("自动检测").clicked() {
                        match netburrow_core::autodetect_game() {
                            Some(path) => {
                                self.settings.game_path = path.display().to_string();
                                self.notice =
                                    Some("已回显检测到的游戏路径，请保存确认。".to_owned());
                            }
                            None => {
                                self.notice =
                                    Some("未找到游戏，请输入游戏 exe 完整路径。".to_owned())
                            }
                        }
                    }
                });
            });
            ui.add_sized(
                [ui.available_width(), 34.0],
                egui::TextEdit::singleline(&mut self.settings.game_path)
                    .hint_text("选择 isaac-ng.exe 的完整路径"),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label("传输方式");
                ui.selectable_value(&mut self.settings.transport, Transport::Tcp, "TCP · 默认");
                ui.selectable_value(&mut self.settings.transport, Transport::Udp, "UDP 优先");
            });
            ui.label(
                RichText::new("UDP 优先模式下，可靠消息仍通过 TCP 发送。")
                    .size(12.0)
                    .color(MUTED),
            );
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button("保存设置").clicked() {
                self.save_only();
            }
            if ui.button("查看日志").clicked() {
                self.show_logs = true;
            }
            if self.requires_explicit_save {
                ui.colored_label(Color32::from_rgb(238, 184, 84), "请重新填写并保存后再启用");
            }
        });
    }

    fn activity_ui(&self, ui: &mut egui::Ui) {
        let (status, color) = self.status_label();
        ui.horizontal(|ui| {
            ui.label(RichText::new("联机状态").strong().size(17.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("●  {status}")).color(color));
            });
        });
        ui.add_space(6.0);
        let detail = if self.client.is_none() && self.settings.allow_late_hook {
            "中途 Hook 已允许：请停在游戏主菜单，再启用联机。"
        } else if self.client.is_none() {
            "启用工具后，再从 Steam 启动游戏。"
        } else {
            &self.last_snapshot.detail
        };
        ui.label(RichText::new(detail).size(13.0).color(MUTED));
        ui.add_space(10.0);
        ui.columns(3, |columns| {
            let ping = self
                .last_snapshot
                .ping_ms
                .map_or_else(|| "—".to_owned(), |v| format!("{v} ms"));
            for (column, (title, value)) in columns.iter_mut().zip([
                ("发送数据包", self.last_snapshot.sent.to_string()),
                ("接收数据包", self.last_snapshot.received.to_string()),
                ("本机 → Relay", ping),
            ]) {
                column.label(RichText::new(title).size(12.0).color(MUTED));
                column.label(RichText::new(value).size(23.0).strong());
            }
        });
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);
        ui.label(
            RichText::new(format!("同组成员 · {} 人", self.last_snapshot.peers.len())).strong(),
        );
        if self.last_snapshot.peers.is_empty() {
            ui.label(
                RichText::new("启用联机后显示同组成员")
                    .color(MUTED)
                    .size(12.0),
            );
        } else {
            ui.add_space(4.0);
            egui::Grid::new("member-status-table")
                .num_columns(5)
                .min_col_width(0.0)
                .spacing(Vec2::new(8.0, 8.0))
                .striped(true)
                .show(ui, |ui| {
                    for title in ["成员", "状态", "延迟", "传输", "收 / 发（包）"] {
                        ui.label(RichText::new(title).size(12.0).color(MUTED));
                    }
                    ui.end_row();
                    for peer in &self.last_snapshot.peers {
                        let report = peer.status.as_ref();
                        let name = report
                            .filter(|r| !r.name.is_empty())
                            .map(|r| r.name.clone())
                            .unwrap_or_else(|| format!("成员 {}", peer.client_id));
                        let display = if peer.is_self {
                            format!("{name} · 本机")
                        } else {
                            name.clone()
                        };
                        ui.add_sized(
                            [134.0, 22.0],
                            egui::Label::new(RichText::new(display).size(13.0)).truncate(),
                        )
                        .on_hover_text(format!("{name}\n连接编号 #{}", peer.client_id));
                        let stale = report.is_some() && peer.status_is_stale();
                        let (label, color) = if stale {
                            ("数据过期", Color32::from_rgb(238, 184, 120))
                        } else if let Some(report) = report {
                            member_phase(report.phase)
                        } else if peer.ready {
                            ("待上报", MUTED)
                        } else {
                            ("等待游戏", MUTED)
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
                            .on_hover_text("该成员上报的到 Relay 往返延迟，不是玩家之间的延迟");
                        let transport = if stale {
                            "—"
                        } else {
                            match report.map(|r| r.transport) {
                                Some(0) => "TCP",
                                Some(1) => "UDP 待绑定",
                                Some(2) => "UDP + TCP",
                                _ => "待上报",
                            }
                        };
                        ui.label(RichText::new(transport).size(12.0))
                            .on_hover_text("UDP + TCP：不可靠消息使用 UDP，可靠消息仍通过 TCP");
                        let traffic = if stale {
                            "—".to_owned()
                        } else {
                            report.map_or_else(
                                || "—".into(),
                                |r| {
                                    format!(
                                        "{} / {}",
                                        packet_count(r.received),
                                        packet_count(r.sent)
                                    )
                                },
                            )
                        };
                        ui.label(RichText::new(traffic).size(12.0))
                            .on_hover_text("从该成员本次启用工具开始累计；不是每秒速度");
                        ui.end_row();
                    }
                });
        }
        ui.add_space(4.0);
        ui.label(
            RichText::new("延迟 = 每人到 Relay 的往返时间 · 约 3 秒更新 · 超过 10 秒标记过期")
                .size(11.0)
                .color(MUTED),
        );
        if self.settings.transport == Transport::Udp {
            ui.label(
                RichText::new(format!(
                    "UDP 发送 {} / 接收 {}",
                    self.last_snapshot.udp_sent, self.last_snapshot.udp_received
                ))
                .size(12.0)
                .color(MUTED),
            );
        }
    }
    fn logs_ui(&mut self, context: &egui::Context) {
        egui::Window::new("运行日志")
            .open(&mut self.show_logs)
            .default_width(520.0)
            .default_height(280.0)
            .show(context, |ui| {
                ui.small("下方为最近状态；详细记录保存在日志文件夹。");
                ui.label(
                    netburrow_core::diagnostics::directory()
                        .display()
                        .to_string(),
                );
                if ui.button("打开日志文件夹").clicked() {
                    let folder = netburrow_core::diagnostics::directory();
                    let result = std::fs::create_dir_all(&folder).and_then(|_| {
                        std::process::Command::new("explorer.exe")
                            .arg(&folder)
                            .spawn()
                            .map(|_| ())
                    });
                    if let Err(error) = result {
                        self.notice = Some(format!("无法打开日志文件夹：{error}"));
                    }
                }
                ui.small(
                    "client.log / injector.log / hook.log；每份 2 MB，保留一份 previous.log。",
                );
                if let Some(error) = netburrow_core::diagnostics::last_error() {
                    ui.colored_label(Color32::from_rgb(238, 150, 83), error);
                }
                ui.separator();
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        if self.last_snapshot.logs.is_empty() {
                            ui.small("暂无日志。启用联机后会显示连接和游戏接入状态。");
                        } else {
                            for line in &self.last_snapshot.logs {
                                ui.monospace(line);
                            }
                        }
                    });
            });
    }
}

impl eframe::App for NetBurrowApp {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        if self.stop_requested.swap(false, Ordering::AcqRel) {
            self.stop();
        }
        if self.quit_requested.load(Ordering::Acquire) {
            self.stop();
        }
        self.poll_core();

        if self
            .smoke_test
            .is_some_and(|started| started.elapsed() >= Duration::from_secs(3))
        {
            self.quit_requested.store(true, Ordering::Release);
            context.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if context.input(|input| input.viewport().close_requested())
            && !self.quit_requested.load(Ordering::Acquire)
            && self.smoke_test.is_none()
        {
            context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            context.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        context.request_repaint_after(Duration::from_millis(400));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        egui::Panel::bottom("primary-action")
            .exact_size(108.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(BACKGROUND)
                    .inner_margin(egui::Margin::symmetric(24, 14)),
            )
            .show(ui, |ui| {
                let active = self.client.is_some();
                let label = if active {
                    "停止联机"
                } else {
                    "启用联机"
                };
                let fill = if active {
                    Color32::from_rgb(83, 54, 60)
                } else {
                    ACCENT
                };
                let foreground = if active { TEXT } else { BACKGROUND };
                let button =
                    egui::Button::new(RichText::new(label).size(16.0).strong().color(foreground))
                        .fill(fill)
                        .stroke(egui::Stroke::NONE)
                        .corner_radius(10);
                if ui.add_sized([ui.available_width(), 46.0], button).clicked() {
                    if active {
                        self.stop();
                    } else {
                        self.start();
                    }
                }
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new("关闭窗口后保留在托盘，退出工具才会停止联机")
                            .size(12.0)
                            .color(MUTED),
                    );
                });
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BACKGROUND).inner_margin(24))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            egui::Frame::new()
                                .fill(ACCENT)
                                .corner_radius(12)
                                .inner_margin(10)
                                .show(ui, |ui| {
                                    ui.label(
                                        RichText::new("NB").size(21.0).strong().color(BACKGROUND),
                                    );
                                });
                            ui.add_space(6.0);
                            ui.vertical(|ui| {
                                ui.label(RichText::new("NetBurrow").size(25.0).strong());
                                ui.label(
                                    RichText::new("以撒的结合：忏悔+  /  好友联机")
                                        .size(12.0)
                                        .color(MUTED),
                                );
                            });
                        });
                        ui.add_space(20.0);
                        if let Some(message) = self.config_error.as_ref().or(self.notice.as_ref()) {
                            egui::Frame::new()
                                .fill(Color32::from_rgb(37, 53, 65))
                                .corner_radius(8)
                                .inner_margin(12)
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.label(RichText::new(message).color(TEXT).size(13.0));
                                });
                            ui.add_space(12.0);
                        }
                        if self.client.is_some()
                            || (self.smoke_test.is_some() && !self.last_snapshot.peers.is_empty())
                        {
                            card().show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                self.activity_ui(ui);
                            });
                            ui.add_space(14.0);
                            card().show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.collapsing("连接设置与日志", |ui| self.settings_ui(ui));
                            });
                        } else {
                            card().show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                self.settings_ui(ui);
                            });
                            ui.add_space(14.0);
                            card().show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                self.activity_ui(ui);
                            });
                        }
                    });
            });
        if self.show_logs {
            self.logs_ui(&context);
        }
    }
}

impl Drop for NetBurrowApp {
    fn drop(&mut self) {
        self.stop();
    }
}

fn member_phase(phase: u8) -> (&'static str, Color32) {
    match phase {
        0 => ("连接中", MUTED),
        1 => ("等待游戏", Color32::from_rgb(103, 181, 226)),
        2 => ("接入中", Color32::from_rgb(238, 184, 120)),
        3 => ("已就绪", ACCENT),
        4 => ("需要重开", Color32::from_rgb(238, 184, 120)),
        5 => ("连接失败", Color32::from_rgb(234, 101, 101)),
        _ => ("已停止", MUTED),
    }
}
fn packet_count(count: u64) -> String {
    if count >= 1_000_000_000 {
        "≥1B".into()
    } else if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 10_000 {
        format!("{:.1}K", count as f64 / 1_000.0)
    } else {
        count.to_string()
    }
}

fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(SURFACE)
        .stroke(egui::Stroke::new(1.0, BORDER))
        .corner_radius(14)
        .inner_margin(18)
}

fn configure_style(context: &egui::Context) {
    // Fix the theme as well as its palette: OS light-mode events must not replace half the UI.
    context.set_theme(egui::ThemePreference::Dark);
    context.all_styles_mut(|style| {
        let mut visuals = egui::Visuals::dark();
        visuals.override_text_color = Some(TEXT);
        visuals.weak_text_color = Some(MUTED);
        visuals.panel_fill = BACKGROUND;
        visuals.window_fill = SURFACE;
        visuals.extreme_bg_color = BACKGROUND;
        visuals.text_edit_bg_color = Some(Color32::from_rgb(19, 27, 38));
        visuals.faint_bg_color = SURFACE;
        visuals.window_corner_radius = egui::CornerRadius::same(12);
        visuals.window_stroke = egui::Stroke::new(1.0, BORDER);
        visuals.selection.bg_fill = Color32::from_rgb(47, 89, 84);
        visuals.selection.stroke = egui::Stroke::new(1.0, ACCENT);
        visuals.hyperlink_color = ACCENT;
        visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, BORDER);
        visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, TEXT);
        for (widget, fill) in [
            (&mut visuals.widgets.inactive, Color32::from_rgb(38, 50, 65)),
            (&mut visuals.widgets.hovered, Color32::from_rgb(51, 68, 85)),
            (&mut visuals.widgets.active, Color32::from_rgb(43, 80, 77)),
            (&mut visuals.widgets.open, Color32::from_rgb(43, 62, 77)),
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
            .insert(egui::TextStyle::Body, egui::FontId::proportional(14.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(13.0));
        style
            .text_styles
            .insert(egui::TextStyle::Small, egui::FontId::proportional(12.0));
    });
}
fn configure_fonts(context: &egui::Context) {
    let candidates = [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyhbd.ttc",
        r"C:\Windows\Fonts\simhei.ttf",
    ];
    let Some(bytes) = candidates
        .iter()
        .find_map(|path| std::fs::read(Path::new(path)).ok())
    else {
        return;
    };

    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "windows-ui".to_owned(),
        std::sync::Arc::new(egui::FontData::from_owned(bytes)),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, "windows-ui".to_owned());
    }
    context.set_fonts(fonts);
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
