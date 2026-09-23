// ABI source: Valve's public SteamNetworking006 / CCallbackBase headers:
// https://github.com/ValveSoftware/source-sdk-2013/tree/master/src/public/steam
// The interface/ABI definitions are used here; the adapter and lifecycle are our own.
#![allow(unsafe_op_in_unsafe_fn)]

use crate::queue::Bridge;
use netburrow_protocol::{
    HOOK_INIT_VERSION, HookInit, IPC_CAPABILITIES, MAX_PAYLOAD, Message,
    local::{Admission, PendingWrite, Reader},
    read_message, write_message,
};
use std::{
    ffi::c_void,
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
static STEAM: OnceLock<Steam> = OnceLock::new();
static LOCK_BUSY: AtomicUsize = AtomicUsize::new(0);
static SEND_LOCK_BUSY: AtomicUsize = AtomicUsize::new(0);
static SEND_INVALID: AtomicUsize = AtomicUsize::new(0);
static INTERFACE_CHANGED: AtomicBool = AtomicBool::new(false);
static CALLBACK_TICKS: AtomicUsize = AtomicUsize::new(0);
// Indices follow the SteamNetworking006 vtable: send, available, read, accept,
// close session, close channel, session state. Atomic updates only on game threads.
static API_CALLS: [AtomicUsize; 7] = [const { AtomicUsize::new(0) }; 7];
static API_BUSY: [AtomicUsize; 7] = [const { AtomicUsize::new(0) }; 7];
static API_INVALID: [AtomicUsize; 7] = [const { AtomicUsize::new(0) }; 7];
static API_NATIVE: [AtomicUsize; 7] = [const { AtomicUsize::new(0) }; 7];
static NATIVE_DISCARDED: AtomicUsize = AtomicUsize::new(0);
static READ_TRUNCATED: AtomicUsize = AtomicUsize::new(0);
// First failure wins. The game-thread failure path never locks or touches queues.
static HOOK_FAILURE: AtomicUsize = AtomicUsize::new(0);
const RUST_PANIC: usize = 1;
const POISONED: usize = 2;
const CHANGED: usize = 3;

fn fail_hook(reason: usize) {
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

// None means native forwarding. Steam calls must remain outside this boundary.
fn hook_call(this: *mut c_void, work: impl FnOnce(&Shared) -> Option<bool>) -> Option<bool> {
    let steam = STEAM.get()?;
    if this as usize != steam.object || !steam.installed.load(Ordering::Acquire) {
        return None;
    }
    if HOOK_FAILURE.load(Ordering::Acquire) != 0 {
        return Some(false);
    }
    rust_boundary(|| BRIDGE.get().map_or(Some(false), |shared| work(shared))).unwrap_or(Some(false))
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
    for (index, name) in [
        "send",
        "available",
        "read",
        "accept",
        "close",
        "close_channel",
        "session",
    ]
    .iter()
    .enumerate()
    {
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
            "callback_ticks={} native_discarded={} read_truncated={}",
            CALLBACK_TICKS.load(Ordering::Relaxed),
            NATIVE_DISCARDED.load(Ordering::Relaxed),
            READ_TRUNCATED.load(Ordering::Relaxed)
        ),
    ));
    crate::diagnostics::record_batch(
        "INFO",
        records.iter().map(|(event, line)| (*event, line.as_str())),
    );
}
struct Steam {
    original: [usize; 7],
    object: usize,
    table: usize,
    installed: AtomicBool,
}

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
    let mut callback_installed = false;
    let (interface, steam_id) = loop {
        let module = unsafe { GetModuleHandleW(wide("steam_api.dll").as_ptr()) };
        if !module.is_null() {
            if !callback_installed {
                unsafe {
                    install_callback_imports()?;
                }
                callback_installed = true;
                crate::diagnostics::record(
                    "INFO",
                    "callbacks",
                    "SteamAPI_RunCallbacks observed; Steam retains callback registration and dispatch",
                );
            }
            if let Some(info) = unsafe { steam_interface(module) } {
                break info;
            }
        }
        if Instant::now() >= limit {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "SteamNetworking006 unavailable",
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
        "Steam identity acquired and networking vtable installed",
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
        CHANGED => {
            "游戏 SteamNetworking006 对象或受控入口已变化，当前游戏接入已停止，请退出游戏后重开"
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

unsafe fn install_interface(object: usize) -> io::Result<()> {
    // The object comes only from the explicitly versioned v006 export. Pointer
    // and PE checks reject malformed layouts; they cannot prove a foreign ABI.
    if object % size_of::<usize>() != 0 || !readable(object, size_of::<usize>()) {
        return Err(io::Error::other("invalid networking object"));
    }
    let table = (object as *const usize).read();
    if table % size_of::<usize>() != 0 || !readable(table, 7 * size_of::<usize>()) {
        return Err(io::Error::other("invalid SteamNetworking006 vtable"));
    }
    let original =
        std::array::from_fn(|i| (*(table as *const AtomicUsize).add(i)).load(Ordering::Acquire));
    if original.iter().any(|&address| !x86_method(address)) {
        return Err(io::Error::other("unsupported networking methods"));
    }
    let protections = slot_protections(table)?;
    STEAM
        .set(Steam {
            original,
            object,
            table,
            installed: AtomicBool::new(false),
        })
        .map_err(|_| io::Error::other("networking already patched"))?;
    let saved = STEAM.get().unwrap();
    // All originals are visible before any game thread can enter a patched slot.
    // During installation (including rollback), every entry only forwards.
    patch_slots(saved, &protections)?;
    saved.installed.store(true, Ordering::Release);
    Ok(())
}

fn replacements() -> [usize; 7] {
    [
        send as *const () as usize,
        available as *const () as usize,
        read as *const () as usize,
        accept as *const () as usize,
        close as *const () as usize,
        close_channel as *const () as usize,
        session as *const () as usize,
    ]
}

unsafe fn patch_slots(saved: &Steam, protections: &[u32; 7]) -> io::Result<()> {
    let replacement = replacements();
    for i in 0..7 {
        let address = saved.table + i * size_of::<usize>();
        if let Err(error) =
            compare_pointer(address, saved.original[i], replacement[i], protections[i])
        {
            // Include the failed slot: CAS may have succeeded but protection
            // restoration failed. Never overwrite a third party's new pointer.
            let mut incomplete = false;
            for j in (0..=i).rev() {
                incomplete |= compare_pointer(
                    saved.table + j * size_of::<usize>(),
                    replacement[j],
                    saved.original[j],
                    protections[j],
                )
                .is_err();
            }
            return Err(io::Error::other(format!(
                "networking installation failed: {error}; rollback {}; saved originals and DLL retained",
                if incomplete {
                    "incomplete or conflicting"
                } else {
                    "complete"
                }
            )));
        }
    }
    if !interface_intact(saved, saved.object) {
        let mut incomplete = false;
        for j in (0..7).rev() {
            incomplete |= compare_pointer(
                saved.table + j * size_of::<usize>(),
                replacement[j],
                saved.original[j],
                protections[j],
            )
            .is_err();
        }
        return Err(io::Error::other(format!(
            "networking changed during installation; rollback incomplete={incomplete}; DLL retained"
        )));
    }
    Ok(())
}

fn slot_protections(table: usize) -> io::Result<[u32; 7]> {
    let mut protections = [0; 7];
    for (i, protection) in protections.iter_mut().enumerate() {
        let address = table + i * size_of::<usize>();
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
        // A vtable is data. Do not remove execute permission from a code page.
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

unsafe fn interface_intact(saved: &Steam, object: usize) -> bool {
    object == saved.object
        && readable(object, size_of::<usize>())
        && (object as *const usize).read() == saved.table
        && readable(saved.table, 7 * size_of::<usize>())
        && replacements().iter().enumerate().all(|(i, &address)| {
            (*(saved.table as *const AtomicUsize).add(i)).load(Ordering::Acquire) == address
        })
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
            || info.Protect & (PAGE_READONLY | PAGE_READWRITE | PAGE_WRITECOPY
                | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY) == 0
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
unsafe fn swap_pointer(address: usize, value: usize) -> io::Result<usize> {
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
    let previous = (*(address as *const AtomicUsize)).swap(value, Ordering::AcqRel);
    let mut ignored = 0;
    if VirtualProtect(
        address as *const c_void,
        size_of::<usize>(),
        old,
        &mut ignored,
    ) == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(previous)
}
fn original(index: usize) -> usize {
    STEAM.get().map(|s| s.original[index]).unwrap_or(0)
}

unsafe extern "thiscall" fn send(
    this: *mut c_void,
    remote: u64,
    data: *const c_void,
    length: u32,
    kind: i32,
    channel: i32,
) -> bool {
    if let Some(result) = hook_call(this, |shared| {
        API_CALLS[0].fetch_add(1, Ordering::Relaxed);
        if length as usize > MAX_PAYLOAD
            || !(0..=3).contains(&kind)
            || (length > 0 && data.is_null())
        {
            API_INVALID[0].fetch_add(1, Ordering::Relaxed);
            SEND_INVALID.fetch_add(1, Ordering::Relaxed);
            return Some(false);
        }
        // Lock order is Bridge -> Outbox. No network I/O or Steam calls under either lock.
        let Some(mut bridge) = bridge_lock(shared, true) else {
            return Some(false);
        };
        let bytes = if length == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(data.cast::<u8>(), length as usize)
        };
        if let Some(result) = bridge.send(remote, bytes, kind as u8, channel) {
            drop(bridge);
            shared.wake.unpark();
            return Some(result);
        }
        None
    }) {
        return result;
    }
    let address = original(0);
    if address == 0 {
        return false;
    }
    let f: unsafe extern "thiscall" fn(*mut c_void, u64, *const c_void, u32, i32, i32) -> bool =
        transmute(address);
    API_NATIVE[0].fetch_add(1, Ordering::Relaxed);
    f(this, remote, data, length, kind, channel)
}
unsafe extern "thiscall" fn available(this: *mut c_void, size: *mut u32, channel: i32) -> bool {
    let mut managed = false;
    if let Some(result) = hook_call(this, |shared| {
        managed = true;
        API_CALLS[1].fetch_add(1, Ordering::Relaxed);
        if size.is_null() {
            API_INVALID[1].fetch_add(1, Ordering::Relaxed);
            return Some(false);
        }
        let Some(mut bridge) = bridge_lock(shared, false) else {
            API_BUSY[1].fetch_add(1, Ordering::Relaxed);
            LOCK_BUSY.fetch_add(1, Ordering::Relaxed);
            return Some(false);
        };
        match bridge.available(channel) {
            Ok(Some(length)) => {
                size.write(length as u32);
                return Some(true);
            }
            Ok(None) => {
                // Reserve the source before releasing the lock: Hook data may
                // arrive while Steam reports the native packet's size.
                if bridge.native_query(channel).is_err() {
                    return Some(false);
                }
            }
            Err(_) => return Some(false),
        }
        None
    }) {
        return result;
    }
    let address = original(1);
    let found = if address == 0 {
        false
    } else {
        let f: unsafe extern "thiscall" fn(*mut c_void, *mut u32, i32) -> bool = transmute(address);
        API_NATIVE[1].fetch_add(1, Ordering::Relaxed);
        f(this, size, channel)
    };
    if !found && managed {
        hook_call(this, |shared| {
            if let Some(mut bridge) = bridge_lock(shared, false) {
                bridge.clear_native_query(channel);
            }
            Some(false)
        });
    }
    found
}
unsafe extern "thiscall" fn read(
    this: *mut c_void,
    destination: *mut c_void,
    capacity: u32,
    size: *mut u32,
    remote: *mut u64,
    channel: i32,
) -> bool {
    let mut managed = false;
    if let Some(result) = hook_call(this, |shared| {
        managed = true;
        API_CALLS[2].fetch_add(1, Ordering::Relaxed);
        if size.is_null() || remote.is_null() || (capacity > 0 && destination.is_null()) {
            API_INVALID[2].fetch_add(1, Ordering::Relaxed);
            return Some(false);
        }
        let Some(mut bridge) = bridge_lock(shared, false) else {
            API_BUSY[2].fetch_add(1, Ordering::Relaxed);
            LOCK_BUSY.fetch_add(1, Ordering::Relaxed);
            return Some(false);
        };
        let packet = match bridge.read(channel) {
            Ok(packet) => packet,
            // The game still has the old Hook packet's buffer size. Do not
            // replace an invalidated query with a packet from native Steam.
            Err(_) => return Some(false),
        };
        if let Some(packet) = packet {
            drop(bridge);
            // Steam's documented ABI consumes/truncates a packet when the caller's buffer is small.
            let count = packet.payload.len().min(capacity as usize);
            if count < packet.payload.len() {
                READ_TRUNCATED.fetch_add(1, Ordering::Relaxed);
            }
            if count > 0 {
                ptr::copy_nonoverlapping(packet.payload.as_ptr(), destination.cast::<u8>(), count);
            }
            size.write(count as u32);
            remote.write_unaligned(packet.from);
            return Some(true);
        }
        None
    }) {
        return result;
    }
    let address = original(2);
    if address == 0 {
        return false;
    }
    let f: unsafe extern "thiscall" fn(
        *mut c_void,
        *mut c_void,
        u32,
        *mut u32,
        *mut u64,
        i32,
    ) -> bool = transmute(address);
    API_NATIVE[2].fetch_add(1, Ordering::Relaxed);
    if !f(this, destination, capacity, size, remote, channel) {
        return false;
    }
    if !managed { return true; }
    hook_call(this, |shared| {
        let ours = bridge_lock(shared, false)
            .map(|mut b| {
                let ours = b.known(remote.read_unaligned());
                if !ours {
                    b.clear_native_query(channel);
                }
                ours
            })
            .unwrap_or(true);
        if ours {
            // The next native packet can be larger than the queried one. Require a
            // fresh size query instead of consuming it with the old buffer.
            NATIVE_DISCARDED.fetch_add(1, Ordering::Relaxed);
            return Some(false);
        }
        Some(true)
    })
    .unwrap_or(true)
}
unsafe extern "thiscall" fn accept(this: *mut c_void, remote: u64) -> bool {
    if let Some(result) = hook_call(this, |shared| {
        API_CALLS[3].fetch_add(1, Ordering::Relaxed);
        let Some(mut bridge) = bridge_lock(shared, false) else {
            API_BUSY[3].fetch_add(1, Ordering::Relaxed);
            return Some(false);
        };
        bridge.accept(remote)
    }) {
        return result;
    }
    let f: unsafe extern "thiscall" fn(*mut c_void, u64) -> bool = transmute(original(3));
    API_NATIVE[3].fetch_add(1, Ordering::Relaxed);
    f(this, remote)
}
unsafe extern "thiscall" fn close(this: *mut c_void, remote: u64) -> bool {
    if let Some(result) = hook_call(this, |shared| {
        API_CALLS[4].fetch_add(1, Ordering::Relaxed);
        let Some(mut bridge) = bridge_lock(shared, false) else {
            API_BUSY[4].fetch_add(1, Ordering::Relaxed);
            return Some(false);
        };
        bridge.close(remote, None)
    }) {
        return result;
    }
    let f: unsafe extern "thiscall" fn(*mut c_void, u64) -> bool = transmute(original(4));
    API_NATIVE[4].fetch_add(1, Ordering::Relaxed);
    f(this, remote)
}
unsafe extern "thiscall" fn close_channel(this: *mut c_void, remote: u64, channel: i32) -> bool {
    if let Some(result) = hook_call(this, |shared| {
        API_CALLS[5].fetch_add(1, Ordering::Relaxed);
        let Some(mut bridge) = bridge_lock(shared, false) else {
            API_BUSY[5].fetch_add(1, Ordering::Relaxed);
            return Some(false);
        };
        bridge.close(remote, Some(channel))
    }) {
        return result;
    }
    let f: unsafe extern "thiscall" fn(*mut c_void, u64, i32) -> bool = transmute(original(5));
    API_NATIVE[5].fetch_add(1, Ordering::Relaxed);
    f(this, remote, channel)
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
    if let Some(value) = hook_call(this, |shared| {
        API_CALLS[6].fetch_add(1, Ordering::Relaxed);
        if result.is_null() {
            API_INVALID[6].fetch_add(1, Ordering::Relaxed);
            return Some(false);
        }
        let Some(mut bridge) = bridge_lock(shared, true) else {
            return Some(false);
        };
        if let Some((active, bytes, packets)) = bridge.session(remote) {
            let error = if bridge.stopped || bridge.peer_failed(remote) {
                4
            } else {
                0
            };
            bridge.observe_session(remote, active, error);
            result.write(SessionState {
                active: u8::from(active),
                connecting: 0,
                error,
                relay: 1,
                bytes: bytes as i32,
                packets: packets as i32,
                ip: 0,
                port: 0,
            });
            return Some(true);
        }
        None
    }) {
        return value;
    }
    let f: unsafe extern "thiscall" fn(*mut c_void, u64, *mut SessionState) -> bool =
        transmute(original(6));
    API_NATIVE[6].fetch_add(1, Ordering::Relaxed);
    f(this, remote, result)
}

static RUN_CALLBACKS: AtomicUsize = AtomicUsize::new(0);

unsafe fn install_callback_imports() -> io::Result<()> {
    let image = GetModuleHandleW(ptr::null()) as usize;
    if !readable(image, 64) {
        return Err(io::Error::other("main image unavailable"));
    }
    let header = (image as *const u8).add(60).cast::<u32>().read_unaligned() as usize;
    if header > 16 * 1024 * 1024 || !readable(image + header, 256) {
        return Err(io::Error::other("invalid main PE header"));
    }
    if ((image + header) as *const u32).read_unaligned() != 0x4550 {
        return Err(io::Error::other("PE signature mismatch"));
    }
    let optional = image + header + 24;
    if (optional as *const u16).read_unaligned() != 0x10b {
        return Err(io::Error::other("expected PE32 imports"));
    }
    let image_size = ((optional + 56) as *const u32).read_unaligned() as usize;
    let import_rva = ((optional + 104) as *const u32).read_unaligned() as usize;
    let in_image = |rva: usize, size: usize| {
        rva.checked_add(size).is_some_and(|end| end <= image_size) && readable(image + rva, size)
    };
    let mut descriptor = import_rva;
    let mut patched = 0;
    while in_image(descriptor, 20) {
        let fields = std::slice::from_raw_parts((image + descriptor) as *const u32, 5);
        let lookup = fields[0] as usize;
        let names = fields[3] as usize;
        let slots = fields[4] as usize;
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
                let name = ((image + lookup + index * 4) as *const u32).read() as usize;
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
                    _ => continue,
                };
                let address = image + slots + index * 4;
                let previous = (address as *const usize).read();
                if !executable(previous) {
                    return Err(io::Error::other("invalid Steam callback import"));
                }
                original.store(previous, Ordering::Release);
                swap_pointer(address, replacement)?;
                patched += 1;
            }
        }
        descriptor += 20;
    }
    if patched != 1 {
        return Err(io::Error::other("required Steam callback imports missing"));
    }
    Ok(())
}
unsafe fn bounded_name<'a>(address: usize, max: usize) -> Option<&'a [u8]> {
    let mut size = 0;
    while size < max.min(128) {
        if !readable(address + size, 1) {
            return None;
        }
        if ((address + size) as *const u8).read() == 0 {
            return Some(std::slice::from_raw_parts(address as *const u8, size));
        }
        size += 1;
    }
    None
}

unsafe extern "C" fn run_callbacks() {
    let f: unsafe extern "C" fn() = transmute(RUN_CALLBACKS.load(Ordering::Acquire));
    f();
    if CALLBACK_TICKS.fetch_add(1, Ordering::Relaxed) % 60 == 0 {
        let Some(saved) = STEAM.get().filter(|s| s.installed.load(Ordering::Acquire)) else {
            return;
        };
        if HOOK_FAILURE.load(Ordering::Acquire) != 0 {
            return;
        }
        // Query a current interface on the game callback thread, never write a stale saved object.
        let Some(module) = rust_boundary(|| GetModuleHandleW(wide("steam_api.dll").as_ptr()))
        else {
            return;
        };
        // Original Steam exports, like the original vtable calls, are outside
        // catch_unwind. We only contain our own Rust processing.
        let current = steam_interface(module);
        rust_boundary(|| {
            if !current.is_some_and(|(object, _)| interface_intact(saved, object)) {
                INTERFACE_CHANGED.store(true, Ordering::Relaxed);
                fail_hook(CHANGED);
            }
        });
    }
    // Steam owns callback registration, object lifetimes, and dispatch.
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_RELEASE, MEM_RESERVE, PAGE_READONLY, VirtualAlloc, VirtualFree,
    };

    #[test]
    fn installation_conflict_restores_owned_slots_and_page_protection() {
        unsafe {
            let page = VirtualAlloc(ptr::null(), 4096, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
            assert!(!page.is_null());
            let table = page as *mut AtomicUsize;
            let originals = [native_send as *const () as usize; 7];
            for (i, &value) in originals.iter().enumerate() {
                table.add(i).write(AtomicUsize::new(value));
            }
            let object = Box::new(page as usize);
            let saved = Steam {
                original: originals,
                object: (&*object) as *const usize as usize,
                table: page as usize,
                installed: AtomicBool::new(false),
            };
            // A third party changes a later slot after we snapshot originals.
            (*table.add(3)).store(0x1234, Ordering::Release);
            let mut ignored = 0;
            assert_ne!(VirtualProtect(page, 4096, PAGE_READONLY, &mut ignored), 0);
            let protections = slot_protections(page as usize).unwrap();
            assert!(patch_slots(&saved, &protections).is_err());
            for (i, &value) in originals.iter().enumerate() {
                assert_eq!(
                    (*table.add(i)).load(Ordering::Acquire),
                    if i == 3 { 0x1234 } else { value }
                );
            }
            assert_eq!(slot_protections(page as usize).unwrap(), [PAGE_READONLY; 7]);
            assert_eq!(*object, page as usize);
            assert!(!saved.installed.load(Ordering::Acquire));
            compare_pointer(
                page as usize + 3 * size_of::<usize>(),
                0x1234,
                originals[3],
                PAGE_READONLY,
            )
            .unwrap();
            patch_slots(&saved, &protections).unwrap();
            assert!(interface_intact(&saved, saved.object));
            assert_eq!(slot_protections(page as usize).unwrap(), [PAGE_READONLY; 7]);
            // A rollback may never undo a subsequent third-party replacement.
            compare_pointer(page as usize, replacements()[0], 0x5678, PAGE_READONLY).unwrap();
            assert!(
                compare_pointer(
                    page as usize,
                    replacements()[0],
                    originals[0],
                    PAGE_READONLY
                )
                .is_err()
            );
            assert_eq!((*table).load(Ordering::Acquire), 0x5678);
            assert!(!interface_intact(&saved, saved.object));
            assert_eq!(slot_protections(page as usize).unwrap(), [PAGE_READONLY; 7]);
            assert_ne!(VirtualFree(page, 0, MEM_RELEASE), 0);
        }
    }

    unsafe extern "thiscall" fn native_send(
        _: *mut c_void,
        remote: u64,
        _: *const c_void,
        length: u32,
        kind: i32,
        channel: i32,
    ) -> bool {
        remote == 0x1234_5678_abcdef01 && length == 0 && kind == 17 && channel == -9
    }

    // One process-global test owns STEAM/BRIDGE; other tests use local state.
    #[test]
    fn installing_forwards_and_rust_failure_stops_ipc_without_reusing_poison() {
        let object = Box::leak(Box::new(0usize)) as *mut usize as usize;
        let shared = Arc::new(Shared {
            bridge: Mutex::new(Bridge::new(101, 11)),
            wake: std::thread::current(),
            stopped: AtomicBool::new(false),
            faults: AtomicBool::new(false),
        });
        assert!(BRIDGE.set(shared.clone()).is_ok());
        assert!(
            STEAM
                .set(Steam {
                    original: [native_send as *const () as usize; 7],
                    object,
                    table: 0,
                    installed: AtomicBool::new(false),
                })
                .is_ok()
        );
        let forward = |object| unsafe {
            send(
                object as *mut c_void,
                0x1234_5678_abcdef01,
                ptr::null(),
                0,
                17,
                -9,
            )
        };
        let guard = shared.bridge.lock().unwrap();
        assert!(
            forward(object),
            "installing must forward even while Bridge is locked"
        );
        STEAM
            .get()
            .unwrap()
            .installed
            .store(true, Ordering::Release);
        assert!(
            forward(object + 4),
            "a shared non-target object must bypass Bridge and validation"
        );
        drop(guard);

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
        assert_eq!(
            hook_call(object as *mut c_void, |shared| {
                let _guard = shared.bridge.lock().unwrap();
                panic!("test game-thread panic while holding Bridge");
            }),
            Some(false)
        );
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
        assert!(!forward(object));
        assert!(forward(object + 4));
        // Simulate first discovery of an already-poisoned lock; it must fail
        // without consuming PoisonError::into_inner or calling native Steam.
        HOOK_FAILURE.store(0, Ordering::Release);
        assert_eq!(
            hook_call(object as *mut c_void, |shared| Some(
                bridge_lock(shared, false).is_some()
            )),
            Some(false)
        );
        assert_eq!(HOOK_FAILURE.load(Ordering::Acquire), POISONED);
        stop_bridge(&shared);
        assert!(shared.bridge.is_poisoned());
    }
}
