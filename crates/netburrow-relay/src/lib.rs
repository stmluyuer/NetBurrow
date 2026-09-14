//! In-memory NB v1 relay.  A group exists only while one or more TCP clients
//! are connected; no group credential, payload, or remote endpoint is logged.

use std::{
    collections::HashMap,
    future::Future,
    io::{self, ErrorKind},
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use netburrow_protocol::{
    Datagram, Group, MemberStatus, Message, Packet, Peer, PeerStatus, Token, decode,
    decode_datagram, encode, encode_datagram, random_token,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::{Mutex, mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
};

const DEFAULT_PORT: u16 = 24_872;
const DEFAULT_MAX_CLIENTS: usize = 1_024;
const DEFAULT_QUEUE_MESSAGES: usize = 32;
const DEFAULT_QUEUE_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_TOTAL_QUEUE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub max_clients: usize,
    pub handshake_timeout: Duration,
    pub outgoing_messages: usize,
    pub outgoing_bytes: usize,
    pub total_outgoing_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT)),
            max_clients: DEFAULT_MAX_CLIENTS,
            handshake_timeout: Duration::from_secs(10),
            outgoing_messages: DEFAULT_QUEUE_MESSAGES,
            outgoing_bytes: DEFAULT_QUEUE_BYTES,
            total_outgoing_bytes: DEFAULT_TOTAL_QUEUE_BYTES,
        }
    }
}

impl Config {
    pub fn validate(&self) -> io::Result<()> {
        if self.max_clients == 0 {
            return Err(invalid("max_clients must be positive"));
        }
        if self.handshake_timeout.is_zero() {
            return Err(invalid("handshake_timeout must be positive"));
        }
        if self.outgoing_messages == 0 {
            return Err(invalid("outgoing_messages must be positive"));
        }
        if self.outgoing_bytes == 0 {
            return Err(invalid("outgoing_bytes must be positive"));
        }
        if self.total_outgoing_bytes < self.outgoing_bytes {
            return Err(invalid("total_outgoing_bytes must cover one client queue"));
        }
        Ok(())
    }
}

pub struct RelayHandle {
    address: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<io::Result<()>>,
}

impl RelayHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    pub async fn shutdown(mut self) -> io::Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task
            .await
            .map_err(|error| io::Error::other(format!("relay task failed: {error}")))?
    }
}

/// Starts a relay and returns its bound address plus a graceful shutdown handle.
/// Supplying port zero is useful for loopback integration tests.
pub async fn spawn(config: Config) -> io::Result<RelayHandle> {
    let (tcp, udp, address) = bind_sockets(&config).await?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        run(tcp, udp, config, async move {
            let _ = shutdown_rx.await;
        })
        .await
    });
    Ok(RelayHandle {
        address,
        shutdown: Some(shutdown_tx),
        task,
    })
}

/// Runs a relay until `shutdown` resolves.  This is the integration point for
/// a service wrapper that owns its own shutdown signal.
pub async fn serve<F>(config: Config, shutdown: F) -> io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let (tcp, udp, _) = bind_sockets(&config).await?;
    run(tcp, udp, config, shutdown).await
}

async fn bind_sockets(config: &Config) -> io::Result<(TcpListener, Arc<UdpSocket>, SocketAddr)> {
    config.validate()?;
    let tcp = TcpListener::bind(config.bind).await?;
    let address = tcp.local_addr()?;
    let udp = Arc::new(UdpSocket::bind(address).await?);
    Ok((tcp, udp, address))
}

async fn run<F>(
    tcp: TcpListener,
    udp: Arc<UdpSocket>,
    config: Config,
    shutdown: F,
) -> io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let state = Arc::new(Mutex::new(State::new(config.total_outgoing_bytes)));
    let (stopping, _) = watch::channel(false);
    let udp_task = tokio::spawn(udp_loop(udp, state.clone(), stopping.subscribe()));
    let mut clients = JoinSet::new();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = tcp.accept() => match accepted {
                Ok((stream, address)) => {
                    if state.lock().await.count() >= config.max_clients {
                        drop(stream);
                    } else {
                        clients.spawn(client_loop(stream, address, state.clone(), config.clone(), stopping.subscribe()));
                    }
                }
                Err(error) => return Err(error),
            },
            Some(_) = clients.join_next(), if !clients.is_empty() => {},
        }
    }

    let _ = stopping.send(true);
    clients.abort_all();
    while clients.join_next().await.is_some() {}
    udp_task.abort();
    let _ = udp_task.await;
    Ok(())
}

