// ABI source: Valve's public SteamNetworking006 / CCallbackBase headers:
// https://github.com/ValveSoftware/source-sdk-2013/tree/master/src/public/steam
// Hook strategy follows TractorBeam ea0393f: Find/RunCallbacks IAT hooks,
// shared vtable slots 0/1/2/6, replacement data path and native session APIs.
// NetBurrow retains its authenticated IPC startup, peer epochs and recovery wire protocol.
#![allow(unsafe_op_in_unsafe_fn)]

use crate::queue::Bridge;
use netburrow_protocol::{
    HOOK_INIT_VERSION, HookInit, IPC_CAPABILITIES, MAX_PAYLOAD, Message,
    local::{Admission, PendingWrite, Reader},
    read_message, write_message,
};
use std::{
    ffi::{c_char, c_void},
    io,
    mem::{size_of, transmute, zeroed},
    net::{Shutdown, SocketAddr, TcpStream},
    ptr,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock, TryLockError,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{HINSTANCE, HMODULE},
    System::{
        LibraryLoader::{DisableThreadLibraryCalls, GetModuleHandleW, GetProcAddress},
        Memory::{
            MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE, PAGE_EXECUTE_READ,
            PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_NOACCESS,
            PAGE_READONLY, PAGE_READWRITE, PAGE_WRITECOPY, VirtualProtect, VirtualQuery,
        },
        Threading::GetCurrentProcessId,
    },
};
use windows_sys::core::BOOL;

struct Shared {
    bridge: Mutex<Bridge>,
    wake: std::thread::Thread,
    stopped: AtomicBool,
    faults: AtomicBool,
}
static BRIDGE: OnceLock<Arc<Shared>> = OnceLock::new();
static STARTED: AtomicBool = AtomicBool::new(false);
// Retain each observed table for process lifetime. A newly returned interface
// must not change the native session function used by an older table.
static STEAM: Mutex<Vec<Arc<Steam>>> = Mutex::new(Vec::new());
// Installation never calls Steam or takes Bridge locks. Serialize it rather
// than skipping a second interface returned during another table's install.
static INSTALL_LOCK: Mutex<()> = Mutex::new(());
static LOCK_BUSY: AtomicUsize = AtomicUsize::new(0);
static SEND_LOCK_BUSY: AtomicUsize = AtomicUsize::new(0);
static SEND_INVALID: AtomicUsize = AtomicUsize::new(0);
static INTERFACE_CHANGED: AtomicBool = AtomicBool::new(false);
static INTERFACE_REPAIRS: AtomicUsize = AtomicUsize::new(0);
static CALLBACK_TICKS: AtomicUsize = AtomicUsize::new(0);
// Indices follow the SteamNetworking006 vtable: send, available, read, accept,
// close session, close channel, session state. Atomic updates only on game threads.
static API_CALLS: [AtomicUsize; 7] = [const { AtomicUsize::new(0) }; 7];
static API_BUSY: [AtomicUsize; 7] = [const { AtomicUsize::new(0) }; 7];
static API_INVALID: [AtomicUsize; 7] = [const { AtomicUsize::new(0) }; 7];
static API_NATIVE: [AtomicUsize; 7] = [const { AtomicUsize::new(0) }; 7];
static READ_TRUNCATED: AtomicUsize = AtomicUsize::new(0);
// First failure wins. The game-thread failure path never locks or touches queues.
static HOOK_FAILURE: AtomicUsize = AtomicUsize::new(0);
const RUST_PANIC: usize = 1;
const POISONED: usize = 2;
const INSTALL_FAILED: usize = 3;

fn fail_hook(reason: usize) {
    if reason == INSTALL_FAILED {
        INTERFACE_CHANGED.store(true, Ordering::Relaxed);
    }
    let _ = HOOK_FAILURE.compare_exchange(0, reason, Ordering::AcqRel, Ordering::Acquire);
    if let Some(shared) = BRIDGE.get() {
        shared.stopped.store(true, Ordering::Release);
        shared.wake.unpark();
    }
}

fn rust_boundary<T>(work: impl FnOnce() -> T) -> Option<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)) {
        Ok(value) => Some(value),
        Err(payload) => {
            fail_hook(RUST_PANIC);
            // A user-supplied panic payload can itself panic in Drop.
            std::mem::forget(payload);
            None
        }
    }
}

fn bridge_lock(shared: &Shared, wait: bool) -> Option<MutexGuard<'_, Bridge>> {
    if HOOK_FAILURE.load(Ordering::Acquire) != 0 {
        return None;
    }
    let result = if wait {
        shared.bridge.lock().map_err(TryLockError::Poisoned)
    } else {
        shared.bridge.try_lock()
    };
    match result {
        Ok(guard) if HOOK_FAILURE.load(Ordering::Acquire) == 0 => Some(guard),
        Err(TryLockError::Poisoned(_)) => {
            fail_hook(POISONED);
            None
        }
        _ => None,
    }
}

// All objects using a patched table use the replacement data path. Steam
// session calls remain outside this Rust boundary and outside Bridge locks.
fn hook_call(work: impl FnOnce(&Shared) -> bool) -> bool {
    if HOOK_FAILURE.load(Ordering::Acquire) != 0 {
        return false;
    }
    rust_boundary(|| {
        BRIDGE
            .get()
            .is_some_and(|shared| !shared.stopped.load(Ordering::Acquire) && work(shared))
    })
    .unwrap_or(false)
}

fn stop_bridge(shared: &Shared) {
    shared.stopped.store(true, Ordering::Release);
    // A fatal state is quarantined until process exit, not repaired or reused.
    // Normal Stop still clears queues and preserves the known-peer tombstones.
    rust_boundary(|| {
        if let Some(mut bridge) = bridge_lock(shared, true) {
            bridge.stop();
        }
    });
}

