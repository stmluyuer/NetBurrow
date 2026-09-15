//! Bounded local handoff, shared by the client and Hook; not a Relay protocol.
use crate::{Message, Packet};
use std::{
    collections::{HashSet, VecDeque},
    time::{Duration, Instant},
};

pub const IPC_TIMEOUT: Duration = Duration::from_secs(30);
pub const IPC_WARNING: Duration = Duration::from_secs(6);
pub const DATA_PACKETS: usize = 4096;
pub const DATA_BYTES: usize = 8 * 1024 * 1024;
pub const PEER_PACKETS: usize = 3072;
pub const PEER_BYTES: usize = 4 * 1024 * 1024;

/// One packet per eligible peer per round; the first candidate for a peer wins.
/// Callers restrict candidates to the current identity barrier / game channel.
pub fn fair_index(
    candidates: impl Iterator<Item = (usize, (u64, u64))>,
    after: Option<(u64, u64)>,
) -> Option<usize> {
    let mut first = None;
    let mut next = None;
    for (index, key) in candidates {
        if first.is_none_or(|(_, old)| key < old) {
            first = Some((index, key));
        }
        if after.is_some_and(|old| key > old) && next.is_none_or(|(_, old)| key < old) {
            next = Some((index, key));
        }
    }
    next.or(first).map(|(index, _)| index)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    Queued,
    Dropped,
    PeerFailed(u64, u64),
}
#[derive(Clone, Copy)]
pub struct Limits {
    pub packets: usize,
    pub bytes: usize,
    pub peer_packets: usize,
    pub peer_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            packets: DATA_PACKETS,
            bytes: DATA_BYTES,
            peer_packets: PEER_PACKETS,
            peer_bytes: PEER_BYTES,
        }
    }
}

struct Queued {
    message: Message,
    udp: bool,
}
impl std::ops::Deref for Queued {
    type Target = Message;
    fn deref(&self) -> &Message {
        &self.message
    }
}

