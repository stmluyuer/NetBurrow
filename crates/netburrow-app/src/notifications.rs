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
                title: "NetBurrow 联机已就绪",
                body: "游戏已接入，可以在游戏中邀请同组朋友。",
                warning: false,
            }
        } else if phase == Phase::Failed {
            Notification {
                title: "NetBurrow 联机失败",
                body: "请打开工具查看具体原因和处理步骤。",
                warning: true,
            }
        } else if phase == Phase::Connecting
            && matches!(
                previous,
                Phase::Ready | Phase::WaitingForGame | Phase::Attaching | Phase::RestartRequired
            )
        {
            Notification {
                title: "NetBurrow 连接中断",
                body: "正在尝试恢复连接。请打开工具查看恢复结果，暂时保留游戏。",
                warning: true,
            }
        } else if phase == Phase::RestartRequired
            && matches!(previous, Phase::Ready | Phase::Attaching)
        {
            Notification {
                title: "NetBurrow 游戏接入异常",
                body: "请打开工具查看原因，处理后退出游戏并从 Steam 重开。",
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
