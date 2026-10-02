use netburrow_core::Phase;
use std::time::{Duration, Instant};

pub struct Notification {
    pub title: &'static str,
    pub body: &'static str,
    pub warning: bool,
}

#[derive(Default)]
pub struct Notifications {
    previous: Phase,
    last_ready: Option<Instant>,
    last_warning: Option<Instant>,
}

impl Notifications {
    pub fn update(&mut self, phase: Phase, enabled: bool, now: Instant) -> Option<Notification> {
        let previous = std::mem::replace(&mut self.previous, phase);
        if !enabled || previous == phase {
            return None;
        }
        let notice = if phase == Phase::Ready {
            Notification {
                title: netburrow_core::text!("NetBurrow 已就绪", "NetBurrow is ready"),
                body: netburrow_core::text!("可在游戏中邀请同组朋友。", "Invite friends in your group from the game."),
                warning: false,
            }
        } else if phase == Phase::Failed {
            Notification {
                title: netburrow_core::text!("NetBurrow 连接失败", "NetBurrow connection failed"),
                body: netburrow_core::text!("打开 NetBurrow 查看原因和处理步骤。", "Open NetBurrow for details and next steps."),
                warning: true,
            }
        } else if phase == Phase::Connecting
            && matches!(
                previous,
                Phase::Ready | Phase::WaitingForGame | Phase::Attaching | Phase::RestartRequired
            )
        {
            Notification {
                title: netburrow_core::text!("NetBurrow 连接中断", "NetBurrow disconnected"),
                body: netburrow_core::text!("正在重连。请保留游戏，打开 NetBurrow 查看进度。", "Reconnecting. Keep the game open and check NetBurrow for progress."),
                warning: true,
            }
        } else if phase == Phase::RestartRequired
            && matches!(previous, Phase::Ready | Phase::Attaching)
        {
            Notification {
                title: netburrow_core::text!("NetBurrow 游戏接入异常", "NetBurrow game attachment failed"),
                body: netburrow_core::text!("打开 NetBurrow 查看原因，处理后从 Steam 重开游戏。", "Open NetBurrow for details, resolve the issue, then restart the game from Steam."),
                warning: true,
            }
        } else {
            return None;
        };
        let last = if notice.warning {
            &mut self.last_warning
        } else {
            &mut self.last_ready
        };
        if last.is_some_and(|last| now.saturating_duration_since(last) < Duration::from_secs(30)) {
            return None;
        }
        *last = Some(now);
        Some(notice)
    }
}