pub struct Frames {
    messages: VecDeque<Queued>,
    failed: HashSet<(u64, u64)>,
    faults: VecDeque<(u64, u64)>,
    incoming: bool,
    bytes: usize,
    packets: usize,
    pub dropped: u64,
    last_peer: Option<(u64, u64)>,
}
impl Frames {
    pub fn usage(&self, peer: Option<(u64, u64)>) -> (usize, usize) {
        match peer {
            None => (self.packets, self.bytes),
            Some(key) => self
                .messages
                .iter()
                .filter_map(|m| match &m.message {
                    Message::Data(p) if self.key(p) == key => Some(p.payload.len()),
                    _ => None,
                })
                .fold((0, 0), |(n, b), v| (n + 1, b + v)),
        }
    }
    pub fn packet_key(&self, p: &Packet) -> (u64, u64) {
        self.key(p)
    }
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
    pub fn new(incoming: bool) -> Self {
        Self {
            messages: VecDeque::new(),
            failed: HashSet::new(),
            faults: VecDeque::new(),
            incoming,
            bytes: 0,
            packets: 0,
            dropped: 0,
            last_peer: None,
        }
    }
    fn key(&self, p: &Packet) -> (u64, u64) {
        if self.incoming {
            (p.from, p.source_epoch)
        } else {
            (p.to, p.target_epoch)
        }
    }
    pub fn fail(&mut self, peer: u64, epoch: u64) {
        if self.failed.insert((peer, epoch)) {
            self.faults.push_back((peer, epoch));
        }
        let incoming = self.incoming;
        self.messages.retain(|m| !matches!(&m.message, Message::Data(p) if if incoming { (p.from,p.source_epoch)==(peer,epoch) } else { (p.to,p.target_epoch)==(peer,epoch) }));
        self.recount();
    }
    pub fn fault(&mut self) -> Option<(u64, u64)> {
        self.faults.pop_front()
    }
    pub fn push(&mut self, message: Message) -> Result<Admission, &'static str> {
        self.push_tagged(message, false)
    }
    pub fn push_tagged(&mut self, message: Message, udp: bool) -> Result<Admission, &'static str> {
        self.push_limited(message, udp, Limits::default())
    }
    pub fn push_limited(
        &mut self,
        message: Message,
        udp: bool,
        limits: Limits,
    ) -> Result<Admission, &'static str> {
        if let Message::Members(peers) = &message {
            self.failed
                .retain(|(id, epoch)| peers.iter().any(|p| p.steam_id == *id && p.epoch == *epoch));
            self.faults
                .retain(|(id, epoch)| peers.iter().any(|p| p.steam_id == *id && p.epoch == *epoch));
        }
        if let Message::Data(p) = &message {
            let key = self.key(p);
            if self.failed.contains(&key) {
                return Ok(Admission::Dropped);
            }
            let mut peer_count = 0;
            let mut peer_bytes = 0;
            let mut unreliable_count = 0;
            let mut unreliable_bytes = 0;
            for queued in &self.messages {
                if let Message::Data(q) = &queued.message {
                    if self.key(q) == key {
                        peer_count += 1;
                        peer_bytes += q.payload.len();
                    }
                    if q.send_type <= 1 {
                        unreliable_count += 1;
                        unreliable_bytes += q.payload.len();
                    }
                }
            }
            if p.send_type <= 1
                && (unreliable_count >= DATA_PACKETS / 2
                    || unreliable_bytes + p.payload.len() > DATA_BYTES / 2)
            {
                self.dropped += 1;
                return Ok(Admission::Dropped);
            }
            let full = self.packets >= limits.packets
                || self.bytes + p.payload.len() > limits.bytes
                || peer_count >= limits.peer_packets
                || peer_bytes + p.payload.len() > limits.peer_bytes;
            if full && p.send_type >= 2 {
                // Reclaim only lossy traffic. Never discard accepted reliable data and continue.
                // A per-peer limit must not evict another peer's packets.
                loop {
                    let (count, bytes) = self.usage(Some(key));
                    let peer_full =
                        count >= limits.peer_packets || bytes + p.payload.len() > limits.peer_bytes;
                    let global_full = self.packets >= limits.packets
                        || self.bytes + p.payload.len() > limits.bytes;
                    if !peer_full && !global_full {
                        break;
                    }
                    let victim = self
                        .messages
                        .iter()
                        .position(|m| {
                            matches!(&m.message,
                        Message::Data(q) if q.send_type <= 1 && self.key(q) == key)
                        })
                        .or_else(|| {
                            (!peer_full)
                                .then(|| {
                                    self.messages.iter().position(|m|
                            matches!(&m.message, Message::Data(q) if q.send_type <= 1))
                                })
                                .flatten()
                        });
                    let Some(index) = victim else {
                        break;
                    };
                    self.messages.remove(index);
                    self.recount();
                    self.dropped += 1;
                }
                let count = self
                    .messages
                    .iter()
                    .filter(|m| matches!(&m.message, Message::Data(q) if self.key(q)==key))
                    .count();
                let bytes: usize = self
                    .messages
                    .iter()
                    .filter_map(|m| match &m.message {
                        Message::Data(q) if self.key(q) == key => Some(q.payload.len()),
                        _ => None,
                    })
                    .sum();
                if self.packets >= limits.packets
                    || self.bytes + p.payload.len() > limits.bytes
                    || count >= limits.peer_packets
                    || bytes + p.payload.len() > limits.peer_bytes
                {
                    self.fail(key.0, key.1);
                    return Ok(Admission::PeerFailed(key.0, key.1));
                }
            } else if full {
                self.dropped += 1;
                return Ok(Admission::Dropped);
            }
            self.bytes += p.payload.len();
            self.packets += 1;
        } else {
            // Replace snapshots/probes only inside the current barrier segment.
            if matches!(
                message,
                Message::IpcHealth(_) | Message::Status(_) | Message::Ping(_)
            ) {
                let kind = std::mem::discriminant(&message);
                let start = self
                    .messages
                    .iter()
                    .rposition(|m| barrier(m))
                    .map_or(0, |i| i + 1);
                if let Some(index) = (start..self.messages.len())
                    .find(|&i| std::mem::discriminant(&self.messages[i].message) == kind)
                {
                    self.messages[index] = Queued { message, udp };
                    return Ok(Admission::Queued);
                }
            }
            if self.messages.len() - self.packets >= 64 {
                return Err("local control queue full");
            }
        }
        self.messages.push_back(Queued { message, udp });
        Ok(Admission::Queued)
    }
    pub fn pop(&mut self) -> Option<Message> {
        self.pop_tagged().map(|(m, _)| m)
    }
    pub fn pop_tagged(&mut self) -> Option<(Message, bool)> {
        let end = self
            .messages
            .iter()
            .position(|m| barrier(m))
            .unwrap_or(self.messages.len());
        let index = (0..end)
            .find(|&i| !matches!(self.messages[i].message, Message::Data(_)))
            .or_else(|| {
                fair_index(
                    (0..end).filter_map(|i| match &self.messages[i].message {
                        Message::Data(p) => Some((i, self.key(p))),
                        _ => None,
                    }),
                    self.last_peer,
                )
            })
            .unwrap_or(0);
        let Queued { message, udp } = self.messages.remove(index)?;
        if let Message::Data(p) = &message {
            self.last_peer = Some(self.key(p));
            self.packets -= 1;
            self.bytes -= p.payload.len();
        } else if barrier(&message) {
            self.last_peer = None;
        }
        Some((message, udp))
    }
    fn recount(&mut self) {
        self.bytes = 0;
        self.packets = 0;
        for m in &self.messages {
            if let Message::Data(p) = &m.message {
                self.packets += 1;
                self.bytes += p.payload.len();
            }
        }
    }
}
fn barrier(m: &Message) -> bool {
    matches!(
        m,
        Message::Bind { .. }
            | Message::Members(_)
            | Message::IpcReady
            | Message::IpcPeerFault { .. }
            | Message::Stop
            | Message::Leave
    )
}