fn record_telemetry(shared: &Shared) {
    let snapshot = {
        let Some(mut bridge) = bridge_lock(shared, true) else {
            return;
        };
        bridge.telemetry.take_snapshot()
    };
    // No Bridge/Outbox lock held during formatting or file writes.
    let mut records: Vec<_> = snapshot
        .lines()
        .into_iter()
        .map(|line| ("game diagnostics", line))
        .collect();
    for (index, name) in [(0, "send"), (1, "available"), (2, "read"), (6, "session")] {
        records.push((
            "game api",
            format!(
                "operation={name} calls={} lock_busy={} invalid={} native_calls={}",
                API_CALLS[index].load(Ordering::Relaxed),
                API_BUSY[index].load(Ordering::Relaxed),
                API_INVALID[index].load(Ordering::Relaxed),
                API_NATIVE[index].load(Ordering::Relaxed)
            ),
        ));
    }
    records.push((
        "game api",
        format!(
            "callback_ticks={} interface_repairs={} read_truncated={}",
            CALLBACK_TICKS.load(Ordering::Relaxed),
            INTERFACE_REPAIRS.load(Ordering::Relaxed),
            READ_TRUNCATED.load(Ordering::Relaxed)
        ),
    ));
    crate::diagnostics::record_batch(
        "INFO",
        records.iter().map(|(event, line)| (*event, line.as_str())),
    );
}
struct Steam {
    original: [AtomicUsize; 4],
    table: usize,
    installed: AtomicBool,
}
const SLOTS: [usize; 4] = [0, 1, 2, 6];

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllMain(module: HINSTANCE, reason: u32, _: *mut c_void) -> BOOL {
    if reason == 1 {
        DisableThreadLibraryCalls(module);
    }
    1
}

/// Called by our helper after LoadLibraryW has returned, outside the loader lock.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn NetBurrowInit(argument: *const HookInit) -> u32 {
    if !readable(argument as usize, size_of::<HookInit>()) {
        return 1;
    }
    let init = argument.read_unaligned();
    if init.version != HOOK_INIT_VERSION
        || init.port == 0
        || init.port > 65535
        || init.pid != GetCurrentProcessId()
        || init.reserved != 0
        || init.epoch == 0
        || init.nonce == [0; 16]
    {
        return 1;
    }
    if STARTED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return 2;
    }
    if std::thread::Builder::new()
        .name("netburrow-hook".into())
        .spawn(move || {
            crate::diagnostics::init("hook");
            let result = std::panic::catch_unwind(|| initialize(init));
            match &result {
                Ok(Ok(())) => crate::diagnostics::record("INFO", "worker", "stopped"),
                Ok(Err(error)) => crate::diagnostics::record("ERROR", "worker", &error.to_string()),
                Err(_) => {
                    fail_hook(RUST_PANIC);
                    crate::diagnostics::record("ERROR", "worker", "panic; Hook stopped");
                }
            }
            if !matches!(result, Ok(Ok(()))) {
                if let Some(shared) = BRIDGE.get() {
                    stop_bridge(shared);
                    shared.wake.unpark();
                }
            }
        })
        .is_err()
    {
        STARTED.store(false, Ordering::Release);
        return 3;
    }
    0
}

fn initialize(init: HookInit) -> io::Result<()> {
    crate::diagnostics::record(
        "INFO",
        "initialize",
        "waiting for SteamNetworking006 (20s timeout)",
    );
    let limit = Instant::now() + Duration::from_secs(20);
    let mut callbacks_installed = false;
    let (interface, steam_id) = loop {
        let module = unsafe { GetModuleHandleW(wide("steam_api.dll").as_ptr()) };
        if !module.is_null() {
            if !callbacks_installed {
                unsafe {
                    install_callback_imports()?;
                }
                callbacks_installed = true;
            }
            if let Some(info) = unsafe { steam_interface(module) } {
                break info;
            }
        }
        if Instant::now() >= limit {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Steam user or SteamNetworking006 unavailable",
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let shared = Arc::new(Shared {
        bridge: Mutex::new(Bridge::new(steam_id, init.epoch)),
        wake: std::thread::current(),
        stopped: AtomicBool::new(false),
        faults: AtomicBool::new(false),
    });
    BRIDGE
        .set(shared.clone())
        .map_err(|_| io::Error::other("Hook already initialized"))?;
    unsafe {
        install_interface(interface)?;
    }
    crate::diagnostics::record(
        "INFO",
        "interface",
        "TractorBeam strategy: Find/RunCallbacks, shared slots 0/1/2/6; replacement data, native sessions",
    );
    let endpoint = SocketAddr::from(([127, 0, 0, 1], init.port as u16));
    let window = Arc::new(Mutex::new(netburrow_protocol::resume::Window::default()));
    let mut connected_once = false;
    let mut disconnected_at = Instant::now();
    loop {
        if shared.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let connection = (|| -> io::Result<TcpStream> {
            let mut socket = TcpStream::connect_timeout(&endpoint, Duration::from_millis(100))?;
            socket.set_nodelay(true)?;
            socket.set_read_timeout(Some(Duration::from_millis(500)))?;
            socket.set_write_timeout(Some(Duration::from_millis(500)))?;
            if connected_once {
                let received = window
                    .lock()
                    .expect("poisoned Hook state")
                    .received_through();
                write_message(
                    &mut socket,
                    &Message::IpcResume {
                        nonce: init.nonce,
                        pid: init.pid,
                        steam_id,
                        epoch: init.epoch,
                        received,
                    },
                )?;
                match read_message(&mut socket)? {
                    Message::SessionAck(n) => {
                        window.lock().expect("poisoned Hook state").acknowledge(n)?
                    }
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "IPC resume rejected",
                        ));
                    }
                }
            } else {
                write_message(
                    &mut socket,
                    &Message::IpcHelloV2 {
                        nonce: init.nonce,
                        pid: init.pid,
                        steam_id,
                        epoch: init.epoch,
                        capabilities: IPC_CAPABILITIES,
                    },
                )?;
                if read_message(&mut socket)? != Message::IpcAccepted(IPC_CAPABILITIES) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "IPC capability mismatch; update complete package",
                    ));
                }
            }
            Ok(socket)
        })();
        let socket = match connection {
            Ok(socket) => socket,
            Err(e) => {
                if matches!(
                    e.kind(),
                    io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput
                ) || disconnected_at.elapsed() >= Duration::from_secs(3)
                {
                    return Err(e);
                }
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        };
        if connected_once {
            crate::diagnostics::record("INFO", "ipc recovery", "original client session resumed");
        }
        connected_once = true;
        match ipc_connection(socket, shared.clone(), window.clone()) {
            Ok(()) => return Ok(()),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput
                ) =>
            {
                return Err(e);
            }
            Err(e) => {
                disconnected_at = Instant::now();
                crate::diagnostics::record(
                    "WARN",
                    "ipc recovery",
                    &format!("connection interrupted; retrying for 3s: {e}"),
                );
            }
        }
    }
}

