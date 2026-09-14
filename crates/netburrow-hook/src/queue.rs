use netburrow_protocol::{MAX_PAYLOAD, Packet, Peer};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

const BYTE_LIMIT: usize = 4 * 1024 * 1024;
const PACKET_LIMIT: usize = 512;

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
}
struct Outgoing {
    packet: Packet,
    queued: Instant,
}

pub struct Bridge {
    pub steam_id: u64,
    pub epoch: u64,
    pub active: bool,
    pub stopped: bool,
    remotes: BTreeMap<u64, Remote>,
    inbound: VecDeque<Packet>,
    outbound: VecDeque<Outgoing>,
    in_bytes: usize,
    out_bytes: usize,
    events: VecDeque<Event>,
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
            outbound: VecDeque::new(),
            in_bytes: 0,
            out_bytes: 0,
            events: VecDeque::new(),
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
            });
            if remote.epoch != peer.epoch {
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
        self.outbound.retain(|p| {
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
        self.outbound.clear();
        self.in_bytes = 0;
        self.out_bytes = 0;
    }
    /// `None` means this peer is not ours, so the original Steam method handles it.
    pub fn send(&mut self, to: u64, bytes: &[u8], send_type: u8, channel: i32) -> Option<bool> {
        let peer = self.remotes.get_mut(&to)?;
        if !self.active
            || !peer.online
            || channel < 0
            || send_type > 3
            || bytes.len() > MAX_PAYLOAD
            || (send_type <= 1 && bytes.len() > 1200)
        {
            return Some(false);
        }
        if send_type == 1 && !peer.accepted {
            return Some(true);
        } // Steam's no-delay mode drops before establishment.
        if self.outbound.len() >= PACKET_LIMIT || self.out_bytes + bytes.len() > BYTE_LIMIT {
            return Some(false);
        }
        peer.accepted = true;
        peer.channels.insert(channel);
        peer.requested = None;
        self.out_bytes += bytes.len();
        self.outbound.push_back(Outgoing {
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
        // Preserve each peer's order. Buffered reliable messages wait up to 200 ms, unless
        // a regular reliable send flushes that peer or enough bytes form a packet.
        let index = self.outbound.iter().enumerate().find_map(|(index, item)| {
            if self
                .outbound
                .iter()
                .take(index)
                .any(|earlier| earlier.packet.to == item.packet.to)
            {
                return None;
            }
            let eligible = item.packet.send_type != 3
                || item.queued.elapsed() >= Duration::from_millis(200)
                || self
                    .outbound
                    .iter()
                    .any(|p| p.packet.to == item.packet.to && p.packet.send_type == 2)
                || self
                    .outbound
                    .iter()
                    .filter(|p| p.packet.to == item.packet.to)
                    .map(|p| p.packet.payload.len())
                    .sum::<usize>()
                    >= 1200;
            eligible.then_some(index)
        })?;
        let item = self.outbound.remove(index)?;
        self.out_bytes -= item.packet.payload.len();
        Some(item.packet)
    }
    /// false is a reliable overflow: the caller must close this instance, not silently skip data.
    pub fn receive(&mut self, packet: Packet) -> bool {
        if !self.active || packet.to != self.steam_id || packet.target_epoch != self.epoch {
            return true;
        }
        let Some(peer) = self.remotes.get_mut(&packet.from) else {
            return true;
        };
        if !peer.online || peer.epoch != packet.source_epoch {
            return true;
        }
        if self.inbound.len() >= PACKET_LIMIT || self.in_bytes + packet.payload.len() > BYTE_LIMIT {
            return packet.send_type <= 1;
        }
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
        self.inbound.push_back(packet);
        true
    }
    pub fn available(&self, channel: i32) -> Option<usize> {
        self.inbound
            .iter()
            .find(|p| p.channel == channel && self.remotes.get(&p.from).is_some_and(|r| r.accepted))
            .map(|p| p.payload.len())
    }
    pub fn read(&mut self, channel: i32) -> Option<Packet> {
        let index = self.inbound.iter().position(|p| {
            p.channel == channel && self.remotes.get(&p.from).is_some_and(|r| r.accepted)
        })?;
        let packet = self.inbound.remove(index)?;
        self.in_bytes -= packet.payload.len();
        Some(packet)
    }
    pub fn accept(&mut self, id: u64) -> Option<bool> {
        let peer = self.remotes.get_mut(&id)?;
        if !self.active || !peer.online {
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
        let packets: Vec<_> = self.outbound.iter().filter(|p| p.packet.to == id).collect();
        Some((
            self.active && peer.online && peer.accepted,
            packets.iter().map(|p| p.packet.payload.len()).sum(),
            packets.len(),
        ))
    }
    pub fn pop_event(&mut self) -> Option<Event> {
        self.events.pop_front()
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
        self.in_bytes = self.inbound.iter().map(|p| p.payload.len()).sum();
        self.out_bytes = self.outbound.iter().map(|p| p.packet.payload.len()).sum();
    }
    fn drop_peer_packets(&mut self, id: u64, channel: Option<i32>) {
        self.inbound
            .retain(|p| p.from != id || channel.is_some_and(|c| p.channel != c));
        self.outbound
            .retain(|p| p.packet.to != id || channel.is_some_and(|c| p.packet.channel != c));
        self.recount();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(b.receive(incoming(1)));
        assert_eq!(b.available(1), None);
        assert_eq!(b.pop_event(), Some(Event::Request(20)));
        assert_eq!(b.accept(20), Some(true));
        assert_eq!(b.available(0), None);
        assert_eq!(b.available(1), Some(3));
        assert_eq!(b.read(1).unwrap().payload, vec![1, 2, 3]);
        assert_eq!(b.available(1), None);
    }
    #[test]
    fn buffering_flush_order_and_no_delay_behavior() {
        let mut b = bridge();
        assert_eq!(b.send(20, b"drop", 1, 0), Some(true));
        assert!(b.pop_outgoing().is_none());
        assert_eq!(b.send(20, b"a", 3, 0), Some(true));
        assert!(b.pop_outgoing().is_none());
        assert_eq!(b.send(20, b"b", 2, 0), Some(true));
        assert_eq!(b.pop_outgoing().unwrap().payload, b"a");
        assert_eq!(b.pop_outgoing().unwrap().payload, b"b");
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
        for _ in 0..4 {
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
}