/// Framing progress survives polling timeouts; only complete validated messages renew liveness.
pub struct Reader {
    bytes: Vec<u8>,
    length: Option<usize>,
    started: Option<Instant>,
    pub active: Instant,
}
impl Default for Reader {
    fn default() -> Self {
        Self {
            bytes: Vec::new(),
            length: None,
            started: None,
            active: Instant::now(),
        }
    }
}
impl Reader {
    pub fn poll(&mut self, input: &mut impl std::io::Read) -> std::io::Result<Option<Message>> {
        use std::io::{Error, ErrorKind};
        loop {
            if self.active.elapsed() >= IPC_TIMEOUT
                || self.started.is_some_and(|t| t.elapsed() >= IPC_TIMEOUT)
            {
                return Err(Error::new(ErrorKind::TimedOut, "IPC receive deadline"));
            }
            let need = self.length.unwrap_or(4).saturating_sub(self.bytes.len());
            let mut buffer = [0u8; 8192];
            let count = need.min(buffer.len());
            match input.read(&mut buffer[..count]) {
                Ok(0) => return Err(Error::new(ErrorKind::UnexpectedEof, "IPC closed")),
                Ok(n) => {
                    self.started.get_or_insert_with(Instant::now);
                    self.bytes.extend_from_slice(&buffer[..n]);
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) =>
                {
                    return Ok(None);
                }
                Err(e) => return Err(e),
            }
            if self.length.is_none() && self.bytes.len() == 4 {
                let size = u32::from_be_bytes(self.bytes[..4].try_into().unwrap()) as usize;
                if !(5..=crate::MAX_FRAME - 4).contains(&size) {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "invalid IPC frame length",
                    ));
                }
                self.length = Some(size + 4);
            }
            if self.length == Some(self.bytes.len()) {
                let message = crate::decode(&self.bytes[4..])?;
                self.bytes.clear();
                self.length = None;
                self.started = None;
                self.active = Instant::now();
                return Ok(Some(message));
            }
        }
    }
}

