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
    if snapshot.relay_recovering {return (Quality::Abnormal,crate::text!("正在恢复会话，游戏接入暂时保留", "Restoring session; game connection retained for now"));}
    if snapshot.phase == Phase::Stopped {
        return (Quality::Pending, crate::text!("连接后开始测量", "Measurement starts when connected"));
    }
    if snapshot.phase == Phase::Failed {
        return (Quality::Abnormal, crate::text!("连接失败", "Connection failed"));
    }
    if snapshot.phase == Phase::Connecting {
        return if snapshot.last_disconnect_at.is_some() {
            (Quality::Abnormal, crate::text!("连接中断，正在重连", "Disconnected. Reconnecting…"))
        } else {
            (Quality::Pending, crate::text!("等待连接", "Waiting for connection"))
        };
    }
    if snapshot.peer_faults > 0 {
        return (
            Quality::Abnormal,
            crate::text!("部分成员会话失败，其他连接保持；本局可能需重开", "Some member sessions failed; other connections remain. You may need to restart the run"),
        );
    }
    if snapshot.ipc_slow {
        return (Quality::Unstable, crate::text!("游戏接入响应较慢，等待恢复", "Game connection is slow. Waiting for recovery"));
    }
    if snapshot.process_unknown {
        return (Quality::Unstable, crate::text!("暂时无法检查游戏状态，连接保持", "Game status unavailable; connection retained"));
    }
    if let Some(h) = &snapshot.hook_health {
        if h.interface_changed {
            return (Quality::Abnormal, crate::text!("游戏接口发生变化，请查看诊断", "Game interface changed. Check Diagnostics"));
        }
        if h.queued_packets > 0
            && (h.oldest_ms >= 2000
                || h.queued_packets >= 3072
                || h.queued_bytes >= 6 * 1024 * 1024)
        {
            return (Quality::Unstable, crate::text!("游戏暂未读取数据，正在缓冲", "Game is not reading data. Buffering…"));
        }
    }
    let Some(last_pong) = snapshot.last_pong_at else {
        return (Quality::Pending, crate::text!("等待延迟数据", "Waiting for latency data"));
    };
    let age = now.saturating_duration_since(last_pong);
    if age >= Duration::from_secs(5) {
        return (Quality::Abnormal, crate::text!("超过 5 秒未收到心跳回复", "No heartbeat response for over 5 seconds"));
    }
    let ping = snapshot.ping_ms.unwrap_or(0);
    if ping >= 300 {
        return (Quality::Abnormal, crate::text!("服务器延迟高", "High server latency"));
    }
    let spread = snapshot
        .rtt_samples
        .iter()
        .max()
        .copied()
        .unwrap_or(0)
        .saturating_sub(snapshot.rtt_samples.iter().min().copied().unwrap_or(0));
    if age >= Duration::from_secs(3) || ping >= 150 || spread >= 80 {
        return (Quality::Unstable, crate::text!("延迟偏高或波动较大", "Latency is high or unstable"));
    }
    if snapshot
        .last_disconnect_at
        .is_some_and(|last| now.saturating_duration_since(last) < Duration::from_secs(60))
    {
        return (Quality::Unstable, crate::text!("最近 60 秒内发生过断线", "Disconnected within the last 60 seconds"));
    }
    (Quality::Normal, crate::text!("心跳与延迟正常", "Heartbeat and latency are normal"))
}
