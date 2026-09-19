//! In-memory NB v1 relay.  A group exists only while one or more TCP clients
//! are connected; no group credential, payload, or remote endpoint is logged.

mod allowed_groups;
mod diagnostics;

pub use allowed_groups::AllowedGroups;

const GROUP_NOT_ALLOWED: &str = "group is not allowed";

use diagnostics::{Stats, increment, record};

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
const DEFAULT_QUEUE_MESSAGES: usize = 320;
const DEFAULT_QUEUE_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_TOTAL_QUEUE_BYTES: usize = 64 * 1024 * 1024;
// Clients send a heartbeat every second. Bound silence and incomplete frames so
// a lost FIN cannot keep an old socket attached throughout the recovery window.
const CLIENT_READ_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub allowed_groups: AllowedGroups,
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
            allowed_groups: AllowedGroups::default(),
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
        if self.allowed_groups.is_empty() {
            return Err(invalid("a nonempty allowed-groups file is required"));
        }
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
    let stats = state.lock().await.stats.clone();
    record(
        "INFO",
        "started",
        format_args!(
            "port={} max_clients={} allowed_groups={} protocol=NBP1 member_status=true",
            tcp.local_addr()?.port(),
            config.max_clients,
            config.allowed_groups.len()
        ),
    );
    let (stopping, _) = watch::channel(false);
    let mut udp_task = tokio::spawn(udp_loop(udp, state.clone(), stopping.subscribe()));
    let mut clients = JoinSet::new();
    let mut summary = tokio::time::interval(Duration::from_secs(10));
    summary.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tokio::pin!(shutdown);

    let result = loop {
        tokio::select! {
            _ = &mut shutdown => break Ok(()),
            _ = summary.tick() => {
                let mut locked=state.lock().await;
                let expired:Vec<_>=locked.clients.iter().filter(|(_,c)|c.resume_failed.load(Ordering::Acquire)||c.detached_until.is_some_and(|t|Instant::now()>=t)).map(|(id,_)|*id).collect();
                for id in expired {for notice in locked.remove(id,Instant::now()){let _=locked.enqueue(notice.target,notice.message);}}
                let line = locked.summary();
                record("INFO", "summary", format_args!("{line}"));
            }
            result = &mut udp_task => {
                increment(&stats.io_failed);
                record("ERROR", "udp_loop_stopped", format_args!("task_failed={}", result.is_err()));
                break Err(io::Error::other("UDP receive loop stopped"));
            }
            accepted = tcp.accept() => match accepted {
                Ok((stream, address)) => {
                    increment(&stats.accepted);
                    if clients.len() >= config.max_clients.saturating_mul(2) {
                        increment(&stats.capacity_rejected);
                        drop(stream);
                    } else {
                        clients.spawn(client_loop(stream, address, state.clone(), config.clone(), stopping.subscribe()));
                    }
                }
                Err(error) => {
                    increment(&stats.io_failed);
                    record("ERROR", "accept_failed", format_args!("kind={:?}", error.kind()));
                    break Err(error);
                },
            },
            Some(result) = clients.join_next(), if !clients.is_empty() => {
                if result.is_err() {
                    increment(&stats.io_failed);
                    record("ERROR", "client_task_failed", format_args!(""));
                }
            },
        }
    };

    let _ = stopping.send(true);
    clients.abort_all();
    while clients.join_next().await.is_some() {}
    if !udp_task.is_finished() {
        udp_task.abort();
        let _ = udp_task.await;
    }
    let line = state.lock().await.summary();
    record(
        "INFO",
        "stopped",
        format_args!("success={} {line}", result.is_ok()),
    );
    result
}

