use crate::{Phase, Snapshot};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    Pending,
    Normal,
    Unstable,
    Abnormal,
}

pub fn connection_quality(snapshot: &Snapshot, now: Instant) -> (Quality, &'static str) {
    if snapshot.relay_recovering {return (Quality::Abnormal,"正在恢复原 Relay 会话，暂时保留游戏接入");}
    if snapshot.phase == Phase::Stopped {
        return (Quality::Pending, "启用后开始测量");
    }
    if snapshot.phase == Phase::Failed {
        return (Quality::Abnormal, "连接服务已失败");
    }
    if snapshot.phase == Phase::Connecting {
        return if snapshot.last_disconnect_at.is_some() {
            (Quality::Abnormal, "连接中断，正在重连")
        } else {
            (Quality::Pending, "等待建立连接")
        };
    }
    if snapshot.peer_faults > 0 {
        return (
            Quality::Abnormal,
            "部分成员会话失败；其他连接保持，本局可能需要重开",
        );
    }
    if snapshot.ipc_slow {
        return (Quality::Unstable, "游戏接入响应慢，正在等待恢复");
    }
    if snapshot.process_unknown {
        return (Quality::Unstable, "游戏状态检查暂不可用，联机保持中");
    }
    if let Some(h) = &snapshot.hook_health {
        if h.interface_changed {
            return (Quality::Abnormal, "游戏接入入口发生变化，请查看诊断");
        }
        if h.queued_packets > 0
            && (h.oldest_ms >= 2000
                || h.queued_packets >= 3072
                || h.queued_bytes >= 6 * 1024 * 1024)
        {
            return (Quality::Unstable, "游戏暂时未读取数据，正在缓冲");
        }
    }
    let Some(last_pong) = snapshot.last_pong_at else {
        return (Quality::Pending, "等待延迟样本");
    };
    let age = now.saturating_duration_since(last_pong);
    if age >= Duration::from_secs(5) {
        return (Quality::Abnormal, "已超过 5 秒未收到心跳回复");
    }
    let ping = snapshot.ping_ms.unwrap_or(0);
    if ping >= 300 {
        return (Quality::Abnormal, "到 Relay 的延迟较高");
    }
    let spread = snapshot
        .rtt_samples
        .iter()
        .max()
        .copied()
        .unwrap_or(0)
        .saturating_sub(snapshot.rtt_samples.iter().min().copied().unwrap_or(0));
    if age >= Duration::from_secs(3) || ping >= 150 || spread >= 80 {
        return (Quality::Unstable, "延迟偏高或近期波动较大");
    }
    if snapshot
        .last_disconnect_at
        .is_some_and(|last| now.saturating_duration_since(last) < Duration::from_secs(60))
    {
        return (Quality::Unstable, "最近 60 秒内发生过断线");
    }
    (Quality::Normal, "近期心跳和延迟正常")
}