pub struct PendingWrite {
    bytes: Vec<u8>,
    offset: usize,
    started: Instant,
}
impl PendingWrite {
    pub fn new(message: &Message) -> std::io::Result<Self> {
        Ok(Self {
            bytes: crate::encode(message)?,
            offset: 0,
            started: Instant::now(),
        })
    }
    pub fn poll(&mut self, output: &mut impl std::io::Write) -> std::io::Result<bool> {
        use std::io::{Error, ErrorKind};
        if self.started.elapsed() >= IPC_TIMEOUT {
            return Err(Error::new(ErrorKind::TimedOut, "IPC write deadline"));
        }
        match output.write(&self.bytes[self.offset..]) {
            Ok(0) => Err(Error::new(ErrorKind::WriteZero, "IPC write closed")),
            Ok(n) => {
                self.offset += n;
                Ok(self.offset == self.bytes.len())
            }
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) =>
            {
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HookHealth, IPC_CAPABILITIES, encode};
    use std::io::{self, Read, Write};
    fn packet(peer: u64) -> Message {
        Message::Data(Packet {
            from: peer,
            to: 10,
            source_epoch: peer * 10,
            target_epoch: 100,
            channel: 0,
            send_type: 2,
            payload: vec![3; 256],
        })
    }
    #[test]
    fn local_messages_roundtrip_and_reject_truncation() {
        for m in [
            Message::IpcHelloV2 {
                nonce: [7; 16],
                pid: 42,
                steam_id: 10,
                epoch: 100,
                capabilities: IPC_CAPABILITIES,
            },
            Message::IpcAccepted(IPC_CAPABILITIES),
            Message::IpcHealth(HookHealth {
                queued_packets: 3000,
                interface_changed: true,
                peers: vec![crate::HookPeerHealth {
                    peer: 20,
                    epoch: 200,
                    send_calls: 17,
                    consumed: 8,
                    queued_packets: 3,
                    ..Default::default()
                }],
                ..HookHealth::default()
            }),
            Message::IpcPeerFault {
                peer: 20,
                epoch: 200,
            },
        ] {
            let frame = encode(&m).unwrap();
            assert_eq!(crate::decode(&frame[4..]).unwrap(), m);
            assert!(crate::decode(&frame[4..frame.len() - 1]).is_err());
            let mut extra = frame[4..].to_vec();
            extra.push(0);
            assert!(crate::decode(&extra).is_err());
        }
    }
    #[test]
    fn peer_health_rejects_duplicate_identity_and_invalid_count() {
        let p = crate::HookPeerHealth {
            peer: 20,
            epoch: 200,
            ..Default::default()
        };
        let duplicate = Message::IpcHealth(HookHealth {
            peers: vec![p.clone(), p.clone()],
            ..Default::default()
        });
        assert!(crate::decode(&encode(&duplicate).unwrap()[4..]).is_err());
        let oversized = Message::IpcHealth(HookHealth {
            peers: vec![p; crate::MAX_HEALTH_PEERS + 1],
            ..Default::default()
        });
        assert!(encode(&oversized).is_err());
    }
    #[test]
    fn control_priority_never_crosses_identity_barrier() {
        let mut q = Frames::new(true);
        q.push(packet(20)).unwrap();
        q.push(Message::Ping(1)).unwrap();
        q.push(Message::Members(vec![])).unwrap();
        q.push(Message::Ping(2)).unwrap();
        assert_eq!(q.pop(), Some(Message::Ping(1)));
        assert_eq!(q.pop(), Some(packet(20)));
        assert_eq!(q.pop(), Some(Message::Members(vec![])));
        assert_eq!(q.pop(), Some(Message::Ping(2)));
    }
    #[test]
    fn peer_round_robin_preserves_order_tags_and_identity_barriers() {
        let mut q = Frames::new(true);
        for peer in [20, 30, 40] {
            for n in 0..4 {
                let Message::Data(mut p) = packet(peer) else {
                    unreachable!()
                };
                p.payload = vec![n];
                p.channel = i32::from(n % 2);
                q.push_tagged(Message::Data(p), n % 2 == 0).unwrap();
            }
        }
        q.push(Message::Members(vec![])).unwrap();
        q.push(packet(5)).unwrap(); // Cannot jump across Members even with a smaller key.
        q.push(Message::Ping(9)).unwrap();
        for n in 0..4 {
            for peer in [20, 30, 40] {
                let (Message::Data(p), udp) = q.pop_tagged().unwrap() else {
                    panic!("barrier crossed")
                };
                assert_eq!((p.from, p.payload, udp), (peer, vec![n], n % 2 == 0));
            }
        }
        assert!(matches!(q.pop(), Some(Message::Members(_))));
        assert_eq!(q.pop(), Some(Message::Ping(9)));
        assert_eq!(q.pop(), Some(packet(5)));
    }
    #[test]
    fn saturated_peer_does_not_evict_other_peers_lossy_data() {
        let mut q = Frames::new(true);
        let limits = Limits {
            packets: 8,
            bytes: 4096,
            peer_packets: 2,
            peer_bytes: 4096,
        };
        q.push_limited(packet(20), false, limits).unwrap();
        q.push_limited(packet(20), false, limits).unwrap();
        let Message::Data(mut lossy) = packet(30) else {
            unreachable!()
        };
        lossy.send_type = 0;
        q.push_limited(Message::Data(lossy.clone()), true, limits)
            .unwrap();
        assert_eq!(
            q.push_limited(packet(20), false, limits).unwrap(),
            Admission::PeerFailed(20, 200)
        );
        assert_eq!(q.dropped, 0);
        assert_eq!(q.pop_tagged(), Some((Message::Data(lossy), true)));
    }
    #[test]
    fn skewed_backlog_fails_only_the_saturated_peer() {
        let mut q = Frames::new(true);
        for _ in 0..2956 {
            assert_eq!(q.push(packet(20)).unwrap(), Admission::Queued);
        }
        for _ in 0..300 {
            assert_eq!(q.push(packet(30)).unwrap(), Admission::Queued);
        }
        for _ in 2956..PEER_PACKETS {
            q.push(packet(20)).unwrap();
        }
        assert_eq!(q.push(packet(20)).unwrap(), Admission::PeerFailed(20, 200));
        assert_eq!(q.fault(), Some((20, 200)));
        assert_eq!(q.push(packet(20)).unwrap(), Admission::Dropped);
        for _ in 0..300 {
            assert_eq!(q.pop(), Some(packet(30)));
        }
        assert!(q.is_empty());
    }
    struct Partial {
        bytes: Vec<u8>,
        available: usize,
        position: usize,
    }
    impl Read for Partial {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.position == self.available {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let n = out.len().min(self.available - self.position);
            out[..n].copy_from_slice(&self.bytes[self.position..self.position + n]);
            self.position += n;
            Ok(n)
        }
    }
    #[test]
    fn partial_read_survives_twenty_seconds_and_expires_at_thirty() {
        for seconds in [5, 8, 15, 20, 29] {
            let frame = encode(&Message::Ping(7)).unwrap();
            let mut input = Partial {
                bytes: frame.clone(),
                available: 6,
                position: 0,
            };
            let mut r = Reader::default();
            assert!(r.poll(&mut input).unwrap().is_none());
            r.active = Instant::now() - Duration::from_secs(seconds);
            r.started = Some(r.active);
            input.available = frame.len();
            assert_eq!(r.poll(&mut input).unwrap(), Some(Message::Ping(7)));
        }
        let mut r = Reader::default();
        r.active = Instant::now() - Duration::from_secs(31);
        assert_eq!(
            r.poll(&mut &[][..]).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }
    struct Limited {
        bytes: Vec<u8>,
        capacity: usize,
    }
    impl Write for Limited {
        fn write(&mut self, input: &[u8]) -> io::Result<usize> {
            if self.capacity == 0 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let n = input.len().min(self.capacity);
            self.bytes.extend_from_slice(&input[..n]);
            self.capacity -= n;
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn partial_write_resumes_without_duplicate_prefix_and_has_fixed_deadline() {
        let message = packet(20);
        let expected = encode(&message).unwrap();
        let mut output = Limited {
            bytes: vec![],
            capacity: 7,
        };
        let mut pending = PendingWrite::new(&message).unwrap();
        assert!(!pending.poll(&mut output).unwrap());
        pending.started = Instant::now() - Duration::from_secs(20);
        assert!(!pending.poll(&mut output).unwrap());
        output.capacity = expected.len();
        assert!(pending.poll(&mut output).unwrap());
        assert_eq!(output.bytes, expected);
        let mut pending = PendingWrite::new(&message).unwrap();
        pending.started = Instant::now() - Duration::from_secs(31);
        assert_eq!(
            pending.poll(&mut output).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }
}
