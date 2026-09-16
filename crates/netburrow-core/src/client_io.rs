//! Local bounded socket handoff. Never await a data-queue permit in a socket reader.
use netburrow_protocol::{
    MAX_FRAME, Message, decode, encode,
    local::{Admission, Frames, IPC_TIMEOUT, Limits},
};
use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::tcp::OwnedWriteHalf,
    sync::Notify,
    task::JoinHandle,
};

#[derive(Clone, Default)]
pub(crate) struct Session {
    pub window: Arc<Mutex<netburrow_protocol::resume::Window>>,
    pub enabled: Arc<AtomicBool>,
}
impl Session {
    pub fn enabled(&self) -> bool {self.enabled.load(Ordering::Acquire)}
    pub fn enable(&self) {self.enabled.store(true,Ordering::Release);}
    pub fn received(&self)->u64 {self.window.lock().unwrap_or_else(|p|p.into_inner()).received_through()}
    pub fn acknowledge(&self,n:u64)->io::Result<()> {self.window.lock().unwrap_or_else(|p|p.into_inner()).acknowledge(n)}
}

struct State {
    frames: Frames,
    closed: bool,
    error: Option<io::Error>,
    active: Instant,
    pong: Instant,
    writing_since: Option<Instant>,
}
#[derive(Clone, Default)]
pub(crate) struct Budget(Arc<Mutex<Vec<std::sync::Weak<Mutex<State>>>>>);
#[derive(Clone)]
pub(crate) struct Mailbox {
    state: Arc<Mutex<State>>,
    notify: Arc<Notify>,
    budget: Budget,
}
impl Mailbox {
    pub fn reopen(&self) {
        let mut s=self.state.lock().unwrap_or_else(|p|p.into_inner());
        s.closed=false;s.error=None;s.active=Instant::now();s.pong=s.active;s.writing_since=None;
    }
    #[cfg(test)]
    pub fn new(by_source: bool) -> Self {
        Self::with_budget(by_source, Budget::default())
    }
    pub fn with_budget(by_source: bool, budget: Budget) -> Self {
        let state = Arc::new(Mutex::new(State {
            frames: Frames::new(by_source),
            closed: false,
            error: None,
            active: Instant::now(),
            pong: Instant::now(),
            writing_since: None,
        }));
        {
            let mut queues = budget.0.lock().unwrap_or_else(|p| p.into_inner());
            queues.retain(|q| q.strong_count() > 0);
            queues.push(Arc::downgrade(&state));
        }
        Self {
            state,
            notify: Arc::new(Notify::new()),
            budget,
        }
    }
    pub fn post(&self, message: Message) -> io::Result<Admission> {
        self.post_tagged(message, false)
    }
    fn post_tagged(&self, message: Message, udp: bool) -> io::Result<Admission> {
        let queues = self.budget.0.lock().unwrap_or_else(|p| p.into_inner());
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "local queue closed",
            ));
        }
        let mut limits = Limits::default();
        if let Message::Data(p) = &message {
            let key = state.frames.packet_key(p);
            for queue in queues
                .iter()
                .filter_map(|q| q.upgrade())
                .filter(|q| !Arc::ptr_eq(q, &self.state))
            {
                let other = queue.lock().unwrap_or_else(|p| p.into_inner());
                let (count, bytes) = other.frames.usage(None);
                let (peer_count, peer_bytes) = other.frames.usage(Some(key));
                limits.packets = limits.packets.saturating_sub(count);
                limits.bytes = limits.bytes.saturating_sub(bytes);
                limits.peer_packets = limits.peer_packets.saturating_sub(peer_count);
                limits.peer_bytes = limits.peer_bytes.saturating_sub(peer_bytes);
            }
        }
        let result = state
            .frames
            .push_limited(message, udp, limits)
            .map_err(io::Error::other);
        drop(state);
        self.notify.notify_one();
        result
    }
    pub fn observed(&self, pong: bool) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.active = Instant::now();
        if pong {
            state.pong = state.active;
        }
    }
    pub fn age(&self) -> Duration {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .active
            .elapsed()
    }
    pub fn pong_age(&self) -> Duration {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pong
            .elapsed()
    }
    pub fn fail_peer(&self, peer: u64, epoch: u64) {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .frames
            .fail(peer, epoch);
        self.notify.notify_one();
    }
    pub fn set_local_epoch(&self, epoch: u64) {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .frames
            .set_local_epoch(epoch);
    }
    pub fn dropped(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .frames
            .dropped
    }
    pub fn fault(&self) -> Option<(u64, u64)> {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .frames
            .fault()
    }
    pub fn close(&self, error: Option<io::Error>) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.closed = true;
        state.error = error;
        drop(state);
        self.notify.notify_one();
    }
    pub async fn recv(&self) -> Option<io::Result<Message>> {
        self.next(true).await.map(|v| v.map(|(m, _)| m))
    }
    async fn next(&self, faults: bool) -> Option<io::Result<(Message, bool)>> {
        loop {
            let notified = self.notify.notified();
            {
                let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(e) = state.error.take() {
                    return Some(Err(e));
                }
                if faults {
                    if let Some((peer, epoch)) = state.frames.fault() {
                        return Some(Ok((Message::IpcPeerFault { peer, epoch }, false)));
                    }
                }
                if let Some(message) = state.frames.pop_tagged() {
                    if !faults {
                        state.writing_since = Some(Instant::now());
                    }
                    return Some(Ok(message));
                }
                if state.closed {
                    return None;
                }
            }
            notified.await;
        }
    }
}

