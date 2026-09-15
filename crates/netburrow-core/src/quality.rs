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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_health_is_not_reported_as_a_relay_failure() {
        let mut s = Snapshot {
            phase: Phase::Ready,
            ..Snapshot::default()
        };
        s.ipc_slow = true;
        assert_eq!(connection_quality(&s, Instant::now()).0, Quality::Unstable);
        s.ipc_slow = false;
        s.process_unknown = true;
        assert_eq!(connection_quality(&s, Instant::now()).0, Quality::Unstable);
        s.process_unknown = false;
        s.peer_faults = 1;
        assert_eq!(connection_quality(&s, Instant::now()).0, Quality::Abnormal);
        assert_eq!(s.phase, Phase::Ready);
    }
    #[test]
    fn quality_distinguishes_stale_latency_jitter_and_recovery() {
        let now = Instant::now();
        let mut snapshot = Snapshot::default();
        assert_eq!(connection_quality(&snapshot, now).0, Quality::Pending);
        snapshot.phase = Phase::Ready;
        snapshot.last_pong_at = Some(now);
        snapshot.ping_ms = Some(40);
        snapshot.rtt_samples = [35, 40].into();
        assert_eq!(connection_quality(&snapshot, now).0, Quality::Normal);
        snapshot.rtt_samples.push_back(130);
        assert_eq!(connection_quality(&snapshot, now).0, Quality::Unstable);
        snapshot.ping_ms = Some(300);
        assert_eq!(connection_quality(&snapshot, now).0, Quality::Abnormal);
        snapshot.ping_ms = Some(40);
        snapshot.rtt_samples.clear();
        assert_eq!(
            connection_quality(&snapshot, now + Duration::from_secs(5)).0,
            Quality::Abnormal
        );
        snapshot.last_disconnect_at = Some(now);
        assert_eq!(connection_quality(&snapshot, now).0, Quality::Unstable);
        snapshot.last_pong_at = Some(now + Duration::from_secs(61));
        assert_eq!(
            connection_quality(&snapshot, now + Duration::from_secs(61)).0,
            Quality::Normal
        );
    }
}
