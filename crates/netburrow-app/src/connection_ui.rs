use super::*;
use netburrow_core::CheckLevel;

#[cfg(test)]
mod tests {
    use super::*;

    fn passed() -> netburrow_core::PreflightReport {
        netburrow_core::PreflightReport {
            checks: vec![netburrow_core::Check {
                name: "fixture",
                level: CheckLevel::Passed,
                detail: "test result".into(),
            }],
        }
    }

    #[test]
    fn changed_settings_and_manual_checks_never_auto_start() {
        for changed in [false, true] {
            let mut app = NetBurrowApp::new(egui::Context::default(), true, None);
            let checked = app.settings.clone();
            if changed {
                app.settings.server = "changed.example:24872".into();
            }
            let (sender, receiver) = std::sync::mpsc::channel();
            app.preflight = Some(PendingPreflight {
                settings: checked,
                receiver,
                start_after: changed,
            });
            sender.send(passed()).unwrap();
            app.poll_preflight();
            assert!(app.client.is_none());
            assert!(app.preflight.is_none());
            assert!(app.preflight_report.is_some());
            if changed {
                assert!(app.notice.as_ref().unwrap().contains("已修改"));
            }
        }
    }

    #[test]
    fn cancelled_preflight_cannot_deliver_a_late_start() {
        let mut app = NetBurrowApp::new(egui::Context::default(), true, None);
        let (sender, receiver) = std::sync::mpsc::channel();
        app.preflight = Some(PendingPreflight {
            settings: app.settings.clone(),
            receiver,
            start_after: true,
        });
        app.stop();
        assert!(sender.send(passed()).is_err());
        app.poll_preflight();
        assert!(app.client.is_none());
        assert!(app.preflight_report.is_none());
    }
}

impl NetBurrowApp {
    pub(super) fn begin_preflight(&mut self, start_after: bool) {
        if self.client.is_some() || self.preflight.is_some() {
            return;
        }
        if self.smoke_test.is_some() {
            self.notice = Some("界面预览不执行网络自检。".into());
            return;
        }
        let settings = self.settings.clone();
        let work = settings.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        match std::thread::Builder::new()
            .name("netburrow-preflight".into())
            .spawn(move || {
                let _ = sender.send(netburrow_core::preflight(&work));
            }) {
            Ok(_) => {
                self.preflight = Some(PendingPreflight {
                    settings,
                    receiver,
                    start_after,
                });
                self.preflight_report = None;
                self.notice = None;
            }
            Err(error) => self.notice = Some(format!("无法开始自检：{error}")),
        }
    }

    pub(super) fn poll_preflight(&mut self) {
        let Some(pending) = &self.preflight else {
            return;
        };
        match pending.receiver.try_recv() {
            Ok(report) => {
                let pending = self.preflight.take().unwrap();
                let unchanged = pending.settings.same_connection(&self.settings);
                let start = pending.start_after && unchanged && report.can_start();
                self.notice = Some(
                    if !unchanged {
                        "连接设置已修改，请重新自检；未自动启用。"
                    } else if !report.can_start() {
                        "自检未通过，请处理下方问题后重试。"
                    } else {
                        "自检完成，请查看结果中的版本兼容性提示。"
                    }
                    .into(),
                );
                self.preflight_report = Some((pending.settings, report));
                if start {
                    self.activate_checked();
                }
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.preflight = None;
                self.notice = Some("自检任务未完成，请重试。".into());
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    pub(super) fn preflight_ui(&self, ui: &mut egui::Ui) {
        if self.preflight.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("正在自检配置、文件和服务器…");
            });
            ui.add_space(8.0);
        }
        if let Some((settings, report)) = &self.preflight_report {
            let stale = !settings.same_connection(&self.settings);
            egui::CollapsingHeader::new(if stale {
                "自检结果（设置已修改，需重新检查）"
            } else {
                "启用前自检结果"
            })
            .default_open(!report.can_start())
            .show(ui, |ui| {
                for check in &report.checks {
                    let (label, color) = match check.level {
                        CheckLevel::Passed => ("通过", ACCENT),
                        CheckLevel::Warning => ("提示", Color32::from_rgb(151, 103, 37)),
                        CheckLevel::Failed => ("未通过", Color32::from_rgb(174, 65, 60)),
                    };
                    ui.label(
                        RichText::new(format!("{} · {label}：{}", check.name, check.detail))
                            .color(color),
                    );
                }
            });
            ui.add_space(8.0);
        }
    }

}
