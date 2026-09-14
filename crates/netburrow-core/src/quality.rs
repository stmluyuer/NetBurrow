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
