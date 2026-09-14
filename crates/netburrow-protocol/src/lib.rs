//! NB v1 framing and message encoding shared by the Windows client and Relay.
//!
//! The module deliberately uses only `std::io`, so it can be used by the IPC
//! client, the relay, and small diagnostic tools without an async runtime.

use std::io::{self, ErrorKind, Read, Write};

pub const MAX_PAYLOAD: usize = 1024 * 1024;
pub const MAX_FRAME: usize = MAX_PAYLOAD + 256;
pub const UDP_LIMIT: usize = 1200;

pub type Token = [u8; 16];
pub type Group = [u8; 32];
pub const HOOK_INIT_VERSION: u32 = 1;

/// Fixed helper-to-DLL initialization ABI.  This is intentionally plain data:
/// the helper writes it to target-process memory before calling the hook's
/// initialization entry point.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HookInit {
    pub version: u32,
    pub port: u32,
    pub pid: u32,
    pub reserved: u32,
    pub epoch: u64,
    pub nonce: Token,
}

impl Default for HookInit {
    fn default() -> Self {
        Self {
            version: HOOK_INIT_VERSION,
            port: 0,
            pid: 0,
            reserved: 0,
            epoch: 0,
            nonce: [0; 16],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub client_id: u64,
    pub steam_id: u64,
    pub epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub from: u64,
    pub to: u64,
    pub source_epoch: u64,
    pub target_epoch: u64,
    pub channel: i32,
    pub send_type: u8,
    pub payload: Vec<u8>,
}

/// Optional member telemetry. RTT is measured by that client against the Relay,
/// not between players. A report contains no destination or claimed sender ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberStatus {
    pub name: String,
    /// 0=connecting, 1=waiting, 2=attaching, 3=ready, 4=restart, 5=failed, 6=stopped.
    pub phase: u8,
    pub ping_ms: Option<u32>,
    /// 0=TCP, 1=UDP requested but not bound, 2=UDP bound (reliable data still TCP).
    pub transport: u8,
    pub sent: u64,
    pub received: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerStatus {
    pub client_id: u64,
    pub game_epoch: u64,
    pub age_ms: u32,
    pub status: MemberStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Join {
        group: Group,
    },
    Welcome {
        client_id: u64,
        udp_token: Token,
    },
    Bind {
        steam_id: u64,
        epoch: u64,
    },
    Members(Vec<Peer>),
    Data(Packet),
    Ping(u64),
    Pong(u64),
    Leave,
    Error(String),
    IpcHello {
        nonce: Token,
        pid: u32,
        steam_id: u64,
        epoch: u64,
    },
    IpcReady,
    Stop,
    Diagnostic(String),
    Status(MemberStatus),
    Statuses(Vec<PeerStatus>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Datagram {
    Bind {
        client_id: u64,
        token: Token,
    },
    Bound {
        client_id: u64,
    },
    Data {
        client_id: u64,
        token: Token,
        packet: Packet,
    },
}

const TCP_MAGIC: [u8; 4] = *b"NBP1";
const UDP_MAGIC: [u8; 4] = *b"NBD1";

const JOIN: u8 = 1;
const WELCOME: u8 = 2;
const BIND: u8 = 3;
const MEMBERS: u8 = 4;
const DATA: u8 = 5;
const PING: u8 = 6;
const PONG: u8 = 7;
const LEAVE: u8 = 8;
const ERROR: u8 = 9;
const IPC_HELLO: u8 = 10;
const IPC_READY: u8 = 11;
const STOP: u8 = 12;
const DIAGNOSTIC: u8 = 13;
const STATUS: u8 = 14;
const STATUSES: u8 = 15;

const UDP_BIND: u8 = 1;
const UDP_BOUND: u8 = 2;
const UDP_DATA: u8 = 3;

pub fn random_token() -> io::Result<Token> {
    let mut value = [0; 16];
    getrandom::fill(&mut value).map_err(random_error)?;
    Ok(value)
}

pub fn random_group() -> io::Result<Group> {
    let mut value = [0; 32];
    getrandom::fill(&mut value).map_err(random_error)?;
    Ok(value)
}

pub fn random_epoch() -> io::Result<u64> {
    let mut value = [0; 8];
    getrandom::fill(&mut value).map_err(random_error)?;
    Ok(u64::from_be_bytes(value))
}

pub fn encode(message: &Message) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    body.extend_from_slice(&TCP_MAGIC);
    match message {
        Message::Join { group } => {
            put_type(&mut body, JOIN);
            body.extend_from_slice(group);
        }
        Message::Welcome {
            client_id,
            udp_token,
        } => {
            put_type(&mut body, WELCOME);
            put_u64(&mut body, *client_id);
            body.extend_from_slice(udp_token);
        }
        Message::Bind { steam_id, epoch } => {
            put_type(&mut body, BIND);
            put_u64(&mut body, *steam_id);
            put_u64(&mut body, *epoch);
        }
        Message::Members(peers) => {
            put_type(&mut body, MEMBERS);
            let count = u16::try_from(peers.len()).map_err(|_| invalid("too many peers"))?;
            put_u16(&mut body, count);
            for peer in peers {
                put_u64(&mut body, peer.client_id);
                put_u64(&mut body, peer.steam_id);
                put_u64(&mut body, peer.epoch);
            }
        }
        Message::Data(packet) => {
            put_type(&mut body, DATA);
            encode_packet(&mut body, packet)?;
        }
        Message::Ping(value) => {
            put_type(&mut body, PING);
            put_u64(&mut body, *value);
        }
        Message::Pong(value) => {
            put_type(&mut body, PONG);
            put_u64(&mut body, *value);
        }
        Message::Leave => put_type(&mut body, LEAVE),
        Message::Error(text) => {
            put_type(&mut body, ERROR);
            put_string(&mut body, text)?;
        }
        Message::IpcHello {
            nonce,
            pid,
            steam_id,
            epoch,
        } => {
            put_type(&mut body, IPC_HELLO);
            body.extend_from_slice(nonce);
            put_u32(&mut body, *pid);
            put_u64(&mut body, *steam_id);
            put_u64(&mut body, *epoch);
        }
        Message::IpcReady => put_type(&mut body, IPC_READY),
        Message::Stop => put_type(&mut body, STOP),
        Message::Diagnostic(text) => {
            put_type(&mut body, DIAGNOSTIC);
            put_string(&mut body, text)?;
        }
        Message::Status(status) => {
            put_type(&mut body, STATUS);
            encode_status(&mut body, status)?;
        }
        Message::Statuses(peers) => {
            put_type(&mut body, STATUSES);
            put_u16(
                &mut body,
                u16::try_from(peers.len()).map_err(|_| invalid("too many status entries"))?,
            );
            for peer in peers {
                put_u64(&mut body, peer.client_id);
                put_u64(&mut body, peer.game_epoch);
                put_u32(&mut body, peer.age_ms);
                encode_status(&mut body, &peer.status)?;
            }
        }
    }
    check_frame_body(&body)?;
    let body_len = u32::try_from(body.len()).map_err(|_| invalid("frame is too large"))?;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&body_len.to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

pub fn decode(body: &[u8]) -> io::Result<Message> {
    check_frame_body(body)?;
    let mut reader = SliceReader::new(body);
    if reader.take_array::<4>()? != TCP_MAGIC {
        return Err(invalid("invalid TCP magic or version"));
    }
    let message = match reader.u8()? {
        JOIN => Message::Join {
            group: reader.take_array()?,
        },
        WELCOME => Message::Welcome {
            client_id: reader.u64()?,
            udp_token: reader.take_array()?,
        },
        BIND => Message::Bind {
            steam_id: reader.u64()?,
            epoch: reader.u64()?,
        },
        MEMBERS => {
            let count = reader.u16()? as usize;
            let mut peers = Vec::with_capacity(count);
            for _ in 0..count {
                peers.push(Peer {
                    client_id: reader.u64()?,
                    steam_id: reader.u64()?,
                    epoch: reader.u64()?,
                });
            }
            Message::Members(peers)
        }
        DATA => Message::Data(decode_packet(&mut reader)?),
        PING => Message::Ping(reader.u64()?),
        PONG => Message::Pong(reader.u64()?),
        LEAVE => Message::Leave,
        ERROR => Message::Error(reader.string()?),
        IPC_HELLO => Message::IpcHello {
            nonce: reader.take_array()?,
            pid: reader.u32()?,
            steam_id: reader.u64()?,
            epoch: reader.u64()?,
        },
        IPC_READY => Message::IpcReady,
        STOP => Message::Stop,
        DIAGNOSTIC => Message::Diagnostic(reader.string()?),
        STATUS => Message::Status(decode_status(&mut reader)?),
        STATUSES => {
            let count = reader.u16()? as usize;
            if count > (reader.bytes.len() - reader.position) / 46 {
                return Err(invalid("truncated member statuses"));
            }
            let mut peers = Vec::with_capacity(count);
            for _ in 0..count {
                peers.push(PeerStatus {
                    client_id: reader.u64()?,
                    game_epoch: reader.u64()?,
                    age_ms: reader.u32()?,
                    status: decode_status(&mut reader)?,
                });
            }
            Message::Statuses(peers)
        }
        _ => return Err(invalid("unknown message type")),
    };
    reader.finish()?;
    Ok(message)
}

fn validate_status(status: &MemberStatus) -> io::Result<()> {
    if status.name.len() > 96
        || status.name.chars().count() > 24
        || status.name.chars().any(char::is_control)
        || status.phase > 6
        || status.transport > 2
        || status.ping_ms == Some(u32::MAX)
    {
        return Err(invalid("invalid member status"));
    }
    Ok(())
}
fn encode_status(body: &mut Vec<u8>, status: &MemberStatus) -> io::Result<()> {
    validate_status(status)?;
    put_string(body, &status.name)?;
    body.push(status.phase);
    put_u32(body, status.ping_ms.unwrap_or(u32::MAX));
    body.push(status.transport);
    put_u64(body, status.sent);
    put_u64(body, status.received);
    Ok(())
}
fn decode_status(reader: &mut SliceReader<'_>) -> io::Result<MemberStatus> {
    let size = reader.u32()? as usize;
    if size > 96 {
        return Err(invalid("display name exceeds maximum"));
    }
    let name = String::from_utf8(reader.bytes(size)?.to_vec())
        .map_err(|_| invalid("invalid UTF-8 display name"))?;
    let phase = reader.u8()?;
    let ping = reader.u32()?;
    let status = MemberStatus {
        name,
        phase,
        ping_ms: (ping != u32::MAX).then_some(ping),
        transport: reader.u8()?,
        sent: reader.u64()?,
        received: reader.u64()?,
    };
    validate_status(&status)?;
    Ok(status)
}

pub fn read_message<R: Read>(reader: &mut R) -> io::Result<Message> {
    let mut length = [0; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if !(5..=MAX_FRAME - 4).contains(&length) {
        return Err(invalid("invalid frame length"));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    decode(&body)
}

pub fn write_message<W: Write>(writer: &mut W, message: &Message) -> io::Result<()> {
    writer.write_all(&encode(message)?)
}

pub fn encode_datagram(datagram: &Datagram) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&UDP_MAGIC);
    match datagram {
        Datagram::Bind { client_id, token } => {
            put_type(&mut bytes, UDP_BIND);
            put_u64(&mut bytes, *client_id);
            bytes.extend_from_slice(token);
        }
        Datagram::Bound { client_id } => {
            put_type(&mut bytes, UDP_BOUND);
            put_u64(&mut bytes, *client_id);
        }
        Datagram::Data {
            client_id,
            token,
            packet,
        } => {
            if is_reliable(packet.send_type) {
                return Err(invalid("reliable packets are TCP-only"));
            }
            put_type(&mut bytes, UDP_DATA);
            put_u64(&mut bytes, *client_id);
            bytes.extend_from_slice(token);
            encode_packet(&mut bytes, packet)?;
        }
    }
    if bytes.len() > UDP_LIMIT {
        return Err(invalid("datagram exceeds UDP limit"));
    }
    Ok(bytes)
}

pub fn decode_datagram(bytes: &[u8]) -> io::Result<Datagram> {
    if bytes.len() > UDP_LIMIT || bytes.len() < 5 {
        return Err(invalid("invalid datagram length"));
    }
    let mut reader = SliceReader::new(bytes);
    if reader.take_array::<4>()? != UDP_MAGIC {
        return Err(invalid("invalid UDP magic or version"));
    }
    let message = match reader.u8()? {
        UDP_BIND => Datagram::Bind {
            client_id: reader.u64()?,
            token: reader.take_array()?,
        },
        UDP_BOUND => Datagram::Bound {
            client_id: reader.u64()?,
        },
        UDP_DATA => {
            let client_id = reader.u64()?;
            let token = reader.take_array()?;
            let packet = decode_packet(&mut reader)?;
            if is_reliable(packet.send_type) {
                return Err(invalid("reliable packets are TCP-only"));
            }
            Datagram::Data {
                client_id,
                token,
                packet,
            }
        }
        _ => return Err(invalid("unknown datagram type")),
    };
    reader.finish()?;
    Ok(message)
}

pub fn is_reliable(send_type: u8) -> bool {
    matches!(send_type, 2 | 3)
}

fn encode_packet(bytes: &mut Vec<u8>, packet: &Packet) -> io::Result<()> {
    if packet.payload.len() > MAX_PAYLOAD {
        return Err(invalid("payload exceeds maximum"));
    }
    put_u64(bytes, packet.from);
    put_u64(bytes, packet.to);
    put_u64(bytes, packet.source_epoch);
    put_u64(bytes, packet.target_epoch);
    bytes.extend_from_slice(&packet.channel.to_be_bytes());
    bytes.push(packet.send_type);
    let length =
        u32::try_from(packet.payload.len()).map_err(|_| invalid("payload is too large"))?;
    put_u32(bytes, length);
    bytes.extend_from_slice(&packet.payload);
    Ok(())
}

fn decode_packet(reader: &mut SliceReader<'_>) -> io::Result<Packet> {
    let from = reader.u64()?;
    let to = reader.u64()?;
    let source_epoch = reader.u64()?;
    let target_epoch = reader.u64()?;
    let channel = i32::from_be_bytes(reader.take_array()?);
    let send_type = reader.u8()?;
    let payload_length = reader.u32()? as usize;
    let packet = Packet {
        from,
        to,
        source_epoch,
        target_epoch,
        channel,
        send_type,
        payload: reader.bytes(payload_length)?.to_vec(),
    };
    if packet.payload.len() > MAX_PAYLOAD {
        return Err(invalid("payload exceeds maximum"));
    }
    Ok(packet)
}

fn put_type(bytes: &mut Vec<u8>, value: u8) {
    bytes.push(value);
}
fn put_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_be_bytes());
}
fn put_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_be_bytes());
}
fn put_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn put_string(bytes: &mut Vec<u8>, text: &str) -> io::Result<()> {
    let raw = text.as_bytes();
    if raw.len() > MAX_PAYLOAD {
        return Err(invalid("string exceeds maximum"));
    }
    let len = u32::try_from(raw.len()).map_err(|_| invalid("string exceeds maximum"))?;
    put_u32(bytes, len);
    bytes.extend_from_slice(raw);
    Ok(())
}

