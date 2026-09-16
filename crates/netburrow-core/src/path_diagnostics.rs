//! Bounded network diagnostics. Metadata never changes game packet admission or delivery.
use netburrow_protocol::{Delivery, Packet, Peer, PeerProbe};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

const MAX_PEERS: usize = 32;
const WINDOW: u64 = 256;
const PROBE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Default)]
pub struct PathDiagnostics {
    pub relay_supported: bool,
    pub peers: Vec<PathPeer>,
    pub omitted: usize,
}
#[derive(Clone, Debug)]
pub struct PathPeer {
    pub member: u64,
    pub supported: bool,
    pub probe_sent: u64,
    pub probe_received: u64,
    pub probe_timeouts: u64,
    pub probe_pending: bool,
    pub probe_timed_out: bool,
    pub rtt_ms: Option<u64>,
    pub success_age_ms: Option<u64>,
    pub flows: [SequenceReport; 2],
}
#[derive(Clone, Debug, Default)]
pub struct SequenceReport {
    pub assigned: u64,
    pub sent_stream: u64,
    pub received_stream: u64,
    pub highest: u64,
    pub received: u64,
    pub gaps_detected: u64,
    pub missing_window: u64,
    pub reordered: u64,
    pub duplicates: u64,
    pub too_old: u64,
    pub stream_changes: u64,
}
impl PathDiagnostics {
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "path diagnostics: relay_supported={} omitted={}",
            self.relay_supported, self.omitted
        )];
        for p in &self.peers {
            lines.push(format!("path member={} supported={} tcp_probe_sent={} replies={} timeouts={} pending={} timed_out={} rtt_ms={:?} success_age_ms={:?}", p.member, p.supported, p.probe_sent, p.probe_received, p.probe_timeouts, p.probe_pending, p.probe_timed_out, p.rtt_ms, p.success_age_ms));
            for (lane, f) in p.flows.iter().enumerate() {
                lines.push(format!("sequence member={} reliable={} assigned={} sent_stream={} received_stream={} highest={} received={} gaps_detected={} missing_window={} reordered={} duplicates={} too_old={} stream_changes={}", p.member, lane == 1, f.assigned, f.sent_stream, f.received_stream, f.highest, f.received, f.gaps_detected, f.missing_window, f.reordered, f.duplicates, f.too_old, f.stream_changes));
            }
        }
        lines
    }
}

