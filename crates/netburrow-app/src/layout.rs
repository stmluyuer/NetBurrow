use super::*;

impl NetBurrowApp {
    pub(super) fn main_ui(&mut self, ui: &mut egui::Ui) {
        // Each page fades in when you navigate to it.
        let entered = widgets::appear(ui.ctx(), egui::Id::new("page-fade"), self.view.page as u8, 0.18);
        ui.multiply_opacity(entered);
        if self.view.page != Page::Home {
            self.secondary_ui(ui);
            return;
        }
        egui::Panel::bottom("home-footer")
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(BACKGROUND)
                    .inner_margin(egui::Margin::symmetric(PAGE_MARGIN, 8)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if widgets::button(
                        ui,
                        widgets::Kind::Ghost,
                        Some(icons::Action::Log),
                        netburrow_core::text!("日志与诊断", "Diagnostics"),
                        Vec2::new(0.0, 32.0),
                    )
                    .clicked()
                    {
                        self.view.page = Page::Diagnostics;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(release) = self.update_check.newer_release() {
                            if ui.link(netburrow_core::text_format!("新版本 v{} · 查看", "Update v{} available · View", release.version)).clicked() {
                                self.open_settings();
                                self.view.settings_tab = SettingsTab::About;
                            }
                        } else {
                            ui.label(
                                RichText::new(concat!("v", env!("CARGO_PKG_VERSION")))
                                    .size(12.0)
                                    .color(SUBTLE),
                            );
                        }
                    });
                });
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BACKGROUND).inner_margin(PAGE_MARGIN))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // Keep cards clear of the floating scroll bar.
                        egui::Frame::new()
                            .inner_margin(egui::Margin { right: 10, ..Default::default() })
                            .show(ui, |ui| self.home_content(ui));
                    });
            });
    }

    fn home_content(&mut self, ui: &mut egui::Ui) {
        self.header_ui(ui);
        ui.add_space(16.0);
        self.status_card(ui);
        self.notice_ui(ui);
        ui.add_space(20.0);
        self.group_header(ui);
        ui.add_space(6.0);
        widgets::card(Tone::Neutral)
            .inner_margin(egui::Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                self.group_field(ui);
                ui.add_space(4.0);
                self.profile_rows(ui);
            });
        ui.add_space(20.0);
        self.members_ui(ui);
        ui.add_space(8.0);
    }

    fn header_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            art::show(ui, art::Art::Logo, Vec2::new(56.0, 44.0), TEXT);
            ui.add_space(2.0);
            ui.vertical(|ui| {
                ui.label(RichText::new("NetBurrow").font(bold(25.0)));
                ui.label(
                    RichText::new(netburrow_core::text!("以撒好友联机", "Isaac co-op with friends"))
                        .size(13.0)
                        .color(MUTED),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::icon_button(ui, icons::Action::Settings, netburrow_core::text!("设置", "Settings"))
                    .clicked()
                {
                    self.open_settings();
                }
            });
        });
    }

    /// The state-driven hero: illustration, status and the main on/off action in one card.
    fn status_card(&mut self, ui: &mut egui::Ui) {
        // Preview captures fake a phase without a client, so treat that as connected too.
        let active = self.client.is_some()
            || self.view.preview_phase.is_some_and(|phase| phase != Phase::Stopped);
        let checking = self.preflight.is_some();
        let phase = self.current_phase();
        let (status, tone) = if checking {
            (netburrow_core::text!("正在检查…", "Checking…"), Tone::Neutral)
        } else {
            self.status_label()
        };
        let (illustration, title) = if checking {
            (art::Art::Connecting, status)
        } else {
            match phase {
                Phase::Stopped => (art::Art::Idle, netburrow_core::text!("联机未启用", "You're offline")),
                Phase::Connecting | Phase::Attaching => (art::Art::Connecting, status),
                Phase::WaitingForGame => (art::Art::Waiting, status),
                Phase::Ready => (art::Art::Connected, status),
                Phase::RestartRequired | Phase::Failed => (art::Art::Error, status),
            }
        };
        let detail: String = if checking {
            if self.preflight.as_ref().is_some_and(|pending| pending.start_after) {
                netburrow_core::text!("检查通过后自动启用联机", "Goes online once the check passes")
            } else {
                netburrow_core::text!("正在检查服务器与游戏设置", "Checking server and game settings")
            }
            .into()
        } else if phase == Phase::Stopped {
            if self.settings.allow_late_hook {
                netburrow_core::text!("启用后，在游戏主菜单接入", "Go online, then attach from the game main menu")
            } else {
                netburrow_core::text!("启用后，从 Steam 启动游戏", "Go online, then launch the game from Steam")
            }
            .into()
        } else {
            let detail = self.last_snapshot.detail.trim();
            if !detail.is_empty() {
                detail.to_owned()
            } else {
                match phase {
                    Phase::WaitingForGame => netburrow_core::text!("从 Steam 启动游戏", "Launch the game from Steam"),
                    Phase::Ready => netburrow_core::text!("游戏已接入", "Game attached"),
                    _ => "",
                }
                .into()
            }
        };
        // Colours ease between states; the illustration and title fade in when the state changes.
        let context = ui.ctx().clone();
        let id = egui::Id::new("status-card");
        const TINT_SECONDS: f32 = 0.3;
        let ink = widgets::animate_color(&context, id.with("ink"), if tone == Tone::Neutral { TEXT } else { tone.fg() }, TINT_SECONDS);
        let detail_color = widgets::animate_color(&context, id.with("detail"), if tone == Tone::Neutral { MUTED } else { tone.fg() }, TINT_SECONDS);
        let fill = widgets::animate_color(&context, id.with("fill"), if tone == Tone::Neutral { CARD } else { tone.soft() }, TINT_SECONDS);
        let border = widgets::animate_color(&context, id.with("border"), tone.border(), TINT_SECONDS);
        let entered = widgets::appear(&context, id.with("enter"), (illustration, title), 0.35);
        // In-progress states breathe so the card reads as "working", not frozen.
        let breathing = illustration == art::Art::Connecting;
        let art_alpha = entered * if breathing { 0.65 + 0.35 * widgets::pulse(&context, 1.6) } else { 1.0 };
        let finished = self.client.as_ref().is_some_and(Client::is_finished);
        let missing_game = !Path::new(self.settings.game_path.trim()).is_file();
        let failed_check = self
            .preflight_report
            .as_ref()
            .is_some_and(|(_, report)| !report.can_start());
        let show_logs = active && matches!(phase, Phase::Connecting | Phase::RestartRequired | Phase::Failed);

        widgets::card(tone).fill(fill).stroke(egui::Stroke::new(1.0, border)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                art::show(ui, illustration, Vec2::new(92.0, 76.0), ink.gamma_multiply(art_alpha));
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.add_space(4.0);
                    ui.label(RichText::new(title).font(bold(19.0)).color(ink.gamma_multiply(entered)));
                    if !detail.is_empty() {
                        ui.add(
                            egui::Label::new(
                                RichText::new(&detail)
                                    .size(13.0)
                                    .color(detail_color.gamma_multiply(entered)),
                            )
                            .wrap(),
                        );
                    }
                    if phase == Phase::Ready && !checking {
                        let (quality, reason) =
                            netburrow_core::connection_quality(&self.last_snapshot, Instant::now());
                        let (label, quality_tone) = match quality {
                            netburrow_core::Quality::Pending => (netburrow_core::text!("待测", "Pending"), Tone::Neutral),
                            netburrow_core::Quality::Normal => (netburrow_core::text!("良好", "Good"), Tone::Accent),
                            netburrow_core::Quality::Unstable => (netburrow_core::text!("波动", "Unstable"), Tone::Warn),
                            netburrow_core::Quality::Abnormal => (netburrow_core::text!("异常", "Abnormal"), Tone::Danger),
                        };
                        let text = match self.last_snapshot.ping_ms {
                            Some(ms) => format!("{ms} ms · {label}"),
                            None => label.to_owned(),
                        };
                        ui.add_space(2.0);
                        widgets::status_chip(ui, &text, quality_tone).on_hover_text(reason);
                    }
                });
            });
            if phase == Phase::Ready && self.last_snapshot.rtt_samples.len() >= 2 {
                let samples: Vec<u64> = self.last_snapshot.rtt_samples.iter().copied().collect();
                widgets::sparkline(ui, &samples, Vec2::new(ui.available_width(), 24.0), ink.gamma_multiply(0.6));
            }
            if active
                && !finished
                && self.last_snapshot.peers.len() <= 1
                && matches!(phase, Phase::WaitingForGame | Phase::Ready)
            {
                ui.add(
                    egui::Label::new(
                        RichText::new(netburrow_core::text!("未看到朋友？核对服务器和完整组码。", "Missing a friend? Check your server and full group code."))
                            .size(12.0)
                            .color(MUTED),
                    )
                    .wrap(),
                );
            }
            if finished {
                ui.add_space(4.0);
                widgets::banner(ui, Tone::Warn, netburrow_core::text!("请断开，处理问题后重连", "Disconnect, resolve the issue, then reconnect"), false);
            }
            if !active && !checking && missing_game {
                ui.add_space(4.0);
                widgets::banner(ui, Tone::Warn, netburrow_core::text!("请设置游戏路径", "Set your game path"), false);
            }
            if let Some(error) = &self.config_error {
                ui.add_space(4.0);
                widgets::banner(ui, Tone::Danger, error, false);
            }
            ui.add_space(10.0);
            let label = if checking {
                netburrow_core::text!("取消检查", "Cancel check")
            } else if active {
                netburrow_core::text!("断开", "Disconnect")
            } else {
                netburrow_core::text!("启用联机", "Go online")
            };
            if widgets::button(
                ui,
                if active || checking { widgets::Kind::Secondary } else { widgets::Kind::Primary },
                Some(icons::Action::Power),
                label,
                Vec2::new(ui.available_width(), 46.0),
            )
            .clicked()
            {
                if active || checking {
                    self.stop();
                } else {
                    self.start();
                }
            }
            if active || failed_check || (missing_game && !checking) {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    if !active && missing_game
                        && widgets::button(ui, widgets::Kind::Ghost, Some(icons::Action::Settings), netburrow_core::text!("打开设置", "Open settings"), Vec2::new(0.0, 28.0)).clicked()
                    {
                        self.open_settings();
                    }
                    if failed_check
                        && widgets::button(ui, widgets::Kind::Ghost, None, netburrow_core::text!("查看检查结果", "View check results"), Vec2::new(0.0, 28.0)).clicked()
                    {
                        self.view.page = Page::Diagnostics;
                    }
                    if show_logs
                        && widgets::button(ui, widgets::Kind::Ghost, Some(icons::Action::Log), netburrow_core::text!("查看日志", "View logs"), Vec2::new(0.0, 28.0)).clicked()
                    {
                        self.view.page = Page::Diagnostics;
                    }
                    if active {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                RichText::new(netburrow_core::text!("断开后需重开游戏", "Restart the game after disconnecting"))
                                    .size(12.0)
                                    .color(MUTED),
                            );
                        });
                    }
                });
            }
        });
    }

    fn members_ui(&mut self, ui: &mut egui::Ui) {
        let show_members = self.client.is_some()
            || (self.smoke_test.is_some() && !self.last_snapshot.peers.is_empty());
        let count = if show_members { self.last_snapshot.peers.len() } else { 0 };
        ui.horizontal(|ui| {
            ui.label(RichText::new(netburrow_core::text!("同组成员", "Group members")).font(bold(18.0)));
            ui.label(
                RichText::new(netburrow_core::text_format!("{count} 人", "{count}"))
                    .size(13.0)
                    .color(MUTED),
            );
        });
        ui.add_space(6.0);
        if count == 0 {
            widgets::card(Tone::Neutral).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.vertical_centered(|ui| {
                    art::show(ui, art::Art::EmptyMembers, Vec2::new(120.0, 76.0), SUBTLE);
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(if self.client.is_some() {
                            netburrow_core::text!("等待成员加入", "Waiting for members")
                        } else {
                            netburrow_core::text!("暂无成员", "No members")
                        })
                        .size(13.0)
                        .color(MUTED),
                    );
                    ui.label(
                        RichText::new(netburrow_core::text!("把组码发给好友，即可同组联机", "Share the group code to play together"))
                            .size(12.0)
                            .color(SUBTLE),
                    );
                    if !self.settings.group.trim().is_empty() {
                        ui.add_space(6.0);
                        if widgets::button(
                            ui,
                            widgets::Kind::Secondary,
                            Some(icons::Action::Copy),
                            netburrow_core::text!("复制组码", "Copy group code"),
                            Vec2::new(0.0, 30.0),
                        )
                        .clicked()
                        {
                            ui.ctx().copy_text(self.settings.group.clone());
                            self.notice = Some(netburrow_core::text!("组码已复制", "Group code copied").into());
                        }
                    }
                });
            });
            return;
        }
        widgets::card(Tone::Neutral)
            .inner_margin(egui::Margin::symmetric(14, 4))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                for (index, peer) in self.last_snapshot.peers.iter().enumerate() {
                    if index > 0 {
                        ui.separator();
                    }
                    member_row(ui, peer);
                }
            });
    }

    fn profile_rows(&mut self, ui: &mut egui::Ui) {
        for is_server in [true, false] {
            ui.separator();
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
                    // Room for the longer English label without widening the Chinese layout.
                    Vec2::new(if netburrow_core::i18n::language() == netburrow_core::i18n::Language::En { 100.0 } else { 64.0 }, 34.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.label(
                            RichText::new(if is_server { netburrow_core::text!("服务器", "Server") } else { netburrow_core::text!("显示名", "Display name") })
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
                                    netburrow_core::text!("未设置", "Not set")
                                } else {
                                    netburrow_core::text!("使用成员编号", "Use member ID")
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
                            RichText::new(if editing { netburrow_core::text!("完成", "Done") } else { netburrow_core::text!("编辑", "Edit") })
                                .size(14.0)
                                .color(ACCENT),
                        )
                        .frame(false),
                    )
                    .on_disabled_hover_text(if self.preflight.is_some() {
                        netburrow_core::text!("检查完成后可编辑", "Wait for the check to finish")
                    } else {
                        netburrow_core::text!("断开后可编辑", "Disconnect to edit")
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

fn member_row(ui: &mut egui::Ui, peer: &netburrow_core::PeerInfo) {
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
    let stale = report.is_some() && peer.status_is_stale();
    let (status, tone) = if stale {
        (netburrow_core::text!("已过期", "Stale"), Tone::Warn)
    } else if let Some(report) = report {
        member_phase(report.phase)
    } else if peer.ready {
        (netburrow_core::text!("等待数据", "Pending"), Tone::Neutral)
    } else {
        (netburrow_core::text!("等待游戏", "Waiting for game"), Tone::Neutral)
    };
    let ping = if stale {
        "—".to_owned()
    } else {
        report
            .and_then(|r| r.ping_ms)
            .map_or_else(|| "—".into(), |ms| format!("{ms} ms"))
    };
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
    ui.horizontal(|ui| {
        ui.set_min_height(50.0);
        widgets::avatar(ui, &name, peer.client_id);
        ui.add_space(2.0);
        let width = (ui.available_width() - 160.0).max(60.0);
        ui.allocate_ui_with_layout(
            Vec2::new(width, 40.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_width(width);
                ui.add(egui::Label::new(RichText::new(display).size(14.0)).truncate())
                    .on_hover_text(netburrow_core::text_format!("{name}\n连接编号 #{}", "{name}\nConnection #{}", peer.client_id));
                ui.label(RichText::new(transport).size(11.5).color(MUTED))
                    .on_hover_text(netburrow_core::text!("不可靠消息使用 UDP，可靠消息使用 TCP", "UDP for unreliable messages; TCP for reliable messages"));
            },
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            widgets::status_chip(ui, status, tone);
            ui.label(RichText::new(ping).size(13.0).color(MUTED))
                .on_hover_text(netburrow_core::text!("成员到服务器的往返延迟，不代表玩家间延迟", "Round-trip latency from this member to the server, not between players"));
        });
    });
}