fn check_frame_body(body: &[u8]) -> io::Result<()> {
    if !(5..=MAX_FRAME - 4).contains(&body.len()) {
        return Err(invalid("invalid frame body length"));
    }
    Ok(())
}

fn random_error(error: getrandom::Error) -> io::Error {
    io::Error::other(format!("secure random generation failed: {error}"))
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}

struct SliceReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> SliceReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }
    fn bytes(&mut self, count: usize) -> io::Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(count)
            .ok_or_else(|| invalid("length overflow"))?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| invalid("truncated message"))?;
        self.position = end;
        Ok(value)
    }
    fn take_array<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        self.bytes(N)?
            .try_into()
            .map_err(|_| invalid("truncated message"))
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take_array::<1>()?[0])
    }
    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_be_bytes(self.take_array()?))
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_be_bytes(self.take_array()?))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_be_bytes(self.take_array()?))
    }
    fn string(&mut self) -> io::Result<String> {
        let size = self.u32()? as usize;
        if size > MAX_PAYLOAD {
            return Err(invalid("string exceeds maximum"));
        }
        let value = self.bytes(size)?;
        String::from_utf8(value.to_vec()).map_err(|_| invalid("invalid UTF-8 string"))
    }
    fn finish(&self) -> io::Result<()> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(invalid("trailing message data"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn member_status_roundtrip_and_limits() {
        let mut status = MemberStatus {
            name: "朋友甲".into(),
            phase: 3,
            ping_ms: Some(27),
            transport: 2,
            sent: 123,
            received: 456,
        };
        for message in [
            Message::Status(status.clone()),
            Message::Statuses(vec![PeerStatus {
                client_id: 5,
                game_epoch: 10,
                age_ms: 3000,
                status: status.clone(),
            }]),
        ] {
            let frame = encode(&message).unwrap();
            assert_eq!(decode(&frame[4..]).unwrap(), message);
        }
        status.ping_ms = None;
        let frame = encode(&Message::Status(status.clone())).unwrap();
        assert_eq!(
            decode(&frame[4..]).unwrap(),
            Message::Status(status.clone())
        );
        status.name = "x".repeat(25);
        assert!(encode(&Message::Status(status.clone())).is_err());
        status.name = "bad\nname".into();
        assert!(encode(&Message::Status(status.clone())).is_err());
        status.name.clear();
        status.phase = 7;
        assert!(encode(&Message::Status(status)).is_err());
        assert!(decode(b"NBP1\x0f\xff\xff").is_err());
        let mut oversized_name = b"NBP1\x0e".to_vec();
        oversized_name.extend_from_slice(&97u32.to_be_bytes());
        assert!(decode(&oversized_name).is_err());
    }

    fn packet(payload: &[u8], send_type: u8) -> Packet {
        Packet {
            from: 11,
            to: 22,
            source_epoch: 33,
            target_epoch: 44,
            channel: -7,
            send_type,
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn message_round_trips_with_big_endian_frame() {
        let message = Message::Data(packet(b"abc", 3));
        let frame = encode(&message).unwrap();
        let expected: &[u8] = &[
            0, 0, 0, 49, b'N', b'B', b'P', b'1', DATA, 0, 0, 0, 0, 0, 0, 0, 11, // from
            0, 0, 0, 0, 0, 0, 0, 22, // to
            0, 0, 0, 0, 0, 0, 0, 33, // source epoch
            0, 0, 0, 0, 0, 0, 0, 44, // target epoch
            255, 255, 255, 249, // channel -7
            3,   // send type
            0, 0, 0, 3, b'a', b'b', b'c',
        ];
        assert_eq!(frame, expected);
        assert_eq!(decode(&frame[4..]).unwrap(), message);
    }

    #[test]
    fn read_and_write_keep_frame_boundaries() {
        let first = Message::Ping(9);
        let second = Message::Members(vec![Peer {
            client_id: 1,
            steam_id: 2,
            epoch: 3,
        }]);
        let mut bytes = Cursor::new(Vec::new());
        write_message(&mut bytes, &first).unwrap();
        write_message(&mut bytes, &second).unwrap();
        bytes.set_position(0);
        assert_eq!(read_message(&mut bytes).unwrap(), first);
        assert_eq!(read_message(&mut bytes).unwrap(), second);
    }

    #[test]
    fn malformed_lengths_and_trailing_bytes_are_rejected() {
        assert!(decode(b"NBP1\x08x").is_err());
        assert!(read_message(&mut Cursor::new(vec![0, 0, 0, 4])).is_err());
        let mut oversized = Vec::new();
        oversized.extend_from_slice(&((MAX_FRAME - 3) as u32).to_be_bytes());
        assert!(read_message(&mut Cursor::new(oversized)).is_err());
    }

    #[test]
    fn udp_has_fixed_limit_and_refuses_reliable_packets() {
        assert!(
            encode_datagram(&Datagram::Data {
                client_id: 1,
                token: [2; 16],
                packet: packet(b"x", 2)
            })
            .is_err()
        );
        let data = Datagram::Data {
            client_id: 1,
            token: [2; 16],
            packet: packet(b"x", 1),
        };
        assert_eq!(
            decode_datagram(&encode_datagram(&data).unwrap()).unwrap(),
            data
        );
        let huge = Datagram::Data {
            client_id: 1,
            token: [2; 16],
            packet: packet(&vec![0; UDP_LIMIT], 0),
        };
        assert!(encode_datagram(&huge).is_err());
    }

    #[test]
    fn hook_init_has_stable_shared_abi_layout() {
        assert_eq!(std::mem::size_of::<HookInit>(), 40);
        assert_eq!(HookInit::default().reserved, 0);
        assert_eq!(HookInit::default().version, HOOK_INIT_VERSION);
    }
}
