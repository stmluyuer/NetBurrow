use netburrow_protocol::{
    HookHealth, HookPeerHealth, MAX_HEALTH_PEERS, MAX_PAYLOAD, Packet, Peer,
    local::{Admission, DATA_BYTES, DATA_PACKETS, PEER_BYTES, PEER_PACKETS},
};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::Instant,
};

const BYTE_LIMIT: usize = 4 * 1024 * 1024;
const PACKET_LIMIT: usize = 1024;
// Bound observed channel metadata independently from queued packets.
const PEER_CHANNEL_LIMIT: usize = DATA_PACKETS;

struct Remote {
    member: u64,
    epoch: u64,
    online: bool,
    channels: HashSet<i32>,
    failed: bool,
    health: HookPeerHealth,
}
struct Incoming {
    packet: Packet,
    queued: Instant,
    // Keep IsP2PPacketAvailable and ReadP2PPacket on the same packet even if
    // the queue changes between the two game calls.
    advertised: bool,
}
impl std::ops::Deref for Incoming {
    type Target = Packet;
    fn deref(&self) -> &Packet {
        &self.packet
    }
}
#[derive(Default)]
pub struct Outbox {
    queue: Mutex<OutgoingQueue>,
}
impl Outbox {
    pub fn pop(&self) -> Option<Packet> {
        let mut queue = self.queue.lock().expect("poisoned Outbox");
        queue.pop()
    }
    fn retain(&self, keep: impl FnMut(&Packet) -> bool) {
        let mut queue = self.queue.lock().expect("poisoned Outbox");
        queue.packets.retain(keep);
        queue.recount();
    }
    fn clear(&self) {
        let mut queue = self.queue.lock().expect("poisoned Outbox");
        *queue = OutgoingQueue::default();
    }
}
#[derive(Default)]
struct OutgoingQueue {
    packets: VecDeque<Packet>,
    bytes: usize,
}
impl OutgoingQueue {
    fn push(&mut self, item: Packet) {
        self.bytes += item.payload.len();
        self.packets.push_back(item);
    }
    fn recount(&mut self) {
        self.bytes = self.packets.iter().map(|p| p.payload.len()).sum();
    }
    fn pop(&mut self) -> Option<Packet> {
        let packet = self.packets.pop_front()?;
        self.bytes -= packet.payload.len();
        Some(packet)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SendRejections {
    pub unavailable: u64,
    pub invalid: u64,
    pub queue_busy: u64,
    pub global_full: u64,
    pub peer_full: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReadError {
    QueryInvalidated,
    TooManyQueries,
}

pub struct Bridge {
    pub steam_id: u64,
    pub epoch: u64,
    pub active: bool,
    pub stopped: bool,
    remotes: BTreeMap<u64, Remote>,
    inbound: VecDeque<Incoming>,
    pub outbound: Arc<Outbox>,
    in_bytes: usize,
    pub health: HookHealth,
    pub send_rejections: SendRejections,
    pub telemetry: crate::telemetry::Telemetry,
    faults: VecDeque<(u64, u64)>,
    // Outlive packet cleanup so a stale size query cannot select another packet.
    // A fresh query renews or clears it; a successful read clears it.
    // Bounded by DATA_PACKETS, including queries whose packets were removed.
    pending_queries: HashSet<i32>,
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
            outbound: Arc::new(Outbox::default()),
            in_bytes: 0,
            health: HookHealth::default(),
            send_rejections: SendRejections::default(),
            telemetry: crate::telemetry::Telemetry::default(),
            faults: VecDeque::new(),
            pending_queries: HashSet::new(),
        }
    }
    pub fn members(&mut self, peers: &[Peer]) {
        let mut changed = Vec::new();
        for (&id, peer) in &mut self.remotes {
            let epoch = peers
                .iter()
                .find(|p| p.steam_id == id && p.epoch != 0)
                .map(|p| p.epoch);
            if epoch != Some(peer.epoch) {
                if peer.online {
                    changed.push(id);
                }
                peer.online = false;
                peer.channels.clear();
            }
        }
        for id in changed {
            self.drop_peer_packets(id, "member_unbound_or_rebound");
        }
        for peer in peers
            .iter()
            .filter(|p| p.steam_id != 0 && p.steam_id != self.steam_id && p.epoch != 0)
        {
            if !self.remotes.contains_key(&peer.steam_id) {
                self.telemetry
                    .event(peer.client_id, "member_bound", None, 0, 0);
            }
            let remote = self.remotes.entry(peer.steam_id).or_insert_with(|| Remote {
                member: peer.client_id,
                epoch: peer.epoch,
                online: true,
                channels: HashSet::new(),
                failed: false,
                health: HookPeerHealth::default(),
            });
            if remote.epoch != peer.epoch || !remote.online {
                self.telemetry
                    .event(peer.client_id, "member_bound", None, 0, 0);
            }
            if remote.epoch != peer.epoch {
                remote.health = HookPeerHealth::default();
                remote.failed = false;
                remote.channels.clear();
            }
            remote.epoch = peer.epoch;
            remote.member = peer.client_id;
            remote.online = true;
        }
        // Keep only packets from the current remote incarnation.
        self.inbound.retain(|p| {
            self.remotes
                .get(&p.from)
                .is_some_and(|r| r.online && r.epoch == p.source_epoch)
        });
        self.outbound.retain(|p| {
            self.remotes
                .get(&p.to)
                .is_some_and(|r| r.online && r.epoch == p.target_epoch)
        });
        self.recount();
    }
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.active = false;
        self.stopped = true;
        let peers: Vec<_> = self.remotes.keys().copied().collect();
        for id in peers {
            self.drop_peer_packets(id, "hook_stopped");
        }
        for remote in self.remotes.values_mut() {
            remote.online = false;
            remote.channels.clear();
        }
        self.inbound.clear();
        self.outbound.clear();
        self.in_bytes = 0;
    }
    /// Replacement mode rejects destinations without a current Relay binding.
    pub fn send(&mut self, to: u64, bytes: &[u8], send_type: u8, channel: i32) -> Option<bool> {
        self.health.send_calls += 1;
        let result = self.send_inner(to, bytes, send_type, channel);
        if let Some(peer) = self.remotes.get_mut(&to) {
            peer.health.send_calls += 1;
            peer.health.send_rejected += u64::from(result == Some(false));
            if let Some(flow) = self.telemetry.flow(peer.member, channel, send_type) {
                if result == Some(true) {
                    flow.sent += 1;
                    flow.sent_bytes += bytes.len() as u64;
                    flow.last_send = Some(Instant::now());
                } else {
                    flow.rejected += 1;
                }
            }
        }
        if result == Some(false) {
            self.health.send_rejected += 1;
        }
        result
    }
    fn send_inner(&mut self, to: u64, bytes: &[u8], send_type: u8, channel: i32) -> Option<bool> {
        let Some(peer) = self.remotes.get_mut(&to) else {
            self.send_rejections.unavailable += 1;
            return Some(false);
        };
        if !self.active || !peer.online || peer.failed {
            self.send_rejections.unavailable += 1;
            return Some(false);
        }
        if channel < 0
            || send_type > 3
            || bytes.len() > MAX_PAYLOAD
            || (send_type <= 1 && bytes.len() > 1200)
        {
            self.send_rejections.invalid += 1;
            return Some(false);
        }
        if !peer.channels.contains(&channel) && peer.channels.len() >= PEER_CHANNEL_LIMIT {
            self.send_rejections.peer_full += 1;
            return Some(false);
        }
        // Copy caller-owned bytes before taking the shared writer queue lock.
        let payload = bytes.to_vec();
        let mut outbound = self.outbound.queue.lock().expect("poisoned Outbox");
        if outbound.packets.len() >= PACKET_LIMIT || outbound.bytes + bytes.len() > BYTE_LIMIT {
            self.send_rejections.global_full += 1;
            return Some(false);
        }
        let (count, size) = outbound
            .packets
            .iter()
            .filter(|p| p.to == to)
            .fold((0, 0), |(n, bytes), p| (n + 1, bytes + p.payload.len()));
        if count >= 512 || size + bytes.len() > 2 * 1024 * 1024 {
            self.send_rejections.peer_full += 1;
            return Some(false);
        }
        outbound.push(Packet {
            delivery: None,
            from: self.steam_id,
            to,
            source_epoch: self.epoch,
            target_epoch: peer.epoch,
            channel,
            send_type,
            payload,
        });
        drop(outbound);
        peer.channels.insert(channel);
        Some(true)
    }
    #[cfg(test)]
    pub fn pop_outgoing(&mut self) -> Option<Packet> {
        self.outbound.pop()
    }
    pub fn receive(&mut self, packet: Packet) -> Admission {
        let id = packet.from;
        let (channel, kind, bytes) = (packet.channel, packet.send_type, packet.payload.len());
        let current = packet.to == self.steam_id
            && packet.target_epoch == self.epoch
            && self
                .remotes
                .get(&id)
                .is_some_and(|p| p.epoch == packet.source_epoch);
        let result = self.receive_inner(packet);
        if let Some(peer) = self.remotes.get(&id) {
            if let Some(flow) = self.telemetry.flow(peer.member, channel, kind) {
                flow.received += 1;
                flow.received_bytes += bytes as u64;
                flow.stale_received += u64::from(!current);
                flow.last_receive = Some(Instant::now());
                if result != Admission::Queued {
                    flow.dropped += 1;
                }
            }
        }
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
        if !peer.channels.contains(&packet.channel) && peer.channels.len() >= PEER_CHANNEL_LIMIT {
            self.fail_peer(peer_id, peer_epoch);
            return Admission::PeerFailed(peer_id, peer_epoch);
        }
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
                    .position(|p| p.from == peer_id && p.send_type <= 1 && !p.advertised)
                    .or_else(|| {
                        (!peer_full)
                            .then(|| {
                                self.inbound
                                    .iter()
                                    .position(|p| p.send_type <= 1 && !p.advertised)
                            })
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
                    if let Some(flow) =
                        self.telemetry
                            .flow(peer.member, removed.channel, removed.send_type)
                    {
                        flow.dropped += 1;
                    }
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
        peer.channels.insert(packet.channel);
        self.in_bytes += packet.payload.len();
        self.inbound.push_back(Incoming {
            packet,
            queued: Instant::now(),
            advertised: false,
        });
        Admission::Queued
    }
    pub fn available(&mut self, channel: i32) -> Result<Option<usize>, ReadError> {
        let result = match self.read_index(channel) {
            Some(_)
                if !self.pending_queries.contains(&channel)
                    && self.pending_queries.len() >= DATA_PACKETS =>
            {
                Err(ReadError::TooManyQueries)
            }
            Some(i) => {
                self.pending_queries.insert(channel);
                let packet = &mut self.inbound[i];
                packet.advertised = true;
                Ok(Some(packet.payload.len()))
            }
            None => {
                self.pending_queries.remove(&channel);
                Ok(None)
            }
        };
        self.telemetry
            .poll(channel, false, matches!(result, Ok(Some(_))));
        result
    }
    fn read_index(&self, channel: i32) -> Option<usize> {
        self.inbound.iter().position(|p| p.channel == channel)
    }
    pub fn read(&mut self, channel: i32) -> Result<Option<Packet>, ReadError> {
        self.health.read_calls += 1;
        let index = self.read_index(channel);
        if self.pending_queries.contains(&channel)
            && !index.is_some_and(|i| self.inbound[i].advertised)
        {
            self.telemetry.poll(channel, true, false);
            return Err(ReadError::QueryInvalidated);
        }
        self.telemetry.poll(channel, true, index.is_some());
        let Some(index) = index else {
            return Ok(None);
        };
        let packet = self.inbound.remove(index).unwrap();
        self.pending_queries.remove(&channel);
        self.in_bytes -= packet.payload.len();
        self.health.consumed += 1;
        if let Some(peer) = self.remotes.get_mut(&packet.from) {
            peer.health.consumed += 1;
            if let Some(flow) = self
                .telemetry
                .flow(peer.member, packet.channel, packet.send_type)
            {
                flow.consumed += 1;
                flow.consumed_bytes += packet.payload.len() as u64;
                flow.last_read = Some(Instant::now());
            }
        }
        Ok(Some(packet.packet))
    }
    pub fn fail_peer(&mut self, id: u64, epoch: u64) {
        let Some(peer) = self.remotes.get_mut(&id) else {
            return;
        };
        if peer.epoch != epoch || peer.failed {
            return;
        }
        peer.failed = true;
        peer.channels.clear();
        self.drop_peer_packets(id, "peer_failed");
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
        let outgoing: Vec<_> = self
            .outbound
            .queue
            .lock()
            .expect("poisoned Outbox")
            .packets
            .iter()
            .map(|p| p.to)
            .collect();
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
                p.outgoing_packets = outgoing.iter().filter(|&&peer| peer == id).count() as u32;
                p
            })
            .collect();
        health
    }
    fn recount(&mut self) {
        self.in_bytes = self.inbound.iter().map(|p| p.payload.len()).sum();
    }
    fn drop_peer_packets(&mut self, id: u64, reason: &'static str) {
        let discarded = self.inbound.iter().filter(|p| p.from == id).count();
        if let Some(peer) = self.remotes.get_mut(&id) {
            peer.health.discarded += discarded as u64;
            for packet in self.inbound.iter().filter(|p| p.from == id) {
                if let Some(flow) =
                    self.telemetry
                        .flow(peer.member, packet.channel, packet.send_type)
                {
                    flow.cleared_in += 1;
                }
            }
            let mut outgoing = self.outbound.queue.lock().expect("poisoned Outbox");
            let mut cleared_out = 0;
            for item in outgoing.packets.iter().filter(|p| p.to == id) {
                cleared_out += 1;
                if let Some(flow) = self
                    .telemetry
                    .flow(peer.member, item.channel, item.send_type)
                {
                    flow.cleared_out += 1;
                }
            }
            self.telemetry
                .event(peer.member, reason, None, discarded, cleared_out);
            outgoing.packets.retain(|p| p.to != id);
            outgoing.recount();
        }
        self.inbound.retain(|p| p.from != id);
        self.recount();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poisoned_outbox_is_not_reused() {
        let mut b = bridge();
        let outbox = b.outbound.clone();
        let _ = std::panic::catch_unwind(|| {
            let _guard = outbox.queue.lock().unwrap();
            panic!("test outbox failure");
        });
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| b.send(20, b"data", 2, 0)))
                .is_err()
        );
        assert!(std::panic::catch_unwind(|| outbox.pop()).is_err());
        assert!(outbox.queue.is_poisoned());
    }
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn advertised_packet_survives_arrivals_and_reads_on_other_channels() {
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
        let other_channel = incoming(1);
        assert_eq!(b.receive(other_channel.clone()), Admission::Queued);
        let mut small = incoming(0);
        small.from = 30;
        small.source_epoch = 300;
        small.send_type = 1;
        small.payload = vec![3; 16];
        assert_eq!(b.receive(small.clone()), Admission::Queued);
        let capacity = b.available(0).unwrap().unwrap();
        assert_eq!(capacity, 16);

        let mut large = incoming(0);
        large.send_type = 1;
        large.payload = vec![2; 40];
        assert_eq!(b.receive(large.clone()), Admission::Queued);
        assert_eq!(b.available(1).unwrap(), Some(other_channel.payload.len()));
        assert_eq!(b.read(1).unwrap(), Some(other_channel));
        let selected = b.read(0).unwrap().unwrap();
        assert!(selected.payload.len() <= capacity);
        assert_eq!(selected, small);
        assert_eq!(b.available(0).unwrap(), Some(40));
        assert_eq!(b.available(0).unwrap(), Some(40));
        assert_eq!(b.read(0).unwrap(), Some(large));
        assert!(b.read(0).unwrap().is_none());
    }

    #[test]
    fn reliable_arrival_does_not_evict_an_advertised_lossy_packet() {
        let mut b = bridge();
        let mut selected = incoming(0);
        selected.send_type = 1;
        selected.payload = vec![1; 16];
        assert_eq!(b.receive(selected.clone()), Admission::Queued);
        assert_eq!(b.available(0).unwrap(), Some(16));
        let mut evictable = incoming(0);
        evictable.send_type = 1;
        evictable.payload = vec![2; 40];
        assert_eq!(b.receive(evictable), Admission::Queued);
        for _ in 2..PEER_PACKETS {
            assert_eq!(b.receive(incoming(0)), Admission::Queued);
        }
        assert_eq!(b.receive(incoming(0)), Admission::Queued);
        assert_eq!(b.read(0).unwrap(), Some(selected));
        assert_eq!(b.health().dropped, 1);
        assert_eq!(b.pop_fault(), None);
    }

    #[test]
    fn removed_advertised_packet_requires_a_fresh_query() {
        let mut b = bridge();
        let remaining = Peer {
            client_id: 3,
            steam_id: 30,
            epoch: 300,
        };
        b.members(&[
            Peer {
                client_id: 2,
                steam_id: 20,
                epoch: 200,
            },
            remaining.clone(),
        ]);
        let mut small = incoming(0);
        small.payload = vec![1; 16];
        assert_eq!(b.receive(small), Admission::Queued);
        assert_eq!(b.available(0).unwrap(), Some(16));
        let mut large = incoming(0);
        large.from = 30;
        large.source_epoch = 300;
        large.payload = vec![2; 40];
        assert_eq!(b.receive(large.clone()), Admission::Queued);
        b.members(&[remaining]);
        assert_eq!(b.read(0), Err(ReadError::QueryInvalidated));
        assert_eq!(b.read(0), Err(ReadError::QueryInvalidated));
        assert_eq!(b.available(0).unwrap(), Some(40));
        assert_eq!(b.read(0).unwrap(), Some(large));
    }

    #[test]
    fn abandoned_queries_are_bounded_and_can_be_requeried() {
        let mut b = bridge();
        let peer = Peer {
            client_id: 2,
            steam_id: 20,
            epoch: 200,
        };
        for channel in 0..DATA_PACKETS as i32 {
            b.members(std::slice::from_ref(&peer));
            assert_eq!(b.receive(incoming(channel)), Admission::Queued);
            assert_eq!(b.available(channel).unwrap(), Some(3));
            b.members(&[]);
        }
        b.members(&[peer]);
        let next = DATA_PACKETS as i32;
        let packet = incoming(next);
        assert_eq!(b.receive(packet.clone()), Admission::Queued);
        assert_eq!(b.available(next), Err(ReadError::TooManyQueries));
        assert_eq!(b.available(0).unwrap(), None);
        assert_eq!(b.available(next).unwrap(), Some(packet.payload.len()));
        assert_eq!(b.read(next).unwrap(), Some(packet));
    }

    #[test]
    fn channel_metadata_is_bounded_when_unread_lossy_packets_are_evicted() {
        let mut b = bridge();
        let mut peers = vec![
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
        let mut healthy = incoming(77);
        healthy.from = 30;
        healthy.source_epoch = 300;
        assert_eq!(b.receive(healthy.clone()), Admission::Queued);
        assert_eq!(b.send(30, b"keep", 2, 77), Some(true));
        assert_eq!(b.send(20, b"clear on failure", 2, 0), Some(true));
        let lossy = |channel| {
            let mut packet = incoming(channel);
            packet.send_type = 0;
            packet
        };
        for channel in 1..=2048 {
            assert_eq!(b.receive(lossy(channel)), Admission::Queued);
        }
        for _ in 0..1024 {
            assert_eq!(b.receive(incoming(0)), Admission::Queued);
        }
        for channel in 2049..PEER_CHANNEL_LIMIT as i32 {
            assert_eq!(b.receive(incoming(0)), Admission::Queued);
            assert!(b.read(0).unwrap().is_some());
            assert_eq!(b.receive(lossy(channel)), Admission::Queued);
            assert_eq!(
                b.inbound.iter().filter(|p| p.from == 20).count(),
                PEER_PACKETS
            );
        }
        assert_eq!(b.remotes[&20].channels.len(), PEER_CHANNEL_LIMIT);
        assert_eq!(b.pop_fault(), None);
        assert_eq!(b.receive(incoming(0)), Admission::Queued);
        assert!(b.read(0).unwrap().is_some());
        assert_eq!(
            b.receive(lossy(PEER_CHANNEL_LIMIT as i32)),
            Admission::PeerFailed(20, 200)
        );
        assert!(b.remotes[&20].channels.is_empty());
        assert_eq!(b.pop_fault(), Some((20, 200)));
        assert_eq!(b.receive(incoming(0)), Admission::Dropped);
        assert_eq!(b.pop_fault(), None);
        assert_eq!(b.send(20, b"failed", 2, 0), Some(false));
        assert_eq!(b.read(77).unwrap(), Some(healthy));
        assert_eq!(b.pop_outgoing().unwrap().to, 30);
        assert!(b.pop_outgoing().is_none());
        assert!(!b.stopped);

        peers[0].epoch = 201;
        b.members(&peers);
        let mut renewed = incoming(1);
        renewed.source_epoch = 201;
        assert_eq!(b.receive(renewed), Admission::Queued);
        assert_eq!(b.pop_fault(), None);
    }

    #[test]
    fn sends_are_immediate_fifo_across_channels_and_peers() {
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
        assert_eq!(b.send(99, b"unknown", 2, 0), Some(false));
        assert_eq!(b.send(20, b"buffered", 3, 0), Some(true));
        assert_eq!(b.pop_outgoing().unwrap().payload, b"buffered");
        for peer in [20, 30] {
            for seq in 0..12u8 {
                assert_eq!(
                    b.send(
                        peer,
                        &[seq],
                        if seq == 11 { 2 } else { 3 },
                        i32::from(seq % 2)
                    ),
                    Some(true)
                );
            }
        }
        for peer in [20, 30] {
            for seq in 0..12u8 {
                let packet = b.pop_outgoing().unwrap();
                assert_eq!(
                    (packet.to, packet.payload[0], packet.channel),
                    (peer, seq, i32::from(seq % 2))
                );
            }
        }
        assert!(b.pop_outgoing().is_none());
        assert_eq!(b.health().dropped, 0);
        b.members(&[Peer {
            client_id: 2,
            steam_id: 20,
            epoch: 201,
        }]);
        b.fail_peer(20, 200);
        assert_eq!(b.send(20, b"new epoch", 2, 0), Some(true));
        assert_eq!(b.pop_outgoing().unwrap().target_epoch, 201);
    }

    #[test]
    fn concurrent_writer_preserves_every_accepted_packet_in_peer_order() {
        let mut b = bridge();
        b.members(
            &(0..3)
                .map(|n| Peer {
                    client_id: n + 2,
                    steam_id: n + 20,
                    epoch: n + 200,
                })
                .collect::<Vec<_>>(),
        );
        let outbox = b.outbound.clone();
        let done = Arc::new(AtomicBool::new(false));
        let worker_done = done.clone();
        let worker = std::thread::spawn(move || {
            let mut received = Vec::new();
            loop {
                if let Some(packet) = outbox.pop() {
                    received.push(packet);
                } else if worker_done.load(Ordering::Acquire)
                    && outbox.queue.lock().unwrap().packets.is_empty()
                {
                    break;
                } else {
                    std::thread::yield_now();
                }
            }
            received
        });
        let mut accepted = [Vec::new(), Vec::new(), Vec::new()];
        for seq in 0..2000u32 {
            for (n, packets) in accepted.iter_mut().enumerate() {
                if b.send(n as u64 + 20, &seq.to_le_bytes(), 2, 0) == Some(true) {
                    packets.push(seq);
                }
            }
            std::thread::yield_now();
        }
        done.store(true, Ordering::Release);
        let received = worker.join().unwrap();
        for (n, expected) in accepted.into_iter().enumerate() {
            assert!(!expected.is_empty());
            let actual: Vec<_> = received
                .iter()
                .filter(|p| p.to == n as u64 + 20)
                .map(|p| u32::from_le_bytes(p.payload.as_slice().try_into().unwrap()))
                .collect();
            assert_eq!(actual, expected);
        }
        assert!(b.outbound.queue.lock().unwrap().packets.is_empty());
    }

    #[test]
    fn four_players_exchange_in_fifo_order_and_restart_only_one_epoch() {
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
        // The writer and each receive channel preserve the order of accepted packets.
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
            for target in 1..=4 {
                if target == games[source].steam_id {
                    continue;
                }
                for n in 0..6u8 {
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
                for source in 1..=4 {
                    if source == game.steam_id {
                        continue;
                    }
                    for n in (channel..6).step_by(2) {
                        assert_eq!(game.available(channel).unwrap(), Some(4));
                        let p = game.read(channel).unwrap().unwrap();
                        assert_eq!((p.from, p.payload[0]), (source, n as u8));
                    }
                }
            }
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
            delivery: None,
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
        for _ in 0..PEER_PACKETS {
            assert_eq!(b.receive(incoming(0)), Admission::Queued);
        }
        assert_eq!(b.available(0).unwrap(), Some(3));
        assert_eq!(b.receive(incoming(0)), Admission::PeerFailed(20, 200));
        assert!(!b.stopped);
        b.members(&peers);
        assert_eq!(b.send(20, b"no", 2, 0), Some(false));
        let mut other = incoming(0);
        other.from = 30;
        other.source_epoch = 300;
        assert_eq!(b.receive(other), Admission::Queued);
        assert_eq!(b.read(0), Err(ReadError::QueryInvalidated));
        assert_eq!(b.available(0).unwrap(), Some(3));
        assert_eq!(b.read(0).unwrap().unwrap().from, 30);
        let mut renewed = peers;
        renewed[0].epoch = 201;
        b.members(&renewed);
        assert_eq!(b.send(20, b"new epoch", 2, 0), Some(true));
    }
}
