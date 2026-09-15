use netburrow_protocol::{
    HookHealth, HookPeerHealth, MAX_HEALTH_PEERS, MAX_PAYLOAD, Packet, Peer,
    local::{Admission, DATA_BYTES, DATA_PACKETS, PEER_BYTES, PEER_PACKETS, fair_index},
};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const BYTE_LIMIT: usize = 4 * 1024 * 1024;
const PACKET_LIMIT: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Request(u64),
    Failed(u64),
}

struct Remote {
    epoch: u64,
    online: bool,
    accepted: bool,
    channels: HashSet<i32>,
    requested: Option<Instant>,
    failed: bool,
    health: HookPeerHealth,
}
struct Incoming {
    packet: Packet,
    queued: Instant,
}
impl std::ops::Deref for Incoming {
    type Target = Packet;
    fn deref(&self) -> &Packet {
        &self.packet
    }
}
struct Outgoing {
    packet: Packet,
    queued: Instant,
}
#[derive(Default)]
pub struct Outbox {
    packets: VecDeque<Outgoing>,
    bytes: usize,
    last_peer: Option<(u64, u64)>,
}
impl Outbox {
    pub fn pop(&mut self) -> Option<Packet> {
        let mut seen = HashSet::new();
        let candidates = self.packets.iter().enumerate().filter_map(|(i, item)| {
            if !seen.insert(item.packet.to) {
                return None;
            }
            let eligible = item.packet.send_type != 3
                || item.queued.elapsed() >= Duration::from_millis(200)
                || self
                    .packets
                    .iter()
                    .any(|p| p.packet.to == item.packet.to && p.packet.send_type == 2)
                || self
                    .packets
                    .iter()
                    .filter(|p| p.packet.to == item.packet.to)
                    .map(|p| p.packet.payload.len())
                    .sum::<usize>()
                    >= 1200;
            eligible.then_some((i, (item.packet.to, item.packet.target_epoch)))
        });
        let index = fair_index(candidates, self.last_peer)?;
        let item = self.packets.remove(index)?;
        self.last_peer = Some((item.packet.to, item.packet.target_epoch));
        self.bytes -= item.packet.payload.len();
        Some(item.packet)
    }
}

pub struct Bridge {
    pub steam_id: u64,
    pub epoch: u64,
    pub active: bool,
    pub stopped: bool,
    remotes: BTreeMap<u64, Remote>,
    inbound: VecDeque<Incoming>,
    pub outbound: Arc<Mutex<Outbox>>,
    in_bytes: usize,
    events: VecDeque<Event>,
    pub health: HookHealth,
    faults: VecDeque<(u64, u64)>,
    read_cursors: BTreeMap<i32, (u64, u64)>,
}