async fn client_loop(
    mut stream: TcpStream,
    remote: SocketAddr,
    state: Arc<Mutex<State>>,
    config: Config,
    mut stopping: watch::Receiver<bool>,
) {
    let stats = state.lock().await.stats.clone();
    // Game traffic consists of latency-sensitive small frames in both directions.
    if let Err(error) = stream.set_nodelay(true) {
        increment(&stats.io_failed);
        record(
            "ERROR",
            "tcp_nodelay_failed",
            format_args!("kind={:?}", error.kind()),
        );
        return;
    }
    let first = tokio::select! {
        result = tokio::time::timeout(config.handshake_timeout, read_tcp_message(&mut stream)) => result,
        _ = stopping.changed() => return,
    };
    let Ok(Ok(first)) = first else {
        increment(&stats.handshake_rejected);
        let _ = write_tcp_message(
            &mut stream,
            &Message::Error("first message must be Join".into()),
        )
        .await;
        return;
    };

    // Authorize before allocating membership or generating a UDP token.
    // Resume can only restore an existing session admitted by this same list;
    // the list is immutable for the lifetime of the process.
    if let Message::Join { group } = &first {
        if !config.allowed_groups.contains(group) {
            increment(&stats.handshake_rejected);
            record("WARN", "group_rejected", format_args!(""));
            let _ = tokio::time::timeout(
                config.handshake_timeout,
                write_tcp_message(&mut stream, &Message::Error(GROUP_NOT_ALLOWED.into())),
            )
            .await;
            return;
        }
    }

    let (sender, receiver) = mpsc::channel(config.outgoing_messages);
    let (closing, _) = watch::channel(false);
    let resumed = matches!(first,Message::Resume{..});
    let joined = if let Message::Resume {client_id,key,received} = first {
        let mut locked=state.lock().await;
        locked.resume(client_id,key,received,Instant::now())
    } else if let Message::Join {group}=first {
        let mut locked = state.lock().await;
        let added=locked.add(
            group,
            remote.ip(),
            sender.clone(),
            closing,
            config.outgoing_bytes,
            config.max_clients,
        );
        if let Ok((id,..))=&added {locked.clients.get_mut(id).unwrap().receiver=Some(Arc::new(Mutex::new(receiver)));}
        added
    } else {
        Err(invalid("first message must be Join or Resume"))
    };
    let joined = match joined {Ok(joined)=>joined,Err(error)=>{
        increment(&stats.capacity_rejected);
        let reason=if resumed {
            record("WARN", "session_resume_rejected", format_args!("kind={:?} reason={error}", error.kind()));
            match error.kind() {
                ErrorKind::WouldBlock => "session resume pending",
                // No old membership remains to take over. A fresh Join must still
                // pass the group allowlist and the ordinary game identity checks.
                ErrorKind::NotFound => netburrow_protocol::RESUME_REJOIN_ALLOWED,
                _ => "session resume rejected",
            }
        } else {"relay is full"};
        let _ = write_tcp_message(&mut stream, &Message::Error(reason.into())).await;
        return;
    }};
    let (client_id, token, members, notices) = joined;
    let (receiver,window,enabled,failed,generation,mut closed)={
        let locked=state.lock().await;
        let client=&locked.clients[&client_id];
        (client.receiver.as_ref().unwrap().clone(),client.window.clone(),client.recovery_enabled.clone(),client.resume_failed.clone(),client.generation,client.closing.subscribe())
    };
    if resumed {
        let received=window.lock().unwrap_or_else(|p|p.into_inner()).received_through();
        if write_tcp_message(&mut stream,&Message::Resumed {client_id,received}).await.is_err() {detach(client_id,generation,&state).await;return;}
    }
    if !resumed {increment(&stats.joined);record("INFO", "joined", format_args!("client={client_id}"));}
    {
        let locked = state.lock().await;
        if !resumed {let _ = locked.enqueue(
            client_id,
            Message::Welcome {
                client_id,
                udp_token: token,
            },
        );}
        let _ = locked.enqueue(client_id, Message::Members(members));
        for notice in notices {
            let _ = locked.enqueue(notice.target, notice.message);
        }
    }

    let (reader, writer) = stream.into_split();
    let writer_stop = stopping.clone();
    let mut writer_task = tokio::spawn(resumable_writer_loop(
        writer,
        receiver,
        writer_stop,
        closed.clone(),
        stats.clone(),
        window.clone(),enabled.clone(),failed.clone(),
    ));
    let mut reader = reader;
    let mut terminal=false;
    loop {
        if failed.load(Ordering::Acquire) || *closed.borrow() {
            terminal = true;
            break;
        }
        tokio::select! {
            _ = &mut writer_task => {terminal=failed.load(Ordering::Acquire);break;},
            message = tokio::time::timeout(CLIENT_READ_TIMEOUT, read_tcp_message(&mut reader)) => {
                // Expiration closes this socket; never retry a partially consumed frame.
                let message = message.unwrap_or_else(|_| {
                    Err(io::Error::new(ErrorKind::TimedOut, "TCP receive deadline"))
                });
                let mut message = match message {
                    Ok(message) => message,
                    Err(error) => {
                        if error.kind() != ErrorKind::UnexpectedEof { increment(&stats.io_failed); }
                        record("INFO", "read_closed", format_args!("client={client_id} kind={:?}", error.kind()));
                        terminal=matches!(error.kind(),ErrorKind::InvalidData|ErrorKind::InvalidInput);
                        break;
                    }
                };
                // A replacement socket owns the session; the previous reader cannot mutate it.
                if !state.lock().await.clients.get(&client_id).is_some_and(|c|c.generation==generation && !c.resume_failed.load(Ordering::Acquire)) {
                    terminal=failed.load(Ordering::Acquire);break;
                }
                let mut sequence=None;
                match &message {
                    Message::SessionAck(n)=>{
                        if window.lock().unwrap_or_else(|p|p.into_inner()).acknowledge(*n).is_err(){terminal=true;break;}
                        continue;
                    }
                    Message::SessionFrame {sequence:n,body}=>{
                        if !enabled.load(Ordering::Acquire){terminal=true;break;}
                        let classification=window.lock().unwrap_or_else(|p|p.into_inner()).classify(*n);
                        match classification {
                            Ok(false)=>{let through=window.lock().unwrap_or_else(|p|p.into_inner()).received_through();let _=state.lock().await.enqueue(client_id,Message::SessionAck(through));continue;}
                            Ok(true)=>{},Err(_)=>{terminal=true;break;}
                        }
                        sequence=Some(*n);
                        match netburrow_protocol::decode_session_body(body) {Ok(inner)=>message=inner,Err(_)=>{terminal=true;break;}}
                    }
                    _=>{}
                }
                let leave = handle_tcp_message(client_id, message, &state).await;
                if failed.load(Ordering::Acquire) { terminal=true;break; }
                if let Some(n)=sequence {
                    if window.lock().unwrap_or_else(|p|p.into_inner()).received(n).is_err(){terminal=true;break;}
                    let _=state.lock().await.enqueue(client_id,Message::SessionAck(n));
                }
                if leave { terminal=true;break; }
            }
            changed = stopping.changed() => {
                if changed.is_ok() && *stopping.borrow() { break; }
            }
            changed = closed.changed() => {
                if changed.is_ok() && *closed.borrow() { terminal=true;break; }
            }
        }
    }
    if !writer_task.is_finished(){writer_task.abort();let _ = writer_task.await;}
    if state.lock().await.clients.get(&client_id).is_some_and(|c|c.generation==generation) {
        if terminal {disconnect(client_id,&state).await;} else {detach(client_id,generation,&state).await;}
    }
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
                    record(
                        "INFO",
                        "game_binding",
                        format_args!("client={client_id} bound={}", steam_id != 0 && epoch != 0),
                    );
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
            let action = {
                let locked = state.lock().await;
                increment(&locked.stats.tcp_data_received);
                locked.route_tcp(client_id, packet)
            };
            execute_route(client_id, action, state, None).await;
            false
        }
        Message::Ping(value) => {
            let mut locked = state.lock().await;
            let _ = locked.enqueue(client_id, Message::Pong(value));
            if value == netburrow_protocol::RECOVERY_PING {
                if let Some(client)=locked.clients.get_mut(&client_id) {
                    if client.resume_key.is_none() {client.resume_key=netburrow_protocol::random_token().ok();}
                    if let Some(key)=client.resume_key {let _=locked.enqueue(client_id,Message::RecoveryOffer(key));}
                }
            }
            if value == netburrow_protocol::DIAGNOSTICS_PING {
                if let Some(client) = locked.clients.get_mut(&client_id) {
                    let changed = !client.diagnostics;
                    client.diagnostics = true;
                    let group = client.group;
                    if changed {
                        for notice in locked.diagnostic_notices(group) { let _ = locked.enqueue(notice.target, notice.message); }
                    }
                }
            }
            false
        }
        Message::PeerProbe(probe) => {
            let mut locked = state.lock().await;
            // Probe overload is lossy telemetry, never a reason to fail a game peer.
            if let Some(target) = locked.probe_target(client_id, &probe, Instant::now()) {
                let _ = locked.enqueue(target, Message::PeerProbe(probe));
            }
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
    let stats = state.lock().await.stats.clone();
    let mut buffer = vec![0; netburrow_protocol::UDP_LIMIT + 1];
    loop {
        tokio::select! {
            received = udp.recv_from(&mut buffer) => {
                let (length, source) = match received {
                    Ok(value) => value,
                    Err(error) => {
                        record("ERROR", "udp_receive_failed", format_args!("kind={:?}", error.kind()));
                        return;
                    }
                };
                let Ok(datagram) = decode_datagram(&buffer[..length]) else {
                    increment(&stats.udp_invalid);
                    continue;
                };
                match datagram {
                    Datagram::Bind { client_id, token } => {
                        let bound = state.lock().await.bind_udp(client_id, token, source);
                        if bound {
                            if let Ok(reply) = encode_datagram(&Datagram::Bound { client_id }) {
                                if udp.send_to(&reply, source).await.is_err() { increment(&stats.io_failed); }
                            }
                        } else {
                            increment(&stats.udp_bind_rejected);
                        }
                    }
                    Datagram::Data { client_id, token, packet } => {
                        increment(&stats.udp_data_received);
                        let route = state.lock().await.route_udp(client_id, token, source, packet);
                        execute_route(client_id, route, &state, Some(&udp)).await;
                    }
                    Datagram::Bound { .. } => { increment(&stats.udp_invalid); }
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
                Err(QueueError::Full(_)) => {
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
                    let result = socket.send_to(&bytes, address).await;
                    let locked = state.lock().await;
                    if result.is_ok() {
                        increment(&locked.stats.udp_data_sent);
                    } else {
                        increment(&locked.stats.io_failed);
                    }
                }
                Err(_) => send_error(client_id, state, "invalid UDP data").await,
            }
            let _ = target;
        }
        Route::Rejected(text) => send_error(client_id, state, text).await,
        Route::TargetUnavailable => {
            increment(&state.lock().await.stats.target_unavailable);
        }
        Route::TargetEpochExpired => {
            increment(&state.lock().await.stats.target_epoch_expired);
        }
    }
}

async fn send_error(client_id: u64, state: &Arc<Mutex<State>>, text: &'static str) {
    let locked = state.lock().await;
    increment(&locked.stats.protocol_rejected);
    record(
        "WARN",
        "protocol_rejected",
        format_args!("client_id={client_id} reason={text}"),
    );
    let _ = locked.enqueue(client_id, Message::Error(text.into()));
}

async fn disconnect(client_id: u64, state: &Arc<Mutex<State>>) {
    let notices = state.lock().await.remove(client_id, Instant::now());
    let locked = state.lock().await;
    for notice in notices {
        let _ = locked.enqueue(notice.target, notice.message);
    }
}
async fn detach(client_id:u64,generation:u64,state:&Arc<Mutex<State>>) {
    let mut locked=state.lock().await;
    if let Some(client)=locked.clients.get_mut(&client_id) {
        if client.generation!=generation {return;}
        if client.recovery_enabled.load(Ordering::Acquire) && !client.resume_failed.load(Ordering::Acquire) {
            client.detached_until=Some(Instant::now()+netburrow_protocol::RECOVERY_TIMEOUT);
            client.udp_address=None;
            record("WARN","session_detached",format_args!("client={client_id} recovery_seconds=120"));
            return;
        }
    }
    for notice in locked.remove(client_id,Instant::now()){let _=locked.enqueue(notice.target,notice.message);}
}

async fn resumable_writer_loop(mut writer:tokio::net::tcp::OwnedWriteHalf,receiver:Arc<Mutex<mpsc::Receiver<Queued>>>,mut stopping:watch::Receiver<bool>,mut closed:watch::Receiver<bool>,stats:Arc<Stats>,window:Arc<std::sync::Mutex<ReplayWindow>>,enabled:Arc<std::sync::atomic::AtomicBool>,failed:Arc<std::sync::atomic::AtomicBool>) {
    if failed.load(Ordering::Acquire) || *closed.borrow() { return; }
    let replay=window.lock().unwrap_or_else(|p|p.into_inner()).pending();
    for (sequence,body) in replay {
        if failed.load(Ordering::Acquire) || *closed.borrow() { return; }
        if !matches!(tokio::time::timeout(Duration::from_secs(15),write_tcp_message(&mut writer,&Message::SessionFrame{sequence,body})).await,Ok(Ok(()))){return;}
    }
    loop {
        let next=async {receiver.lock().await.recv().await};
        tokio::select! {
            item=next=>{
                let Some(mut queued)=item else{return;};
                if failed.load(Ordering::Acquire) || *closed.borrow() { return; }
                let offer=matches!(queued.message,Message::RecoveryOffer(_));
                let message=if enabled.load(Ordering::Acquire)&&netburrow_protocol::replayable(&queued.message) {
                    let Ok(body)=encode(&queued.message) else{return;};
                    let sequence=match window.lock().unwrap_or_else(|p|p.into_inner()).retain(body.clone()) {Ok(n)=>n,Err(_)=>{failed.store(true,Ordering::Release);return;}};
                    // Transfer the existing queue reservation to replay storage until ACK/drop.
                    queued.released=true;
                    Message::SessionFrame{sequence,body}
                } else {queued.message.clone()};
                let result=tokio::time::timeout(Duration::from_secs(15),write_tcp_message(&mut writer,&message)).await;
                queued.release();
                if !matches!(result,Ok(Ok(()))){return;}
                if offer {enabled.store(true,Ordering::Release);}
                if matches!(queued.message,Message::Data(_)){increment(&stats.tcp_data_written);}
            }
            _=stopping.changed()=>return,
            _=closed.changed()=>return,
        }
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

struct State {
    next_client_id: u64,
    clients: HashMap<u64, Client>,
    queued_total: Arc<AtomicUsize>,
    total_queue_limit: usize,
    stats: Arc<Stats>,
}

struct ReplayWindow {
    frames:netburrow_protocol::resume::Window,
    budget:Arc<AtomicUsize>,
    total:Arc<AtomicUsize>,
}
impl ReplayWindow {
    fn new(budget:Arc<AtomicUsize>,total:Arc<AtomicUsize>)->Self{Self{frames:Default::default(),budget,total}}
    fn acknowledge(&mut self,n:u64)->io::Result<()> {
        let before=self.frames.pending_bytes();self.frames.acknowledge(n)?;
        let released=before-self.frames.pending_bytes();
        self.budget.fetch_sub(released,Ordering::AcqRel);self.total.fetch_sub(released,Ordering::AcqRel);Ok(())
    }
}
impl std::ops::Deref for ReplayWindow {type Target=netburrow_protocol::resume::Window;fn deref(&self)->&Self::Target{&self.frames}}
impl std::ops::DerefMut for ReplayWindow {fn deref_mut(&mut self)->&mut Self::Target{&mut self.frames}}
impl Drop for ReplayWindow {fn drop(&mut self){let bytes=self.frames.pending_bytes();self.budget.fetch_sub(bytes,Ordering::AcqRel);self.total.fetch_sub(bytes,Ordering::AcqRel);}}

struct Client {
    receiver: Option<Arc<Mutex<mpsc::Receiver<Queued>>>>,
    window: Arc<std::sync::Mutex<ReplayWindow>>,
    recovery_enabled: Arc<std::sync::atomic::AtomicBool>,
    resume_failed: Arc<std::sync::atomic::AtomicBool>,
    resume_key: Option<Token>,
    detached_until: Option<Instant>,
    generation: u64,
    diagnostics: bool,
    probe_window: Option<Instant>,
    probe_count: u32,
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

#[derive(Debug)]
enum QueueError {
    Full(&'static str),
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
    TargetUnavailable,
    TargetEpochExpired,
}

impl State {
    fn new(total_queue_limit: usize) -> Self {
        Self {
            next_client_id: 0,
            clients: HashMap::new(),
            queued_total: Arc::new(AtomicUsize::new(0)),
            total_queue_limit,
            stats: Arc::new(Stats::default()),
        }
    }

    fn summary(&self) -> String {
        self.stats.summary(
            self.clients.len(),
            self.clients
                .values()
                .filter(|client| client.epoch != 0)
                .count(),
            self.clients
                .values()
                .filter(|client| client.udp_address.is_some())
                .count(),
            self.queued_total.load(Ordering::Relaxed),
        )
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
        let queued_bytes=Arc::new(AtomicUsize::new(0));
        let window=Arc::new(std::sync::Mutex::new(ReplayWindow::new(queued_bytes.clone(),self.queued_total.clone())));
        self.clients.insert(
            client_id,
            Client {
                receiver:None,window,recovery_enabled:Default::default(),resume_failed:Default::default(),resume_key:None,detached_until:None,generation:0,
                diagnostics: false,
                probe_window: None,
                probe_count: 0,
                group,
                token,
                ip,
                steam_id: 0,
                epoch: 0,
                udp_address: None,
                output,
                queued_bytes,
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

    fn resume(&mut self,id:u64,key:Token,received:u64,now:Instant)->io::Result<(u64,Token,Vec<Peer>,Vec<Notice>)> {
        let client=self.clients.get_mut(&id).ok_or_else(||io::Error::new(ErrorKind::NotFound,"unknown resume session"))?;
        if client.resume_key!=Some(key)||!client.recovery_enabled.load(Ordering::Acquire)||client.resume_failed.load(Ordering::Acquire) {return Err(invalid("invalid resume session"));}
        match client.detached_until {None=>return Err(io::Error::new(ErrorKind::WouldBlock,"session still attached")),Some(deadline) if now>=deadline=>return Err(invalid("expired resume session")),_=>{}}
        client.window.lock().unwrap_or_else(|p|p.into_inner()).acknowledge(received)?;
        client.detached_until=None;
        client.generation=client.generation.checked_add(1).ok_or_else(||invalid("session generation exhausted"))?;
        let token=client.token;let group=client.group;
        record("INFO","session_resumed",format_args!("client={id}"));
        Ok((id,token,self.members(group),self.diagnostic_notices(group)))
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
                .any(|(id, client)| *id != client_id && client.group == group && client.steam_id == steam_id)
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
            // TCP may deliver periodic reports in a burst. Keep the last accepted
            // snapshot and its timestamp without treating telemetry as a protocol error.
            return Ok(Vec::new());
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
        let target_id = match self.valid_target(source, &packet) {
            Ok(id) => id,
            Err(route) => return route,
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
        let target_id = match self.valid_target(source, &packet) {
            Ok(id) => id,
            Err(route) => return route,
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

    fn valid_target(&self, source: &Client, packet: &Packet) -> Result<u64, Route> {
        if source.resume_failed.load(Ordering::Acquire) {
            return Err(Route::Rejected("connection is not active"));
        }
        if source.steam_id == 0
            || source.epoch == 0
            || packet.from != source.steam_id
            || packet.source_epoch != source.epoch
        {
            return Err(Route::Rejected("packet source or epoch is invalid"));
        }
        // Members updates race with packets already in transit. A departed or
        // restarted peer must not turn the sender's whole session into a failure.
        // Only search this group; never expose whether an identity exists elsewhere.
        let Some((id, target)) = self.clients.iter().find(|(_, target)| {
            target.group == source.group && target.steam_id != 0 && target.steam_id == packet.to
                && !target.resume_failed.load(Ordering::Acquire)
        }) else {
            return Err(Route::TargetUnavailable);
        };
        if target.epoch != packet.target_epoch {
            return Err(Route::TargetEpochExpired);
        }
        if packet.delivery.is_some() && (!source.diagnostics || !target.diagnostics) {
            return Err(Route::Rejected("packet diagnostics were not negotiated"));
        }
        Ok(*id)
    }

    fn probe_target(&mut self, client_id: u64, p: &netburrow_protocol::PeerProbe, now: Instant) -> Option<u64> {
        let source = self.clients.get_mut(&client_id)?;
        if !source.diagnostics || p.from != client_id || p.to == client_id || p.source_epoch != source.epoch || source.epoch == 0 || p.id == 0 { return None; }
        if source.probe_window.is_none_or(|t| now.saturating_duration_since(t) >= Duration::from_secs(1)) {
            source.probe_window = Some(now);
            source.probe_count = 0;
        }
        if source.probe_count >= 32 { return None; }
        source.probe_count += 1;
        let group = source.group;
        let target = self.clients.get(&p.to)?;
        (target.group == group && target.diagnostics && target.epoch != 0 && target.epoch == p.target_epoch).then_some(p.to)
    }

    fn diagnostic_notices(&self, group: Group) -> Vec<Notice> {
        let mut peers: Vec<_> = self.clients.iter().filter(|(_, c)| c.group == group && c.diagnostics).map(|(id, _)| *id).collect();
        peers.sort_unstable();
        peers.iter().map(|id| Notice { target: *id, message: Message::DiagnosticsPeers(peers.clone()) }).collect()
    }

    fn enqueue(&self, client_id: u64, message: Message) -> Result<(), QueueError> {
        let client = self.clients.get(&client_id).ok_or(QueueError::Closed)?;
        if client.resume_failed.load(Ordering::Acquire) { return Err(QueueError::Closed); }
        if client.detached_until.is_some() && !netburrow_protocol::replayable(&message) {return Ok(());}
        let members = matches!(&message, Message::Members(_));
        let result = enqueue(
            &client.output,
            message,
            client.max_queued_bytes,
            client.queued_bytes.clone(),
            self.total_queue_limit,
            self.queued_total.clone(),
        );
        if let Err(QueueError::Full(reason)) = &result {
            // Concurrent sending/ACKs can advance these snapshots after the rejection.
            let replay_bytes = client.window.lock().unwrap_or_else(|p| p.into_inner()).pending_bytes();
            record("WARN", "queue_full", format_args!(
                "client={client_id} reason={reason} queued_messages={} max_messages={} queued_bytes={} max_bytes={} replay_bytes={replay_bytes} total_queued_bytes={} total_max_bytes={}",
                client.output.max_capacity() - client.output.capacity(), client.output.max_capacity(),
                client.queued_bytes.load(Ordering::Acquire), client.max_queued_bytes,
                self.queued_total.load(Ordering::Acquire), self.total_queue_limit,
            ));
        }
        if result.is_err() {
            increment(&self.stats.queue_failed);
            if members {
                // A missing identity barrier makes later data unsafe to acknowledge.
                // Retained sessions must also fail: replay cannot recreate this notice.
                client.resume_failed.store(true, Ordering::Release);
                client.closing.send_replace(true);
                record("WARN", "members_queue_failed", format_args!("client={client_id}"));
            }
        }
        result
    }

    fn remove(&mut self, client_id: u64, now: Instant) -> Vec<Notice> {
        let Some(client) = self.clients.remove(&client_id) else {
            return Vec::new();
        };
        increment(&self.stats.disconnected);
        record("INFO", "disconnected", format_args!("client={client_id}"));
        let _ = client.closing.send(true);
        let mut notices = self.notices_for_group(client.group, None);
        notices.extend(self.status_notices_for_group(client.group, now));
        notices.extend(self.diagnostic_notices(client.group));
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
        return Err(QueueError::Full("client_bytes"));
    }
    let total_reserved =
        total_budget.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current
                .checked_add(bytes)
                .filter(|next| *next <= total_maximum)
        });
    if total_reserved.is_err() {
        budget.fetch_sub(bytes, Ordering::AcqRel);
        return Err(QueueError::Full("total_bytes"));
    }
    match output.try_send(Queued {
        message,
        bytes,
        budget: budget.clone(),
        total_budget: total_budget.clone(),
        released: false,
    }) {
        Ok(()) => Ok(()),
        // The returned Queued owns the reservation and releases it on drop.
        Err(mpsc::error::TrySendError::Full(_)) => Err(QueueError::Full("messages")),
        Err(mpsc::error::TrySendError::Closed(_)) => Err(QueueError::Closed),
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

    #[test]
    fn rejected_queue_items_release_only_their_own_reservation() {
        let frame_bytes = encode(&Message::Ping(1)).unwrap().len();
        for rejection in ["messages", "closed", "client_bytes", "total_bytes"] {
            let (sender, mut receiver) = mpsc::channel(1);
            let budget = Arc::new(AtomicUsize::new(0));
            let total = Arc::new(AtomicUsize::new(0));
            enqueue(&sender, Message::Ping(1), 1024, budget.clone(), 4096, total.clone()).unwrap();
            if rejection == "closed" { receiver.close(); }
            let maximum = if rejection == "client_bytes" { frame_bytes } else { 1024 };
            let total_maximum = if rejection == "total_bytes" { frame_bytes } else { 4096 };
            let result = enqueue(&sender, Message::Ping(2), maximum, budget.clone(), total_maximum, total.clone());
            match result {
                Err(QueueError::Full(reason)) => assert_eq!(reason, rejection),
                Err(QueueError::Closed) => assert_eq!(rejection, "closed"),
                other => panic!("unexpected result for {rejection}: {other:?}"),
            }
            assert_eq!(budget.load(Ordering::Acquire), frame_bytes, "{rejection}");
            assert_eq!(total.load(Ordering::Acquire), frame_bytes, "{rejection}");
            drop(receiver.try_recv().unwrap());
            assert_eq!(budget.load(Ordering::Acquire), 0, "{rejection}");
            assert_eq!(total.load(Ordering::Acquire), 0, "{rejection}");

            let (other, mut other_receiver) = mpsc::channel(1);
            let other_budget = Arc::new(AtomicUsize::new(0));
            enqueue(&other, Message::Ping(3), 1024, other_budget.clone(), 4096, total.clone()).unwrap();
            drop(other_receiver.try_recv().unwrap());
            assert_eq!(other_budget.load(Ordering::Acquire), 0);
            assert_eq!(total.load(Ordering::Acquire), 0);
        }
    }

    #[tokio::test]
    async fn lost_members_barrier_fails_only_affected_session_and_cannot_resume() {
        for byte_pressure in [false, true] {
            for (previous, next) in [((0, 0), (22, 222)), ((22, 222), (22, 223)), ((22, 222), (0, 0))] {
                let mut state = State::new(16384);
                let (sender, mut receiver) = mpsc::channel(if byte_pressure { 32 } else { 1 });
                let (closing, _) = watch::channel(false);
                let a = state.add(group(1), "127.0.0.1".parse().unwrap(), sender, closing, 4096, 8).unwrap().0;
                let (sender, mut healthy_receiver) = mpsc::channel(32);
                let (closing, _) = watch::channel(false);
                let b = state.add(group(1), "127.0.0.1".parse().unwrap(), sender, closing, 4096, 8).unwrap().0;
                let (sender, mut other_receiver) = mpsc::channel(32);
                let (closing, _) = watch::channel(false);
                let c = state.add(group(2), "127.0.0.1".parse().unwrap(), sender, closing, 4096, 8).unwrap().0;
                state.bind(a, 11, 111, Instant::now()).unwrap();
                state.bind(b, previous.0, previous.1, Instant::now()).unwrap();
                let members_bytes = encode(&Message::Members(state.members(group(1)))).unwrap().len();
                let client = state.clients.get_mut(&a).unwrap();
                if byte_pressure { client.max_queued_bytes = members_bytes; }
                client.recovery_enabled.store(true, Ordering::Release);
                client.resume_key = Some([7; 16]);
                state.enqueue(a, Message::Ping(1)).unwrap();
                let state = Arc::new(Mutex::new(state));
                assert!(!handle_tcp_message(b, Message::Bind { steam_id: next.0, epoch: next.1 }, &state).await);
                {
                    let mut locked = state.lock().await;
                    assert!(*locked.clients[&a].closing.borrow());
                    assert!(locked.clients[&a].resume_failed.load(Ordering::Acquire));
                    assert!(!locked.clients[&b].resume_failed.load(Ordering::Acquire));
                    assert!(!locked.clients[&c].resume_failed.load(Ordering::Acquire));
                    assert!(matches!(locked.enqueue(a, Message::Ping(2)), Err(QueueError::Closed)));
                    locked.enqueue(c, Message::Ping(3)).unwrap();
                    if next.0 != 0 {
                        assert!(matches!(locked.route_tcp(b, packet(next.0, 11, next.1, 111, 2, b"no stale delivery")), Route::TargetUnavailable));
                    }
                    assert!(matches!(healthy_receiver.try_recv().unwrap().message, Message::Members(ref peers) if peers.iter().any(|p| p.client_id == b && p.epoch == next.1)));
                    // No watch receiver is required to remember failure while detached.
                    locked.clients.get_mut(&a).unwrap().detached_until = Some(Instant::now() + netburrow_protocol::RECOVERY_TIMEOUT);
                    assert!(locked.resume(a, [7; 16], 0, Instant::now()).is_err());
                }
                detach(a, 0, &state).await;
                let locked = state.lock().await;
                assert!(!locked.clients.contains_key(&a));
                assert!(locked.clients.contains_key(&b) && locked.clients.contains_key(&c));
                assert!(matches!(healthy_receiver.try_recv().unwrap().message, Message::Members(ref peers) if peers.iter().all(|p| p.client_id != a)));
                while let Ok(queued) = receiver.try_recv() { drop(queued); }
                while let Ok(queued) = healthy_receiver.try_recv() { drop(queued); }
                while let Ok(queued) = other_receiver.try_recv() { drop(queued); }
                assert_eq!(locked.queued_total.load(Ordering::Acquire), 0);
            }
        }
    }

    #[tokio::test]
    async fn allowlist_rejects_without_membership_or_token_and_keeps_groups_isolated() {
        let relay = spawn(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            allowed_groups: AllowedGroups::parse(&format!(
                "NB1-{}\nNB1-{}",
                "01".repeat(32),
                "02".repeat(32)
            ))
            .unwrap(),
            ..Config::default()
        })
        .await
        .unwrap();
        let (mut one, one_id, _) = connect(relay.local_addr(), group(1)).await;
        // Only missing sessions permit a fresh Join, which still needs group authorization.
        for (client_id,expected) in [
            (one_id,"session resume rejected"),
            (u64::MAX,netburrow_protocol::RESUME_REJOIN_ALLOWED),
        ] {
            let mut resume=TcpStream::connect(relay.local_addr()).await.unwrap();
            write_tcp_message(&mut resume,&Message::Resume {client_id,key:[0;16],received:0}).await.unwrap();
            assert_eq!(read_tcp_message(&mut resume).await.unwrap(),Message::Error(expected.into()));
        }
        let mut denied = TcpStream::connect(relay.local_addr()).await.unwrap();
        write_tcp_message(&mut denied, &Message::Join { group: group(3) })
            .await
            .unwrap();
        assert_eq!(
            read_tcp_message(&mut denied).await.unwrap(),
            Message::Error(GROUP_NOT_ALLOWED.into())
        );
        let mut byte = [0];
        assert_eq!(
            timeout(Duration::from_secs(1), denied.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        let (mut two, two_id, _) = connect(relay.local_addr(), group(2)).await;
        assert_eq!(
            two_id,
            one_id + 1,
            "rejected join must not allocate membership/token"
        );
        bind(&mut one, 101, 1).await;
        bind(&mut two, 202, 1).await;
        recv_until(&mut two, |message| matches!(message, Message::Members(peers) if peers.len() == 1 && peers[0].steam_id == 202)).await;
        recv_until(&mut one, |message| matches!(message, Message::Members(peers) if peers.len() == 1 && peers[0].steam_id == 101)).await;
        write_tcp_message(
            &mut one,
            &Message::Data(packet(101, 202, 1, 1, 3, b"isolated")),
        )
        .await
        .unwrap();
        write_tcp_message(&mut one, &Message::Ping(41)).await.unwrap();
        assert_eq!(read_tcp_message(&mut one).await.unwrap(), Message::Pong(41));
        write_tcp_message(&mut two, &Message::Ping(42)).await.unwrap();
        assert_eq!(read_tcp_message(&mut two).await.unwrap(), Message::Pong(42));
        let mut empty = State::new(4096);
        assert!(!empty.bind_udp(99, [0; 16], "127.0.0.1:12345".parse().unwrap()));
        assert!(matches!(
            empty.route_udp(
                99,
                [0; 16],
                "127.0.0.1:12345".parse().unwrap(),
                packet(101, 202, 1, 1, 1, b"no session")
            ),
            Route::Rejected(_)
        ));
        relay.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn replay_keeps_queue_budget_charged_until_ack_or_session_drop() {
        let listener=TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut receiving=TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let (socket,_)=listener.accept().await.unwrap();let(_,writer)=socket.into_split();
        let mut state=State::new(128);
        let(sender,receiver)=mpsc::channel(8);let(closing,closed)=watch::channel(false);
        let id=state.add(group(1),"127.0.0.1".parse().unwrap(),sender,closing,128,8).unwrap().0;
        let window=state.clients[&id].window.clone();
        let enabled=Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (stop,stopped)=watch::channel(false);
        let task=tokio::spawn(resumable_writer_loop(writer,Arc::new(Mutex::new(receiver)),stopped,closed,state.stats.clone(),window.clone(),enabled,Default::default()));
        let message=Message::Data(packet(11,22,111,222,2,&[1;40]));
        assert!(state.enqueue(id,message.clone()).is_ok());
        let frame=timeout(Duration::from_secs(1),read_tcp_message(&mut receiving)).await.unwrap().unwrap();
        let Message::SessionFrame{sequence,..}=frame else{panic!("expected replay frame")};
        assert_eq!(state.queued_total.load(Ordering::Acquire),encode(&message).unwrap().len());
        assert!(matches!(state.enqueue(id,message.clone()),Err(QueueError::Full(_))));
        window.lock().unwrap().acknowledge(sequence).unwrap();
        assert_eq!(state.queued_total.load(Ordering::Acquire),0);
        assert!(state.enqueue(id,message).is_ok());
        assert!(matches!(timeout(Duration::from_secs(1),read_tcp_message(&mut receiving)).await.unwrap().unwrap(),Message::SessionFrame{..}));
        stop.send(true).unwrap();task.await.unwrap();drop(window);state.remove(id,Instant::now());
        assert_eq!(state.queued_total.load(Ordering::Acquire),0);
    }

    #[tokio::test]
    async fn session_resume_rejects_wrong_key_expiry_and_invalid_ack_without_rebinding() {
        let mut state=State::new(4096);let id=add_state_client(&mut state,group(1));let now=Instant::now();
        state.bind(id,11,111,now).unwrap();
        let peer=state.clients.get_mut(&id).unwrap();peer.resume_key=Some([7;16]);peer.recovery_enabled.store(true,Ordering::Release);peer.detached_until=Some(now+Duration::from_secs(2));
        peer.window.lock().unwrap().retain(vec![]).unwrap();
        assert!(state.resume(id,[8;16],0,now).is_err());
        assert!(state.resume(id,[7;16],2,now).is_err());
        assert!(state.resume(id,[7;16],0,now+Duration::from_secs(2)).is_err());
        assert_eq!(state.clients[&id].generation,0);
        let (resumed,_,members,_)=state.resume(id,[7;16],1,now).unwrap();
        assert_eq!(resumed,id);assert_eq!((members[0].steam_id,members[0].epoch),(11,111));
        assert_eq!(state.clients[&id].generation,1);
        assert_eq!(state.clients[&id].window.lock().unwrap().pending_len(),0);
        assert!(state.resume(id,[7;16],1,now).is_err());
        state.remove(id,now);assert!(state.resume(id,[7;16],1,now).is_err());
    }

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
            delivery: None,
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

    async fn assert_healthy(stream: &mut TcpStream) {
        write_tcp_message(stream, &Message::Ping(987))
            .await
            .unwrap();
        // TCP ordering makes Pong a barrier for all earlier requests. Do not
        // silently skip Error here: the real client treats it as fatal.
        recv_until(stream, |message| {
            assert!(!matches!(message, Message::Error(_) | Message::Data(_)));
            matches!(message, Message::Pong(987))
        })
        .await;
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

    #[tokio::test]
    async fn loopback_tcp_udp_and_group_isolation() {
        let relay = spawn(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            allowed_groups: test_allowed_groups(),
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
        assert_healthy(&mut one).await;
        assert_healthy(&mut other).await;
        relay.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn rejects_duplicate_old_epochs_and_cleans_unbind_disconnect() {
        let relay = spawn(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            allowed_groups: test_allowed_groups(),
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

#[cfg(test)]
fn test_allowed_groups() -> AllowedGroups {
    let text = (1u8..=16)
        .map(|byte| format!("NB1-{}", format!("{byte:02x}").repeat(32)))
        .collect::<Vec<_>>()
        .join("\n");
    AllowedGroups::parse(&text).unwrap()
}