async fn client_loop(
    mut stream: TcpStream,
    remote: SocketAddr,
    state: Arc<Mutex<State>>,
    config: Config,
    mut stopping: watch::Receiver<bool>,
) {
    let first = tokio::select! {
        result = tokio::time::timeout(config.handshake_timeout, read_tcp_message(&mut stream)) => result,
        _ = stopping.changed() => return,
    };
    let Ok(Ok(Message::Join { group })) = first else {
        let _ = write_tcp_message(
            &mut stream,
            &Message::Error("first message must be Join".into()),
        )
        .await;
        return;
    };

    let (sender, receiver) = mpsc::channel(config.outgoing_messages);
    let (closing, mut closed) = watch::channel(false);
    let joined = {
        let mut locked = state.lock().await;
        locked.add(
            group,
            remote.ip(),
            sender.clone(),
            closing,
            config.outgoing_bytes,
            config.max_clients,
        )
    };
    let Ok(joined) = joined else {
        let _ = write_tcp_message(&mut stream, &Message::Error("relay is full".into())).await;
        return;
    };
    let (client_id, token, members, notices) = joined;
    {
        let locked = state.lock().await;
        let _ = locked.enqueue(
            client_id,
            Message::Welcome {
                client_id,
                udp_token: token,
            },
        );
        let _ = locked.enqueue(client_id, Message::Members(members));
        for notice in notices {
            let _ = locked.enqueue(notice.target, notice.message);
        }
    }

    let (reader, writer) = stream.into_split();
    let writer_stop = stopping.clone();
    let writer_task = tokio::spawn(writer_loop(writer, receiver, writer_stop, closed.clone()));
    let mut reader = reader;
    loop {
        tokio::select! {
            message = read_tcp_message(&mut reader) => {
                let Ok(message) = message else { break; };
                let leave = handle_tcp_message(client_id, message, &state).await;
                if leave { break; }
            }
            changed = stopping.changed() => {
                if changed.is_ok() && *stopping.borrow() { break; }
            }
            changed = closed.changed() => {
                if changed.is_ok() && *closed.borrow() { break; }
            }
        }
    }
    writer_task.abort();
    let _ = writer_task.await;
    disconnect(client_id, &state).await;
}

async fn handle_tcp_message(client_id: u64, message: Message, state: &Arc<Mutex<State>>) -> bool {
    match message {
        Message::Bind { steam_id, epoch } => {
            let result = state
                .lock()
                .await
                .bind(client_id, steam_id, epoch, Instant::now());
            match result {
                Ok(notices) => {
                    let locked = state.lock().await;
                    for notice in notices {
                        let _ = locked.enqueue(notice.target, notice.message);
                    }
                }
                Err(text) => send_error(client_id, state, text).await,
            }
            false
        }
        Message::Status(status) => {
            let result = state
                .lock()
                .await
                .report_status(client_id, status, Instant::now());
            match result {
                Ok(notices) => {
                    let locked = state.lock().await;
                    for notice in notices {
                        let _ = locked.enqueue(notice.target, notice.message);
                    }
                }
                Err(text) => send_error(client_id, state, text).await,
            }
            false
        }
        Message::Data(packet) => {
            let action = state.lock().await.route_tcp(client_id, packet);
            execute_route(client_id, action, state, None).await;
            false
        }
        Message::Ping(value) => {
            let _ = state.lock().await.enqueue(client_id, Message::Pong(value));
            false
        }
        Message::Leave => true,
        Message::Join { .. } => {
            send_error(client_id, state, "Join is only valid during handshake").await;
            true
        }
        _ => {
            send_error(client_id, state, "message is not accepted from a client").await;
            false
        }
    }
}

