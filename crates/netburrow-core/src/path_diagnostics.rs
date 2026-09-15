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

#[cfg(test)]
mod tests {
    use super::*;
    fn peers() -> Vec<Peer> {
        vec![
            Peer {
                client_id: 1,
                steam_id: 11,
                epoch: 111,
            },
            Peer {
                client_id: 2,
                steam_id: 22,
                epoch: 222,
            },
        ]
    }
    fn pair() -> (Tracker, Tracker) {
        let mut a = Tracker::default();
        let mut b = Tracker::default();
        a.members(1, &peers());
        b.members(2, &peers());
        a.capabilities(vec![1, 2]);
        b.capabilities(vec![1, 2]);
        (a, b)
    }
    fn packet(kind: u8) -> Packet {
        Packet {
            delivery: None,
            from: 11,
            to: 22,
            source_epoch: 111,
            target_epoch: 222,
            channel: 0,
            send_type: kind,
            payload: vec![1, 2, 3],
        }
    }
    #[test]
    fn sequences_separate_reliability_and_classify_gaps_late_and_duplicate_data() {
        let (mut a, mut b) = pair();
        let mut sent = Vec::new();
        for kind in [2, 3, 2] {
            let mut p = packet(kind);
            a.assign(&mut p);
            sent.push(p);
        }
        b.receive(&sent[0]);
        b.receive(&sent[2]);
        assert_eq!(
            b.snapshot(Instant::now()).peers[0].flows[1].missing_window,
            1
        );
        b.receive(&sent[1]);
        b.receive(&sent[1]);
        let report = b.snapshot(Instant::now());
        let f = &report.peers[0].flows[1];
        assert_eq!(
            (
                f.highest,
                f.gaps_detected,
                f.missing_window,
                f.reordered,
                f.duplicates
            ),
            (3, 1, 0, 1, 1)
        );
        let mut unreliable = packet(0);
        a.assign(&mut unreliable);
        b.receive(&unreliable);
        assert_eq!(unreliable.delivery.unwrap().sequence, 1);
        assert_ne!(
            unreliable.delivery.unwrap().stream,
            sent[0].delivery.unwrap().stream
        );
        assert_eq!(sent[0].payload, vec![1, 2, 3]);
        let mut huge = sent[0].clone();
        huge.delivery.as_mut().unwrap().sequence = u64::MAX;
        b.receive(&huge);
        b.receive(&sent[0]);
        let f = &b.peers[&1].flows[1];
        assert!(f.seen.len() <= WINDOW as usize);
        assert_eq!((f.report.missing_window, f.report.too_old), (255, 1));
    }
    #[test]
    fn probe_matches_exact_session_and_id_expires_and_resets_on_rebind() {
        let (mut a, mut b) = pair();
        let now = Instant::now();
        let request = a.tick(now).remove(0);
        let reply = b.probe(request, now).unwrap();
        let mut spoof = reply.clone();
        spoof.id += 1;
        a.probe(spoof, now);
        assert_eq!(a.snapshot(now).peers[0].probe_received, 0);
        a.probe(reply.clone(), now + Duration::from_millis(40));
        a.probe(reply, now + Duration::from_millis(41));
        assert_eq!(
            (
                a.snapshot(now).peers[0].probe_received,
                a.snapshot(now).peers[0].rtt_ms
            ),
            (1, Some(40))
        );
        let late = a.tick(now + PROBE_INTERVAL).remove(0);
        a.tick(now + PROBE_INTERVAL * 2);
        let late_reply = b.probe(late, now).unwrap();
        a.probe(late_reply, now + PROBE_INTERVAL * 2);
        assert_eq!(a.snapshot(now).peers[0].probe_timeouts, 1);
        let mut updated = peers();
        updated[1].epoch += 1;
        a.members(1, &updated);
        let p = &a.snapshot(now).peers[0];
        assert_eq!((p.probe_received, p.probe_timeouts, p.rtt_ms), (0, 0, None));
        let old_reply = b.probe(a.tick(now).remove(0), now);
        assert!(old_reply.is_none());
        updated[0].epoch += 1;
        a.members(1, &updated);
        assert_eq!(a.snapshot(now).peers[0].probe_sent, 0);
        a.members(1, &[]);
        assert!(a.snapshot(now).peers.is_empty());
    }
    #[test]
    fn legacy_peers_are_uninstrumented_and_tracking_is_bounded() {
        let mut a = Tracker::default();
        a.members(1, &peers());
        let mut p = packet(2);
        a.assign(&mut p);
        assert!(p.delivery.is_none());
        assert!(a.tick(Instant::now()).is_empty());
        a.capabilities(vec![1]);
        a.assign(&mut p);
        assert!(p.delivery.is_none());
        let members: Vec<_> = (1..100)
            .map(|i| Peer {
                client_id: i,
                steam_id: i * 11,
                epoch: i * 111,
            })
            .collect();
        a.members(1, &members);
        a.capabilities((1..100).collect());
        assert_eq!((a.peers.len(), a.omitted), (32, 66));
        assert_eq!(a.tick(Instant::now()).len(), 4);
    }
}
