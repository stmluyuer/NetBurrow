use crate::Settings;
use std::{
    sync::{Arc, Mutex},
    thread::JoinHandle,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Stopped,
    Connecting,
    WaitingForGame,
    Attaching,
    Ready,
    RestartRequired,
    Failed,
}

#[derive(Clone, Debug)]
pub struct PeerInfo {
    pub client_id: u64,
    pub game_epoch: u64,
    pub steam_id: u64,
    pub ready: bool,
    pub is_self: bool,
    pub status: Option<netburrow_protocol::MemberStatus>,
    pub status_updated: Option<std::time::Instant>,
}

impl PeerInfo {
    pub fn status_is_stale(&self) -> bool {
        self.status_updated
            .is_none_or(|updated| updated.elapsed() > std::time::Duration::from_secs(10))
    }
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub phase: Phase,
    pub detail: String,
    pub peers: Vec<PeerInfo>,
    pub sent: u64,
    pub received: u64,
    pub udp_sent: u64,
    pub udp_received: u64,
    pub ping_ms: Option<u64>,
    pub rtt_samples: std::collections::VecDeque<u64>,
    pub last_pong_at: Option<std::time::Instant>,
    pub last_disconnect_at: Option<std::time::Instant>,
    pub disconnects: u64,
    pub heartbeat_timeouts: u64,
    pub logs: Vec<String>,
}

type Shared = Arc<Mutex<Snapshot>>;

pub(crate) fn probe_relay(settings: &Settings) -> Result<(), String> {
    #[cfg(windows)]
    {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|error| format!("无法启动自检：{error}"))?;
        runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(8), runtime::probe(settings)).await
                .map_err(|_| "服务器自检超时，请检查网络、地址和端口后重试。".to_owned())?
        })
    }
    #[cfg(not(windows))]
    { let _ = settings; Err("客户端自检需要 Windows".into()) }
}
fn change(shared: &Shared, update: impl FnOnce(&mut Snapshot)) {
    update(&mut shared.lock().unwrap_or_else(|p| p.into_inner()));
}
fn status(shared: &Shared, phase: Phase, detail: impl Into<String>) {
    let detail = detail.into();
    change(shared, |s| {
        if s.phase != phase || s.detail != detail {
            crate::diagnostics::record("INFO", "state", &format!("{phase:?}: {detail}"));
            s.logs.push(detail.clone());
            if s.logs.len() > 64 {
                s.logs.remove(0);
            }
        }
        s.phase = phase;
        s.detail = detail;
    });
}

pub struct Client {
    stop: tokio::sync::watch::Sender<bool>,
    state: Shared,
    worker: Option<JoinHandle<()>>,
}

impl Client {
    pub fn start(settings: Settings) -> Result<Self, String> {
        crate::diagnostics::record(
            "INFO",
            "enable",
            &format!(
                "transport={:?} allow_late_hook={}",
                settings.transport, settings.allow_late_hook
            ),
        );
        settings.validate()?;
        #[cfg(not(windows))]
        return Err("NetBurrow 客户端需要 Windows".into());
        #[cfg(windows)]
        {
            let directory = std::env::current_exe()
                .map_err(|e| e.to_string())?
                .parent()
                .ok_or("程序目录不可用")?
                .to_owned();
            if !directory.join("netburrow-injector.exe").is_file()
                || !directory.join("netburrow_hook.dll").is_file()
            {
                return Err("缺少自己的 helper 或 Hook DLL，请完整解压 NetBurrow 包。".into());
            }
            let (stop, stopped) = tokio::sync::watch::channel(false);
            let state = Arc::new(Mutex::new(Snapshot::default()));
            let shared = state.clone();
            let worker = std::thread::Builder::new()
                .name("netburrow-client".into())
                .spawn(move || {
                    match tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(2)
                        .enable_all()
                        .build()
                    {
                        Ok(runtime) => runtime.block_on(runtime::run(
                            settings,
                            directory,
                            stopped,
                            shared.clone(),
                        )),
                        Err(_) => status(&shared, Phase::Failed, "无法创建客户端运行线程"),
                    }
                })
                .map_err(|e| e.to_string())?;
            Ok(Self {
                stop,
                state,
                worker: Some(worker),
            })
        }
    }
    pub fn snapshot(&self) -> Snapshot {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    pub fn stop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(windows)]
mod runtime {
    use super::*;
    use crate::diagnostics::record as log;
    use crate::{
        Transport,
        process::{GameProcess, find_games},
        settings::parse_group,
    };
    use netburrow_protocol::{
        Datagram, Group, MAX_FRAME, MemberStatus, Message, Packet, Peer, PeerStatus, Token, decode,
        decode_datagram, encode, encode_datagram, random_epoch, random_token,
    };
    use std::{
        collections::HashSet,
        io::{self, Read, Write},
        os::windows::process::CommandExt,
        path::{Path, PathBuf},
        process::{Child, Command, Stdio},
        time::Duration,
    };
    use tokio::{
        io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
        net::{TcpListener, TcpStream, UdpSocket, tcp::OwnedWriteHalf},
        sync::{mpsc, watch},
        task::JoinHandle as Task,
        time::{Instant, interval, timeout},
    };

    const IO_TIMEOUT: Duration = Duration::from_secs(5);
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    pub(super) async fn probe(settings: &Settings) -> Result<(), String> {
        let group = parse_group(&settings.group)?;
        let mut network = Network::connect(settings.server.trim(), group, Transport::Tcp).await
            .map_err(|error| connection_hint(&error).split('；').next().unwrap_or("服务器连接失败").to_owned())?;
        network.send(&Message::Status(MemberStatus { name: String::new(), phase: 0, ping_ms: None, transport: 0, sent: 0, received: 0 })).await
            .map_err(|_| "服务器状态检查发送失败，请稍后重试。".to_owned())?;
        let result = loop {
            match network.events.recv().await {
                Some(NetworkEvent::Tcp(Message::Statuses(_))) => break Ok(()),
                Some(NetworkEvent::Tcp(Message::Members(_))) => {},
                Some(NetworkEvent::Tcp(Message::Ping(value))) => { if network.send(&Message::Pong(value)).await.is_err() { break Err("服务器连接中断，请重试。".into()); } },
                Some(NetworkEvent::Tcp(Message::Error(_))) => break Err("服务器拒绝状态检查，请确认连接的是支持成员状态的 NetBurrow Relay。".into()),
                _ => break Err("服务器握手或协议检查失败，请检查地址和服务状态。".into()),
            }
        };
        let _ = network.send(&Message::Leave).await;
        result
    }