fn ipc_connection(
    mut socket: TcpStream,
    shared: Arc<Shared>,
    window: Arc<Mutex<netburrow_protocol::resume::Window>>,
) -> io::Result<()> {
    let broken = Arc::new(AtomicBool::new(false));
    let read_broken = broken.clone();
    socket.set_nodelay(true)?;
    write_message(
        &mut socket,
        &Message::Diagnostic("自己的 SteamNetworking006 接口与回调入口已接入".into()),
    )?;
    write_message(&mut socket, &Message::IpcReady)?;
    socket.set_read_timeout(None)?;
    socket.set_write_timeout(None)?;
    socket.set_nonblocking(true)?;
    crate::diagnostics::record(
        "INFO",
        "ipc",
        "authenticated hello and ready sent to client",
    );
    let mut input = socket.try_clone()?;
    let receive_shared = shared.clone();
    let receive_window = window.clone();
    let reader = std::thread::Builder::new()
        .name("netburrow-ipc-read".into())
        .spawn(move || {
            rust_boundary(|| {
                let mut decoder = Reader::default();
                loop {
                    if receive_shared.stopped.load(Ordering::Acquire) {
                        break;
                    }
                    let mut message = match decoder.poll(&mut input) {
                        Ok(Some(message)) => message,
                        Ok(None) => {
                            std::thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(error) => {
                            crate::diagnostics::record("ERROR", "ipc read", &error.to_string());
                            if matches!(
                                error.kind(),
                                io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput
                            ) {
                                receive_shared.stopped.store(true, Ordering::Release);
                            }
                            break;
                        }
                    };
                    let mut sequence = None;
                    match &message {
                        Message::SessionAck(n) => {
                            if receive_window
                                .lock()
                                .expect("poisoned Hook state")
                                .acknowledge(*n)
                                .is_err()
                            {
                                receive_shared.stopped.store(true, Ordering::Release);
                                break;
                            }
                            continue;
                        }
                        Message::SessionFrame { sequence: n, body } => {
                            match receive_window
                                .lock()
                                .expect("poisoned Hook state")
                                .classify(*n)
                            {
                                Ok(false) => continue,
                                Ok(true) => {}
                                Err(_) => {
                                    receive_shared.stopped.store(true, Ordering::Release);
                                    break;
                                }
                            }
                            sequence = Some(*n);
                            match netburrow_protocol::decode_session_body(body) {
                                Ok(m) => message = m,
                                Err(_) => {
                                    receive_shared.stopped.store(true, Ordering::Release);
                                    break;
                                }
                            }
                        }
                        _ => {}
                    }
                    // Logging can touch disk; keep it outside the game send lock.
                    match &message {
                        Message::IpcReady => crate::diagnostics::record(
                            "INFO",
                            "ipc",
                            "client acknowledged Relay binding; forwarding active",
                        ),
                        Message::Stop => {
                            crate::diagnostics::record("INFO", "ipc", "client requested stop")
                        }
                        Message::Members(_)
                        | Message::Data(_)
                        | Message::IpcPeerFault { .. }
                        | Message::Pong(_) => {}
                        _ => crate::diagnostics::record("ERROR", "ipc", "unexpected message type"),
                    }
                    let Some(mut bridge) = bridge_lock(&receive_shared, true) else {
                        break;
                    };
                    let valid = match message {
                        Message::Members(members) => {
                            bridge.members(&members);
                            true
                        }
                        Message::IpcReady => {
                            if !bridge.stopped {
                                bridge.active = true;
                            }
                            true
                        }
                        Message::Data(packet) => {
                            if matches!(bridge.receive(packet), Admission::PeerFailed(..)) {
                                receive_shared.faults.store(true, Ordering::Release);
                            }
                            true
                        }
                        Message::IpcPeerFault { peer, epoch } => {
                            bridge.fail_peer(peer, epoch);
                            receive_shared.faults.store(true, Ordering::Release);
                            true
                        }
                        Message::Pong(_) => true,
                        Message::Stop => false,
                        _ => false,
                    };
                    drop(bridge);
                    if valid {
                        if let Some(n) = sequence {
                            let _ = receive_window
                                .lock()
                                .expect("poisoned Hook state")
                                .received(n);
                        }
                    }
                    if !valid {
                        receive_shared.stopped.store(true, Ordering::Release);
                        crate::diagnostics::record(
                            "INFO",
                            "ipc",
                            "receiver stopped; protocol error or explicit Stop",
                        );
                        break;
                    }
                }
            });
            read_broken.store(true, Ordering::Release);
            receive_shared.wake.unpark();
        })?;
    let mut ping = Instant::now() - Duration::from_secs(2);
    let mut callback_report = Instant::now() - Duration::from_secs(10);
    let mut health_report = Instant::now();
    let mut pending: Option<PendingWrite> = None;
    let result = rust_boundary(|| {
    let mut replay:std::collections::VecDeque<_>=window.lock().expect("poisoned Hook state").pending().into();
    let mut last_ack=u64::MAX;
    let outbound = match bridge_lock(&shared, true) {
        Some(bridge) => bridge.outbound.clone(),
        None => return Ok(()),
    };
    loop {
        if shared.stopped.load(Ordering::Acquire) {
            break Ok(());
        }
        if broken.load(Ordering::Acquire){break Err(io::Error::new(io::ErrorKind::ConnectionReset,"IPC reader disconnected"));}
        if let Some(frame) = pending.as_mut() {
            match frame.poll(&mut socket) {
                Ok(true) => pending = None,
                Ok(false) => {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(e) => break Err(e),
            }
        }
        if callback_report.elapsed() >= Duration::from_secs(10) {
            let rejected = match bridge_lock(&shared, true) {
                Some(bridge) => bridge.send_rejections,
                None => break Ok(()),
            };
            crate::diagnostics::record(
                "INFO",
                "send rejections",
                &format!(
                    "bridge_busy={} queue_busy={} unavailable={} invalid={} global_full={} peer_full={}",
                    SEND_LOCK_BUSY.load(Ordering::Relaxed),
                    rejected.queue_busy,
                    rejected.unavailable,
                    rejected.invalid + SEND_INVALID.load(Ordering::Relaxed) as u64,
                    rejected.global_full,
                    rejected.peer_full,
                ),
            );
            callback_report = Instant::now();
            record_telemetry(&shared);
        }
        let received=window.lock().expect("poisoned Hook state").received_through();
        let message = if received!=last_ack {last_ack=received;Some(Message::SessionAck(received))}
        else if let Some((sequence,body))=replay.pop_front(){Some(Message::SessionFrame{sequence,body})}
        else if ping.elapsed() >= Duration::from_secs(2) {
            ping = Instant::now();
            Some(Message::Ping(0))
        } else if shared.faults.load(Ordering::Acquire) {
            let Some(mut bridge) = bridge_lock(&shared, true) else { break Ok(()); };
            match bridge.pop_fault() {
                Some((peer, epoch)) => Some(Message::IpcPeerFault { peer, epoch }),
                None => {
                    shared.faults.store(false, Ordering::Release);
                    None
                }
            }
        } else if health_report.elapsed() >= Duration::from_secs(1) {
            health_report = Instant::now();
            let mut health = match bridge_lock(&shared, true) {
                Some(bridge) => bridge.health(),
                None => break Ok(()),
            };
            health.lock_busy += LOCK_BUSY.load(Ordering::Relaxed) as u64;
            health.interface_changed = INTERFACE_CHANGED.load(Ordering::Relaxed);
            Some(Message::IpcHealth(health))
        } else if let Some(packet) = outbound.pop() {
            Some(Message::Data(packet))
        } else {
            None
        };
        if let Some(message) = message {
            let message=if netburrow_protocol::replayable(&message) {
                let body=match netburrow_protocol::encode(&message) {Ok(body)=>body,Err(e)=>break Err(e)};
                let sequence=match window.lock().expect("poisoned Hook state").retain(body.clone()) {Ok(n)=>n,Err(e)=>break Err(io::Error::new(io::ErrorKind::InvalidData,format!("IPC resume buffer exhausted: {e}")))};
                Message::SessionFrame{sequence,body}
            }else{message};
            match PendingWrite::new(&message) {
                Ok(frame) => pending = Some(frame),
                Err(e) => break Err(e),
            }
        } else {
            std::thread::park_timeout(Duration::from_millis(10));
        }
    }
    }).unwrap_or_else(|| Err(io::Error::other("Hook Rust panic")));
    if shared.stopped.load(Ordering::Acquire) {
        stop_bridge(&shared);
    }
    if HOOK_FAILURE.load(Ordering::Acquire) == 0 {
        rust_boundary(|| record_telemetry(&shared));
    }
    if HOOK_FAILURE.load(Ordering::Acquire) != 0 {
        // Finish any partial frame before sending Stop; never splice a control
        // frame into partially written data. This path uses no Bridge/Outbox.
        if let Err(error) = notify_failure(&mut socket, pending.take()) {
            crate::diagnostics::record("ERROR", "hook stop notification", &error.to_string());
        }
    }
    let _ = socket.shutdown(Shutdown::Both);
    let _ = reader.join();
    if shared.stopped.load(Ordering::Acquire) {
        stop_bridge(&shared);
    }
    if HOOK_FAILURE.load(Ordering::Acquire) != 0 {
        Ok(())
    } else {
        result
    }
}

fn notify_failure(socket: &mut TcpStream, pending: Option<PendingWrite>) -> io::Result<()> {
    let reason = match HOOK_FAILURE.load(Ordering::Acquire) {
        INSTALL_FAILED => {
            "游戏 SteamNetworking006 接入安装或恢复失败，当前游戏接入已停止，请退出游戏后重开"
        }
        POISONED => "Hook 内部锁状态异常，当前游戏接入已停止，请退出游戏后重开",
        _ => "Hook 内部 Rust 异常，当前游戏接入已停止，请退出游戏后重开",
    };
    crate::diagnostics::record("ERROR", "hook stopped", reason);
    let deadline = Instant::now() + Duration::from_millis(500);
    for mut frame in pending.into_iter().chain([
        PendingWrite::new(&Message::Diagnostic(reason.into()))?,
        PendingWrite::new(&Message::Stop)?,
    ]) {
        loop {
            if frame.poll(socket)? {
                break;
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Hook stop notification timed out",
                ));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    Ok(())
}

unsafe fn steam_interface(module: HMODULE) -> Option<(usize, u64)> {
    let user_handle: unsafe extern "C" fn() -> i32 = transmute(GetProcAddress(
        module,
        c"SteamAPI_GetHSteamUser".as_ptr().cast(),
    )?);
    // Restore the original startup gate: do not request interfaces before Steam
    // has initialized its user handle in the game process.
    if user_handle() == 0 {
        return None;
    }
    let networking: unsafe extern "C" fn() -> *mut c_void = transmute(GetProcAddress(
        module,
        c"SteamAPI_SteamNetworking_v006".as_ptr().cast(),
    )?);
    let user: unsafe extern "C" fn() -> *mut c_void = transmute(GetProcAddress(
        module,
        c"SteamAPI_SteamUser_v023".as_ptr().cast(),
    )?);
    let identity: unsafe extern "C" fn(*mut c_void) -> u64 = transmute(GetProcAddress(
        module,
        c"SteamAPI_ISteamUser_GetSteamID".as_ptr().cast(),
    )?);
    let user = user();
    if user.is_null() {
        return None;
    }
    let id = identity(user);
    let interface = networking();
    (id != 0 && !interface.is_null()).then_some((interface as usize, id))
}

type FindInterface = unsafe extern "C" fn(i32, *const c_char) -> *mut c_void;
static FIND_INTERFACE: AtomicUsize = AtomicUsize::new(0);
static RUN_CALLBACKS: AtomicUsize = AtomicUsize::new(0);

unsafe fn probe_interface(module: HMODULE) -> io::Result<()> {
    let find = GetProcAddress(
        module,
        c"SteamInternal_FindOrCreateUserInterface".as_ptr().cast(),
    )
    .map(|f| f as usize)
    .unwrap_or_else(|| FIND_INTERFACE.load(Ordering::Acquire));
    if find == 0 {
        return Err(io::Error::other("Steam interface factory unavailable"));
    }
    let user = GetProcAddress(module, c"SteamAPI_GetHSteamUser".as_ptr().cast())
        .map(|f| transmute::<_, unsafe extern "C" fn() -> i32>(f)())
        .unwrap_or(0);
    let find: FindInterface = transmute(find);
    let object = find(user, c"SteamNetworking006".as_ptr());
    if !object.is_null() {
        install_interface(object as usize)?;
    }
    Ok(())
}

unsafe fn install_interface(object: usize) -> io::Result<()> {
    let _installing = INSTALL_LOCK
        .lock()
        .map_err(|_| io::Error::other("poisoned interface installation"))?;
    if object % size_of::<usize>() != 0 || !readable(object, size_of::<usize>()) {
        return Err(io::Error::other("invalid networking object"));
    }
    let table = (object as *const usize).read();
    if table % size_of::<usize>() != 0 || !readable(table, 7 * size_of::<usize>()) {
        return Err(io::Error::other("invalid SteamNetworking006 vtable"));
    }
    let saved = {
        let mut registered = STEAM.lock().expect("poisoned interface registry");
        if let Some(saved) = registered.iter().find(|s| s.table == table) {
            saved.clone()
        } else {
            let pointers: [usize; 4] = std::array::from_fn(|i| {
                (*(table as *const AtomicUsize).add(SLOTS[i])).load(Ordering::Acquire)
            });
            if pointers
                .iter()
                .any(|&p| !x86_method(p) || replacements().contains(&p))
            {
                return Err(io::Error::other("unsupported networking methods"));
            }
            let saved = Arc::new(Steam {
                original: pointers.map(AtomicUsize::new),
                table,
                installed: AtomicBool::new(false),
            });
            // Publish originals before any patched entry can execute.
            registered.push(saved.clone());
            saved
        }
    };
    patch_slots(&saved)
}

fn replacements() -> [usize; 4] {
    [
        send as *const () as usize,
        available as *const () as usize,
        read as *const () as usize,
        session as *const () as usize,
    ]
}

unsafe fn patch_slots(saved: &Steam) -> io::Result<()> {
    let protections = slot_protections(saved.table)?;
    let replacement = replacements();
    let mut changed = Vec::new();
    for i in 0..SLOTS.len() {
        let address = saved.table + SLOTS[i] * size_of::<usize>();
        let previous = (*(address as *const AtomicUsize)).load(Ordering::Acquire);
        if previous == replacement[i] {
            continue;
        }
        if !x86_method(previous) || replacement.contains(&previous) {
            rollback_slots(saved, &changed, &protections);
            return Err(io::Error::other("invalid restored networking method"));
        }
        // Refresh the native function before reinstalling, as in TractorBeam.
        saved.original[i].store(previous, Ordering::Release);
        changed.push((i, previous));
        if let Err(error) = compare_pointer(address, previous, replacement[i], protections[i]) {
            rollback_slots(saved, &changed, &protections);
            return Err(error);
        }
    }
    if !changed.is_empty() && saved.installed.load(Ordering::Acquire) {
        INTERFACE_REPAIRS.fetch_add(1, Ordering::Relaxed);
        // No file I/O on the game's callback thread; the worker's telemetry
        // records the repair count without marking a repaired entry as failed.
    }
    saved.installed.store(true, Ordering::Release);
    Ok(())
}

unsafe fn rollback_slots(saved: &Steam, changed: &[(usize, usize)], protections: &[u32; 4]) {
    for &(i, previous) in changed.iter().rev() {
        let _ = compare_pointer(
            saved.table + SLOTS[i] * size_of::<usize>(),
            replacements()[i],
            previous,
            protections[i],
        );
    }
}

fn slot_protections(table: usize) -> io::Result<[u32; 4]> {
    let mut protections = [0; 4];
    for (i, protection) in protections.iter_mut().enumerate() {
        let address = table + SLOTS[i] * size_of::<usize>();
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { zeroed() };
        if !readable(address, size_of::<usize>())
            || unsafe {
                VirtualQuery(
                    address as *const c_void,
                    &mut info,
                    size_of::<MEMORY_BASIC_INFORMATION>(),
                )
            } == 0
        {
            return Err(io::Error::other("unreadable networking slot"));
        }
        if info.Protect
            & (PAGE_EXECUTE | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY)
            != 0
        {
            return Err(io::Error::other(
                "networking vtable unexpectedly shares executable memory",
            ));
        }
        *protection = info.Protect;
    }
    Ok(protections)
}

unsafe fn compare_pointer(
    address: usize,
    expected: usize,
    value: usize,
    protection: u32,
) -> io::Result<()> {
    if address % size_of::<usize>() != 0 || !readable(address, size_of::<usize>()) {
        return Err(io::Error::other("invalid patch address"));
    }
    let mut old = 0;
    if VirtualProtect(
        address as *const c_void,
        size_of::<usize>(),
        PAGE_READWRITE,
        &mut old,
    ) == 0
    {
        return Err(io::Error::last_os_error());
    }
    let previous = (*(address as *const AtomicUsize)).compare_exchange(
        expected,
        value,
        Ordering::AcqRel,
        Ordering::Acquire,
    );
    let mut ignored = 0;
    if VirtualProtect(
        address as *const c_void,
        size_of::<usize>(),
        protection,
        &mut ignored,
    ) == 0
    {
        return Err(io::Error::other(format!(
            "patch page protection restoration failed: {}",
            io::Error::last_os_error()
        )));
    }
    if previous.is_err_and(|actual| actual != value) {
        return Err(io::Error::other("patch slot changed by another writer"));
    }
    Ok(())
}

fn x86_method(address: usize) -> bool {
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { zeroed() };
    if !executable(address)
        || unsafe {
            VirtualQuery(
                address as *const c_void,
                &mut info,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        } == 0
    {
        return false;
    }
    let base = info.AllocationBase as usize;
    if !readable(base, 64) {
        return false;
    }
    unsafe {
        if (base as *const u16).read_unaligned() != 0x5a4d {
            return false;
        }
        let offset = ((base + 60) as *const u32).read_unaligned() as usize;
        if offset > 16 * 1024 * 1024 || !readable(base + offset, 26) {
            return false;
        }
        ((base + offset) as *const u32).read_unaligned() == 0x4550
            && ((base + offset + 4) as *const u16).read_unaligned() == 0x14c
            && ((base + offset + 24) as *const u16).read_unaligned() == 0x10b
    }
}

fn readable(address: usize, bytes: usize) -> bool {
    if address == 0 || bytes == 0 {
        return false;
    }
    let Some(end) = address.checked_add(bytes) else {
        return false;
    };
    let mut cursor = address;
    while cursor < end {
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { zeroed() };
        if unsafe {
            VirtualQuery(
                cursor as *const c_void,
                &mut info,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        } == 0
            || info.State != MEM_COMMIT
            || info.Protect & (PAGE_NOACCESS | PAGE_GUARD) != 0
            || info.Protect
                & (PAGE_READONLY
                    | PAGE_READWRITE
                    | PAGE_WRITECOPY
                    | PAGE_EXECUTE_READ
                    | PAGE_EXECUTE_READWRITE
                    | PAGE_EXECUTE_WRITECOPY)
                == 0
        {
            return false;
        }
        let Some(next) = (info.BaseAddress as usize).checked_add(info.RegionSize) else {
            return false;
        };
        if next <= cursor {
            return false;
        }
        cursor = next;
    }
    true
}
fn executable(address: usize) -> bool {
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { zeroed() };
    (unsafe {
        VirtualQuery(
            address as *const c_void,
            &mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    }) != 0
        && info.State == MEM_COMMIT
        && info.Protect & (PAGE_GUARD | PAGE_NOACCESS) == 0
        && info.Protect
            & (PAGE_EXECUTE | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY)
            != 0
}

unsafe extern "thiscall" fn send(
    _this: *mut c_void,
    remote: u64,
    data: *const c_void,
    length: u32,
    kind: i32,
    channel: i32,
) -> bool {
    hook_call(|shared| {
        API_CALLS[0].fetch_add(1, Ordering::Relaxed);
        if length as usize > MAX_PAYLOAD
            || !(0..=3).contains(&kind)
            || (length > 0 && data.is_null())
        {
            API_INVALID[0].fetch_add(1, Ordering::Relaxed);
            SEND_INVALID.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let Some(mut bridge) = bridge_lock(shared, true) else {
            return false;
        };
        let bytes = if length == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(data.cast::<u8>(), length as usize)
        };
        let sent = bridge
            .send(remote, bytes, kind as u8, channel)
            .unwrap_or(false);
        drop(bridge);
        if sent {
            shared.wake.unpark();
        }
        sent
    })
}

unsafe extern "thiscall" fn available(_this: *mut c_void, size: *mut u32, channel: i32) -> bool {
    hook_call(|shared| {
        API_CALLS[1].fetch_add(1, Ordering::Relaxed);
        let Some(mut bridge) = bridge_lock(shared, true) else {
            return false;
        };
        if let Ok(Some(length)) = bridge.available(channel) {
            if !size.is_null() {
                size.write_unaligned(length as u32);
            }
            return true;
        }
        false
    })
}

unsafe extern "thiscall" fn read(
    _this: *mut c_void,
    destination: *mut c_void,
    capacity: u32,
    size: *mut u32,
    remote: *mut u64,
    channel: i32,
) -> bool {
    hook_call(|shared| {
        API_CALLS[2].fetch_add(1, Ordering::Relaxed);
        if destination.is_null() {
            API_INVALID[2].fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let Some(mut bridge) = bridge_lock(shared, true) else {
            return false;
        };
        let Ok(Some(packet)) = bridge.read(channel) else {
            return false;
        };
        drop(bridge);
        let count = packet.payload.len().min(capacity as usize);
        if count < packet.payload.len() {
            READ_TRUNCATED.fetch_add(1, Ordering::Relaxed);
        }
        if count > 0 {
            ptr::copy_nonoverlapping(packet.payload.as_ptr(), destination.cast::<u8>(), count);
        }
        if !size.is_null() {
            size.write_unaligned(count as u32);
        }
        if !remote.is_null() {
            remote.write_unaligned(packet.from);
        }
        true
    })
}

#[repr(C)]
struct SessionState {
    active: u8,
    connecting: u8,
    error: u8,
    relay: u8,
    bytes: i32,
    packets: i32,
    ip: u32,
    port: u16,
}
const _: () = assert!(
    size_of::<SessionState>() == 20
        && std::mem::align_of::<SessionState>() == 4
        && std::mem::offset_of!(SessionState, bytes) == 4
        && std::mem::offset_of!(SessionState, packets) == 8
        && std::mem::offset_of!(SessionState, ip) == 12
        && std::mem::offset_of!(SessionState, port) == 16
);

unsafe extern "thiscall" fn session(
    this: *mut c_void,
    remote: u64,
    result: *mut SessionState,
) -> bool {
    // Native session state is never synthesized, including after Stop/failure.
    let address = rust_boundary(|| {
        if !readable(this as usize, size_of::<usize>()) {
            return None;
        }
        let table = (this as *const usize).read_unaligned();
        STEAM
            .lock()
            .expect("poisoned interface registry")
            .iter()
            .find(|s| s.table == table)
            .map(|s| s.original[3].load(Ordering::Acquire))
    })
    .flatten();
    let Some(address) = address else {
        return false;
    };
    let f: unsafe extern "thiscall" fn(*mut c_void, u64, *mut SessionState) -> bool =
        transmute(address);
    API_CALLS[6].fetch_add(1, Ordering::Relaxed);
    API_NATIVE[6].fetch_add(1, Ordering::Relaxed);
    f(this, remote, result)
}

unsafe fn install_callback_imports() -> io::Result<()> {
    let image = GetModuleHandleW(ptr::null()) as usize;
    if !readable(image, 64) || (image as *const u16).read_unaligned() != 0x5a4d {
        return Err(io::Error::other("main image unavailable"));
    }
    let header = ((image + 60) as *const u32).read_unaligned() as usize;
    if header > 16 * 1024 * 1024
        || !readable(image + header, 256)
        || ((image + header) as *const u32).read_unaligned() != 0x4550
    {
        return Err(io::Error::other("invalid main PE header"));
    }
    let optional = image + header + 24;
    if (optional as *const u16).read_unaligned() != 0x10b {
        return Err(io::Error::other("expected PE32 imports"));
    }
    let image_size = ((optional + 56) as *const u32).read_unaligned() as usize;
    let import_rva = ((optional + 104) as *const u32).read_unaligned() as usize;
    let import_size = ((optional + 108) as *const u32).read_unaligned() as usize;
    let in_image = |rva: usize, size: usize| {
        rva.checked_add(size).is_some_and(|end| end <= image_size) && readable(image + rva, size)
    };
    if import_rva == 0 || !in_image(import_rva, import_size) {
        return Err(io::Error::other("Steam imports unavailable"));
    }
    let mut patches = Vec::new();
    let mut descriptor = import_rva;
    while descriptor + 20 <= import_rva + import_size && in_image(descriptor, 20) {
        let fields = std::slice::from_raw_parts((image + descriptor) as *const u32, 5);
        let (lookup, names, slots) = (fields[0] as usize, fields[3] as usize, fields[4] as usize);
        if names == 0 {
            break;
        }
        if in_image(names, 14)
            && bounded_name(image + names, image_size - names)
                .is_some_and(|s| s.eq_ignore_ascii_case(b"steam_api.dll"))
        {
            if lookup == 0 {
                return Err(io::Error::other("Steam import names unavailable"));
            }
            for index in 0..1024 {
                if !in_image(lookup + index * 4, 4) || !in_image(slots + index * 4, 4) {
                    break;
                }
                let name = ((image + lookup + index * 4) as *const u32).read_unaligned() as usize;
                if name == 0 {
                    break;
                }
                if name & 0x80000000 != 0 || !in_image(name, 3) {
                    continue;
                }
                let Some(name) = bounded_name(image + name + 2, image_size - name - 2) else {
                    continue;
                };
                let (replacement, original) = match name {
                    b"SteamAPI_RunCallbacks" => {
                        (run_callbacks as *const () as usize, &RUN_CALLBACKS)
                    }
                    b"SteamInternal_FindOrCreateUserInterface" => {
                        (find_interface as *const () as usize, &FIND_INTERFACE)
                    }
                    _ => continue,
                };
                let address = image + slots + index * 4;
                let previous = (address as *const usize).read_unaligned();
                if !executable(previous) {
                    return Err(io::Error::other("invalid Steam import"));
                }
                let mut info: MEMORY_BASIC_INFORMATION = zeroed();
                if VirtualQuery(
                    address as *const c_void,
                    &mut info,
                    size_of::<MEMORY_BASIC_INFORMATION>(),
                ) == 0
                {
                    return Err(io::Error::last_os_error());
                }
                patches.push((address, previous, replacement, original, info.Protect));
            }
        }
        descriptor += 20;
    }
    if patches.len() != 2
        || !patches.iter().any(|p| ptr::eq(p.3, &RUN_CALLBACKS))
        || !patches.iter().any(|p| ptr::eq(p.3, &FIND_INTERFACE))
    {
        return Err(io::Error::other(
            "required Steam Find/RunCallbacks imports missing",
        ));
    }
    // Save both originals before publishing either hook; rollback only our slots.
    for &(_, previous, _, original, _) in &patches {
        original.store(previous, Ordering::Release);
    }
    for (i, &(address, previous, replacement, _, protection)) in patches.iter().enumerate() {
        if let Err(error) = compare_pointer(address, previous, replacement, protection) {
            for &(address, previous, replacement, _, protection) in patches[..=i].iter().rev() {
                let _ = compare_pointer(address, replacement, previous, protection);
            }
            return Err(error);
        }
    }
    Ok(())
}

unsafe fn bounded_name<'a>(address: usize, max: usize) -> Option<&'a [u8]> {
    for size in 0..max.min(128) {
        if !readable(address + size, 1) {
            return None;
        }
        if ((address + size) as *const u8).read() == 0 {
            return Some(std::slice::from_raw_parts(address as *const u8, size));
        }
    }
    None
}

unsafe extern "C" fn find_interface(user: i32, version: *const c_char) -> *mut c_void {
    let address = FIND_INTERFACE.load(Ordering::Acquire);
    if address == 0 {
        return ptr::null_mut();
    }
    let f: FindInterface = transmute(address);
    let object = f(user, version);
    // Imports are observed before startup completes; keep native behavior until
    // the worker has acquired both the Steam identity and initial interface.
    if !object.is_null() && BRIDGE.get().is_some() && HOOK_FAILURE.load(Ordering::Acquire) == 0 {
        rust_boundary(|| {
            if bounded_name(version as usize, 128) == Some(b"SteamNetworking006")
                && install_interface(object as usize).is_err()
            {
                fail_hook(INSTALL_FAILED);
            }
        });
    }
    object
}

unsafe extern "C" fn run_callbacks() {
    let address = RUN_CALLBACKS.load(Ordering::Acquire);
    if address == 0 {
        return;
    }
    let f: unsafe extern "C" fn() = transmute(address);
    f();
    if BRIDGE.get().is_none() {
        return;
    }
    let count = CALLBACK_TICKS
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    if count != 1 && count % 60 != 0 || HOOK_FAILURE.load(Ordering::Acquire) != 0 {
        return;
    }
    let tables = rust_boundary(|| STEAM.lock().expect("poisoned interface registry").clone());
    let Some(tables) = tables else {
        return;
    };
    if tables.is_empty() {
        // Only probe when no interface has been registered. Once installed,
        // callbacks repair the cached tables without calling the factory again.
        let module = GetModuleHandleW(wide("steam_api.dll").as_ptr());
        if !module.is_null() {
            rust_boundary(|| {
                if probe_interface(module).is_err() {
                    fail_hook(INSTALL_FAILED);
                }
            });
        }
    } else {
        rust_boundary(|| {
            let _installing = INSTALL_LOCK
                .lock()
                .expect("poisoned interface installation");
            for saved in &tables {
                if patch_slots(saved).is_err() {
                    fail_hook(INSTALL_FAILED);
                    break;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{MEM_RELEASE, MEM_RESERVE, VirtualAlloc, VirtualFree};

    unsafe extern "thiscall" fn native() -> bool {
        true
    }
    unsafe extern "thiscall" fn restored() -> bool {
        false
    }

    #[test]
    fn four_slot_installation_repairs_restores_and_preserves_page_protection() {
        unsafe {
            let page = VirtualAlloc(ptr::null(), 4096, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
            assert!(!page.is_null());
            let table = page as *mut AtomicUsize;
            for i in 0..7 {
                table
                    .add(i)
                    .write(AtomicUsize::new(native as *const () as usize));
            }
            let saved = Steam {
                original: std::array::from_fn(|_| AtomicUsize::new(native as *const () as usize)),
                table: page as usize,
                installed: AtomicBool::new(false),
            };
            let mut old = 0;
            assert_ne!(VirtualProtect(page, 4096, PAGE_READONLY, &mut old), 0);
            patch_slots(&saved).unwrap();
            for i in 3..6 {
                assert_eq!(
                    (*table.add(i)).load(Ordering::Acquire),
                    native as *const () as usize
                );
            }
            compare_pointer(
                page as usize + 6 * size_of::<usize>(),
                replacements()[3],
                restored as *const () as usize,
                PAGE_READONLY,
            )
            .unwrap();
            patch_slots(&saved).unwrap();
            assert_eq!(
                saved.original[3].load(Ordering::Acquire),
                restored as *const () as usize
            );
            for (i, slot) in SLOTS.iter().enumerate() {
                assert_eq!(
                    (*table.add(*slot)).load(Ordering::Acquire),
                    replacements()[i]
                );
            }
            assert_eq!(slot_protections(page as usize).unwrap(), [PAGE_READONLY; 4]);
            // A later invalid slot rolls back earlier writes without replacing
            // the conflicting writer's value.
            compare_pointer(
                page as usize,
                replacements()[0],
                native as *const () as usize,
                PAGE_READONLY,
            )
            .unwrap();
            compare_pointer(page as usize + 4, replacements()[1], 0x1234, PAGE_READONLY).unwrap();
            assert!(patch_slots(&saved).is_err());
            assert_eq!(
                (*table).load(Ordering::Acquire),
                native as *const () as usize
            );
            assert_eq!((*table.add(1)).load(Ordering::Acquire), 0x1234);
            assert_eq!(slot_protections(page as usize).unwrap(), [PAGE_READONLY; 4]);
            assert_ne!(VirtualFree(page, 0, MEM_RELEASE), 0);
        }

        // A factory result arriving during another installation must wait,
        // not return an unregistered table that callbacks can never discover.
        let blocked = INSTALL_LOCK.lock().unwrap();
        let (ready, completed) = std::sync::mpsc::channel();
        let mut workers = Vec::new();
        let mut objects = Vec::new();
        let started = Arc::new(std::sync::Barrier::new(3));
        for _ in 0..2 {
            let table = Box::leak(Box::new([native as *const () as usize; 7]));
            let object = Box::leak(Box::new(table.as_ptr() as usize)) as *mut usize as usize;
            objects.push(object);
            let ready = ready.clone();
            let started = started.clone();
            workers.push(std::thread::spawn(move || {
                started.wait();
                let result = unsafe { install_interface(object) };
                ready.send(result.is_ok()).unwrap();
                result.unwrap();
            }));
        }
        started.wait();
        assert!(completed.recv_timeout(Duration::from_millis(50)).is_err());
        drop(blocked);
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(completed.recv().unwrap() && completed.recv().unwrap());
        for object in objects {
            let table = unsafe { *(object as *const usize) as *const usize };
            for (i, slot) in SLOTS.iter().enumerate() {
                assert_eq!(unsafe { *table.add(*slot) }, replacements()[i]);
            }
        }
    }

    #[test]
    fn rust_failure_stops_ipc_without_reusing_poison_or_native_data_fallback() {
        let shared = Arc::new(Shared {
            bridge: Mutex::new(Bridge::new(101, 11)),
            wake: std::thread::current(),
            stopped: AtomicBool::new(false),
            faults: AtomicBool::new(false),
        });
        assert!(BRIDGE.set(shared.clone()).is_ok());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let socket = listener.accept().unwrap().0;
        let worker_shared = shared.clone();
        let worker = std::thread::spawn(move || {
            ipc_connection(socket, worker_shared, Arc::new(Mutex::default()))
        });
        assert!(matches!(
            read_message(&mut client).unwrap(),
            Message::Diagnostic(_)
        ));
        assert_eq!(read_message(&mut client).unwrap(), Message::IpcReady);
        assert!(!hook_call(|shared| {
            let _guard = shared.bridge.lock().unwrap();
            panic!("test panic");
        }));
        let mut explained = false;
        loop {
            match read_message(&mut client).unwrap() {
                Message::Diagnostic(text) => explained |= text.contains("已停止"),
                Message::Stop => break,
                _ => {}
            }
        }
        worker.join().unwrap().unwrap();
        assert!(explained && shared.stopped.load(Ordering::Acquire));
        assert!(shared.bridge.is_poisoned());
        assert!(!unsafe { send(ptr::null_mut(), 202, b"test".as_ptr().cast(), 4, 2, 0) });
        HOOK_FAILURE.store(0, Ordering::Release);
        assert!(bridge_lock(&shared, true).is_none());
        assert_eq!(HOOK_FAILURE.load(Ordering::Acquire), POISONED);
        stop_bridge(&shared);
        assert!(shared.bridge.is_poisoned());
    }
}