async fn udp_loop(
    udp: Arc<UdpSocket>,
    state: Arc<Mutex<State>>,
    mut stopping: watch::Receiver<bool>,
) {
    let mut buffer = vec![0; netburrow_protocol::UDP_LIMIT + 1];
    loop {
        tokio::select! {
            received = udp.recv_from(&mut buffer) => {
                let Ok((length, source)) = received else { return; };
                let Ok(datagram) = decode_datagram(&buffer[..length]) else { continue; };
                match datagram {
                    Datagram::Bind { client_id, token } => {
                        let bound = state.lock().await.bind_udp(client_id, token, source);
                        if bound {
                            if let Ok(reply) = encode_datagram(&Datagram::Bound { client_id }) { let _ = udp.send_to(&reply, source).await; }
                        }
                    }
                    Datagram::Data { client_id, token, packet } => {
                        let route = state.lock().await.route_udp(client_id, token, source, packet);
                        execute_route(client_id, route, &state, Some(&udp)).await;
                    }
                    Datagram::Bound { .. } => {}
                }
            }
            changed = stopping.changed() => {
                if changed.is_ok() && *stopping.borrow() { return; }
            }
        }
    }
}

async fn execute_route(
    client_id: u64,
    route: Route,
    state: &Arc<Mutex<State>>,
    udp: Option<&Arc<UdpSocket>>,
) {
    match route {
        Route::Tcp { target, message } => {
            let queued = { state.lock().await.enqueue(target, message) };
            match queued {
                Err(QueueError::Full) => {
                    disconnect(target, state).await;
                    send_error(client_id, state, "target connection is slow").await;
                }
                Err(QueueError::Closed) => {
                    disconnect(target, state).await;
                    send_error(client_id, state, "target connection closed").await;
                }
                Err(QueueError::Invalid) => send_error(client_id, state, "invalid TCP data").await,
                Ok(()) => {}
            }
        }
        Route::Udp {
            target,
            address,
            datagram,
        } => {
            let Some(socket) = udp else {
                return;
            };
            match encode_datagram(&datagram) {
                Ok(bytes) => {
                    let _ = socket.send_to(&bytes, address).await;
                }
                Err(_) => send_error(client_id, state, "invalid UDP data").await,
            }
            let _ = target;
        }
        Route::Rejected(text) => send_error(client_id, state, text).await,
    }
}

async fn send_error(client_id: u64, state: &Arc<Mutex<State>>, text: &'static str) {
    let _ = state
        .lock()
        .await
        .enqueue(client_id, Message::Error(text.into()));
}

async fn disconnect(client_id: u64, state: &Arc<Mutex<State>>) {
    let notices = state.lock().await.remove(client_id, Instant::now());
    let locked = state.lock().await;
    for notice in notices {
        let _ = locked.enqueue(notice.target, notice.message);
    }
}

async fn read_tcp_message<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Message> {
    let length = reader.read_u32().await? as usize;
    if !(5..=netburrow_protocol::MAX_FRAME - 4).contains(&length) {
        return Err(invalid("invalid frame length"));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await?;
    decode(&body)
}

async fn write_tcp_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &Message,
) -> io::Result<()> {
    writer.write_all(&encode(message)?).await
}

async fn writer_loop(
    mut writer: tokio::net::tcp::OwnedWriteHalf,
    mut receiver: mpsc::Receiver<Queued>,
    mut stopping: watch::Receiver<bool>,
    mut closed: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            queued = receiver.recv() => match queued {
                Some(mut queued) => {
                    let result = write_tcp_message(&mut writer, &queued.message).await;
                    queued.release();
                    if result.is_err() { return; }
                }
                None => return,
            },
            changed = stopping.changed() => {
                if changed.is_ok() && *stopping.borrow() { return; }
            }
            changed = closed.changed() => {
                if changed.is_ok() && *closed.borrow() { return; }
            }
        }
    }
}

struct State {
    next_client_id: u64,
    clients: HashMap<u64, Client>,
    queued_total: Arc<AtomicUsize>,
    total_queue_limit: usize,
}

struct Client {
    group: Group,
    token: Token,
    ip: IpAddr,
    steam_id: u64,
    epoch: u64,
    udp_address: Option<SocketAddr>,
    output: mpsc::Sender<Queued>,
    queued_bytes: Arc<AtomicUsize>,
    max_queued_bytes: usize,
    closing: watch::Sender<bool>,
    status_subscribed: bool,
    last_status_at: Option<Instant>,
    reported_status: Option<ReportedStatus>,
}

