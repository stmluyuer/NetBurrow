//! Passive local observations: no new network requests and no changes to packet delivery.
use crate::Snapshot;
use std::{
    sync::{Arc, Mutex, mpsc},
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(crate) struct Monitor {
    stop: mpsc::Sender<()>,
    worker: Option<JoinHandle<()>>,
}
impl Monitor {
    pub fn start(state: Arc<Mutex<Snapshot>>) -> Option<Self> {
        let (stop, stopped) = mpsc::channel();
        let worker = std::thread::Builder::new().name("netburrow-network-history".into()).spawn(move || {
            let start = Instant::now();
            let mut previous = start;
            let mut interfaces = String::new();
            let mut last_interfaces = start;
            crate::diagnostics::network_history(&["start interval_ms=1000; adapters describe local links only, not Internet or Steam reachability; peer probes use existing 5s TCP probes; sub-second outages may be missed; game counters are latest Hook reports, not packet timestamps".into()]);
            loop {
                let now = Instant::now();
                // Copy under the lock; OS queries and disk writes do not hold the shared state lock.
                let snapshot = state.lock().unwrap_or_else(|p| p.into_inner()).clone();
                let mut lines = sample(&snapshot, now, now.duration_since(start), now.duration_since(previous));
                let current = adapter_states();
                if current != interfaces || now.duration_since(last_interfaces) >= Duration::from_secs(30) {
                    lines.push(format!("adapters {current}"));
                    interfaces = current;
                    last_interfaces = now;
                }
                crate::diagnostics::network_history(&lines);
                previous = now;
                match stopped.recv_timeout(Duration::from_secs(1)) {
                    Err(mpsc::RecvTimeoutError::Timeout) => {},
                    _ => break,
                }
            }
            crate::diagnostics::network_history(&["stop monitor ended".into()]);
        });
        match worker {
            Ok(worker) => Some(Self {
                stop,
                worker: Some(worker),
            }),
            Err(error) => {
                crate::diagnostics::record(
                    "WARN",
                    "network history",
                    &format!("monitor unavailable: {error}"),
                );
                None
            }
        }
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn sample(s: &Snapshot, now: Instant, elapsed: Duration, interval: Duration) -> Vec<String> {
    let age =
        |instant: Option<Instant>| instant.map(|t| now.saturating_duration_since(t).as_millis());
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let mut lines = vec![format!(
        "sample unix_ms={unix_ms} elapsed_ms={} interval_ms={} phase={:?} relay_recovering={} relay_recoveries={} relay_pong_age_ms={:?} relay_rtt_ms={:?} disconnects={} heartbeat_timeouts={} ipc_slow={} ipc_recoveries={} sent={} received={}",
        elapsed.as_millis(),
        interval.as_millis(),
        s.phase,
        s.relay_recovering,
        s.relay_recoveries,
        age(s.last_pong_at),
        s.ping_ms,
        s.disconnects,
        s.heartbeat_timeouts,
        s.ipc_slow,
        s.ipc_recoveries,
        s.sent,
        s.received
    )];
    for p in s.peers.iter().filter(|p| !p.is_self).take(32) {
        let path = s
            .path_diagnostics
            .peers
            .iter()
            .find(|d| d.member == p.client_id);
        let health = s.hook_health.as_ref().and_then(|h| {
            h.peers
                .iter()
                .find(|h| h.peer == p.steam_id && h.epoch == p.game_epoch)
        });
        let mut line = format!(
            "peer member={} bound={} status_age_ms={:?}",
            p.client_id,
            p.ready,
            age(p.status_updated)
        );
        if let Some(d) = path {
            line.push_str(&format!(" probe_supported={} probes={} replies={} timeouts={} timed_out={} rtt_ms={:?} success_age_ms={:?} assigned={} received={}",
                d.supported, d.probe_sent, d.probe_received, d.probe_timeouts, d.probe_timed_out, d.rtt_ms, d.success_age_ms,
                d.flows.iter().map(|f| f.assigned).sum::<u64>(), d.flows.iter().map(|f| f.received).sum::<u64>()));
        }
        if let Some(h) = health {
            line.push_str(&format!(" game_send_calls={} rejected={} hook_received={} consumed={} queued={} dropped={} failed={}",
                h.send_calls, h.send_rejected, h.received, h.consumed, h.queued_packets, h.dropped, h.failed));
        }
        lines.push(line);
    }
    lines.push(format!(
        "members own={:?} total={} omitted={}",
        s.peers.iter().find(|p| p.is_self).map(|p| p.client_id),
        s.peers.len(),
        s.peers
            .iter()
            .filter(|p| !p.is_self)
            .count()
            .saturating_sub(32)
    ));
    lines
}

fn adapter_states() -> String {
    use windows_sys::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2};
    unsafe {
        let mut table = std::ptr::null_mut();
        let error = GetIfTable2(&mut table);
        if error != 0 {
            return format!("query_error={error}");
        }
        if table.is_null() {
            return "query_error=null_table".into();
        }
        let rows =
            std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize);
        // Use only numeric local interface indexes, never names, MAC/IP addresses or SSIDs.
        let mut states: Vec<_> = rows
            .iter()
            .filter(|r| r.Type != 24)
            .map(|r| (r.InterfaceIndex, r.Type, r.OperStatus, r.MediaConnectState))
            .collect();
        FreeMibTable(table.cast());
        states.sort_unstable();
        let omitted = states.len().saturating_sub(32);
        let text = states
            .iter()
            .take(32)
            .map(|(index, kind, oper, media)| {
                format!("if={index},type={kind},oper={oper},media={media}")
            })
            .collect::<Vec<_>>()
            .join(";");
        format!(
            "{text} omitted={omitted}; oper=1 means up; media=1 connected,2 disconnected; no route or Steam inference"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_preserves_stale_heartbeat_and_recovery_without_identities() {
        // Exercise the real read-only Windows call, including allocation/free and ABI.
        let adapters = adapter_states();
        assert!(!adapters.contains("query_error="), "{adapters}");
        let now = Instant::now();
        let mut s = Snapshot::default();
        s.last_pong_at = Some(now - Duration::from_secs(4));
        s.ping_ms = Some(30);
        s.peers.push(crate::PeerInfo {
            client_id: 7,
            game_epoch: 9,
            steam_id: 999000111222333444,
            ready: true,
            is_self: false,
            status: Some(crate::MemberStatus {
                name: "private-name".into(),
                phase: 3,
                ping_ms: None,
                transport: 0,
                sent: 0,
                received: 0,
            }),
            status_updated: None,
        });
        let stalled = sample(&s, now, Duration::from_secs(10), Duration::from_secs(3)).join("\n");
        assert!(stalled.contains("relay_pong_age_ms=Some(4000)"));
        assert!(stalled.contains("interval_ms=3000"));
        assert!(!stalled.contains("999000111222333444") && !stalled.contains("private-name"));
        s.last_pong_at = Some(now);
        s.relay_recoveries = 1;
        let recovered = sample(&s, now, Duration::from_secs(11), Duration::from_secs(1)).join("\n");
        assert!(recovered.contains("relay_pong_age_ms=Some(0)"));
        assert!(recovered.contains("relay_recoveries=1"));
    }
}