#[derive(Default)]
struct Flow {
    report: SequenceReport,
    seen: BTreeSet<u64>,
}
impl Flow {
    fn receive(&mut self, d: Delivery) {
        let r = &mut self.report;
        if d.stream < r.received_stream {
            r.too_old += 1;
            return;
        }
        if d.stream != r.received_stream {
            if r.received_stream != 0 {
                r.stream_changes += 1;
            }
            r.received_stream = d.stream;
            r.highest = 0;
            self.seen.clear();
        }
        r.received += 1;
        if d.sequence <= r.highest.saturating_sub(WINDOW) {
            r.too_old += 1;
            return;
        }
        if self.seen.contains(&d.sequence) {
            r.duplicates += 1;
            return;
        }
        if d.sequence > r.highest {
            r.gaps_detected = r.gaps_detected.saturating_add(d.sequence - r.highest - 1);
            r.highest = d.sequence;
            self.seen.retain(|n| *n > r.highest.saturating_sub(WINDOW));
        } else {
            r.reordered += 1;
        }
        self.seen.insert(d.sequence);
        r.missing_window = r.highest.min(WINDOW) - self.seen.len() as u64;
    }
}
struct PeerState {
    peer: Peer,
    flows: [Flow; 2],
    pending: Option<(u64, Instant)>,
    last_attempt: Option<Instant>,
    last_success: Option<Instant>,
    rtt: Option<u64>,
    sent: u64,
    replies: u64,
    timeouts: u64,
    timed_out: bool,
}
impl PeerState {
    fn new(peer: Peer) -> Self {
        Self {
            peer,
            flows: Default::default(),
            pending: None,
            last_attempt: None,
            last_success: None,
            rtt: None,
            sent: 0,
            replies: 0,
            timeouts: 0,
            timed_out: false,
        }
    }
}
#[derive(Default)]
pub(crate) struct Tracker {
    own: Option<Peer>,
    peers: BTreeMap<u64, PeerState>,
    capable: BTreeSet<u64>,
    supported: bool,
    next_stream: u64,
    next_probe: u64,
    omitted: usize,
}
impl Tracker {
    pub fn capabilities(&mut self, peers: Vec<u64>) {
        self.supported = true;
        self.capable = peers.into_iter().take(1024).collect();
        for (id, p) in &mut self.peers {
            if !self.capable.contains(id) {
                p.pending = None;
                p.rtt = None;
                p.last_success = None;
            }
        }
    }
    pub fn members(&mut self, own_id: u64, peers: &[Peer]) {
        let own = peers
            .iter()
            .find(|p| p.client_id == own_id && p.epoch != 0 && p.steam_id != 0)
            .cloned();
        if own != self.own {
            self.peers.clear();
            self.own = own;
        }
        self.peers.retain(|_, old| peers.contains(&old.peer));
        self.omitted = 0;
        if self.own.is_none() {
            return;
        }
        for peer in peers
            .iter()
            .filter(|p| p.client_id != own_id && p.epoch != 0 && p.steam_id != 0)
        {
            if self.peers.contains_key(&peer.client_id) {
                continue;
            }
            if self.peers.len() >= MAX_PEERS {
                self.omitted += 1;
                continue;
            }
            self.peers
                .insert(peer.client_id, PeerState::new(peer.clone()));
        }
    }
    pub fn assign(&mut self, packet: &mut Packet) {
        packet.delivery = None;
        let Some(own) = &self.own else {
            return;
        };
        if packet.from != own.steam_id || packet.source_epoch != own.epoch {
            return;
        }
        let Some((id, peer)) = self
            .peers
            .iter_mut()
            .find(|(_, p)| p.peer.steam_id == packet.to && p.peer.epoch == packet.target_epoch)
        else {
            return;
        };
        if !self.supported || !self.capable.contains(id) {
            return;
        }
        let r = &mut peer.flows[usize::from(packet.send_type >= 2)].report;
        if r.sent_stream == 0 {
            let Some(stream) = self.next_stream.checked_add(1) else {
                return;
            };
            self.next_stream = stream;
            r.sent_stream = stream;
        }
        let Some(sequence) = r.assigned.checked_add(1) else {
            return;
        };
        r.assigned = sequence;
        packet.delivery = Some(Delivery {
            stream: r.sent_stream,
            sequence,
        });
    }
    pub fn receive(&mut self, packet: &Packet) {
        let (Some(own), Some(d)) = (&self.own, packet.delivery) else {
            return;
        };
        if packet.to != own.steam_id || packet.target_epoch != own.epoch {
            return;
        }
        if let Some((id, peer)) = self
            .peers
            .iter_mut()
            .find(|(_, p)| p.peer.steam_id == packet.from && p.peer.epoch == packet.source_epoch)
        {
            if self.capable.contains(id) {
                peer.flows[usize::from(packet.send_type >= 2)].receive(d);
            }
        }
    }
    pub fn tick(&mut self, now: Instant) -> Vec<PeerProbe> {
        let Some(own) = &self.own else {
            return Vec::new();
        };
        let mut probes = Vec::new();
        for (id, p) in &mut self.peers {
            if !self.supported || !self.capable.contains(id) {
                continue;
            }
            if p.pending
                .is_some_and(|(_, t)| now.saturating_duration_since(t) >= PROBE_INTERVAL)
            {
                p.pending = None;
                p.timeouts += 1;
                p.timed_out = true;
            }
            if p.pending.is_none()
                && p.last_attempt
                    .is_none_or(|t| now.saturating_duration_since(t) >= PROBE_INTERVAL)
            {
                if probes.len() >= 4 {
                    continue;
                }
                let Some(probe_id) = self.next_probe.checked_add(1) else {
                    continue;
                };
                self.next_probe = probe_id;
                p.pending = Some((probe_id, now));
                p.last_attempt = Some(now);
                p.sent += 1;
                probes.push(PeerProbe {
                    from: own.client_id,
                    to: *id,
                    source_epoch: own.epoch,
                    target_epoch: p.peer.epoch,
                    id: probe_id,
                    reply: false,
                });
            }
        }
        probes
    }
    pub fn probe(&mut self, probe: PeerProbe, now: Instant) -> Option<PeerProbe> {
        let own = self.own.as_ref()?;
        if probe.to != own.client_id
            || probe.target_epoch != own.epoch
            || !self.capable.contains(&probe.from)
        {
            return None;
        }
        let p = self.peers.get_mut(&probe.from)?;
        if p.peer.epoch != probe.source_epoch {
            return None;
        }
        if !probe.reply {
            return Some(PeerProbe {
                from: probe.to,
                to: probe.from,
                source_epoch: probe.target_epoch,
                target_epoch: probe.source_epoch,
                id: probe.id,
                reply: true,
            });
        }
        if let Some((id, started)) = p.pending {
            if id == probe.id {
                p.pending = None;
                if now.saturating_duration_since(started) >= PROBE_INTERVAL {
                    p.timeouts += 1;
                    p.timed_out = true;
                } else {
                    p.replies += 1;
                    p.timed_out = false;
                    p.last_success = Some(now);
                    p.rtt = Some(now.saturating_duration_since(started).as_millis() as u64);
                }
            }
        }
        None
    }
    pub fn snapshot(&self, now: Instant) -> PathDiagnostics {
        PathDiagnostics {
            relay_supported: self.supported,
            omitted: self.omitted,
            peers: self
                .peers
                .iter()
                .map(|(id, p)| PathPeer {
                    member: *id,
                    supported: self.capable.contains(id),
                    probe_sent: p.sent,
                    probe_received: p.replies,
                    probe_timeouts: p.timeouts,
                    probe_pending: p.pending.is_some(),
                    probe_timed_out: p.timed_out,
                    rtt_ms: p.rtt,
                    success_age_ms: p
                        .last_success
                        .map(|t| now.saturating_duration_since(t).as_millis() as u64),
                    flows: std::array::from_fn(|i| p.flows[i].report.clone()),
                })
                .collect(),
        }
    }
}