struct ReportedStatus {
    status: MemberStatus,
    game_epoch: u64,
    reported_at: Instant,
}

struct Queued {
    message: Message,
    bytes: usize,
    budget: Arc<AtomicUsize>,
    total_budget: Arc<AtomicUsize>,
    released: bool,
}

impl Queued {
    fn release(&mut self) {
        if !self.released {
            self.budget.fetch_sub(self.bytes, Ordering::AcqRel);
            self.total_budget.fetch_sub(self.bytes, Ordering::AcqRel);
            self.released = true;
        }
    }
}

impl Drop for Queued {
    fn drop(&mut self) {
        self.release();
    }
}

struct Notice {
    target: u64,
    message: Message,
}

enum QueueError {
    Full,
    Closed,
    Invalid,
}

enum Route {
    Tcp {
        target: u64,
        message: Message,
    },
    Udp {
        target: u64,
        address: SocketAddr,
        datagram: Datagram,
    },
    Rejected(&'static str),
}

impl State {
    fn new(total_queue_limit: usize) -> Self {
        Self {
            next_client_id: 0,
            clients: HashMap::new(),
            queued_total: Arc::new(AtomicUsize::new(0)),
            total_queue_limit,
        }
    }

    fn count(&self) -> usize {
        self.clients.len()
    }

    fn add(
        &mut self,
        group: Group,
        ip: IpAddr,
        output: mpsc::Sender<Queued>,
        closing: watch::Sender<bool>,
        max_queued_bytes: usize,
        max_clients: usize,
    ) -> io::Result<(u64, Token, Vec<Peer>, Vec<Notice>)> {
        if self.clients.len() >= max_clients {
            return Err(invalid("relay is full"));
        }
        self.next_client_id = self
            .next_client_id
            .checked_add(1)
            .ok_or_else(|| invalid("client id exhausted"))?;
        let client_id = self.next_client_id;
        let token = random_token()?;
        self.clients.insert(
            client_id,
            Client {
                group,
                token,
                ip,
                steam_id: 0,
                epoch: 0,
                udp_address: None,
                output,
                queued_bytes: Arc::new(AtomicUsize::new(0)),
                max_queued_bytes,
                closing,
                status_subscribed: false,
                last_status_at: None,
                reported_status: None,
            },
        );
        let members = self.members(group);
        let notices = self.notices_for_group(group, Some(client_id));
        Ok((client_id, token, members, notices))
    }

    fn bind(
        &mut self,
        client_id: u64,
        steam_id: u64,
        epoch: u64,
        now: Instant,
    ) -> Result<Vec<Notice>, &'static str> {
        if (steam_id == 0) != (epoch == 0) {
            return Err("steam_id and epoch must both be zero or nonzero");
        }
        let group = self
            .clients
            .get(&client_id)
            .ok_or("connection is not active")?
            .group;
        if steam_id != 0
            && self
                .clients
                .iter()
                .any(|(id, client)| *id != client_id && client.steam_id == steam_id)
        {
            return Err("steam_id is already bound in this group");
        }
        let client = self
            .clients
            .get_mut(&client_id)
            .ok_or("connection is not active")?;
        let game_changed = client.steam_id != steam_id || client.epoch != epoch;
        client.steam_id = steam_id;
        client.epoch = epoch;
        client.udp_address = None;
        if game_changed {
            client.last_status_at = None;
            client.reported_status = None;
        }
        let mut notices = self.notices_for_group(group, None);
        notices.extend(self.status_notices_for_group(group, now));
        Ok(notices)
    }

