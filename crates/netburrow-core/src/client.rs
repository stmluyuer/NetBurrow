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
    pub relay_recovering: bool,
    pub relay_recoveries: u64,
    pub ipc_recoveries: u64,
    pub path_diagnostics: crate::PathDiagnostics,
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
    pub hook_health: Option<netburrow_protocol::HookHealth>,
    pub ipc_slow: bool,
    pub process_unknown: bool,
    pub peer_faults: u64,
    pub logs: Vec<String>,
}

type Shared = Arc<Mutex<Snapshot>>;

/// Local per-peer stages, mapped to the Relay's temporary member number, never Steam IDs.
pub(crate) fn peer_diagnostic_lines(snapshot: &Snapshot) -> Vec<String> {
    let Some(health) = &snapshot.hook_health else { return Vec::new(); };
    health.peers.iter().filter_map(|h| {
        let member = snapshot.peers.iter().find(|p| !p.is_self && p.steam_id == h.peer && p.game_epoch == h.epoch)?;
        Some(format!("member={} hook_send_calls={} hook_send_rejected={} hook_received={} game_consumed={} dropped={} discarded={} queued_packets={} queued_bytes={} oldest_ms={} outgoing_packets={} failed={}",
            member.client_id,h.send_calls,h.send_rejected,h.received,h.consumed,h.dropped,h.discarded,h.queued_packets,h.queued_bytes,h.oldest_ms,h.outgoing_packets,h.failed))
    }).collect()
}