pub(crate) struct Writer {
    pub session: Session,
    pub queue: Mailbox,
    pub completed: Arc<AtomicU64>,
    pub udp_completed: Arc<AtomicU64>,
    failure: Arc<Mutex<Option<(io::ErrorKind,String)>>>,
    deadline: Duration,
    task: JoinHandle<()>,
}
impl Writer {
    pub fn with_budget(
        socket: OwnedWriteHalf,
        by_source: bool,
        deadline: Duration,
        budget: Budget,
    ) -> Self {
        let queue = Mailbox::with_budget(by_source, budget);
        let input = queue.clone();
        let completed = Arc::new(AtomicU64::new(0));
        let done = completed.clone();
        let failure = Arc::new(Mutex::new(None));
        let udp_completed = Arc::new(AtomicU64::new(0));
        let session=Session::default();
        let task=spawn_writer(socket,input,done,udp_completed.clone(),failure.clone(),deadline,session.clone());
        Self {session,queue,completed,udp_completed,failure,deadline,task}
    }
    pub async fn suspend(&mut self) {
        self.task.abort();
        // Cancellation must finish before a replacement snapshots the replay window.
        let _ = (&mut self.task).await;
    }
    pub fn rebind(&mut self,socket:OwnedWriteHalf) {
        self.task.abort();self.queue.reopen();
        *self.failure.lock().unwrap_or_else(|p|p.into_inner())=None;
        self.task=spawn_writer(socket,self.queue.clone(),self.completed.clone(),self.udp_completed.clone(),self.failure.clone(),self.deadline,self.session.clone());
    }
    pub fn send(&self, message: Message) -> io::Result<Admission> {
        if !self.session.enabled() {self.check()?;}
        self.queue.post(message)
    }
    pub fn check(&self) -> io::Result<()> {
        match self.failure.lock().unwrap_or_else(|p|p.into_inner()).as_ref() {
            Some((kind,e)) => Err(io::Error::new(*kind,e.clone())), None => Ok(())
        }
    }
    pub fn report_failure(&self, error: io::Error) {
        *self.failure.lock().unwrap_or_else(|p| p.into_inner()) = Some((error.kind(),error.to_string()));
        self.task.abort();
        self.queue.close(None);
    }
    pub fn count(&self) -> u64 { self.completed.load(Ordering::Relaxed) }
    pub fn slow(&self) -> bool {
        self.queue.state.lock().unwrap_or_else(|p| p.into_inner()).writing_since.is_some_and(|t| t.elapsed() >= netburrow_protocol::local::IPC_WARNING)
    }
    pub fn udp_count(&self) -> u64 {self.udp_completed.load(Ordering::Relaxed)}
    pub fn send_data(&self, message: Message, udp: bool) -> io::Result<Admission> {
        if !self.session.enabled() {self.check()?;}
        self.queue.post_tagged(message, udp)
    }
    pub async fn drain(&self) -> io::Result<()> {
        tokio::time::timeout(Duration::from_millis(250), async {
            loop {
                self.check()?;
                let idle = {let state = self.queue.state.lock().unwrap_or_else(|p| p.into_inner());state.writing_since.is_none() && state.frames.is_empty()};
                if idle {return Ok(());}
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }).await.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "control drain deadline"))?
    }
}