    fn report_status(
        &mut self,
        client_id: u64,
        mut status: MemberStatus,
        now: Instant,
    ) -> Result<Vec<Notice>, &'static str> {
        let client = self
            .clients
            .get_mut(&client_id)
            .ok_or("connection is not active")?;
        if client
            .last_status_at
            .is_some_and(|last| now.saturating_duration_since(last) < Duration::from_secs(1))
        {
            return Err("status reports are limited to once per second");
        }
        if status.phase == 3 && client.steam_id == 0 {
            status.phase = 1;
        }
        if status.transport == 2 && client.udp_address.is_none() {
            status.transport = 1;
        }
        let group = client.group;
        client.status_subscribed = true;
        client.last_status_at = Some(now);
        client.reported_status = Some(ReportedStatus {
            status,
            game_epoch: client.epoch,
            reported_at: now,
        });
        Ok(self.status_notices_for_group(group, now))
    }

    fn bind_udp(&mut self, client_id: u64, token: Token, address: SocketAddr) -> bool {
        let Some(client) = self.clients.get_mut(&client_id) else {
            return false;
        };
        if client.token != token || client.ip != address.ip() {
            return false;
        }
        client.udp_address = Some(address);
        true
    }

    fn route_tcp(&self, client_id: u64, packet: Packet) -> Route {
        let Some(source) = self.clients.get(&client_id) else {
            return Route::Rejected("connection is not active");
        };
        let Some(target_id) = self.valid_target(source, &packet) else {
            return Route::Rejected("packet source, target, or epoch is invalid");
        };
        Route::Tcp {
            target: target_id,
            message: Message::Data(packet),
        }
    }

    fn route_udp(
        &self,
        client_id: u64,
        token: Token,
        address: SocketAddr,
        packet: Packet,
    ) -> Route {
        let Some(source) = self.clients.get(&client_id) else {
            return Route::Rejected("connection is not active");
        };
        if source.token != token || source.udp_address != Some(address) {
            return Route::Rejected("UDP endpoint is not bound");
        }
        let Some(target_id) = self.valid_target(source, &packet) else {
            return Route::Rejected("packet source, target, or epoch is invalid");
        };
        let target = &self.clients[&target_id];
        match target.udp_address {
            Some(destination) => Route::Udp {
                target: target_id,
                address: destination,
                datagram: Datagram::Data {
                    client_id: target_id,
                    token: target.token,
                    packet,
                },
            },
            None => Route::Tcp {
                target: target_id,
                message: Message::Data(packet),
            },
        }
    }

    fn valid_target(&self, source: &Client, packet: &Packet) -> Option<u64> {
        if source.steam_id == 0
            || source.epoch == 0
            || packet.from != source.steam_id
            || packet.source_epoch != source.epoch
        {
            return None;
        }
        self.clients.iter().find_map(|(id, target)| {
            (target.group == source.group
                && target.steam_id != 0
                && target.steam_id == packet.to
                && target.epoch == packet.target_epoch)
                .then_some(*id)
        })
    }

    fn enqueue(&self, client_id: u64, message: Message) -> Result<(), QueueError> {
        let client = self.clients.get(&client_id).ok_or(QueueError::Closed)?;
        enqueue(
            &client.output,
            message,
            client.max_queued_bytes,
            client.queued_bytes.clone(),
            self.total_queue_limit,
            self.queued_total.clone(),
        )
    }

    fn remove(&mut self, client_id: u64, now: Instant) -> Vec<Notice> {
        let Some(client) = self.clients.remove(&client_id) else {
            return Vec::new();
        };
        let _ = client.closing.send(true);
        let mut notices = self.notices_for_group(client.group, None);
        notices.extend(self.status_notices_for_group(client.group, now));
        notices
    }

    fn members(&self, group: Group) -> Vec<Peer> {
        let mut peers: Vec<_> = self
            .clients
            .iter()
            .filter(|(_, client)| client.group == group)
            .map(|(id, client)| Peer {
                client_id: *id,
                steam_id: client.steam_id,
                epoch: client.epoch,
            })
            .collect();
        peers.sort_unstable_by_key(|peer| peer.client_id);
        peers
    }

    fn notices_for_group(&self, group: Group, except: Option<u64>) -> Vec<Notice> {
        let message = Message::Members(self.members(group));
        self.clients
            .iter()
            .filter(|(id, client)| client.group == group && Some(**id) != except)
            .map(|(id, _)| Notice {
                target: *id,
                message: message.clone(),
            })
            .collect()
    }

    fn status_notices_for_group(&self, group: Group, now: Instant) -> Vec<Notice> {
        let snapshot = self.status_snapshot(group, now);
        self.clients
            .iter()
            .filter(|(_, client)| client.group == group && client.status_subscribed)
            .map(|(id, _)| Notice {
                target: *id,
                message: Message::Statuses(snapshot.clone()),
            })
            .collect()
    }

    fn status_snapshot(&self, group: Group, now: Instant) -> Vec<PeerStatus> {
        let mut statuses: Vec<_> = self
            .clients
            .iter()
            .filter(|(_, client)| client.group == group)
            .filter_map(|(id, client)| {
                client.reported_status.as_ref().map(|reported| PeerStatus {
                    client_id: *id,
                    game_epoch: reported.game_epoch,
                    age_ms: now
                        .saturating_duration_since(reported.reported_at)
                        .as_millis()
                        .min(u32::MAX as u128) as u32,
                    status: reported.status.clone(),
                })
            })
            .collect();
        statuses.sort_unstable_by_key(|status| status.client_id);
        statuses
    }
}