impl Bridge {
    pub fn new(steam_id: u64, epoch: u64) -> Self {
        Self {
            steam_id,
            epoch,
            active: false,
            stopped: false,
            remotes: BTreeMap::new(),
            inbound: VecDeque::new(),
            outbound: Arc::new(Mutex::new(Outbox::default())),
            in_bytes: 0,
            events: VecDeque::new(),
            health: HookHealth::default(),
            faults: VecDeque::new(),
            read_cursors: BTreeMap::new(),
        }
    }
    pub fn known(&self, id: u64) -> bool {
        self.remotes.contains_key(&id)
    }
    pub fn members(&mut self, peers: &[Peer]) {
        let mut changed = Vec::new();
        for (&id, peer) in &mut self.remotes {
            let epoch = peers
                .iter()
                .find(|p| p.steam_id == id && p.epoch != 0)
                .map(|p| p.epoch);
            if epoch != Some(peer.epoch) {
                if peer.accepted {
                    changed.push(id);
                }
                peer.online = false;
                peer.accepted = false;
                peer.channels.clear();
                peer.requested = None;
            }
        }
        for id in changed {
            self.drop_peer_packets(id, None);
            self.event(Event::Failed(id));
        }
        for peer in peers
            .iter()
            .filter(|p| p.steam_id != 0 && p.steam_id != self.steam_id && p.epoch != 0)
        {
            let remote = self.remotes.entry(peer.steam_id).or_insert_with(|| Remote {
                epoch: peer.epoch,
                online: true,
                accepted: false,
                channels: HashSet::new(),
                requested: None,
                failed: false,
                health: HookPeerHealth::default(),
            });
            if remote.epoch != peer.epoch {
                remote.health = HookPeerHealth::default();
                remote.failed = false;
                remote.accepted = false;
                remote.channels.clear();
                remote.requested = None;
            }
            remote.epoch = peer.epoch;
            remote.online = true;
        }
        // Keep only packets from the current remote incarnation, including unaccepted queues.
        self.inbound.retain(|p| {
            self.remotes
                .get(&p.from)
                .is_some_and(|r| r.online && r.epoch == p.source_epoch)
        });
        self.outbound
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .packets
            .retain(|p| {
                self.remotes
                    .get(&p.packet.to)
                    .is_some_and(|r| r.online && r.epoch == p.packet.target_epoch)
            });
        self.recount();
    }
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.active = false;
        self.stopped = true;
        self.events.clear();
        let active: Vec<_> = self
            .remotes
            .iter()
            .filter(|(_, r)| r.accepted)
            .map(|(&id, _)| id)
            .collect();
        for id in active {
            self.event(Event::Failed(id));
        }
        for remote in self.remotes.values_mut() {
            remote.online = false;
            remote.accepted = false;
            remote.channels.clear();
        }
        self.inbound.clear();
        *self.outbound.lock().unwrap_or_else(|p| p.into_inner()) = Outbox::default();
        self.in_bytes = 0;
    }
    /// `None` means this peer is not ours, so the original Steam method handles it.
    pub fn send(&mut self, to: u64, bytes: &[u8], send_type: u8, channel: i32) -> Option<bool> {
        self.health.send_calls += 1;
        let result = self.send_inner(to, bytes, send_type, channel);
        if let Some(peer) = self.remotes.get_mut(&to) {
            peer.health.send_calls += 1;
            peer.health.send_rejected += u64::from(result == Some(false));
        }
        if result == Some(false) {
            self.health.send_rejected += 1;
        }
        result
    }
    fn send_inner(&mut self, to: u64, bytes: &[u8], send_type: u8, channel: i32) -> Option<bool> {
        let peer = self.remotes.get_mut(&to)?;
        if !self.active
            || !peer.online
            || peer.failed
            || channel < 0
            || send_type > 3
            || bytes.len() > MAX_PAYLOAD
            || (send_type <= 1 && bytes.len() > 1200)
        {
            return Some(false);
        }
        let Ok(mut outbound) = self.outbound.try_lock() else {
            self.health.lock_busy += 1;
            return Some(false);
        };
        if outbound.packets.len() >= PACKET_LIMIT || outbound.bytes + bytes.len() > BYTE_LIMIT {
            return Some(false);
        }
        let (count, size) = outbound
            .packets
            .iter()
            .filter(|p| p.packet.to == to)
            .fold((0, 0), |(n, b), p| (n + 1, b + p.packet.payload.len()));
        if count >= 512 || size + bytes.len() > 2 * 1024 * 1024 {
            return Some(false);
        }
        // Relay binding already supplies the route. Sending also accepts the peer,
        // including no-delay first packets: Isaac uses these to start communication.
        // Dropping them until acceptance would leave both sides waiting forever.
        peer.accepted = true;
        peer.channels.insert(channel);
        peer.requested = None;
        outbound.bytes += bytes.len();
        outbound.packets.push_back(Outgoing {
            packet: Packet {
                from: self.steam_id,
                to,
                source_epoch: self.epoch,
                target_epoch: peer.epoch,
                channel,
                send_type,
                payload: bytes.to_vec(),
            },
            queued: Instant::now(),
        });
        Some(true)
    }
    pub fn pop_outgoing(&mut self) -> Option<Packet> {
        self.outbound
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pop()
    }
    pub fn receive(&mut self, packet: Packet) -> Admission {
        let id = packet.from;
        let current = packet.to == self.steam_id
            && packet.target_epoch == self.epoch
            && self
                .remotes
                .get(&id)
                .is_some_and(|p| p.epoch == packet.source_epoch);
        let result = self.receive_inner(packet);
        if current {
            if let Some(peer) = self.remotes.get_mut(&id) {
                peer.health.received += 1;
                match result {
                    Admission::Dropped => peer.health.dropped += 1,
                    Admission::PeerFailed(..) => peer.health.discarded += 1,
                    Admission::Queued => {}
                }
            }
        }
        result
    }
    fn receive_inner(&mut self, packet: Packet) -> Admission {
        if !self.active || packet.to != self.steam_id || packet.target_epoch != self.epoch {
            return Admission::Dropped;
        }
        let Some(peer) = self.remotes.get_mut(&packet.from) else {
            return Admission::Dropped;
        };
        if !peer.online || peer.failed || peer.epoch != packet.source_epoch {
            return Admission::Dropped;
        }
        let peer_id = packet.from;
        let peer_epoch = packet.source_epoch;
        let full = |queue: &VecDeque<Incoming>, bytes: usize| {
            queue.len() >= DATA_PACKETS
                || bytes + packet.payload.len() > DATA_BYTES
                || queue.iter().filter(|p| p.from == peer_id).count() >= PEER_PACKETS
                || queue
                    .iter()
                    .filter(|p| p.from == peer_id)
                    .map(|p| p.payload.len())
                    .sum::<usize>()
                    + packet.payload.len()
                    > PEER_BYTES
        };
        let lossy_full = packet.send_type <= 1
            && (self.inbound.iter().filter(|p| p.send_type <= 1).count() >= DATA_PACKETS / 2
                || self
                    .inbound
                    .iter()
                    .filter(|p| p.send_type <= 1)
                    .map(|p| p.payload.len())
                    .sum::<usize>()
                    + packet.payload.len()
                    > DATA_BYTES / 2);
        if packet.send_type >= 2 && full(&self.inbound, self.in_bytes) {
            while full(&self.inbound, self.in_bytes) {
                let count = self.inbound.iter().filter(|p| p.from == peer_id).count();
                let bytes: usize = self
                    .inbound
                    .iter()
                    .filter(|p| p.from == peer_id)
                    .map(|p| p.payload.len())
                    .sum();
                let peer_full = count >= PEER_PACKETS || bytes + packet.payload.len() > PEER_BYTES;
                let victim = self
                    .inbound
                    .iter()
                    .position(|p| p.from == peer_id && p.send_type <= 1)
                    .or_else(|| {
                        (!peer_full)
                            .then(|| self.inbound.iter().position(|p| p.send_type <= 1))
                            .flatten()
                    });
                let Some(index) = victim else {
                    break;
                };
                let removed = self.inbound.remove(index).unwrap();
                self.in_bytes -= removed.payload.len();
                self.health.dropped += 1;
                if let Some(peer) = self.remotes.get_mut(&removed.from) {
                    peer.health.dropped += 1;
                }
            }
        }
        if lossy_full || full(&self.inbound, self.in_bytes) {
            if packet.send_type <= 1 {
                self.health.dropped += 1;
                return Admission::Dropped;
            }
            self.fail_peer(peer_id, peer_epoch);
            return Admission::PeerFailed(peer_id, peer_epoch);
        }
        let peer = self.remotes.get_mut(&peer_id).unwrap();
        let request = !peer.accepted
            && peer
                .requested
                .is_none_or(|t| t.elapsed() > Duration::from_secs(1));
        if request {
            peer.requested = Some(Instant::now());
        }
        peer.channels.insert(packet.channel);
        if request {
            self.event(Event::Request(packet.from));
        }
        self.in_bytes += packet.payload.len();
        self.inbound.push_back(Incoming {
            packet,
            queued: Instant::now(),
        });
        Admission::Queued
    }
    pub fn available(&self, channel: i32) -> Option<usize> {
        self.read_index(channel)
            .map(|i| self.inbound[i].payload.len())
    }
    fn read_index(&self, channel: i32) -> Option<usize> {
        fair_index(
            self.inbound.iter().enumerate().filter_map(|(i, p)| {
                (p.channel == channel && self.remotes.get(&p.from).is_some_and(|r| r.accepted))
                    .then_some((i, (p.from, p.source_epoch)))
            }),
            self.read_cursors.get(&channel).copied(),
        )
    }
    pub fn read(&mut self, channel: i32) -> Option<Packet> {
        self.health.read_calls += 1;
        let index = self.read_index(channel)?;
        let packet = self.inbound.remove(index)?;
        self.read_cursors
            .insert(channel, (packet.from, packet.source_epoch));
        if !self.inbound.iter().any(|p| p.channel == channel) {
            self.read_cursors.remove(&channel);
        }
        self.in_bytes -= packet.payload.len();
        self.health.consumed += 1;
        if let Some(peer) = self.remotes.get_mut(&packet.from) {
            peer.health.consumed += 1;
        }
        Some(packet.packet)
    }
    pub fn accept(&mut self, id: u64) -> Option<bool> {
        let peer = self.remotes.get_mut(&id)?;
        if !self.active || !peer.online || peer.failed {
            return Some(false);
        }
        peer.accepted = true;
        peer.requested = None;
        Some(true)
    }
    pub fn close(&mut self, id: u64, channel: Option<i32>) -> Option<bool> {
        let peer = self.remotes.get_mut(&id)?;
        if let Some(channel) = channel {
            peer.channels.remove(&channel);
        } else {
            peer.channels.clear();
        }
        if peer.channels.is_empty() {
            peer.accepted = false;
            peer.requested = None;
        }
        self.drop_peer_packets(id, channel);
        Some(true)
    }
    pub fn session(&self, id: u64) -> Option<(bool, usize, usize)> {
        let peer = self.remotes.get(&id)?;
        let outbound = self.outbound.try_lock().ok()?;
        let packets: Vec<_> = outbound
            .packets
            .iter()
            .filter(|p| p.packet.to == id)
            .collect();
        Some((
            self.active && peer.online && peer.accepted && !peer.failed,
            packets.iter().map(|p| p.packet.payload.len()).sum(),
            packets.len(),
        ))
    }
    pub fn pop_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }
    pub fn peer_failed(&self, id: u64) -> bool {
        self.remotes.get(&id).is_some_and(|p| p.failed)
    }
    pub fn fail_peer(&mut self, id: u64, epoch: u64) {
        let Some(peer) = self.remotes.get_mut(&id) else {
            return;
        };
        if peer.epoch != epoch || peer.failed {
            return;
        }
        peer.failed = true;
        peer.accepted = false;
        self.drop_peer_packets(id, None);
        self.event(Event::Failed(id));
        self.faults.push_back((id, epoch));
    }
    pub fn pop_fault(&mut self) -> Option<(u64, u64)> {
        self.faults.pop_front()
    }
    pub fn health(&self) -> HookHealth {
        let mut health = self.health.clone();
        health.queued_packets = self.inbound.len() as u32;
        health.queued_bytes = self.in_bytes as u32;
        health.oldest_ms = self.inbound.front().map_or(0, |p| {
            p.queued.elapsed().as_millis().min(u32::MAX as u128) as u32
        });
        let outbound = self.outbound.lock().unwrap_or_else(|p| p.into_inner());
        let online = self.remotes.values().filter(|r| r.online).count();
        health.peers_omitted = online.saturating_sub(MAX_HEALTH_PEERS) as u32;
        health.peers = self
            .remotes
            .iter()
            .filter(|(_, r)| r.online)
            .take(MAX_HEALTH_PEERS)
            .map(|(&id, r)| {
                let mut p = r.health.clone();
                p.peer = id;
                p.epoch = r.epoch;
                p.failed = r.failed;
                let incoming: Vec<_> = self.inbound.iter().filter(|p| p.from == id).collect();
                p.queued_packets = incoming.len() as u32;
                p.queued_bytes = incoming.iter().map(|p| p.payload.len() as u32).sum();
                p.oldest_ms = incoming.first().map_or(0, |p| {
                    p.queued.elapsed().as_millis().min(u32::MAX as u128) as u32
                });
                p.outgoing_packets = outbound
                    .packets
                    .iter()
                    .filter(|p| p.packet.to == id)
                    .count() as u32;
                p
            })
            .collect();
        health
    }
    pub fn defer_event(&mut self, event: Event) {
        self.event(event);
    }
    pub fn needs_request(&self, id: u64) -> bool {
        self.active
            && self
                .remotes
                .get(&id)
                .is_some_and(|p| p.online && !p.accepted)
            && self.inbound.iter().any(|p| p.from == id)
    }
    fn event(&mut self, event: Event) {
        if self.events.len() < 64 && !self.events.contains(&event) {
            self.events.push_back(event);
        }
    }
    fn recount(&mut self) {
        self.read_cursors
            .retain(|channel, _| self.inbound.iter().any(|p| p.channel == *channel));
        self.in_bytes = self.inbound.iter().map(|p| p.payload.len()).sum();
        let mut outbound = self.outbound.lock().unwrap_or_else(|p| p.into_inner());
        outbound.bytes = outbound
            .packets
            .iter()
            .map(|p| p.packet.payload.len())
            .sum();
    }
    fn drop_peer_packets(&mut self, id: u64, channel: Option<i32>) {
        let discarded = self
            .inbound
            .iter()
            .filter(|p| p.from == id && channel.is_none_or(|c| p.channel == c))
            .count();
        if let Some(peer) = self.remotes.get_mut(&id) {
            peer.health.discarded += discarded as u64;
        }
        self.inbound
            .retain(|p| p.from != id || channel.is_some_and(|c| p.channel != c));
        self.outbound
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .packets
            .retain(|p| p.packet.to != id || channel.is_some_and(|c| p.packet.channel != c));
        self.recount();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn congested_peer_keeps_other_players_packets_and_diagnostics_intact() {
        let mut b = bridge();
        b.members(&[
            Peer {
                client_id: 2,
                steam_id: 20,
                epoch: 200,
            },
            Peer {
                client_id: 3,
                steam_id: 30,
                epoch: 300,
            },
        ]);
        b.accept(20);
        b.accept(30);
        for _ in 0..PEER_PACKETS {
            assert_eq!(b.receive(incoming(0)), Admission::Queued);
        }
        let mut p = incoming(0);
        p.from = 30;
        p.source_epoch = 300;
        p.send_type = 0;
        assert_eq!(b.receive(p), Admission::Queued);
        assert_eq!(b.receive(incoming(0)), Admission::PeerFailed(20, 200));
        assert_eq!(b.read(0).unwrap().from, 30);
        let health = b.health();
        let failed = health.peers.iter().find(|p| p.peer == 20).unwrap();
        assert_eq!(
            (failed.received, failed.discarded, failed.failed),
            (PEER_PACKETS as u64 + 1, PEER_PACKETS as u64 + 1, true)
        );
        let healthy = health.peers.iter().find(|p| p.peer == 30).unwrap();
        assert_eq!(
            (
                healthy.received,
                healthy.consumed,
                healthy.dropped,
                healthy.failed
            ),
            (1, 1, 0, false)
        );
    }

    #[test]
    fn four_players_exchange_fairly_and_restart_only_one_epoch() {
        let mut games: Vec<_> = (1..=4).map(|id| Bridge::new(id, id * 10)).collect();
        let mut peers: Vec<_> = (1..=4)
            .map(|id| Peer {
                client_id: id,
                steam_id: id,
                epoch: id * 10,
            })
            .collect();
        for game in &mut games {
            game.active = true;
            game.members(&peers);
        }
        // Queue by destination (not alternately): the scheduler must do the interleaving.
        for game in &mut games {
            for target in 1..=4 {
                if target == game.steam_id {
                    continue;
                }
                for n in 0..6u8 {
                    assert_eq!(game.send(target, &[n; 4], 2, i32::from(n % 2)), Some(true));
                }
            }
        }
        for source in 0..4 {
            for n in 0..6u8 {
                for target in 1..=4 {
                    if target == games[source].steam_id {
                        continue;
                    }
                    let packet = games[source].pop_outgoing().unwrap();
                    assert_eq!((packet.to, packet.payload[0]), (target, n));
                    assert_eq!(
                        games[(target - 1) as usize].receive(packet),
                        Admission::Queued
                    );
                }
            }
        }
        for game in &mut games {
            for channel in 0..2 {
                for n in (channel..6).step_by(2) {
                    for source in 1..=4 {
                        if source == game.steam_id {
                            continue;
                        }
                        assert_eq!(game.available(channel), Some(4));
                        let p = game.read(channel).unwrap();
                        assert_eq!((p.from, p.payload[0]), (source, n as u8));
                    }
                }
            }
            assert!(game.read_cursors.is_empty());
            for h in game.health().peers {
                assert_eq!(
                    (h.send_calls, h.received, h.consumed, h.queued_packets),
                    (6, 6, 6, 0)
                );
            }
        }
        games[0].fail_peer(4, 40);
        games[0].members(&peers);
        assert_eq!(games[0].send(4, b"failed", 2, 0), Some(false));
        peers[3].steam_id = 0;
        peers[3].epoch = 0;
        for game in &mut games[..3] {
            game.members(&peers);
        }
        peers[3].steam_id = 4;
        peers[3].epoch = 41;
        for game in &mut games[..3] {
            game.members(&peers);
        }
        games[0].fail_peer(4, 40); // Late fault must not fail the new incarnation.
        assert_eq!(games[0].send(4, b"new", 2, 0), Some(true));
        let h = games[0].health();
        let new = h.peers.iter().find(|p| p.peer == 4).unwrap();
        assert_eq!(
            (new.epoch, new.send_calls, new.received, new.failed),
            (41, 1, 0, false)
        );
        let other = h.peers.iter().find(|p| p.peer == 2).unwrap();
        assert_eq!((other.send_calls, other.consumed), (6, 6));
    }
    #[test]
    fn available_and_read_use_the_same_fair_peer_and_keep_channels_separate() {
        let mut b = bridge();
        b.members(&[
            Peer {
                client_id: 2,
                steam_id: 20,
                epoch: 200,
            },
            Peer {
                client_id: 3,
                steam_id: 30,
                epoch: 300,
            },
        ]);
        b.accept(20);
        b.accept(30);
        for _ in 0..20 {
            b.receive(incoming(0));
        }
        let mut p = incoming(0);
        p.from = 30;
        p.source_epoch = 300;
        p.payload = vec![9; 77];
        b.receive(p);
        assert_eq!(b.available(1), None);
        assert_eq!(b.available(0), Some(3));
        assert_eq!(b.read(0).unwrap().from, 20);
        assert_eq!(b.available(0), Some(77));
        assert_eq!(b.read(0).unwrap().from, 30);
    }
    fn bridge() -> Bridge {
        let mut b = Bridge::new(10, 100);
        b.members(&[Peer {
            client_id: 2,
            steam_id: 20,
            epoch: 200,
        }]);
        b.active = true;
        b
    }
    fn incoming(channel: i32) -> Packet {
        Packet {
            from: 20,
            to: 10,
            source_epoch: 200,
            target_epoch: 100,
            channel,
            send_type: 2,
            payload: vec![1, 2, 3],
        }
    }
    #[test]
    fn receive_waits_for_accept_and_channels_remain_separate() {
        let mut b = bridge();
        assert_eq!(b.receive(incoming(1)), Admission::Queued);
        assert_eq!(b.available(1), None);
        assert_eq!(b.pop_event(), Some(Event::Request(20)));
        assert_eq!(b.accept(20), Some(true));
        assert_eq!(b.available(0), None);
        assert_eq!(b.available(1), Some(3));
        assert_eq!(b.read(1).unwrap().payload, vec![1, 2, 3]);
        assert_eq!(b.available(1), None);
    }
    #[test]
    fn buffering_flush_order() {
        let mut b = bridge();
        assert_eq!(b.send(20, b"a", 3, 0), Some(true));
        assert!(b.pop_outgoing().is_none());
        assert_eq!(b.send(20, b"b", 2, 0), Some(true));
        assert_eq!(b.pop_outgoing().unwrap().payload, b"a");
        assert_eq!(b.pop_outgoing().unwrap().payload, b"b");
    }
    #[test]
    fn no_delay_peers_exchange_first_packets_without_callbacks() {
        for receive_before_send in [false, true] {
            let mut a = bridge();
            let mut b = Bridge::new(20, 200);
            b.members(&[Peer {
                client_id: 1,
                steam_id: 10,
                epoch: 100,
            }]);
            b.active = true;
            assert_eq!(a.send(20, b"first", 1, 0), Some(true));
            let first = a
                .pop_outgoing()
                .expect("first no-delay packet must leave Hook");
            assert_eq!(first.send_type, 1);
            if receive_before_send {
                assert_eq!(b.receive(first.clone()), Admission::Queued);
                assert_eq!(
                    b.available(0),
                    None,
                    "unsolicited receive still needs acceptance"
                );
            }
            assert_eq!(b.send(10, b"reply", 1, 0), Some(true));
            let reply = b
                .pop_outgoing()
                .expect("peer must also send without a callback");
            assert_eq!(reply.send_type, 1);
            if !receive_before_send {
                assert_eq!(b.receive(first), Admission::Queued);
            }
            assert_eq!(a.receive(reply), Admission::Queued);
            assert_eq!(b.read(0).unwrap().payload, b"first");
            assert_eq!(a.read(0).unwrap().payload, b"reply");
            assert_eq!(a.session(20), Some((true, 0, 0)));
            assert_eq!(b.session(10), Some((true, 0, 0)));
            assert!(!b.needs_request(10));
        }
    }
    #[test]
    fn no_delay_send_keeps_readiness_and_size_limits() {
        let mut b = bridge();
        b.active = false;
        assert_eq!(b.send(20, b"waiting", 1, 0), Some(false));
        b.active = true;
        assert_eq!(b.send(20, &vec![0; 1201], 1, 0), Some(false));
        assert!(b.pop_outgoing().is_none());
        assert_eq!(b.session(20), Some((false, 0, 0)));
        assert_eq!(b.send(99, b"unknown", 1, 0), None);
        b.members(&[]);
        assert_eq!(b.send(20, b"offline", 1, 0), Some(false));
        b.stop();
        assert_eq!(b.send(20, b"stopped", 1, 0), Some(false));
        assert!(b.pop_outgoing().is_none());
    }
    #[test]
    fn new_epoch_and_disconnect_discard_queued_data() {
        let mut b = bridge();
        b.send(20, b"old", 2, 0);
        b.receive(incoming(0));
        b.members(&[Peer {
            client_id: 2,
            steam_id: 20,
            epoch: 201,
        }]);
        assert!(b.pop_outgoing().is_none());
        assert!(b.read(0).is_none());
        assert_eq!(b.send(99, b"not ours", 2, 0), None);
        b.stop();
        assert_eq!(b.send(20, b"no fallback", 2, 0), Some(false));
    }
    #[test]
    fn reliable_queue_pressure_reports_failure() {
        let mut b = bridge();
        let data = vec![0; MAX_PAYLOAD];
        for _ in 0..2 {
            assert_eq!(b.send(20, &data, 2, 0), Some(true));
        }
        assert_eq!(b.send(20, b"full", 2, 0), Some(false));
        assert_eq!(b.send(20, &vec![0; 1201], 0, 0), Some(false));
        assert_eq!(b.close(20, None), Some(true));
        assert!(b.pop_outgoing().is_none());
    }
    #[test]
    fn repeated_stop_preserves_pending_disconnect_callback() {
        let mut b = bridge();
        assert_eq!(b.accept(20), Some(true));
        b.stop();
        b.stop();
        assert_eq!(b.pop_event(), Some(Event::Failed(20)));
        assert_eq!(b.pop_event(), None);
    }

    #[test]
    fn twenty_second_backlog_skew_prefill_and_repeated_drain() {
        for counts in [[1000, 1000, 1000], [2700, 200, 100]] {
            for prefill in [0, 256] {
                let mut b = bridge();
                let peers: Vec<_> = (0..3)
                    .map(|n| Peer {
                        client_id: n + 2,
                        steam_id: 20 + n,
                        epoch: 200 + n,
                    })
                    .collect();
                b.members(&peers);
                for p in &peers {
                    b.accept(p.steam_id);
                }
                for _ in 0..2 {
                    for (n, p) in peers.iter().enumerate() {
                        for seq in 0..counts[n] + if n == 0 { prefill } else { 0 } {
                            let mut packet = incoming(n as i32);
                            packet.from = p.steam_id;
                            packet.source_epoch = p.epoch;
                            packet.payload = vec![0; 256];
                            packet.payload[..4].copy_from_slice(&(seq as u32).to_le_bytes());
                            assert_eq!(b.receive(packet), Admission::Queued);
                        }
                    }
                    assert!(!b.stopped);
                    for n in 0..3 {
                        for seq in 0..counts[n] + if n == 0 { prefill } else { 0 } {
                            assert_eq!(
                                &b.read(n as i32).unwrap().payload[..4],
                                &(seq as u32).to_le_bytes()
                            );
                        }
                    }
                    assert_eq!(b.health().queued_packets, 0);
                }
            }
        }
    }
    #[test]
    fn reliable_overflow_is_terminal_for_only_that_peer_and_epoch() {
        let mut b = bridge();
        let peers = vec![
            Peer {
                client_id: 2,
                steam_id: 20,
                epoch: 200,
            },
            Peer {
                client_id: 3,
                steam_id: 30,
                epoch: 300,
            },
        ];
        b.members(&peers);
        b.accept(20);
        b.accept(30);
        for _ in 0..PEER_PACKETS {
            assert_eq!(b.receive(incoming(0)), Admission::Queued);
        }
        assert_eq!(b.receive(incoming(0)), Admission::PeerFailed(20, 200));
        assert!(!b.stopped);
        b.members(&peers);
        b.close(20, None);
        assert_eq!(b.accept(20), Some(false));
        assert_eq!(b.send(20, b"no", 2, 0), Some(false));
        assert!(b.known(20));
        let mut other = incoming(0);
        other.from = 30;
        other.source_epoch = 300;
        assert_eq!(b.receive(other), Admission::Queued);
        assert_eq!(b.read(0).unwrap().from, 30);
        let mut renewed = peers;
        renewed[0].epoch = 201;
        b.members(&renewed);
        assert_eq!(b.accept(20), Some(true));
    }
}