pub(crate) fn probe_relay(settings: &Settings) -> Result<(), String> {
    #[cfg(windows)]
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("无法启动自检：{error}"))?;
        runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(8), runtime::probe(settings))
                .await
                .map_err(|_| "服务器自检超时，请检查网络、地址和端口后重试。".to_owned())?
        })
    }
    #[cfg(not(windows))]
    {
        let _ = settings;
        Err("客户端自检需要 Windows".into())
    }
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
    use crate::client_io::{Budget, Mailbox, Writer as SocketWriter, reader_resumable as socket_reader};
    use crate::diagnostics::record as log;
    use crate::{
        Transport,
        process::{GameProcess, ProcessMonitor, ProcessState, find_games},
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
        net::{TcpListener, TcpStream, UdpSocket},
        sync::watch,
        task::JoinHandle as Task,
        time::{Instant, interval, timeout},
    };

    const IO_TIMEOUT: Duration = Duration::from_secs(5);
    use netburrow_protocol::{
        IPC_CAPABILITIES,
        local::{IPC_TIMEOUT, IPC_WARNING},
    };
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    const GROUP_NOT_ALLOWED_HINT: &str =
        "该组码未获服务器授权，请联系管理员添加，或更换已授权组码后重新启用联机。";

    #[derive(Debug)]
    struct GroupNotAllowed;

    impl std::fmt::Display for GroupNotAllowed {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Relay group is not allowed")
        }
    }

    impl std::error::Error for GroupNotAllowed {}

    fn group_not_allowed(error: &io::Error) -> bool {
        error
            .get_ref()
            .is_some_and(|cause| cause.is::<GroupNotAllowed>())
    }

    pub(super) async fn probe(settings: &Settings) -> Result<(), String> {
        let group = parse_group(&settings.group)?;
        let mut network = Network::connect(settings.server.trim(), group, Transport::Tcp)
            .await
            .map_err(|error| {
                connection_hint(&error)
                    .split('；')
                    .next()
                    .unwrap_or("服务器连接失败")
                    .to_owned()
            })?;
        network
            .send(&Message::Status(MemberStatus {
                name: String::new(),
                phase: 0,
                ping_ms: None,
                transport: 0,
                sent: 0,
                received: 0,
            }))
            .await
            .map_err(|_| "服务器状态检查发送失败，请稍后重试。".to_owned())?;
        let result = loop {
            match network.events.recv().await {
                Some(NetworkEvent::Tcp(Message::Statuses(_))) => break Ok(()),
                Some(NetworkEvent::Tcp(Message::Members(_))) => {}
                Some(NetworkEvent::Tcp(Message::Ping(value))) => {
                    if network.send(&Message::Pong(value)).await.is_err() {
                        break Err("服务器连接中断，请重试。".into());
                    }
                }
                Some(NetworkEvent::Tcp(Message::Error(_))) => {
                    break Err(
                        "服务器拒绝状态检查，请确认连接的是支持成员状态的 NetBurrow Relay。".into(),
                    );
                }
                _ => break Err("服务器握手或协议检查失败，请检查地址和服务状态。".into()),
            }
        };
        let _ = network.send(&Message::Leave).await;
        let _ = network.writer.drain().await;
        result
    }

    fn connection_hint(error: &io::Error) -> &'static str {
        if group_not_allowed(error) {
            return GROUP_NOT_ALLOWED_HINT;
        }
        match error.kind() {
            io::ErrorKind::TimedOut => "连接服务器超时，请检查地址、网络和防火墙；3 秒后重试。",
            io::ErrorKind::ConnectionRefused => {
                "服务器拒绝连接，请检查服务是否启动、端口是否开放；3 秒后重试。"
            }
            io::ErrorKind::WouldBlock => "服务器已满，请稍后再试；3 秒后重试。",
            io::ErrorKind::PermissionDenied => {
                "服务器拒绝加入，请核对服务器地址和版本；3 秒后重试。"
            }
            io::ErrorKind::InvalidData => "服务器协议不匹配，请核对端口和版本；3 秒后重试。",
            _ => "无法连接服务器，请检查地址、网络和服务状态；3 秒后重试。",
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
        Fault(io::Error),
    }
    struct NetworkEvents {
        tcp: Mailbox,
        udp: Mailbox,
    }
    impl NetworkEvents {
        async fn recv(&mut self) -> Option<NetworkEvent> {
            tokio::select! {
                value=self.tcp.recv()=>Some(match value {Some(Ok(m))=>NetworkEvent::Tcp(m),Some(Err(e))=>NetworkEvent::Fault(e),_=>NetworkEvent::Closed}),
                value=self.udp.recv()=>Some(match value {Some(Ok(Message::Data(p)))=>NetworkEvent::Udp(p),Some(Ok(Message::IpcReady))=>NetworkEvent::UdpBound,Some(Ok(m))=>NetworkEvent::Tcp(m),_=>NetworkEvent::Closed}),
            }
        }
    }
    struct Network {
        resume_key: Option<Token>,
        recovering: bool,
        diagnostics: crate::path_diagnostics::Tracker,
        writer: SocketWriter,
        events: NetworkEvents,
        tasks: Vec<Task<()>>,
        udp: Option<Arc<UdpSocket>>,
        client_id: u64,
        token: Token,
        udp_bound: bool,
        udp_written: u64,
        incoming_budget: Budget,
        outgoing_budget: Budget,
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
                Message::Welcome {
                    client_id,
                    udp_token,
                } => (client_id, udp_token),
                Message::Error(reason) if reason == "relay is full" => {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, "Relay is full"));
                }
                Message::Error(reason) if reason == "group is not allowed" => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        GroupNotAllowed,
                    ));
                }
                Message::Error(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "Relay rejected join",
                    ));
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Unexpected Relay handshake",
                    ));
                }
            };
            let incoming_budget = Budget::default();
            let outgoing_budget = Budget::default();
            let events = NetworkEvents {
                tcp: Mailbox::with_budget(true, incoming_budget.clone()),
                udp: Mailbox::with_budget(true, incoming_budget.clone()),
            };
            let writer=SocketWriter::with_budget(writer,false,IO_TIMEOUT,outgoing_budget.clone());
            let reader = socket_reader(input, events.tcp.clone(), false,writer.session.clone(),writer.queue.clone());
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
                let udp_events = events.udp.clone();
                tasks.push(tokio::spawn(async move {
                    let mut buffer = [0; netburrow_protocol::UDP_LIMIT + 1];
                    while let Ok(count) = udp_input.recv(&mut buffer).await {
                        let event = match decode_datagram(&buffer[..count]) {
                            Ok(Datagram::Bound { client_id: id }) if id == client_id => {
                                Message::IpcReady
                            }
                            Ok(Datagram::Data {
                                client_id: id,
                                token: key,
                                packet,
                            }) if id == client_id && key == token => Message::Data(packet),
                            _ => continue,
                        };
                        if udp_events.post(event).is_err() {
                            break;
                        }
                    }
                }));
                Some(socket)
            } else {
                None
            };
            Ok(Self {
                resume_key:None,recovering:false,
                diagnostics: Default::default(),
                writer,
                events,
                tasks,
                udp,
                client_id,
                token,
                udp_bound: false,
                udp_written: 0,
                incoming_budget,
                outgoing_budget,
            })
        }
        async fn send(&mut self, message: &Message) -> io::Result<()> {
            if self.recovering && !netburrow_protocol::replayable(message) {return Ok(());}
            if matches!(message, Message::Bind { .. }) {
                self.udp_bound = false;
            }
            self.writer.send(message.clone()).map(|_| ())
        }
        async fn bind_udp(&self) -> io::Result<()> {
            if let Some(socket) = &self.udp {
                let bytes = encode_datagram(&Datagram::Bind {
                    client_id: self.client_id,
                    token: self.token,
                })?;
                match socket.try_send(&bytes) {
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }
        async fn packet(&mut self, mut packet: Packet) -> io::Result<bool> {
            if self.recovering && packet.send_type<=1 {return Ok(false);}
            self.diagnostics.assign(&mut packet);
            if self.udp_bound && packet.send_type <= 1 {
                if let Some(socket) = &self.udp {
                    if let Ok(bytes) = encode_datagram(&Datagram::Data {
                        client_id: self.client_id,
                        token: self.token,
                        packet: packet.clone(),
                    }) {
                        match socket.try_send(&bytes) {
                            Ok(_) => self.udp_written += 1,
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                            Err(e) => return Err(e),
                        }
                        return Ok(true);
                    }
                }
            }
            self.send(&Message::Data(packet)).await?;
            Ok(false)
        }
    }

    fn recoverable(error:&io::Error)->bool {
        !matches!(error.kind(),io::ErrorKind::InvalidData|io::ErrorKind::InvalidInput|io::ErrorKind::PermissionDenied|io::ErrorKind::Unsupported)
    }
    impl Network {
        async fn begin_resume(&mut self,server:String,mut stop:watch::Receiver<bool>)->io::Result<Task<io::Result<(TcpStream,u64)>>> {
            let key=self.resume_key.ok_or_else(||io::Error::new(io::ErrorKind::ConnectionReset,"Relay disconnected; original session recovery unavailable"))?;
            self.recovering=true;self.udp_bound=false;
            for task in &self.tasks {task.abort();}
            self.writer.suspend().await;
            // No old reader may advance the receive watermark after the handshake snapshot.
            for task in &mut self.tasks {let _ = task.await;}
            // Old UDP binding replies carry no generation. Resume on TCP until next enable.
            self.udp=None;
            self.tasks.truncate(1);
            let received=self.writer.session.received();let client_id=self.client_id;
            Ok(tokio::spawn(async move {
                let deadline=Instant::now()+netburrow_protocol::RECOVERY_TIMEOUT;
                loop {
                    if *stop.borrow(){return Err(io::Error::new(io::ErrorKind::Interrupted,"recovery stopped"));}
                    if Instant::now()>=deadline{return Err(io::Error::new(io::ErrorKind::TimedOut,"Relay session recovery exhausted"));}
                    let attempt=async {
                        let mut socket=TcpStream::connect(&server).await?;socket.set_nodelay(true)?;
                        write(&mut socket,&Message::Resume{client_id,key,received}).await?;
                        match read(&mut socket).await? {
                            Message::Resumed {client_id:id,received} if id==client_id=>Ok((socket,received)),
                            Message::Error(reason) if reason=="session resume pending"=>Err(io::Error::new(io::ErrorKind::WouldBlock,"waiting for old connection to detach")),
                            _=>Err(io::Error::new(io::ErrorKind::PermissionDenied,"Relay rejected original session recovery")),
                        }
                    };
                    let attempt_limit=Duration::from_secs(5).min(deadline.saturating_duration_since(Instant::now()));
                    let result=tokio::select!{_=stop.changed()=>return Err(io::Error::new(io::ErrorKind::Interrupted,"recovery stopped")),r=timeout(attempt_limit,attempt)=>r};
                    match result {Ok(Ok(result))=>return Ok(result),Ok(Err(e)) if !recoverable(&e)=>return Err(e),_=>{}}
                    if Instant::now()>=deadline {return Err(io::Error::new(io::ErrorKind::TimedOut,"Relay session recovery exhausted"));}
                    tokio::select!{_=stop.changed()=>return Err(io::Error::new(io::ErrorKind::Interrupted,"recovery stopped")),_=tokio::time::sleep(Duration::from_millis(250))=>{}}
                }
            }))
        }
        fn finish_resume(&mut self,socket:TcpStream,received:u64)->io::Result<()> {
            self.writer.session.acknowledge(received)?;
            let(input,output)=socket.into_split();self.events.tcp.reopen();self.writer.rebind(output);
            self.tasks[0]=socket_reader(input,self.events.tcp.clone(),false,self.writer.session.clone(),self.writer.queue.clone());
            self.recovering=false;Ok(())
        }
    }

    struct Launch {
        game: GameProcess,
        epoch: u64,
        nonce: Token,
        since: Instant,
        helper: Option<Child>,
        monitor: watch::Receiver<ProcessState>,
        monitor_task: Task<()>,
    }
    impl Drop for Launch {
        fn drop(&mut self) {
            self.monitor_task.abort();
            if let Some(mut child) = self.helper.take() {
                if child.try_wait().ok().flatten().is_none() {
                    let _ = child.kill();
                }
                let _ = child.wait();
            }
        }
    }
    struct Hook {
        pid:u32,
        nonce:Token,
        recovering_since:Option<Instant>,
        writer: SocketWriter,
        input: Mailbox,
        reader: Task<()>,
        steam_id: u64,
        epoch: u64,
        ready: bool,
        bound: bool,
        acknowledged: bool,
        received_base: u64,
        udp_received_base: u64,
        failed: HashSet<(u64, u64)>,
    }
    impl Drop for Hook {
        fn drop(&mut self) {
            self.reader.abort();
        }
    }
    impl Hook {
        async fn begin_resume(&mut self) {
            if self.recovering_since.is_none(){
                self.recovering_since=Some(Instant::now());
                self.reader.abort();
                self.writer.suspend().await;
                let _ = (&mut self.reader).await;
            }
        }
        async fn resume_candidate(mut socket:TcpStream,pid:u32,nonce:Token,steam_id:u64,epoch:u64,session:crate::client_io::Session)->io::Result<TcpStream> {
            timeout(Duration::from_millis(500),async {
                match read(&mut socket).await? {
                    Message::IpcResume{nonce:n,pid:p,steam_id:s,epoch:e,received} if n==nonce&&p==pid&&s==steam_id&&e==epoch=>session.acknowledge(received)?,
                    _=>return Err(io::Error::new(io::ErrorKind::PermissionDenied,"IPC resume identity mismatch")),
                }
                write(&mut socket,&Message::SessionAck(session.received())).await?;
                Ok(socket)
            }).await.map_err(|_|io::Error::new(io::ErrorKind::TimedOut,"IPC resume handshake timeout"))?
        }
        fn finish_resume(&mut self,socket:TcpStream) {
            let(input,output)=socket.into_split();self.input.reopen();self.writer.rebind(output);
            self.reader=socket_reader(input,self.input.clone(),true,self.writer.session.clone(),self.writer.queue.clone());
            self.recovering_since=None;
        }
        async fn accept(
            mut socket: TcpStream,
            expected_pid: u32,
            expected_nonce: Token,
            expected_epoch: u64,
            incoming_budget: Budget,
            outgoing_budget: Budget,
        ) -> io::Result<Self> {
            socket.set_nodelay(true)?;
            let hello = timeout(IO_TIMEOUT, read(&mut socket))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "IPC hello timeout"))??;
            let old_known = matches!(&hello,Message::IpcHello {nonce,pid,steam_id,epoch} if *nonce==expected_nonce && *pid==expected_pid && *steam_id!=0 && *epoch==expected_epoch);
            let unsupported_known = matches!(&hello,Message::IpcHelloV2 {nonce,pid,steam_id,epoch,capabilities} if *nonce==expected_nonce && *pid==expected_pid && *steam_id!=0 && *epoch==expected_epoch && *capabilities!=IPC_CAPABILITIES);
            if old_known || unsupported_known {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Hook capabilities require a complete package update",
                ));
            }
            if !matches!(hello, Message::IpcHelloV2 { nonce, pid, steam_id, epoch, capabilities } if nonce == expected_nonce && pid == expected_pid && steam_id != 0 && epoch == expected_epoch && capabilities==IPC_CAPABILITIES)
            {
                return Err(io::Error::other(
                    "IPC identity/capability mismatch; update the complete client package and restart the game",
                ));
            }
            let Message::IpcHelloV2 {
                steam_id, epoch, ..
            } = hello
            else {
                unreachable!()
            };
            write(&mut socket, &Message::IpcAccepted(IPC_CAPABILITIES)).await?;
            let (input, writer) = socket.into_split();
            let rx = Mailbox::with_budget(false, outgoing_budget);
            let writer=SocketWriter::with_budget(writer,true,IPC_TIMEOUT,incoming_budget);
            writer.session.enable();
            let reader = socket_reader(input, rx.clone(), true,writer.session.clone(),writer.queue.clone());
            Ok(Self {
                writer,
                input: rx,
                pid:expected_pid,nonce:expected_nonce,recovering_since:None,
                reader,
                steam_id,
                epoch,
                ready: false,
                bound: false,
                acknowledged: false,
                received_base: 0,
                udp_received_base: 0,
                failed: HashSet::new(),
            })
        }
        async fn send(&mut self, message: &Message) -> io::Result<()> {
            if let Err(error) = self.writer.send(message.clone()) {
                self.writer.report_failure(error);
            }
            Ok(()) // Tick handles an IPC failure without tearing down the Relay connection.
        }
        fn fail_peer(&mut self, peer: u64, epoch: u64) -> io::Result<()> {
            if self.failed.insert((peer, epoch)) {
                self.writer.queue.fail_peer(peer, epoch);
                self.input.fail_peer(peer, epoch);
                if let Err(error) = self.writer.send(Message::IpcPeerFault { peer, epoch }) {
                    self.writer.report_failure(error);
                }
            }
            Ok(())
        }
    }

    fn launch(game: GameProcess, directory: &Path, port: u16) -> io::Result<Launch> {
        let process_monitor = ProcessMonitor::open(&game)?;
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
        let (observed, monitor) = watch::channel(process_monitor.state());
        let monitor_task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let state = process_monitor.state();
                if observed.send(state).is_err() || state == ProcessState::Exited {
                    break;
                }
            }
        });
        Ok(Launch {
            game,
            epoch,
            nonce,
            since: Instant::now(),
            helper: Some(child),
            monitor,
            monitor_task,
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
                    if group_not_allowed(&error) {
                        change(&state, |s| {
                            s.peers.clear();
                            s.ping_ms = None;
                            s.rtt_samples.clear();
                            s.last_pong_at = None;
                        });
                        let detail = if seen.is_empty() {
                            GROUP_NOT_ALLOWED_HINT.to_owned()
                        } else {
                            format!("{GROUP_NOT_ALLOWED_HINT} 已运行的游戏请退出后重开。")
                        };
                        status(&state, Phase::Failed, detail);
                        return;
                    }
                    status(&state, Phase::Connecting, connection_hint(&error));
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
                s.path_diagnostics = Default::default();
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
            let _ = network.writer.drain().await;
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
            if result.as_ref().is_err_and(|error| {
                error.kind() == io::ErrorKind::Unsupported
                    || (error.kind() == io::ErrorKind::PermissionDenied
                        && state.lock().unwrap_or_else(|p| p.into_inner()).phase == Phase::Failed)
            }) {
                change(&state, |s| {
                    s.peers.clear();
                    s.ping_ms = None;
                });
                if result
                    .as_ref()
                    .is_err_and(|error| error.kind() == io::ErrorKind::Unsupported)
                {
                    status(
                        &state,
                        Phase::Failed,
                        "服务器版本不兼容：Relay 尚不支持成员状态，请更新服务器后重新启用联机。",
                    );
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
        let mut last_stats = Instant::now();
        let mut last_report = Instant::now() - Duration::from_secs(3);
        let mut status_supported = false;
        let sent_base = state.lock().unwrap_or_else(|p| p.into_inner()).sent;
        let udp_base = state.lock().unwrap_or_else(|p| p.into_inner()).udp_sent;
        let mut candidate: Option<Task<io::Result<Hook>>> = None;
        let mut discovery: Option<Task<io::Result<Vec<GameProcess>>>> = None;
        let mut next_discovery = Instant::now();
        let mut relay_recovery:Option<Task<io::Result<(TcpStream,u64)>>>=None;
        let mut ipc_recovery:Option<Task<io::Result<TcpStream>>>=None;
        let result = async {
            network.send(&Message::Ping(netburrow_protocol::DIAGNOSTICS_PING)).await?;
            network.send(&Message::Ping(netburrow_protocol::RECOVERY_PING)).await?;
            loop {
                tokio::select! {
                    _ = stop.changed() => return Ok(()),
                    recovered=async {match relay_recovery.as_mut(){Some(task)=>task.await,None=>std::future::pending().await}}=>{
                        relay_recovery=None;
                        let(socket,received)=recovered.map_err(io::Error::other)??;
                        network.finish_resume(socket,received)?;network.bind_udp().await?;
                        change(state,|s|{s.relay_recovering=false;s.relay_recoveries+=1;s.ping_ms=None;s.rtt_samples.clear();});
                        log("INFO","relay recovery","original session resumed; pending reliable frames replayed with duplicate suppression");
                        status(state,if hook.as_ref().is_some_and(|h|h.acknowledged){Phase::Ready}else{Phase::WaitingForGame},"原 Relay 会话已恢复；请确认游戏是否继续推进");
                    }
                    event = network.events.recv(), if relay_recovery.is_none() => {
                        match event {
                            Some(NetworkEvent::Tcp(Message::Members(members))) => {
                                log("INFO", "members", &format!("online={} game_bound={}", members.len(), members.iter().filter(|p| p.steam_id != 0).count()));
                                peers = members;
                                network.diagnostics.members(network.client_id, &peers);
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
                            Some(NetworkEvent::Tcp(Message::DiagnosticsPeers(members))) => network.diagnostics.capabilities(members),
                            Some(NetworkEvent::Tcp(Message::RecoveryOffer(key)))=>{network.resume_key=Some(key);log("INFO","relay recovery","original session recovery negotiated");}
                            Some(NetworkEvent::Tcp(Message::PeerProbe(probe))) => {
                                if let Some(reply) = network.diagnostics.probe(probe, std::time::Instant::now()) { network.send(&Message::PeerProbe(reply)).await?; }
                            }
                            Some(NetworkEvent::Tcp(Message::Data(packet))) => {
                                network.diagnostics.receive(&packet);
                                deliver(packet, false, &peers, &mut hook, state).await?;
                            }
                            Some(NetworkEvent::Udp(packet)) => {
                                network.diagnostics.receive(&packet);
                                deliver(packet, true, &peers, &mut hook, state).await?;
                            }
                            Some(NetworkEvent::Tcp(Message::Pong(netburrow_protocol::DIAGNOSTICS_PING))) => {},
                            Some(NetworkEvent::Tcp(Message::Pong(netburrow_protocol::RECOVERY_PING))) => {},
                            Some(NetworkEvent::Tcp(Message::Pong(sequence))) => {
                                change(state, |s| {
                                    let ping = (since.elapsed().as_millis() as u64).saturating_sub(sequence);
                                    s.ping_ms = Some(ping);
                                    s.last_pong_at = Some(std::time::Instant::now());
                                    s.rtt_samples.push_back(ping);
                                    if s.rtt_samples.len() > 20 { s.rtt_samples.pop_front(); }
                                });
                            }
                            Some(NetworkEvent::Tcp(Message::Ping(value))) => network.send(&Message::Pong(value)).await?,
                            Some(NetworkEvent::UdpBound) if network.udp.is_some() => { network.udp_bound = true; log("INFO", "udp", "endpoint bound; unreliable packets may use UDP"); }
                            Some(NetworkEvent::UdpBound)=>{},
                            Some(NetworkEvent::Tcp(Message::IpcPeerFault {peer,epoch})) => {
                                if peers.iter().any(|p|p.steam_id==peer && p.epoch==epoch) {
                                    if let Some(h)=hook.as_mut() { h.fail_peer(peer,epoch)?; }
                                }
                            }
                            Some(NetworkEvent::Tcp(Message::Error(reason))) if matches!(reason.as_str(), "target connection is slow" | "target connection closed") => {
                                log("WARN", "relay peer disconnected", relay_reason(&reason));
                                // Members is authoritative for peer cleanup; these errors carry no target identity.
                                let phase = state.lock().unwrap_or_else(|p| p.into_inner()).phase;
                                status(state, phase, if reason == "target connection is slow" {
                                    "对方连接拥堵，已断开；本机仍在线，等待对方重新加入。"
                                } else {
                                    "对方已断开；本机仍在线，等待对方重新加入。"
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
                            Some(NetworkEvent::Closed) | None => {
                                relay_recovery=Some(network.begin_resume(settings.server.clone(),stop.clone()).await?);
                                change(state,|s|{s.relay_recovering=true;s.disconnects+=1;s.last_disconnect_at=Some(std::time::Instant::now());});
                                status(state,Phase::Connecting,"连接中断，正在恢复原会话；暂时保留游戏接入");
                            }
                            Some(NetworkEvent::Fault(e))=>{
                                if !recoverable(&e){return Err(e);}
                                relay_recovery=Some(network.begin_resume(settings.server.clone(),stop.clone()).await?);
                                change(state,|s|{s.relay_recovering=true;s.disconnects+=1;s.last_disconnect_at=Some(std::time::Instant::now());});
                                status(state,Phase::Connecting,"连接中断，正在恢复原会话；暂时保留游戏接入");
                            }
                            _ => return Err(io::Error::other("unexpected Relay message")),
                        }
                    }
                    incoming = listener.accept() => {
                        let (socket, address) = incoming?;
                        if !address.ip().is_loopback() || candidate.is_some() || ipc_recovery.is_some() { continue; }
                        if let Some(h)=hook.as_mut() {
                            if h.recovering_since.is_some() {ipc_recovery=Some(tokio::spawn(Hook::resume_candidate(socket,h.pid,h.nonce,h.steam_id,h.epoch,h.writer.session.clone())));}
                            continue;
                        }
                        if let Some(p) = pending.as_ref() {
                            let (pid,nonce,epoch)=(p.game.pid,p.nonce,p.epoch);
                            candidate=Some(tokio::spawn(Hook::accept(socket,pid,nonce,epoch,network.incoming_budget.clone(),network.outgoing_budget.clone())));
                        }
                    }
                    accepted = async { match candidate.as_mut() {Some(task)=>Some(task.await),None=>std::future::pending().await} } => {
                            candidate=None;
                            if let Some(Ok(Ok(mut h))) = accepted {
                                if hook.is_some() || pending.as_ref().is_none_or(|p|p.epoch!=h.epoch) {continue;}
                                log("INFO", "ipc", "Hook hello identity verified; requesting Relay game binding");
                                h.received_base=state.lock().unwrap_or_else(|p|p.into_inner()).received;
                                h.udp_received_base=state.lock().unwrap_or_else(|p|p.into_inner()).udp_received;
                                network.send(&Message::Bind { steam_id: h.steam_id, epoch: h.epoch }).await?;
                                hook = Some(h);
                            } else {
                                log("WARN", "ipc hello rejected", "identity/capability verification failed; use the complete client package");
                                if let Some(Ok(Err(error)))=accepted {
                                    if error.kind()==io::ErrorKind::Unsupported {
                                        pending=None;
                                        status(state,Phase::RestartRequired,"客户端与 Hook 版本不匹配，请完整解压同一版本并重开游戏；Relay 保持连接");
                                    }
                                }
                            }
                    }
                    recovered=async {match ipc_recovery.as_mut(){Some(task)=>task.await,None=>std::future::pending().await}}=>{
                        ipc_recovery=None;
                        if let Ok(Ok(socket))=recovered {if let Some(h)=hook.as_mut(){h.finish_resume(socket);change(state,|s|s.ipc_recoveries+=1);log("INFO","ipc recovery","original Hook session resumed");}}
                    }
                    message = async { match hook.as_mut() { Some(h) if h.recovering_since.is_none() => h.input.recv().await, _ => std::future::pending().await } } => {
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
                                    if h.acknowledged && !h.failed.contains(&(packet.to,packet.target_epoch)) && packet.from == h.steam_id && packet.source_epoch == h.epoch && peers.iter().any(|p| p.steam_id == packet.to && p.epoch == packet.target_epoch && p.steam_id != 0) {
                                        network.packet(packet).await?;
                                    }
                                }
                            }
                            Some(Ok(Message::IpcHealth(mut health))) => {
                                health.peers.retain(|h|peers.iter().any(|p|p.steam_id==h.peer && p.epoch==h.epoch));
                                change(state,|s|s.hook_health=Some(health));
                            }
                            Some(Ok(Message::IpcPeerFault {peer,epoch})) => {
                                if peers.iter().any(|p|p.steam_id==peer && p.epoch==epoch) {
                                    if let Some(h)=hook.as_mut() {h.fail_peer(peer,epoch)?;}
                                    network.writer.queue.fail_peer(peer,epoch);
                                    network.events.tcp.fail_peer(peer,epoch);
                                    network.events.udp.fail_peer(peer,epoch);
                                    log("WARN","peer fault","one peer session failed; other peers and Relay remain active");
                                }
                            }
                            Some(Ok(Message::Diagnostic(text))) => { log("INFO", "hook", &text); change(state, |s| { s.logs.push(text); if s.logs.len() > 64 { s.logs.remove(0); } }); },
                            Some(Ok(Message::Ping(value))) => { if let Some(h) = hook.as_mut() { h.send(&Message::Pong(value)).await?; } }
                            ended @ (Some(Ok(Message::Stop)) | Some(Err(_)) | None) => {
                                let recover=match &ended {Some(Ok(Message::Stop))=>false,Some(Err(e))=>recoverable(e),_=>true};
                                if let Some(Err(error)) = ended { log("ERROR", "ipc disconnected", &error.to_string()); }
                                else { log("INFO", "ipc disconnected", "Hook stopped or reader channel closed"); }
                                if recover {if let Some(h)=hook.as_mut(){h.begin_resume().await;}log("WARN","ipc recovery","waiting up to 3s for original Hook to reconnect");}
                                else {disconnect_hook(&mut hook, &mut pending, network).await;status(state, Phase::RestartRequired, "游戏接入已停止，请退出游戏后重开");}
                            }
                            _ => return Err(io::Error::other("unexpected Hook message")),
                        }
                    }
                    _ = clock.tick() => {
                        if !network.recovering {for probe in network.diagnostics.tick(std::time::Instant::now()) { network.send(&Message::PeerProbe(probe)).await?; }}
                        change(state, |s| s.path_diagnostics = network.diagnostics.snapshot(std::time::Instant::now()));
                        if relay_recovery.is_none() {
                            if let Err(e)=network.writer.check() {
                                if !recoverable(&e){return Err(e);}
                                relay_recovery=Some(network.begin_resume(settings.server.clone(),stop.clone()).await?);
                                change(state,|s|{s.relay_recovering=true;s.disconnects+=1;s.last_disconnect_at=Some(std::time::Instant::now());});
                                status(state,Phase::Connecting,"发送中断，正在恢复原会话；暂时保留游戏接入");
                            }
                        }
                        while let Some((peer,epoch))=network.writer.queue.fault() { if let Some(h)=hook.as_mut() {h.fail_peer(peer,epoch)?;} }
                        let ipc_error=hook.as_ref().filter(|h|h.recovering_since.is_none()).and_then(|h|h.writer.check().err().or_else(||(h.input.age()>=IPC_TIMEOUT).then(||io::Error::new(io::ErrorKind::TimedOut,"IPC liveness deadline"))));
                        if let Some(error)=ipc_error {
                            log("ERROR","ipc interrupted",&error.to_string());
                            if recoverable(&error) {if let Some(h)=hook.as_mut(){h.begin_resume().await;}}
                            else {disconnect_hook(&mut hook,&mut pending,network).await;status(state,Phase::RestartRequired,"游戏接入出现不可恢复错误，请退出游戏后重开");}
                        }
                        if hook.as_ref().is_some_and(|h|h.recovering_since.is_some_and(|t|t.elapsed()>=Duration::from_secs(3))) {
                            if let Some(task)=ipc_recovery.take(){task.abort();}
                            disconnect_hook(&mut hook,&mut pending,network).await;
                            status(state,Phase::RestartRequired,"本地连接恢复超时，请退出游戏后重开");
                        }
                        if let Some(h)=hook.as_mut() {
                            while let Some((peer,epoch))=h.writer.queue.fault() {h.fail_peer(peer,epoch)?;}
                            change(state,|s| {s.received=h.received_base+h.writer.count();s.udp_received=h.udp_received_base+h.writer.udp_count();s.ipc_slow=h.recovering_since.is_some() || h.input.age()>=IPC_WARNING || h.writer.slow();s.peer_faults=h.failed.len() as u64;});
                        } else {
                            change(state,|s| {s.ipc_slow=false;s.hook_health=None;s.process_unknown=false;s.peer_faults=0;});
                        }
                        change(state,|s|{s.sent=sent_base+network.writer.count()+network.udp_written;s.udp_sent=udp_base+network.udp_written;});
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
                            let health=state.lock().unwrap_or_else(|p|p.into_inner()).hook_health.clone();
                            if let Some(h)=health {log("INFO","hook health",&format!("send_calls={} rejected={} read_calls={} consumed={} dropped={} lock_busy={} queued_packets={} queued_bytes={} oldest_ms={} interface_changed={}",h.send_calls,h.send_rejected,h.read_calls,h.consumed,h.dropped,h.lock_busy,h.queued_packets,h.queued_bytes,h.oldest_ms,h.interface_changed));}
                            let peer_lines=super::peer_diagnostic_lines(&state.lock().unwrap_or_else(|p|p.into_inner()));
                            for line in peer_lines {log("INFO","peer health",&line);}
                            let path_lines = network.diagnostics.snapshot(std::time::Instant::now()).lines();
                            crate::diagnostics::record_batch("INFO", path_lines.iter().map(|line| ("network diagnostics", line.as_str())));
                            let drops=network.events.tcp.dropped()+network.events.udp.dropped()+network.writer.queue.dropped()+hook.as_ref().map_or(0,|h|h.input.dropped()+h.writer.queue.dropped());
                            log("INFO","local handoff",&format!("unreliable_dropped={drops}"));
                            last_stats = Instant::now();
                        }
                        if relay_recovery.is_none() && network.events.tcp.pong_age() > Duration::from_secs(15) {
                            change(state, |s| s.heartbeat_timeouts += 1);
                            relay_recovery=Some(network.begin_resume(settings.server.clone(),stop.clone()).await?);
                            change(state,|s|{s.relay_recovering=true;s.disconnects+=1;s.last_disconnect_at=Some(std::time::Instant::now());});
                            status(state,Phase::Connecting,"心跳超时，正在恢复原会话；暂时保留游戏接入");
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
                        let mut games=Vec::new();
                        if pending.is_none() {
                            if discovery.as_ref().is_some_and(|task|task.is_finished()) {
                                match discovery.take().unwrap().await {
                                    Ok(Ok(found))=> {games=found;seen.retain(|key|games.iter().any(|p|(p.pid,p.created)==*key));},
                                    _=>log("WARN","process discovery","process enumeration temporarily unavailable"),
                                }
                            }
                            if discovery.is_none() && Instant::now()>=next_discovery {let path=game_path.to_owned();discovery=Some(tokio::task::spawn_blocking(move||find_games(&path)));next_discovery=Instant::now()+Duration::from_secs(1);}
                        }
                        if let Some(p) = pending.as_mut() {
                            let process_state=*p.monitor.borrow();
                            let unknown=matches!(process_state,ProcessState::Unknown(_));
                            change(state,|s| {if s.process_unknown!=unknown {log("WARN","process monitor",&format!("state={process_state:?}"));}s.process_unknown=unknown;});
                            if process_state==ProcessState::Exited {
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
        if let Some(task) = candidate {
            task.abort();
        }
        if let Some(task) = discovery {
            task.abort();
        }
        if let Some(task)=relay_recovery {task.abort();}
        if let Some(task)=ipc_recovery {task.abort();}
        change(state,|s|s.relay_recovering=false);
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
            | "packet source or epoch is invalid"
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
        mut packet: Packet,
        udp: bool,
        peers: &[Peer],
        hook: &mut Option<Hook>,
        state: &Shared,
    ) -> io::Result<()> {
        if let Some(h) = hook.as_mut() {
            if h.acknowledged
                && !h.failed.contains(&(packet.from, packet.source_epoch))
                && packet.to == h.steam_id
                && packet.target_epoch == h.epoch
                && peers.iter().any(|p| {
                    p.steam_id == packet.from && p.epoch == packet.source_epoch && p.steam_id != 0
                })
            {
                packet.delivery = None;
                if let Err(error) = h.writer.send_data(Message::Data(packet), udp) {
                    h.writer.report_failure(error);
                }
                let _ = state; // Actual socket completion is counted by the writer task.
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use netburrow_relay::{Config, spawn};

        #[tokio::test]
        async fn hook_resume_keeps_pending_frames_and_rejects_changed_identity() {
            let listener=TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut game=TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
            let (server,_) = listener.accept().await.unwrap();
            write(&mut game,&Message::IpcHelloV2{nonce:[7;16],pid:42,steam_id:101,epoch:1001,capabilities:IPC_CAPABILITIES}).await.unwrap();
            let mut hook=Hook::accept(server,42,[7;16],1001,Budget::default(),Budget::default()).await.unwrap();
            assert_eq!(read(&mut game).await.unwrap(),Message::IpcAccepted(IPC_CAPABILITIES));
            let expected=Message::Data(packet(202,101,2002,1001,2));
            hook.send(&expected).await.unwrap();
            let Message::SessionFrame{sequence,body}=read(&mut game).await.unwrap() else{panic!("expected reliable IPC envelope");};
            assert_eq!(netburrow_protocol::decode_session_body(&body).unwrap(),expected);
            hook.begin_resume().await;drop(game);
            let mut bad=TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();let(server,_)=listener.accept().await.unwrap();
            write(&mut bad,&Message::IpcResume{nonce:[8;16],pid:42,steam_id:101,epoch:1001,received:0}).await.unwrap();
            assert!(Hook::resume_candidate(server,42,[7;16],101,1001,hook.writer.session.clone()).await.is_err());
            let mut game=TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();let(server,_)=listener.accept().await.unwrap();
            write(&mut game,&Message::IpcResume{nonce:[7;16],pid:42,steam_id:101,epoch:1001,received:0}).await.unwrap();
            let server=Hook::resume_candidate(server,42,[7;16],101,1001,hook.writer.session.clone()).await.unwrap();
            assert_eq!(read(&mut game).await.unwrap(),Message::SessionAck(0));hook.finish_resume(server);
            assert_eq!(read(&mut game).await.unwrap(),Message::SessionFrame{sequence,body});
            write(&mut game,&Message::SessionAck(sequence)).await.unwrap();
            timeout(Duration::from_secs(1),async{while hook.writer.session.window.lock().unwrap().pending_len()!=0 {tokio::task::yield_now().await;}}).await.unwrap();
            assert!(hook.recovering_since.is_none());
        }

        #[tokio::test]
        async fn relay_resume_preserves_identity_queues_and_unacknowledged_reliable_data() {
            let relay=spawn(Config{bind:"127.0.0.1:0".parse().unwrap(),..Config::default()}).await.unwrap();
            let address=relay.local_addr().to_string();
            let mut a=Network::connect(&address,[9;32],Transport::Tcp).await.unwrap();
            let mut b=Network::connect(&address,[9;32],Transport::Tcp).await.unwrap();
            a.send(&Message::Bind{steam_id:101,epoch:1001}).await.unwrap();
            b.send(&Message::Bind{steam_id:202,epoch:2002}).await.unwrap();
            wait_member(&mut a,202,2002).await;wait_member(&mut b,101,1001).await;
            for n in [&mut a,&mut b] {
                n.send(&Message::Ping(netburrow_protocol::RECOVERY_PING)).await.unwrap();
                timeout(Duration::from_secs(2),async{loop{if let Some(NetworkEvent::Tcp(Message::RecoveryOffer(key)))=n.events.recv().await{n.resume_key=Some(key);break;}}}).await.unwrap();
            }
            let original_id=a.client_id;
            // Lose ACKs without losing the application's outbound reliable frames.
            a.tasks[0].abort();
            for i in 0..8 {let mut p=packet(101,202,1001,2002,2);p.payload=vec![i];a.packet(p).await.unwrap();}
            for i in 0..8 {assert_eq!(wait_packet(&mut b,false).await.payload,vec![i]);}
            assert!(a.writer.session.window.lock().unwrap().pending_len()>0);
            let (_stop,stopped)=watch::channel(false);
            let recovery=a.begin_resume(address.clone(),stopped).await.unwrap();
            for _ in 0..256 {a.send(&Message::Ping(123)).await.unwrap();}
            // The source can keep accepting game data while its connection is being restored.
            for i in 8..16 {let mut p=packet(101,202,1001,2002,3);p.payload=vec![i];a.packet(p).await.unwrap();}
            let mut incoming=packet(202,101,2002,1001,2);incoming.payload=b"during recovery".to_vec();
            b.packet(incoming.clone()).await.unwrap();
            let (socket,received)=timeout(Duration::from_secs(3),recovery).await.unwrap().unwrap().unwrap();
            a.finish_resume(socket,received).unwrap();
            assert_eq!(a.client_id,original_id);
            for i in 8..16 {assert_eq!(wait_packet(&mut b,false).await.payload,vec![i]);}
            assert_eq!(wait_packet(&mut a,false).await,incoming);
            // A control reply after all frames proves no replay duplicate is left in the data queue.
            b.send(&Message::Ping(987654)).await.unwrap();
            timeout(Duration::from_secs(2),async{loop{match b.events.recv().await{Some(NetworkEvent::Tcp(Message::Data(_)))=>panic!("duplicate delivered"),Some(NetworkEvent::Tcp(Message::Pong(987654)))=>break,_=>{}}}}).await.unwrap();
            drop(a);drop(b);relay.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn network_diagnostics_follow_tcp_udp_fallback_and_real_peer_echo() {
            let relay = spawn(Config { bind: "127.0.0.1:0".parse().unwrap(), ..Config::default() }).await.unwrap();
            let address = relay.local_addr().to_string();
            let mut a = Network::connect(&address, [8;32], Transport::Udp).await.unwrap();
            let mut b = Network::connect(&address, [8;32], Transport::Udp).await.unwrap();
            for (n, steam, epoch) in [(&mut a,101,1001),(&mut b,202,2002)] {
                n.send(&Message::Bind { steam_id:steam,epoch }).await.unwrap();
                n.send(&Message::Ping(netburrow_protocol::DIAGNOSTICS_PING)).await.unwrap();
            }
            for n in [&mut a, &mut b] {
                timeout(Duration::from_secs(2), async {
                    loop {
                        match n.events.recv().await {
                            Some(NetworkEvent::Tcp(Message::Members(peers))) => n.diagnostics.members(n.client_id,&peers),
                            Some(NetworkEvent::Tcp(Message::DiagnosticsPeers(peers))) => n.diagnostics.capabilities(peers),
                            _ => {}
                        }
                        let snapshot = n.diagnostics.snapshot(std::time::Instant::now());
                        if snapshot.peers.len() == 1 && snapshot.peers[0].supported { break; }
                    }
                }).await.unwrap();
            }
            // Only the source is UDP-bound: Relay must preserve metadata during TCP fallback.
            a.bind_udp().await.unwrap(); wait_udp_bound(&mut a).await;
            assert!(a.packet(packet(101,202,1001,2002,0)).await.unwrap());
            let first = wait_packet(&mut b,false).await;
            assert_eq!(first.delivery.unwrap().sequence,1); b.diagnostics.receive(&first);
            b.bind_udp().await.unwrap(); wait_udp_bound(&mut b).await;
            assert!(a.packet(packet(101,202,1001,2002,1)).await.unwrap());
            let second = wait_packet(&mut b,true).await;
            assert_eq!(second.delivery.unwrap().sequence,2); b.diagnostics.receive(&second);
            assert!(!a.packet(packet(101,202,1001,2002,2)).await.unwrap());
            let reliable = wait_packet(&mut b,false).await;
            assert_eq!(reliable.delivery.unwrap().sequence,1);
            assert_ne!(reliable.delivery.unwrap().stream,first.delivery.unwrap().stream);
            b.diagnostics.receive(&reliable);
            let now = std::time::Instant::now();
            let request = a.diagnostics.tick(now).remove(0);
            a.send(&Message::PeerProbe(request)).await.unwrap();
            timeout(Duration::from_secs(2), async {
                loop {
                    if let Some(NetworkEvent::Tcp(Message::PeerProbe(probe))) = b.events.recv().await {
                        let reply = b.diagnostics.probe(probe,std::time::Instant::now()).unwrap();
                        b.send(&Message::PeerProbe(reply)).await.unwrap(); break;
                    }
                }
                loop {
                    if let Some(NetworkEvent::Tcp(Message::PeerProbe(probe))) = a.events.recv().await {
                        a.diagnostics.probe(probe,std::time::Instant::now()); break;
                    }
                }
            }).await.unwrap();
            assert_eq!(a.diagnostics.snapshot(std::time::Instant::now()).peers[0].probe_received,1);
            assert_eq!(b.diagnostics.snapshot(std::time::Instant::now()).peers[0].flows[0].missing_window,0);
            // Production delivery strips the extension before writing IPC to the Hook.
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let game = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
            let (mut receiver,_) = listener.accept().await.unwrap();
            write(&mut receiver,&Message::IpcHelloV2 { nonce:[7;16],pid:42,steam_id:202,epoch:2002,capabilities:IPC_CAPABILITIES }).await.unwrap();
            let mut h = Hook::accept(game,42,[7;16],2002,Budget::default(),Budget::default()).await.unwrap();
            assert_eq!(read(&mut receiver).await.unwrap(),Message::IpcAccepted(IPC_CAPABILITIES));
            h.acknowledged = true;
            let mut hook = Some(h);
            let shared = Arc::new(Mutex::new(Snapshot::default()));
            let peers = vec![Peer { client_id:a.client_id,steam_id:101,epoch:1001 }];
            let mut expected = reliable.clone(); expected.delivery = None;
            deliver(reliable,false,&peers,&mut hook,&shared).await.unwrap();
            let Message::SessionFrame {sequence,body}=timeout(Duration::from_secs(2),read(&mut receiver)).await.unwrap().unwrap() else {panic!("expected resumable IPC data");};
            assert_eq!(netburrow_protocol::decode_session_body(&body).unwrap(),Message::Data(expected));
            write(&mut receiver,&Message::SessionAck(sequence)).await.unwrap();
            relay.shutdown().await.unwrap();
        }
        use tokio::time::{Duration, timeout};

        #[tokio::test]
        async fn hook_capabilities_are_checked_before_binding() {
            for supported in [false, true] {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let client = tokio::spawn(async move {
                    let mut socket = TcpStream::connect(address).await.unwrap();
                    let hello = if supported {
                        Message::IpcHelloV2 {
                            nonce: [7; 16],
                            pid: 42,
                            steam_id: 101,
                            epoch: 11,
                            capabilities: IPC_CAPABILITIES,
                        }
                    } else {
                        Message::IpcHello {
                            nonce: [7; 16],
                            pid: 42,
                            steam_id: 101,
                            epoch: 11,
                        }
                    };
                    write(&mut socket, &hello).await.unwrap();
                    if supported {
                        assert_eq!(
                            read(&mut socket).await.unwrap(),
                            Message::IpcAccepted(IPC_CAPABILITIES)
                        );
                    } else {
                        assert!(read(&mut socket).await.is_err());
                    }
                });
                let (socket, _) = listener.accept().await.unwrap();
                let result = Hook::accept(
                    socket,
                    42,
                    [7; 16],
                    11,
                    Budget::default(),
                    Budget::default(),
                )
                .await;
                assert_eq!(result.is_ok(), supported);
                if let Ok(hook) = result {
                    assert!(!hook.bound);
                    assert!(!hook.acknowledged);
                    client.await.unwrap();
                    drop(hook);
                } else {
                    client.await.unwrap();
                }
            }
        }

        #[tokio::test]
        async fn preflight_probes_existing_protocol_and_rejects_unsupported_server() {
            let relay = spawn(Config {
                bind: "127.0.0.1:0".parse().unwrap(),
                ..Config::default()
            })
            .await
            .unwrap();
            let mut settings = Settings {
                server: relay.local_addr().to_string(),
                group: crate::new_group().unwrap(),
                ..Settings::default()
            };
            timeout(Duration::from_secs(3), probe(&settings))
                .await
                .unwrap()
                .unwrap();
            relay.shutdown().await.unwrap();
            let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
            settings.server = server.local_addr().unwrap().to_string();
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
                assert!(matches!(
                    read(&mut socket).await.unwrap(),
                    Message::Status(_)
                ));
                write(
                    &mut socket,
                    &Message::Error("message is not accepted from a client".into()),
                )
                .await
                .unwrap();
                assert!(matches!(read(&mut socket).await.unwrap(), Message::Leave));
            });
            assert!(
                timeout(Duration::from_secs(3), probe(&settings))
                    .await
                    .unwrap()
                    .unwrap_err()
                    .contains("状态检查")
            );
            task.await.unwrap();
        }

        #[tokio::test]
        async fn unauthorized_group_fails_probe_and_finishes_without_retry() {
            let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut settings = Settings {
                server: server.local_addr().unwrap().to_string(),
                group: crate::new_group().unwrap(),
                game_path: "netburrow-authorization-test-no-game.exe".into(),
                ..Settings::default()
            };
            let task = tokio::spawn(async move {
                for _ in 0..2 {
                    let (mut socket, _) = server.accept().await.unwrap();
                    assert!(matches!(
                        read(&mut socket).await.unwrap(),
                        Message::Join { .. }
                    ));
                    write(&mut socket, &Message::Error("group is not allowed".into()))
                        .await
                        .unwrap();
                }
                // A fresh manual attempt can still use the same server after rejection.
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
                assert!(matches!(
                    read(&mut socket).await.unwrap(),
                    Message::Status(_)
                ));
                write(&mut socket, &Message::Statuses(vec![]))
                    .await
                    .unwrap();
                assert!(matches!(read(&mut socket).await.unwrap(), Message::Leave));
            });
            assert_eq!(
                timeout(Duration::from_secs(2), probe(&settings))
                    .await
                    .unwrap()
                    .unwrap_err(),
                GROUP_NOT_ALLOWED_HINT
            );
            let state = Arc::new(Mutex::new(Snapshot {
                ping_ms: Some(42),
                last_pong_at: Some(std::time::Instant::now()),
                rtt_samples: [42].into(),
                peers: vec![PeerInfo {
                    client_id: 2,
                    game_epoch: 1,
                    steam_id: 2,
                    ready: true,
                    is_self: false,
                    status: None,
                    status_updated: None,
                }],
                ..Snapshot::default()
            }));
            let (_stop, stopped) = watch::channel(false);
            // Finishing before the existing three-second retry proves this is terminal.
            timeout(
                Duration::from_secs(2),
                run(settings.clone(), PathBuf::new(), stopped, state.clone()),
            )
            .await
            .unwrap();
            {
                let snapshot = state.lock().unwrap();
                assert_eq!(snapshot.phase, Phase::Failed);
                assert_eq!(snapshot.detail, GROUP_NOT_ALLOWED_HINT);
                assert!(snapshot.peers.is_empty());
                assert!(snapshot.ping_ms.is_none());
                assert!(snapshot.rtt_samples.is_empty());
                assert!(snapshot.last_pong_at.is_none());
            }
            settings.group = crate::new_group().unwrap();
            timeout(Duration::from_secs(2), probe(&settings))
                .await
                .unwrap()
                .unwrap();
            task.await.unwrap();
        }

        #[tokio::test]
        async fn handshake_errors_have_distinct_safe_guidance() {
            for (reply, expected, hint) in [
                (
                    Message::Error("relay is full".into()),
                    io::ErrorKind::WouldBlock,
                    "服务器已满",
                ),
                (
                    Message::Error("group is not allowed".into()),
                    io::ErrorKind::PermissionDenied,
                    GROUP_NOT_ALLOWED_HINT,
                ),
                (
                    Message::Error("untrusted secret text".into()),
                    io::ErrorKind::PermissionDenied,
                    "服务器拒绝加入",
                ),
                (
                    Message::Pong(0),
                    io::ErrorKind::InvalidData,
                    "服务器协议不匹配",
                ),
            ] {
                let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let endpoint = server.local_addr().unwrap().to_string();
                let task = tokio::spawn(async move {
                    let (mut socket, _) = server.accept().await.unwrap();
                    let _ = read(&mut socket).await.unwrap();
                    write(&mut socket, &reply).await.unwrap();
                });
                let error = Network::connect(&endpoint, [5; 32], Transport::Tcp)
                    .await
                    .err()
                    .unwrap();
                assert_eq!(error.kind(), expected);
                assert!(connection_hint(&error).contains(hint));
                assert_eq!(group_not_allowed(&error), hint == GROUP_NOT_ALLOWED_HINT);
                assert!(!error.to_string().contains("untrusted secret text"));
                task.await.unwrap();
            }
            let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = server.local_addr().unwrap().to_string();
            drop(server);
            let error = Network::connect(&endpoint, [5; 32], Transport::Tcp)
                .await
                .err()
                .unwrap();
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
                delivery: None,
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
                ("target connection closed", "对方已断开"),
                ("status reports are limited to once per second", ""),
            ] {
                let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let endpoint = server.local_addr().unwrap().to_string();
                let (stop, mut stopped) = watch::channel(false);
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
                    write(&mut socket, &Message::Error(reason.into()))
                        .await
                        .unwrap();
                    write(&mut socket, &Message::Ping(12345)).await.unwrap();
                    loop {
                        match read(&mut socket).await.unwrap() {
                            Message::Pong(12345) => break,
                            Message::Ping(n) => {
                                write(&mut socket, &Message::Pong(n)).await.unwrap()
                            }
                            Message::Status(_) | Message::Bind { .. } => {}
                            other => panic!("unexpected message after peer error: {other:?}"),
                        }
                    }
                    stop.send(true).unwrap();
                    // Keep the socket alive until the client's normal shutdown.
                    let _ = read(&mut socket).await;
                });
                let mut network = Network::connect(&endpoint, [5; 32], Transport::Tcp)
                    .await
                    .unwrap();
                let ipc = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let state = Arc::new(Mutex::new(Snapshot {
                    phase: Phase::Ready,
                    ..Snapshot::default()
                }));
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
                .expect("client stopped responding after peer error");
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