fn enqueue(
    output: &mpsc::Sender<Queued>,
    message: Message,
    maximum: usize,
    budget: Arc<AtomicUsize>,
    total_maximum: usize,
    total_budget: Arc<AtomicUsize>,
) -> Result<(), QueueError> {
    let bytes = encode(&message).map_err(|_| QueueError::Invalid)?.len();
    let reserved = budget.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        current.checked_add(bytes).filter(|next| *next <= maximum)
    });
    if reserved.is_err() {
        return Err(QueueError::Full);
    }
    let total_reserved =
        total_budget.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current
                .checked_add(bytes)
                .filter(|next| *next <= total_maximum)
        });
    if total_reserved.is_err() {
        budget.fetch_sub(bytes, Ordering::AcqRel);
        return Err(QueueError::Full);
    }
    match output.try_send(Queued {
        message,
        bytes,
        budget: budget.clone(),
        total_budget: total_budget.clone(),
        released: false,
    }) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(_)) => {
            budget.fetch_sub(bytes, Ordering::AcqRel);
            total_budget.fetch_sub(bytes, Ordering::AcqRel);
            Err(QueueError::Full)
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            budget.fetch_sub(bytes, Ordering::AcqRel);
            total_budget.fetch_sub(bytes, Ordering::AcqRel);
            Err(QueueError::Closed)
        }
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use netburrow_protocol::{Group, UDP_LIMIT};
    use tokio::time::{Duration, timeout};

    fn group(byte: u8) -> Group {
        [byte; 32]
    }
    fn packet(
        from: u64,
        to: u64,
        source_epoch: u64,
        target_epoch: u64,
        kind: u8,
        payload: &[u8],
    ) -> Packet {
        Packet {
            from,
            to,
            source_epoch,
            target_epoch,
            channel: 0,
            send_type: kind,
            payload: payload.to_vec(),
        }
    }
    async fn recv_until<F>(stream: &mut TcpStream, predicate: F) -> Message
    where
        F: Fn(&Message) -> bool,
    {
        for _ in 0..12 {
            let message = timeout(Duration::from_secs(1), read_tcp_message(stream))
                .await
                .unwrap()
                .unwrap();
            if predicate(&message) {
                return message;
            }
        }
        panic!("expected message was not received")
    }
    async fn connect(address: SocketAddr, group: Group) -> (TcpStream, u64, Token) {
        let mut stream = TcpStream::connect(address).await.unwrap();
        write_tcp_message(&mut stream, &Message::Join { group })
            .await
            .unwrap();
        let welcome = recv_until(&mut stream, |message| {
            matches!(message, Message::Welcome { .. })
        })
        .await;
        let Message::Welcome {
            client_id,
            udp_token,
        } = welcome
        else {
            unreachable!()
        };
        let _ = recv_until(&mut stream, |message| {
            matches!(message, Message::Members(_))
        })
        .await;
        (stream, client_id, udp_token)
    }
    async fn bind(stream: &mut TcpStream, steam_id: u64, epoch: u64) {
        write_tcp_message(stream, &Message::Bind { steam_id, epoch })
            .await
            .unwrap();
    }

    fn member_status(name: &str, phase: u8, transport: u8) -> MemberStatus {
        MemberStatus {
            name: name.into(),
            phase,
            ping_ms: Some(12),
            transport,
            sent: 5,
            received: 7,
        }
    }

    fn add_state_client(state: &mut State, group: Group) -> u64 {
        let (sender, _receiver) = mpsc::channel(4);
        let (closing, _closed) = watch::channel(false);
        state
            .add(
                group,
                "127.0.0.1".parse().unwrap(),
                sender,
                closing,
                1024,
                16,
            )
            .unwrap()
            .0
    }

    fn status_targets(notices: &[Notice]) -> Vec<u64> {
        let mut targets: Vec<_> = notices
            .iter()
            .filter(|notice| matches!(notice.message, Message::Statuses(_)))
            .map(|notice| notice.target)
            .collect();
        targets.sort_unstable();
        targets
    }

    fn statuses_for(notices: &[Notice], target: u64) -> &[PeerStatus] {
        notices
            .iter()
            .find_map(|notice| match (&notice.message, notice.target == target) {
                (Message::Statuses(statuses), true) => Some(statuses.as_slice()),
                _ => None,
            })
            .expect("missing status snapshot")
    }

    #[test]
    fn status_is_scoped_to_reporters_and_uses_actual_connection_identity() {
        let now = Instant::now();
        let mut state = State::new(4096);
        let legacy = add_state_client(&mut state, group(1));
        let one = add_state_client(&mut state, group(1));
        let two = add_state_client(&mut state, group(1));
        let other_group = add_state_client(&mut state, group(2));
        state.bind(one, 101, 1001, now).unwrap();
        state.bind(two, 202, 2002, now).unwrap();

        let first = state
            .report_status(one, member_status("one", 3, 2), now)
            .unwrap();
        assert_eq!(status_targets(&first), vec![one]);
        let first_snapshot = statuses_for(&first, one);
        assert_eq!(first_snapshot[0].client_id, one);
        assert_eq!(first_snapshot[0].status.transport, 1);
        assert_eq!(first_snapshot[0].age_ms, 0);
        assert!(!status_targets(&first).contains(&legacy));

        let isolated = state
            .report_status(other_group, member_status("other", 3, 2), now)
            .unwrap();
        assert_eq!(status_targets(&isolated), vec![other_group]);
        let isolated_snapshot = statuses_for(&isolated, other_group);
        assert_eq!(isolated_snapshot[0].client_id, other_group);
        assert_eq!(isolated_snapshot[0].status.phase, 1);
        assert_eq!(isolated_snapshot[0].status.transport, 1);

        let shared = state
            .report_status(
                two,
                member_status("claims-to-be-one", 3, 0),
                now + Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(status_targets(&shared), vec![one, two]);
        let snapshot = statuses_for(&shared, one);
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[0].client_id, one);
        assert_eq!(snapshot[1].client_id, two);
        assert_eq!(snapshot[1].status.name, "claims-to-be-one");
        assert!(!status_targets(&shared).contains(&legacy));
        assert!(!status_targets(&shared).contains(&other_group));
    }

    #[test]
    fn status_throttle_and_lifecycle_updates_clear_old_game_statistics() {
        let now = Instant::now();
        let mut state = State::new(4096);
        let one = add_state_client(&mut state, group(3));
        let two = add_state_client(&mut state, group(3));
        state.bind(one, 11, 111, now).unwrap();
        state.bind(two, 22, 222, now).unwrap();
        state
            .report_status(one, member_status("one", 3, 0), now)
            .unwrap();
        state
            .report_status(two, member_status("two", 3, 0), now)
            .unwrap();
        assert!(
            state
                .report_status(
                    one,
                    member_status("one", 3, 0),
                    now + Duration::from_millis(999)
                )
                .is_err()
        );

        let rebound = state
            .bind(one, 11, 112, now + Duration::from_secs(1))
            .unwrap();
        assert_eq!(status_targets(&rebound), vec![one, two]);
        let rebound_snapshot = statuses_for(&rebound, two);
        assert_eq!(rebound_snapshot.len(), 1);
        assert_eq!(rebound_snapshot[0].client_id, two);
        assert_eq!(rebound_snapshot[0].game_epoch, 222);

        let cleared = state.remove(two, now + Duration::from_secs(2));
        assert_eq!(status_targets(&cleared), vec![one]);
        assert!(statuses_for(&cleared, one).is_empty());
    }

    #[tokio::test]
    async fn loopback_tcp_udp_and_group_isolation() {
        let relay = spawn(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            ..Config::default()
        })
        .await
        .unwrap();
        let address = relay.local_addr();
        let (mut one, one_id, one_token) = connect(address, group(1)).await;
        let (mut two, two_id, two_token) = connect(address, group(1)).await;
        bind(&mut one, 101, 1001).await;
        bind(&mut two, 202, 2002).await;
        let _ = recv_until(&mut two, |message| {
            matches!(message, Message::Members(peers) if peers.iter().any(|peer| peer.steam_id == 101 && peer.epoch == 1001)
                && peers.iter().any(|peer| peer.steam_id == 202 && peer.epoch == 2002))
        })
        .await;

        write_tcp_message(
            &mut one,
            &Message::Data(packet(101, 202, 1001, 2002, 3, b"reliable")),
        )
        .await
        .unwrap();
        let received = recv_until(&mut two, |message| matches!(message, Message::Data(Packet { payload, .. }) if payload == b"reliable")).await;
        assert_eq!(
            received,
            Message::Data(packet(101, 202, 1001, 2002, 3, b"reliable"))
        );

        let first_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        first_udp.connect(address).await.unwrap();
        let second_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        second_udp.connect(address).await.unwrap();
        first_udp
            .send(
                &encode_datagram(&Datagram::Bind {
                    client_id: one_id,
                    token: one_token,
                })
                .unwrap(),
            )
            .await
            .unwrap();
        second_udp
            .send(
                &encode_datagram(&Datagram::Bind {
                    client_id: two_id,
                    token: two_token,
                })
                .unwrap(),
            )
            .await
            .unwrap();
        let mut buf = [0; UDP_LIMIT];
        let count = first_udp.recv(&mut buf).await.unwrap();
        assert!(
            matches!(decode_datagram(&buf[..count]).unwrap(), Datagram::Bound { client_id } if client_id == one_id)
        );
        let count = second_udp.recv(&mut buf).await.unwrap();
        assert!(
            matches!(decode_datagram(&buf[..count]).unwrap(), Datagram::Bound { client_id } if client_id == two_id)
        );
        let datagram = Datagram::Data {
            client_id: one_id,
            token: one_token,
            packet: packet(101, 202, 1001, 2002, 1, b"fast"),
        };
        first_udp
            .send(&encode_datagram(&datagram).unwrap())
            .await
            .unwrap();
        let count = timeout(Duration::from_secs(1), second_udp.recv(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(decode_datagram(&buf[..count]).unwrap(), Datagram::Data { packet: Packet { payload, .. }, .. } if payload == b"fast")
        );

        let (mut other, _, _) = connect(address, group(2)).await;
        bind(&mut other, 303, 3003).await;
        write_tcp_message(
            &mut one,
            &Message::Data(packet(101, 303, 1001, 3003, 0, b"wrong group")),
        )
        .await
        .unwrap();
        assert!(matches!(
            recv_until(&mut one, |m| matches!(m, Message::Error(_))).await,
            Message::Error(_)
        ));
        relay.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn rejects_duplicate_old_epochs_and_cleans_unbind_disconnect() {
        let relay = spawn(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            ..Config::default()
        })
        .await
        .unwrap();
        let address = relay.local_addr();
        let (mut one, _, _) = connect(address, group(9)).await;
        let (mut two, _, _) = connect(address, group(9)).await;
        bind(&mut one, 11, 111).await;
        bind(&mut two, 11, 222).await;
        assert!(matches!(
            recv_until(&mut two, |m| matches!(m, Message::Error(_))).await,
            Message::Error(_)
        ));
        bind(&mut two, 22, 222).await;
        write_tcp_message(
            &mut one,
            &Message::Data(packet(11, 22, 999, 222, 0, b"old")),
        )
        .await
        .unwrap();
        assert!(matches!(
            recv_until(&mut one, |m| matches!(m, Message::Error(_))).await,
            Message::Error(_)
        ));
        bind(&mut one, 0, 0).await;
        let members = recv_until(
            &mut two,
            |m| matches!(m, Message::Members(peers) if peers.iter().any(|peer| peer.steam_id == 0)),
        )
        .await;
        assert!(matches!(members, Message::Members(_)));
        drop(one);
        let members = recv_until(
            &mut two,
            |m| matches!(m, Message::Members(peers) if peers.len() == 1),
        )
        .await;
        assert!(matches!(members, Message::Members(_)));
        relay.shutdown().await.unwrap();
    }
}