    fn connection_hint(error: &io::Error) -> &'static str {
        match error.kind() {
            io::ErrorKind::TimedOut => "连接服务器超时：请检查服务器地址、网络和防火墙；3 秒后自动重试。修改连接设置前请先停止联机。",
            io::ErrorKind::ConnectionRefused => "服务器拒绝连接：请确认 Relay 已启动、端口填写正确且已放行；3 秒后自动重试。修改连接设置前请先停止联机。",
            io::ErrorKind::WouldBlock => "服务器已满：请稍后再试，或联系服务器管理员增加容量；3 秒后自动重试。",
            io::ErrorKind::PermissionDenied => "服务器拒绝加入：请确认地址指向 NetBurrow Relay，并核对工具与服务器版本；3 秒后自动重试。可先停止联机再修改设置。",
            io::ErrorKind::InvalidData => "服务器协议不匹配：请确认端口指向 NetBurrow Relay，且双方版本一致；3 秒后自动重试。可先停止联机再修改设置。",
            _ => "无法连接服务器：请检查地址、域名解析、网络及 Relay 服务状态；3 秒后自动重试。查看日志可获取底层错误，修改设置前请先停止联机。",
        }
    }

    async fn read<R: AsyncRead + Unpin>(socket: &mut R) -> io::Result<Message> {
        let length = socket.read_u32().await? as usize;
        if length == 0 || length > MAX_FRAME {
            return Err(io::Error::other("invalid message length"));
        }
        let mut body = vec![0; length];
        socket.read_exact(&mut body).await?;
        decode(&body)
    }
    async fn write<W: AsyncWrite + Unpin>(socket: &mut W, message: &Message) -> io::Result<()> {
        let data = encode(message)?;
        timeout(IO_TIMEOUT, socket.write_all(&data))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "write timeout"))?
    }

    enum NetworkEvent {
        Tcp(Message),
        Udp(Packet),
        UdpBound,
        Closed,
    }
    struct Network {
        writer: OwnedWriteHalf,
        events: mpsc::Receiver<NetworkEvent>,
        tasks: Vec<Task<()>>,
        udp: Option<Arc<UdpSocket>>,
        client_id: u64,
        token: Token,
        udp_bound: bool,
    }
    impl Drop for Network {
        fn drop(&mut self) {
            for task in &self.tasks {
                task.abort();
            }
        }
    }
    impl Network {
        async fn connect(server: &str, group: Group, transport: Transport) -> io::Result<Self> {
            let socket = timeout(IO_TIMEOUT, TcpStream::connect(server))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect timeout"))??;
            socket.set_nodelay(true)?;
            let address = socket.peer_addr()?;
            let (mut input, mut writer) = socket.into_split();
            write(&mut writer, &Message::Join { group }).await?;
            let hello = timeout(IO_TIMEOUT, read(&mut input))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "join timeout"))??;
            let (client_id, token) = match hello {
                Message::Welcome { client_id, udp_token } => (client_id, udp_token),
                Message::Error(reason) if reason == "relay is full" => {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, "Relay is full"));
                }
                Message::Error(_) => {
                    return Err(io::Error::new(io::ErrorKind::PermissionDenied, "Relay rejected join"));
                }
                _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "Unexpected Relay handshake")),
            };
            let (tx, events) = mpsc::channel(16);
            let tcp_tx = tx.clone();
            let reader = tokio::spawn(async move {
                while let Ok(message) = read(&mut input).await {
                    if tcp_tx.send(NetworkEvent::Tcp(message)).await.is_err() {
                        return;
                    }
                }
                let _ = tcp_tx.send(NetworkEvent::Closed).await;
            });
            let mut tasks = vec![reader];
            let udp = if transport == Transport::Udp {
                let socket = Arc::new(
                    UdpSocket::bind(if address.is_ipv4() {
                        "0.0.0.0:0"
                    } else {
                        "[::]:0"
                    })
                    .await?,
                );
                socket.connect(address).await?;
                let udp_input = socket.clone();
                tasks.push(tokio::spawn(async move {
                    let mut buffer = [0; netburrow_protocol::UDP_LIMIT + 1];
                    while let Ok(count) = udp_input.recv(&mut buffer).await {
                        let event = match decode_datagram(&buffer[..count]) {
                            Ok(Datagram::Bound { client_id: id }) if id == client_id => {
                                NetworkEvent::UdpBound
                            }
                            Ok(Datagram::Data {
                                client_id: id,
                                token: key,
                                packet,
                            }) if id == client_id && key == token => NetworkEvent::Udp(packet),
                            _ => continue,
                        };
                        if tx.send(event).await.is_err() {
                            break;
                        }
                    }
                }));
                Some(socket)
            } else {
                None
            };
            Ok(Self {
                writer,
                events,
                tasks,
                udp,
                client_id,
                token,
                udp_bound: false,
            })
        }
        async fn send(&mut self, message: &Message) -> io::Result<()> {
            if matches!(message, Message::Bind { .. }) {
                self.udp_bound = false;
            }
            write(&mut self.writer, message).await
        }
        async fn bind_udp(&self) -> io::Result<()> {
            if let Some(socket) = &self.udp {
                socket
                    .send(&encode_datagram(&Datagram::Bind {
                        client_id: self.client_id,
                        token: self.token,
                    })?)
                    .await?;
            }
            Ok(())
        }
        async fn packet(&mut self, packet: Packet) -> io::Result<bool> {
            if self.udp_bound && packet.send_type <= 1 {
                if let Some(socket) = &self.udp {
                    if let Ok(bytes) = encode_datagram(&Datagram::Data {
                        client_id: self.client_id,
                        token: self.token,
                        packet: packet.clone(),
                    }) {
                        socket.send(&bytes).await?;
                        return Ok(true);
                    }
                }
            }
            self.send(&Message::Data(packet)).await?;
            Ok(false)
        }
    }

    struct Launch {
        game: GameProcess,
        epoch: u64,
        nonce: Token,
        since: Instant,
        helper: Option<Child>,
    }
    impl Drop for Launch {
        fn drop(&mut self) {
            if let Some(mut child) = self.helper.take() {
                if child.try_wait().ok().flatten().is_none() {
                    let _ = child.kill();
                }
                let _ = child.wait();
            }
        }
    }
    struct Hook {
        writer: OwnedWriteHalf,
        input: mpsc::Receiver<io::Result<Message>>,
        reader: Task<()>,
        steam_id: u64,
        epoch: u64,
        ready: bool,
        bound: bool,
        acknowledged: bool,
    }
    impl Drop for Hook {
        fn drop(&mut self) {
            self.reader.abort();
        }
    }
    impl Hook {
        async fn accept(mut socket: TcpStream, launch: &Launch) -> io::Result<Self> {
            socket.set_nodelay(true)?;
            let hello = timeout(IO_TIMEOUT, read(&mut socket))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "IPC hello timeout"))??;
            if !matches!(hello, Message::IpcHello { nonce, pid, steam_id, epoch } if nonce == launch.nonce && pid == launch.game.pid && steam_id != 0 && epoch == launch.epoch)
            {
                return Err(io::Error::other("IPC identity mismatch"));
            }
            let Message::IpcHello {
                steam_id, epoch, ..
            } = hello
            else {
                unreachable!()
            };
            let (mut input, writer) = socket.into_split();
            let (tx, rx) = mpsc::channel(16);
            let reader = tokio::spawn(async move {
                loop {
                    let result = read(&mut input).await;
                    let ended = result.is_err();
                    if tx.send(result).await.is_err() || ended {
                        break;
                    }
                }
            });
            Ok(Self {
                writer,
                input: rx,
                reader,
                steam_id,
                epoch,
                ready: false,
                bound: false,
                acknowledged: false,
            })
        }
        async fn send(&mut self, message: &Message) -> io::Result<()> {
            write(&mut self.writer, message).await
        }
    }

    fn launch(game: GameProcess, directory: &Path, port: u16) -> io::Result<Launch> {
        log(
            "INFO",
            "inject",
            &format!(
                "launch helper for pid={} created={}",
                game.pid, game.created
            ),
        );
        let nonce = random_token()?;
        let epoch = random_epoch()?;
        let mut child = Command::new(directory.join("netburrow-injector.exe"))
            .arg("--pid")
            .arg(game.pid.to_string())
            .arg("--created")
            .arg(game.created.to_string())
            .arg("--exe")
            .arg(&game.path)
            .arg("--dll")
            .arg(directory.join("netburrow_hook.dll"))
            .arg("--port")
            .arg(port.to_string())
            .arg("--epoch")
            .arg(epoch.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?;
        if let Err(e) = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("helper stdin unavailable"))?
            .write_all(&nonce)
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        Ok(Launch {
            game,
            epoch,
            nonce,
            since: Instant::now(),
            helper: Some(child),
        })
    }

    async fn disconnect_hook(
        hook: &mut Option<Hook>,
        pending: &mut Option<Launch>,
        network: &mut Network,
    ) {
        if let Some(mut link) = hook.take() {
            log("INFO", "ipc", "stopping Hook and clearing game binding");
            let _ = link.send(&Message::Stop).await;
        }
        *pending = None;
        let _ = network
            .send(&Message::Bind {
                steam_id: 0,
                epoch: 0,
            })
            .await;
    }

    pub async fn run(
        settings: Settings,
        directory: PathBuf,
        mut stop: watch::Receiver<bool>,
        state: Shared,
    ) {
        let group = match parse_group(&settings.group) {
            Ok(g) => g,
            Err(e) => {
                status(&state, Phase::Failed, e);
                return;
            }
        };
        let path = PathBuf::from(settings.game_path.trim());
        let listener = match TcpListener::bind("127.0.0.1:0").await {
            Ok(l) => l,
            Err(_) => {
                status(&state, Phase::Failed, "无法创建本地 IPC 端口");
                return;
            }
        };
        let port = listener.local_addr().unwrap().port();
        let mut seen: HashSet<(u32, u64)> = match find_games(&path) {
            // Opt-in changes only the initial baseline. Every attempted process is still
            // recorded below, so reconnects cannot reinitialize the same loaded Hook.
            Ok(games) => games
                .into_iter()
                .filter(|_| !settings.allow_late_hook)
                .map(|g| (g.pid, g.created))
                .collect(),
            Err(_) => {
                status(&state, Phase::Failed, "无法枚举当前用户的游戏进程");
                return;
            }
        };
        log(
            "INFO",
            "process baseline",
            &format!(
                "excluded_running_processes={} allow_late_hook={}",
                seen.len(),
                settings.allow_late_hook
            ),
        );
        status(&state, Phase::Connecting, "正在连接 Relay…");
        while !*stop.borrow() {
            let result = tokio::select! { biased; _ = stop.changed() => break, result = Network::connect(settings.server.trim(), group, settings.transport) => result };
            let mut network = match result {
                Ok(n) => n,
                Err(error) => {
                    log("WARN", "relay connect", &error.to_string());
                    status(
                        &state,
                        Phase::Connecting,
                        connection_hint(&error),
                    );
                    tokio::select! { _ = stop.changed() => break, _ = tokio::time::sleep(Duration::from_secs(3)) => {} }
                    continue;
                }
            };
            log(
                "INFO",
                "relay",
                "joined successfully; control TCP connected",
            );
            change(&state, |s| {
                s.peers.clear();
                s.ping_ms = None;
                s.rtt_samples.clear();
                s.last_pong_at = None;
            });
            status(
                &state,
                if seen.is_empty() {
                    Phase::WaitingForGame
                } else {
                    Phase::RestartRequired
                },
                if seen.is_empty() {
                    "Relay 已连接，请从 Steam 正常启动游戏"
                } else {
                    "发现已运行的游戏，请退出游戏后从 Steam 重开"
                },
            );
            let result = connected(
                &settings,
                &directory,
                &path,
                &listener,
                port,
                &mut seen,
                &mut network,
                &mut stop,
                &state,
            )
            .await;
            let _ = network.send(&Message::Leave).await;
            if let Err(error) = &result {
                log("ERROR", "connection ended", &error.to_string());
            }
            if *stop.borrow() {
                break;
            }
            if result.is_err() {
                change(&state, |s| {
                    s.disconnects += 1;
                    s.last_disconnect_at = Some(std::time::Instant::now());
                });
            }
            if result
                .as_ref()
                .is_err_and(|error| error.kind() == io::ErrorKind::Unsupported
                    || (error.kind() == io::ErrorKind::PermissionDenied
                        && state.lock().unwrap_or_else(|p| p.into_inner()).phase == Phase::Failed))
            {
                change(&state, |s| {
                    s.peers.clear();
                    s.ping_ms = None;
                });
                if result.as_ref().is_err_and(|error| error.kind() == io::ErrorKind::Unsupported) {
                    status(&state, Phase::Failed, "服务器版本不兼容：Relay 尚不支持成员状态，请更新服务器后重新启用联机。");
                }
                return;
            }
            if result.is_err() {
                status(
                    &state,
                    Phase::Connecting,
                    "连接中断，正在重连；已接入的游戏需要重开",
                );
            }
            tokio::select! { _ = stop.changed() => break, _ = tokio::time::sleep(Duration::from_secs(2)) => {} }
        }
        change(&state, |s| {
            s.peers.clear();
            s.ping_ms = None;
        });
        status(
            &state,
            Phase::Stopped,
            "已停止；已接入的游戏需要重开后恢复普通网络",
        );
    }

    #[allow(clippy::too_many_arguments)]
    async fn connected(
        settings: &Settings,
        directory: &Path,
        game_path: &Path,
        listener: &TcpListener,
        port: u16,
        seen: &mut HashSet<(u32, u64)>,
        network: &mut Network,
        stop: &mut watch::Receiver<bool>,
        state: &Shared,
    ) -> io::Result<()> {
        let mut pending: Option<Launch> = None;
        let mut hook: Option<Hook> = None;
        let mut peers = Vec::<Peer>::new();
        let mut clock = interval(Duration::from_millis(250));
        clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let since = Instant::now();
        let mut last_ping = Instant::now() - Duration::from_secs(1);
        let mut last_pong = Instant::now();
        let mut last_stats = Instant::now();
        let mut last_report = Instant::now() - Duration::from_secs(3);
        let mut status_supported = false;
        let result = async {
            loop {
                tokio::select! {
                    biased;
                    _ = stop.changed() => return Ok(()),
                    event = network.events.recv() => {
                        match event {
                            Some(NetworkEvent::Tcp(Message::Members(members))) => {
                                log("INFO", "members", &format!("online={} game_bound={}", members.len(), members.iter().filter(|p| p.steam_id != 0).count()));
                                peers = members;
                                change(state, |s| {
                                    let old = std::mem::take(&mut s.peers);
                                    s.peers = peers.iter().map(|p| {
                                        let previous = old.iter().find(|old| old.client_id == p.client_id && old.game_epoch == p.epoch);
                                        PeerInfo { client_id:p.client_id, game_epoch:p.epoch, steam_id:p.steam_id, ready:p.steam_id != 0, is_self:p.client_id == network.client_id,
                                            status:previous.and_then(|p| p.status.clone()), status_updated:previous.and_then(|p| p.status_updated) }
                                    }).collect();
                                });
                                if let Some(h) = hook.as_mut() {
                                    let was_bound = h.bound;
                                    h.bound = peers.iter().any(|p| p.client_id == network.client_id && p.steam_id == h.steam_id && p.epoch == h.epoch);
                                    if h.bound && !was_bound { network.udp_bound = false; network.bind_udp().await?; }
                                    h.send(&Message::Members(peers.clone())).await?;
                                    if h.ready && h.bound && !h.acknowledged { h.send(&Message::IpcReady).await?; h.acknowledged = true; status(state, Phase::Ready, "游戏已接入 NetBurrow，可在游戏中邀请同组朋友"); }
                                    if !h.bound && h.acknowledged { return Err(io::Error::other("Relay unbound current game")); }
                                }
                            }
                            Some(NetworkEvent::Tcp(Message::Statuses(reports))) => {
                                status_supported = true;
                                change(state, |s| apply_statuses(&mut s.peers, &reports));
                            }
                            Some(NetworkEvent::Tcp(Message::Data(packet))) => deliver(packet, false, &peers, &mut hook, state).await?,
                            Some(NetworkEvent::Udp(packet)) => deliver(packet, true, &peers, &mut hook, state).await?,
                            Some(NetworkEvent::Tcp(Message::Pong(sequence))) => {
                                last_pong = Instant::now();
                                change(state, |s| {
                                    let ping = (since.elapsed().as_millis() as u64).saturating_sub(sequence);
                                    s.ping_ms = Some(ping);
                                    s.last_pong_at = Some(std::time::Instant::now());
                                    s.rtt_samples.push_back(ping);
                                    if s.rtt_samples.len() > 20 { s.rtt_samples.pop_front(); }
                                });
                            }
                            Some(NetworkEvent::Tcp(Message::Ping(value))) => network.send(&Message::Pong(value)).await?,
                            Some(NetworkEvent::UdpBound) => { network.udp_bound = true; log("INFO", "udp", "endpoint bound; unreliable packets may use UDP"); }
                            Some(NetworkEvent::Tcp(Message::Error(reason))) if matches!(reason.as_str(), "target connection is slow" | "target connection closed") => {
                                log("WARN", "relay peer disconnected", relay_reason(&reason));
                                // Members is authoritative for peer cleanup; these errors carry no target identity.
                                let phase = state.lock().unwrap_or_else(|p| p.into_inner()).phase;
                                status(state, phase, if reason == "target connection is slow" {
                                    "对方连接拥堵或接收不及时，已断开；本机仍连接 Relay，可等待对方重新加入。"
                                } else {
                                    "对方连接已关闭；本机仍连接 Relay，可等待对方重新加入。"
                                });
                            }
                            Some(NetworkEvent::Tcp(Message::Error(reason))) if reason == "status reports are limited to once per second" => {
                                log("WARN", "relay status throttled", relay_reason(&reason));
                            }
                            Some(NetworkEvent::Tcp(Message::Error(reason))) => {
                                log("ERROR", "relay rejected", relay_reason(&reason));
                                if !status_supported && reason == "message is not accepted from a client" {
                                    return Err(io::Error::new(io::ErrorKind::Unsupported, "Relay does not support member status; update Relay"));
                                }
                                status(state, Phase::Failed, if reason == "steam_id is already bound in this group" {
                                    "游戏身份重复：同组已有相同 Steam 身份。请退出重复的游戏或工具实例，再重开游戏并重新启用联机。"
                                } else {
                                    "服务器拒绝游戏数据：请查看排查日志中的具体原因，再重开游戏并重新启用联机。"
                                }); return Err(io::Error::new(io::ErrorKind::PermissionDenied, "Relay rejected state"));
                            }
                            Some(NetworkEvent::Closed) | None => return Err(io::Error::other("Relay disconnected")),
                            _ => return Err(io::Error::other("unexpected Relay message")),
                        }
                    }
                    incoming = listener.accept() => {
                        let (socket, address) = incoming?;
                        if !address.ip().is_loopback() || hook.is_some() { continue; }
                        if let Some(p) = pending.as_ref() {
                            let accepted = tokio::select! { _ = stop.changed() => return Ok(()), value = Hook::accept(socket, p) => value };
                            if let Ok(h) = accepted {
                                log("INFO", "ipc", "Hook hello identity verified; requesting Relay game binding");
                                network.send(&Message::Bind { steam_id: h.steam_id, epoch: h.epoch }).await?;
                                hook = Some(h);
                            } else if let Err(error) = accepted {
                                log("WARN", "ipc hello rejected", &error.to_string());
                            }
                        }
                    }
                    message = async { match hook.as_mut() { Some(h) => h.input.recv().await, None => std::future::pending().await } } => {
                        match message {
                            Some(Ok(Message::IpcReady)) => {
                                log("INFO", "hook ready", "received adapter readiness; awaiting/confirming Relay binding");
                                if let Some(h) = hook.as_mut() {
                                    h.ready = true;
                                    if h.bound && !h.acknowledged { h.send(&Message::IpcReady).await?; h.acknowledged = true; status(state, Phase::Ready, "游戏已接入 NetBurrow，可在游戏中邀请同组朋友"); }
                                }
                            }
                            Some(Ok(Message::Data(packet))) => {
                                if let Some(h) = hook.as_ref() {
                                    if h.acknowledged && packet.from == h.steam_id && packet.source_epoch == h.epoch && peers.iter().any(|p| p.steam_id == packet.to && p.epoch == packet.target_epoch && p.steam_id != 0) {
                                        let udp = network.packet(packet).await?;
                                        change(state, |s| { s.sent += 1; s.udp_sent += u64::from(udp); });
                                    }
                                }
                            }
                            Some(Ok(Message::Diagnostic(text))) => { log("INFO", "hook", &text); change(state, |s| { s.logs.push(text); if s.logs.len() > 64 { s.logs.remove(0); } }); },
                            Some(Ok(Message::Ping(value))) => { if let Some(h) = hook.as_mut() { h.send(&Message::Pong(value)).await?; } }
                            ended @ (Some(Ok(Message::Stop)) | Some(Err(_)) | None) => {
                                if let Some(Err(error)) = ended { log("ERROR", "ipc disconnected", &error.to_string()); }
                                else { log("INFO", "ipc disconnected", "Hook stopped or reader channel closed"); }
                                disconnect_hook(&mut hook, &mut pending, network).await;
                                status(state, Phase::RestartRequired, "游戏接入已断开，请退出游戏后重开");
                            }
                            _ => return Err(io::Error::other("unexpected Hook message")),
                        }
                    }
                    _ = clock.tick() => {
                        if last_report.elapsed() >= Duration::from_secs(3) {
                            let report = {
                                let s = state.lock().unwrap_or_else(|p| p.into_inner());
                                MemberStatus { name:settings.display_name.trim().to_owned(), phase:phase_code(s.phase),
                                    ping_ms:s.ping_ms.map(|ms| ms.min(u32::MAX as u64 - 1) as u32),
                                    transport:if settings.transport == Transport::Tcp { 0 } else if network.udp_bound { 2 } else { 1 },
                                    sent:s.sent, received:s.received }
                            };
                            network.send(&Message::Status(report)).await?;
                            last_report = Instant::now();
                        }
                        if last_stats.elapsed() >= Duration::from_secs(10) {
                            change(state, |s| log("INFO", "traffic", &format!("sent={} received={} udp_sent={} udp_received={} ping_ms={:?} udp_bound={}", s.sent, s.received, s.udp_sent, s.udp_received, s.ping_ms, network.udp_bound)));
                            last_stats = Instant::now();
                        }
                        if last_pong.elapsed() > Duration::from_secs(15) {
                            change(state, |s| s.heartbeat_timeouts += 1);
                            return Err(io::Error::new(io::ErrorKind::TimedOut, "Relay heartbeat timeout"));
                        }
                        if last_ping.elapsed() >= Duration::from_secs(1) {
                            network.send(&Message::Ping(since.elapsed().as_millis() as u64)).await?;
                            if settings.transport == Transport::Udp && !network.udp_bound { network.bind_udp().await?; }
                            last_ping = Instant::now();
                        }
                        // Confirm the Relay extension before attaching a game to an old server.
                        if !status_supported {
                            if since.elapsed() > Duration::from_secs(10) { return Err(io::Error::new(io::ErrorKind::Unsupported, "Relay member status handshake timed out")); }
                            continue;
                        }
                        let games = find_games(game_path)?;
                        seen.retain(|key| games.iter().any(|p| (p.pid,p.created) == *key));
                        if let Some(p) = pending.as_mut() {
                            if !games.iter().any(|g| g.pid == p.game.pid && g.created == p.game.created) {
                                disconnect_hook(&mut hook, &mut pending, network).await;
                                status(state, Phase::WaitingForGame, "游戏已退出，等待下次从 Steam 启动");
                                continue;
                            }
                            if let Some(child) = p.helper.as_mut() {
                                if let Some(exit) = child.try_wait()? {
                                    log("INFO", "helper exit", &format!("code={:?} elapsed_ms={}", exit.code(), p.since.elapsed().as_millis()));
                                    let mut child = p.helper.take().unwrap();
                                    if !exit.success() {
                                        let mut diagnostic = String::new();
                                        if let Some(stderr) = child.stderr.take() { let _ = stderr.take(2048).read_to_string(&mut diagnostic); }
                                        log("ERROR", "helper", &diagnostic);
                                        change(state, |s| { s.logs.push(diagnostic.trim().to_owned()); if s.logs.len()>64 {s.logs.remove(0);} });
                                        disconnect_hook(&mut hook, &mut pending, network).await;
                                        status(state, Phase::RestartRequired, "游戏接入失败：Hook 加载失败。请完整解压工具、检查安全软件是否拦截文件，并查看日志；处理后退出游戏，从 Steam 重开。");
                                        continue;
                                    }
                                }
                            }
                            if p.since.elapsed() > Duration::from_secs(35) && !hook.as_ref().is_some_and(|h| h.acknowledged) {
                                disconnect_hook(&mut hook, &mut pending, network).await;
                                status(state, Phase::RestartRequired, "游戏接入超时：请确认运行的是受支持的忏悔+版本，查看日志后退出游戏并从 Steam 重开。");
                            }
                        } else if games.len() > 1 {
                            status(state, Phase::RestartRequired, "发现多个游戏进程，请只保留一个并重开");
                        } else if let Some(game) = games.into_iter().next() {
                            let key = (game.pid, game.created);
                            if !seen.contains(&key) {
                                seen.insert(key);
                                match launch(game, directory, port) {
                                    Ok(value) => { pending = Some(value); status(state, Phase::Attaching, "已发现游戏进程，正在加载自己的 Hook…"); }
                                    Err(error) => { log("ERROR", "helper launch", &error.to_string()); status(state, Phase::RestartRequired, "无法启动 Hook helper，请检查文件权限并重开游戏"); },
                                }
                            }
                        } else if seen.is_empty() && hook.is_none() {
                            status(state, Phase::WaitingForGame, "Relay 已连接，请从 Steam 正常启动游戏");
                        }
                    }
                }
            }
        }.await;
        disconnect_hook(&mut hook, &mut pending, network).await;
        result
    }

    fn phase_code(phase: Phase) -> u8 {
        match phase {
            Phase::Connecting => 0,
            Phase::WaitingForGame => 1,
            Phase::Attaching => 2,
            Phase::Ready => 3,
            Phase::RestartRequired => 4,
            Phase::Failed => 5,
            Phase::Stopped => 6,
        }
    }

    fn apply_statuses(peers: &mut [PeerInfo], reports: &[PeerStatus]) {
        let now = std::time::Instant::now();
        for peer in peers {
            let report = reports
                .iter()
                .find(|r| r.client_id == peer.client_id && r.game_epoch == peer.game_epoch);
            peer.status = report.map(|r| r.status.clone());
            peer.status_updated =
                report.and_then(|r| now.checked_sub(Duration::from_millis(r.age_ms as u64)));
        }
    }

    fn relay_reason(reason: &str) -> &str {
        match reason {
            "steam_id and epoch must both be zero or nonzero"
            | "steam_id is already bound in this group"
            | "connection is not active"
            | "packet source, target, or epoch is invalid"
            | "UDP endpoint is not bound"
            | "target connection is slow"
            | "target connection closed"
            | "invalid TCP data"
            | "invalid UDP data"
            | "status reports are limited to once per second"
            | "Join is only valid during handshake"
            | "message is not accepted from a client" => reason,
            _ => "unrecognized server error (text omitted)",
        }
    }

    async fn deliver(
        packet: Packet,
        udp: bool,
        peers: &[Peer],
        hook: &mut Option<Hook>,
        state: &Shared,
    ) -> io::Result<()> {
        if let Some(h) = hook.as_mut() {
            if h.acknowledged
                && packet.to == h.steam_id
                && packet.target_epoch == h.epoch
                && peers.iter().any(|p| {
                    p.steam_id == packet.from && p.epoch == packet.source_epoch && p.steam_id != 0
                })
            {
                h.send(&Message::Data(packet)).await?;
                change(state, |s| {
                    s.received += 1;
                    s.udp_received += u64::from(udp);
                });
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use netburrow_relay::{Config, spawn};
        use tokio::time::{Duration, timeout};

        #[tokio::test]
        async fn preflight_probes_existing_protocol_and_rejects_unsupported_server() {
            let relay = spawn(Config { bind: "127.0.0.1:0".parse().unwrap(), ..Config::default() }).await.unwrap();
            let mut settings = Settings { server: relay.local_addr().to_string(), group: crate::new_group().unwrap(), ..Settings::default() };
            timeout(Duration::from_secs(3), probe(&settings)).await.unwrap().unwrap();
            relay.shutdown().await.unwrap();
            let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
            settings.server = server.local_addr().unwrap().to_string();
            let task = tokio::spawn(async move {
                let (mut socket, _) = server.accept().await.unwrap();
                assert!(matches!(read(&mut socket).await.unwrap(), Message::Join { .. }));
                write(&mut socket, &Message::Welcome { client_id: 1, udp_token: [8; 16] }).await.unwrap();
                assert!(matches!(read(&mut socket).await.unwrap(), Message::Status(_)));
                write(&mut socket, &Message::Error("message is not accepted from a client".into())).await.unwrap();
                assert!(matches!(read(&mut socket).await.unwrap(), Message::Leave));
            });
            assert!(timeout(Duration::from_secs(3), probe(&settings)).await.unwrap().unwrap_err().contains("状态检查"));
            task.await.unwrap();
        }

        #[tokio::test]
        async fn handshake_errors_have_distinct_safe_guidance() {
            for (reply, expected, hint) in [
                (Message::Error("relay is full".into()), io::ErrorKind::WouldBlock, "服务器已满"),
                (Message::Error("untrusted secret text".into()), io::ErrorKind::PermissionDenied, "服务器拒绝加入"),
                (Message::Pong(0), io::ErrorKind::InvalidData, "服务器协议不匹配"),
            ] {
                let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let endpoint = server.local_addr().unwrap().to_string();
                let task = tokio::spawn(async move {
                    let (mut socket, _) = server.accept().await.unwrap();
                    let _ = read(&mut socket).await.unwrap();
                    write(&mut socket, &reply).await.unwrap();
                });
                let error = Network::connect(&endpoint, [5; 32], Transport::Tcp).await.err().unwrap();
                assert_eq!(error.kind(), expected);
                assert!(connection_hint(&error).contains(hint));
                assert!(!error.to_string().contains("untrusted secret text"));
                task.await.unwrap();
            }
            let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = server.local_addr().unwrap().to_string();
            drop(server);
            let error = Network::connect(&endpoint, [5; 32], Transport::Tcp).await.err().unwrap();
            assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
            assert!(connection_hint(&error).contains("服务器拒绝连接"));
        }

        #[test]
        fn member_status_ages_and_cannot_reuse_an_old_game_epoch() {
            let mut peers = vec![PeerInfo {
                client_id: 2,
                game_epoch: 20,
                steam_id: 202,
                ready: true,
                is_self: false,
                status: None,
                status_updated: None,
            }];
            let mut report = PeerStatus {
                client_id: 2,
                game_epoch: 20,
                age_ms: 11_000,
                status: MemberStatus {
                    name: "朋友".into(),
                    phase: 3,
                    ping_ms: Some(42),
                    transport: 0,
                    sent: 8,
                    received: 9,
                },
            };
            apply_statuses(&mut peers, &[report.clone()]);
            assert_eq!(peers[0].status.as_ref().unwrap().ping_ms, Some(42));
            assert!(peers[0].status_is_stale());
            report.age_ms = 0;
            apply_statuses(&mut peers, &[report.clone()]);
            assert!(!peers[0].status_is_stale());
            peers[0].game_epoch = 21;
            apply_statuses(&mut peers, &[report]);
            assert!(peers[0].status.is_none());
        }

        fn packet(
            from: u64,
            to: u64,
            source_epoch: u64,
            target_epoch: u64,
            send_type: u8,
        ) -> Packet {
            Packet {
                from,
                to,
                source_epoch,
                target_epoch,
                channel: 0,
                send_type,
                payload: vec![7, 8, 9],
            }
        }

        async fn wait_udp_bound(network: &mut Network) {
            timeout(Duration::from_secs(2), async {
                loop {
                    if matches!(network.events.recv().await, Some(NetworkEvent::UdpBound)) {
                        network.udp_bound = true;
                        return;
                    }
                }
            })
            .await
            .expect("Relay did not bind UDP");
        }

        async fn wait_member(network: &mut Network, steam_id: u64, epoch: u64) {
            timeout(Duration::from_secs(2), async {
                loop {
                    if let Some(NetworkEvent::Tcp(Message::Members(value))) =
                        network.events.recv().await
                        && value
                            .iter()
                            .any(|peer| peer.steam_id == steam_id && peer.epoch == epoch)
                    {
                        return;
                    }
                }
            })
            .await
            .expect("Relay did not publish bound member");
        }

        async fn wait_statuses(network: &mut Network, count: usize) -> Vec<PeerStatus> {
            timeout(Duration::from_secs(2), async {
                loop {
                    if let Some(NetworkEvent::Tcp(Message::Statuses(reports))) =
                        network.events.recv().await
                    {
                        if reports.len() == count {
                            return reports;
                        }
                    }
                }
            })
            .await
            .expect("member statuses did not arrive")
        }

        #[tokio::test]
        async fn nonfatal_errors_keep_relay_session_alive() {
            for (reason, hint) in [
                ("target connection is slow", "对方连接拥堵"),
                ("target connection closed", "对方连接已关闭"),
                ("status reports are limited to once per second", ""),
            ] {
                let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let endpoint = server.local_addr().unwrap().to_string();
                let (stop, mut stopped) = watch::channel(false);
                let task = tokio::spawn(async move {
                    let (mut socket, _) = server.accept().await.unwrap();
                    assert!(matches!(read(&mut socket).await.unwrap(), Message::Join { .. }));
                    write(&mut socket, &Message::Welcome {
                        client_id: 1, udp_token: [8; 16],
                    }).await.unwrap();
                    write(&mut socket, &Message::Error(reason.into())).await.unwrap();
                    write(&mut socket, &Message::Ping(12345)).await.unwrap();
                    loop {
                        match read(&mut socket).await.unwrap() {
                            Message::Pong(12345) => break,
                            Message::Ping(n) => write(&mut socket, &Message::Pong(n)).await.unwrap(),
                            Message::Status(_) | Message::Bind { .. } => {},
                            other => panic!("unexpected message after peer error: {other:?}"),
                        }
                    }
                    stop.send(true).unwrap();
                    // Keep the socket alive until the client's normal shutdown.
                    let _ = read(&mut socket).await;
                });
                let mut network = Network::connect(&endpoint, [5; 32], Transport::Tcp).await.unwrap();
                let ipc = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let state = Arc::new(Mutex::new(Snapshot {
                    phase: Phase::Ready,
                    ..Snapshot::default()
                }));
                let settings = Settings { allow_late_hook: true, ..Settings::default() };
                let result = timeout(Duration::from_secs(2), connected(
                    &settings, Path::new("missing-binaries"), Path::new("missing-game"),
                    &ipc, ipc.local_addr().unwrap().port(), &mut HashSet::new(),
                    &mut network, &mut stopped, &state,
                )).await.expect("client stopped responding after peer error");
                assert!(result.is_ok(), "peer error terminated session: {result:?}");
                let snapshot = state.lock().unwrap().clone();
                assert_eq!(snapshot.phase, Phase::Ready);
                assert!(snapshot.detail.contains(hint));
                if reason == "status reports are limited to once per second" {
                    assert!(snapshot.detail.is_empty());
                    assert_eq!(relay_reason(reason), reason);
                }
                assert!(!snapshot.detail.contains("版本"));
                drop(network);
                task.await.unwrap();
            }
        }

        #[tokio::test]
        async fn old_relay_is_rejected_before_process_detection_or_injection() {
            let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = server.local_addr().unwrap().to_string();
            let task = tokio::spawn(async move {
                let (mut socket, _) = server.accept().await.unwrap();
                assert!(matches!(
                    read(&mut socket).await.unwrap(),
                    Message::Join { .. }
                ));
                write(
                    &mut socket,
                    &Message::Welcome {
                        client_id: 1,
                        udp_token: [8; 16],
                    },
                )
                .await
                .unwrap();
                loop {
                    let Ok(message) = read(&mut socket).await else {
                        break;
                    };
                    match message {
                        Message::Status(_) => {
                            write(
                                &mut socket,
                                &Message::Error("message is not accepted from a client".into()),
                            )
                            .await
                            .unwrap();
                        }
                        Message::Ping(n) => write(&mut socket, &Message::Pong(n)).await.unwrap(),
                        Message::Bind { .. } => {}
                        Message::Leave => break,
                        _ => panic!("unexpected request to old Relay"),
                    }
                }
            });
            let mut network = Network::connect(&endpoint, [5; 32], Transport::Tcp)
                .await
                .unwrap();
            let ipc = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (_stop, mut stopped) = watch::channel(false);
            let state = Arc::new(Mutex::new(Snapshot::default()));
            let settings = Settings {
                allow_late_hook: true,
                ..Settings::default()
            };
            let result = timeout(
                Duration::from_secs(2),
                connected(
                    &settings,
                    Path::new("missing-binaries"),
                    Path::new("missing-game"),
                    &ipc,
                    ipc.local_addr().unwrap().port(),
                    &mut HashSet::new(),
                    &mut network,
                    &mut stopped,
                    &state,
                ),
            )
            .await
            .unwrap();
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Unsupported);
            drop(network);
            task.await.unwrap();
        }

        async fn wait_packet(network: &mut Network, udp: bool) -> Packet {
            timeout(Duration::from_secs(2), async {
                loop {
                    match network.events.recv().await {
                        Some(NetworkEvent::Udp(value)) if udp => return value,
                        Some(NetworkEvent::Tcp(Message::Data(value))) if !udp => return value,
                        Some(_) => continue,
                        None => panic!("Relay connection closed"),
                    }
                }
            })
            .await
            .expect("Relay did not forward packet")
        }

        #[tokio::test]
        async fn loopback_relay_keeps_reliable_packets_on_tcp_and_releases_old_connection() {
            let relay = spawn(Config {
                bind: "127.0.0.1:0".parse().unwrap(),
                ..Config::default()
            })
            .await
            .unwrap();
            let group = [5; 32];
            let address = relay.local_addr().to_string();
            let mut one = Network::connect(&address, group, Transport::Udp)
                .await
                .unwrap();
            let mut two = Network::connect(&address, group, Transport::Udp)
                .await
                .unwrap();
            one.bind_udp().await.unwrap();
            wait_udp_bound(&mut one).await;
            assert!(one.udp_bound);
            one.send(&Message::Bind {
                steam_id: 101,
                epoch: 1001,
            })
            .await
            .unwrap();
            assert!(
                !one.udp_bound,
                "new game binding invalidates the previous UDP qualification"
            );
            two.send(&Message::Bind {
                steam_id: 202,
                epoch: 2002,
            })
            .await
            .unwrap();
            wait_member(&mut one, 202, 2002).await;
            wait_member(&mut two, 101, 1001).await;
            one.bind_udp().await.unwrap();
            two.bind_udp().await.unwrap();
            wait_udp_bound(&mut one).await;
            wait_udp_bound(&mut two).await;

            for (network, name, ping) in [(&mut one, "甲", 27), (&mut two, "乙", 52)] {
                network
                    .send(&Message::Status(MemberStatus {
                        name: name.into(),
                        phase: 3,
                        ping_ms: Some(ping),
                        transport: 2,
                        sent: 20,
                        received: 18,
                    }))
                    .await
                    .unwrap();
            }
            let reports_one = wait_statuses(&mut one, 2).await;
            let reports_two = wait_statuses(&mut two, 2).await;
            assert_eq!(
                reports_one
                    .iter()
                    .map(|p| (p.client_id, &p.status))
                    .collect::<Vec<_>>(),
                reports_two
                    .iter()
                    .map(|p| (p.client_id, &p.status))
                    .collect::<Vec<_>>()
            );
            assert_eq!(reports_one[0].status.ping_ms, Some(27));
            assert_eq!(reports_one[1].status.ping_ms, Some(52));
            assert!(reports_one.iter().all(|r| r.status.transport == 2));

            let unreliable = packet(101, 202, 1001, 2002, 0);
            assert!(one.packet(unreliable.clone()).await.unwrap());
            assert_eq!(wait_packet(&mut two, true).await, unreliable);

            let reliable = packet(101, 202, 1001, 2002, 2);
            assert!(!one.packet(reliable.clone()).await.unwrap());
            assert_eq!(wait_packet(&mut two, false).await, reliable);

            drop(one);
            tokio::time::sleep(Duration::from_millis(80)).await;
            let mut restarted = Network::connect(&address, group, Transport::Tcp)
                .await
                .unwrap();
            restarted
                .send(&Message::Bind {
                    steam_id: 101,
                    epoch: 3003,
                })
                .await
                .unwrap();
            let members = timeout(Duration::from_secs(2), async {
                loop {
                    if let Some(NetworkEvent::Tcp(Message::Members(value))) =
                        restarted.events.recv().await
                    {
                        if value
                            .iter()
                            .any(|peer| peer.steam_id == 101 && peer.epoch == 3003)
                        {
                            return value;
                        }
                    }
                }
            })
            .await
            .expect("restarted client did not receive members");
            assert!(
                !members
                    .iter()
                    .any(|peer| peer.steam_id == 101 && peer.epoch == 1001)
            );
            relay.shutdown().await.unwrap();
        }
    }
}