fn spawn_writer(mut socket:OwnedWriteHalf,input:Mailbox,done:Arc<AtomicU64>,udp_done:Arc<AtomicU64>,error:Arc<Mutex<Option<(io::ErrorKind,String)>>>,deadline:Duration,session:Session)->JoinHandle<()> {
    tokio::spawn(async move {
        let result=async {
            let replay=session.window.lock().unwrap_or_else(|p|p.into_inner()).pending();
            for (sequence,body) in replay {
                let bytes=encode(&Message::SessionFrame {sequence,body})?;
                tokio::time::timeout(deadline,socket.write_all(&bytes)).await.map_err(|_|io::Error::new(io::ErrorKind::TimedOut,"replay write deadline"))??;
            }
            while let Some(message) = input.next(false).await {
                let result = async {
                    let (message, udp) = message?;
                    let data = matches!(message, Message::Data(_));
                    let bytes = if session.enabled() && netburrow_protocol::replayable(&message) {
                        let body=encode(&message)?;
                        let sequence=session.window.lock().unwrap_or_else(|p|p.into_inner()).retain(body.clone()).map_err(|e|io::Error::new(io::ErrorKind::InvalidData,format!("resume buffer exhausted: {e}")))?;
                        encode(&Message::SessionFrame {sequence,body})?
                    } else {encode(&message)?};
                    drop(message);
                    tokio::time::timeout(deadline, socket.write_all(&bytes))
                        .await
                        .map_err(|_| {
                            io::Error::new(io::ErrorKind::TimedOut, "socket frame write deadline")
                        })??;
                    if data {
                        done.fetch_add(1, Ordering::Relaxed);
                        if udp {
                            udp_done.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Ok::<_, io::Error>(())
                }
                .await;
                if let Err(e) = result {
                    return Err(e);
                }
                input
                    .state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .writing_since = None;
            }
            Ok::<_,io::Error>(())
        }.await;
        if let Err(e)=result {
            *error.lock().unwrap_or_else(|p|p.into_inner())=Some((e.kind(),e.to_string()));
            if !session.enabled() {input.close(None);}
        }
    })
}
impl Drop for Writer {
    fn drop(&mut self) {
        self.task.abort();
        self.queue.close(None);
    }
}

#[cfg(test)]
pub(crate) fn reader(
    socket: impl AsyncRead + Unpin + Send + 'static,
    queue: Mailbox,
    ipc: bool,
) -> JoinHandle<()> {
    reader_inner(socket,queue,ipc,None)
}
pub(crate) fn reader_resumable(socket:impl AsyncRead+Unpin+Send+'static,queue:Mailbox,ipc:bool,session:Session,ack_queue:Mailbox)->JoinHandle<()> {
    reader_inner(socket,queue,ipc,Some((session,ack_queue)))
}
fn reader_inner(mut socket:impl AsyncRead+Unpin+Send+'static,queue:Mailbox,ipc:bool,session:Option<(Session,Mailbox)>)->JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let result = read_frame(&mut socket, ipc).await;
            match result {
                Ok(mut message) => {
                    let mut sequence=None;
                    if let Some((link,ack))=&session {
                        let result=(||->io::Result<bool>{
                            match &message {
                                Message::SessionAck(n)=>{link.acknowledge(*n)?;return Ok(false);}
                                Message::RecoveryOffer(_)=>link.enable(),
                                Message::SessionFrame {sequence:n,body}=>{
                                    if !link.enabled(){return Err(io::Error::new(io::ErrorKind::InvalidData,"resume frame before negotiation"));}
                                    if !link.window.lock().unwrap_or_else(|p|p.into_inner()).classify(*n)? {
                                        ack.post(Message::SessionAck(link.received()))?;return Ok(false);
                                    }
                                    sequence=Some(*n);
                                    message=netburrow_protocol::decode_session_body(body)?;
                                }
                                _=>{}
                            }
                            Ok(true)
                        })();
                        match result {Ok(false)=>continue,Ok(true)=>{},Err(e)=>{queue.close(Some(e));return;}}
                    }
                    if !ipc
                        && matches!(
                            message,
                            Message::IpcHelloV2 { .. }
                                | Message::IpcAccepted(_)
                                | Message::IpcHealth(_)
                                | Message::IpcPeerFault { .. }
                        )
                    {
                        queue.close(Some(io::Error::other("local message received from Relay")));
                        return;
                    }
                    queue.observed(matches!(message, Message::Pong(_)));
                    if let Err(e) = queue.post(message) {
                        queue.close(Some(e));
                        return;
                    }
                    if let (Some(n),Some((link,ack)))=(sequence,&session) {
                        let result=link.window.lock().unwrap_or_else(|p|p.into_inner()).received(n);
                        if let Err(e)=result.and_then(|_|ack.post(Message::SessionAck(link.received())).map(|_|())) {queue.close(Some(e));return;}
                    }
                }
                Err(e) => {
                    queue.close(Some(e));
                    return;
                }
            }
        }
    })
}
async fn read_frame(input: &mut (impl AsyncRead + Unpin), ipc: bool) -> io::Result<Message> {
    let mut length = [0; 4];
    // Idle and in-progress framing have separate deadlines; no cancellation loses a prefix.
    if ipc {
        tokio::time::timeout(IPC_TIMEOUT, input.read_exact(&mut length[..1]))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "IPC liveness deadline"))??;
    } else {
        input.read_exact(&mut length[..1]).await?;
    }
    let body = async {
        input.read_exact(&mut length[1..]).await?;
        let size = u32::from_be_bytes(length) as usize;
        if !(5..=MAX_FRAME - 4).contains(&size) {
            return Err(io::Error::other("invalid frame length"));
        }
        let mut body = vec![0; size];
        input.read_exact(&mut body).await?;
        decode(&body)
    };
    if ipc {
        tokio::time::timeout(IPC_TIMEOUT, body)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "IPC partial frame deadline"))?
    } else {
        body.await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn all_handoff_stages_share_one_direction_budget() {
        let budget = Budget::default();
        let first = Mailbox::with_budget(true, budget.clone());
        let second = Mailbox::with_budget(true, budget);
        let packet = || {
            Message::Data(netburrow_protocol::Packet {
                delivery: None,
                from: 20,
                to: 10,
                source_epoch: 200,
                target_epoch: 100,
                channel: 0,
                send_type: 2,
                payload: vec![1; 256],
            })
        };
        for _ in 0..2000 {
            first.post(packet()).unwrap();
        }
        for _ in 0..1072 {
            assert_eq!(second.post(packet()).unwrap(), Admission::Queued);
        }
        assert_eq!(
            second.post(packet()).unwrap(),
            Admission::PeerFailed(20, 200)
        );
        first.fail_peer(20, 200);
        assert_eq!(
            first.recv().await.unwrap().unwrap(),
            Message::IpcPeerFault {
                peer: 20,
                epoch: 200
            }
        );
    }
    #[tokio::test]
    async fn relay_cannot_send_local_fault_commands() {
        let (mut tx, rx) = tokio::io::duplex(64);
        let queue = Mailbox::new(true);
        let task = reader(rx, queue.clone(), false);
        tx.write_all(
            &encode(&Message::IpcPeerFault {
                peer: 10,
                epoch: 100,
            })
            .unwrap(),
        )
        .await
        .unwrap();
        assert!(queue.recv().await.unwrap().is_err());
        task.await.unwrap();
    }
}
