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
        Arc, Mutex, OnceLock,
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
            PAGE_READWRITE, VirtualProtect, VirtualQuery,
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
struct Steam {
    original: [usize; 22],
    object: usize,
    table: usize,
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
                Err(_) => crate::diagnostics::record("ERROR", "worker", "panic; Hook stopped"),
            }
            if !matches!(result, Ok(Ok(()))) {
                if let Some(shared) = BRIDGE.get() {
                    shared
                        .bridge
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .stop();
                    shared.stopped.store(true, Ordering::Release);
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
                    "Steam callback imports intercepted; previously registered callbacks are not captured",
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
    let mut socket = TcpStream::connect_timeout(&endpoint, Duration::from_secs(3))?;
    socket.set_nodelay(true)?;
    socket.set_read_timeout(Some(Duration::from_secs(5)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
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
        return Err(io::Error::other(
            "IPC capability mismatch; update the complete client package",
        ));
    }
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
    let reader = std::thread::Builder::new()
        .name("netburrow-ipc-read".into())
        .spawn(move || {
            let mut decoder = Reader::default();
            loop {
                let message = match decoder.poll(&mut input) {
                    Ok(Some(message)) => message,
                    Ok(None) => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => {
                        crate::diagnostics::record("ERROR", "ipc read", &error.to_string());
                        break;
                    }
                };
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
                let mut bridge = receive_shared
                    .bridge
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
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
                if !valid {
                    crate::diagnostics::record(
                        "INFO",
                        "ipc",
                        "receiver stopped; protocol error or explicit Stop",
                    );
                    break;
                }
            }
            receive_shared
                .bridge
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .stop();
            receive_shared.stopped.store(true, Ordering::Release);
            receive_shared.wake.unpark();
        })?;
    let mut ping = Instant::now() - Duration::from_secs(2);
    let mut callback_report = Instant::now() - Duration::from_secs(10);
    let mut health_report = Instant::now();
    let mut pending: Option<PendingWrite> = None;
    let outbound = shared
        .bridge
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .outbound
        .clone();
    let result = loop {
        if shared.stopped.load(Ordering::Acquire) {
            break Ok(());
        }
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
            let rejected = shared
                .bridge
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .send_rejections;
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
        }
        let message = if ping.elapsed() >= Duration::from_secs(2) {
            ping = Instant::now();
            Some(Message::Ping(0))
        } else if shared.faults.load(Ordering::Acquire) {
            let mut bridge = shared.bridge.lock().unwrap_or_else(|p| p.into_inner());
            match bridge.pop_fault() {
                Some((peer, epoch)) => Some(Message::IpcPeerFault { peer, epoch }),
                None => {
                    shared.faults.store(false, Ordering::Release);
                    None
                }
            }
        } else if health_report.elapsed() >= Duration::from_secs(1) {
            health_report = Instant::now();
            let mut health = shared
                .bridge
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .health();
            health.lock_busy += LOCK_BUSY.load(Ordering::Relaxed) as u64;
            health.interface_changed = INTERFACE_CHANGED.load(Ordering::Relaxed);
            Some(Message::IpcHealth(health))
        } else if let Some(packet) = outbound.pop() {
            Some(Message::Data(packet))
        } else {
            None
        };
        if let Some(message) = message {
            match PendingWrite::new(&message) {
                Ok(frame) => pending = Some(frame),
                Err(e) => break Err(e),
            }
        } else {
            std::thread::park_timeout(Duration::from_millis(10));
        }
    };
    shared
        .bridge
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .stop();
    let _ = socket.shutdown(Shutdown::Both);
    let _ = reader.join();
    result
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
    if !readable(object, 4) {
        return Err(io::Error::other("invalid networking object"));
    }
    let table = (object as *const usize).read();
    if !readable(table, 22 * 4) {
        return Err(io::Error::other("invalid SteamNetworking006 vtable"));
    }
    let mut original = [0usize; 22];
    ptr::copy_nonoverlapping(table as *const usize, original.as_mut_ptr(), 22);
    if original.iter().any(|&address| !executable(address)) {
        return Err(io::Error::other("unsupported networking methods"));
    }
    let mut replacement = Box::new(original);
    replacement[..7].copy_from_slice(&[
        send as *const () as usize,
        available as *const () as usize,
        read as *const () as usize,
        accept as *const () as usize,
        close as *const () as usize,
        close_channel as *const () as usize,
        session as *const () as usize,
    ]);
    let address = Box::into_raw(replacement) as usize;
    STEAM
        .set(Steam {
            original,
            object,
            table: address,
        })
        .map_err(|_| io::Error::other("networking already patched"))?;
    swap_pointer(object, address)?;
    Ok(())
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
    if length as usize > MAX_PAYLOAD || !(0..=3).contains(&kind) || (length > 0 && data.is_null()) {
        SEND_INVALID.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    if let Some(shared) = BRIDGE.get() {
        // Lock order is Bridge -> Outbox. No network I/O or Steam calls under either lock.
        let mut bridge = shared.bridge.lock().unwrap_or_else(|p| p.into_inner());
        let bytes = if length == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(data.cast::<u8>(), length as usize)
        };
        if let Some(result) = bridge.send(remote, bytes, kind as u8, channel) {
            drop(bridge);
            shared.wake.unpark();
            return result;
        }
    }
    let address = original(0);
    if address == 0 {
        return false;
    }
    let f: unsafe extern "thiscall" fn(*mut c_void, u64, *const c_void, u32, i32, i32) -> bool =
        transmute(address);
    f(this, remote, data, length, kind, channel)
}
unsafe extern "thiscall" fn available(this: *mut c_void, size: *mut u32, channel: i32) -> bool {
    if size.is_null() {
        return false;
    }
    if let Some(shared) = BRIDGE.get() {
        let Ok(bridge) = shared.bridge.try_lock() else {
            LOCK_BUSY.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        if let Some(length) = bridge.available(channel) {
            size.write(length as u32);
            return true;
        }
    }
    let address = original(1);
    if address == 0 {
        return false;
    }
    let f: unsafe extern "thiscall" fn(*mut c_void, *mut u32, i32) -> bool = transmute(address);
    f(this, size, channel)
}
unsafe extern "thiscall" fn read(
    this: *mut c_void,
    destination: *mut c_void,
    capacity: u32,
    size: *mut u32,
    remote: *mut u64,
    channel: i32,
) -> bool {
    if size.is_null() || remote.is_null() || (capacity > 0 && destination.is_null()) {
        return false;
    }
    if let Some(shared) = BRIDGE.get() {
        let Ok(mut bridge) = shared.bridge.try_lock() else {
            LOCK_BUSY.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        if let Some(packet) = bridge.read(channel) {
            drop(bridge);
            // Steam's documented ABI consumes/truncates a packet when the caller's buffer is small.
            let count = packet.payload.len().min(capacity as usize);
            if count > 0 {
                ptr::copy_nonoverlapping(packet.payload.as_ptr(), destination.cast::<u8>(), count);
            }
            size.write(count as u32);
            remote.write_unaligned(packet.from);
            return true;
        }
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
    // Discard a bounded number of stale native-path packets from already-owned peers.
    for _ in 0..16 {
        if !f(this, destination, capacity, size, remote, channel) {
            return false;
        }
        let ours = BRIDGE.get().is_some_and(|s| {
            s.bridge
                .try_lock()
                .map(|b| b.known(remote.read_unaligned()))
                .unwrap_or(true)
        });
        if !ours {
            return true;
        }
    }
    false
}
unsafe extern "thiscall" fn accept(this: *mut c_void, remote: u64) -> bool {
    if let Some(shared) = BRIDGE.get() {
        let Ok(mut bridge) = shared.bridge.try_lock() else {
            return false;
        };
        if let Some(result) = bridge.accept(remote) {
            return result;
        }
    }
    let f: unsafe extern "thiscall" fn(*mut c_void, u64) -> bool = transmute(original(3));
    f(this, remote)
}
unsafe extern "thiscall" fn close(this: *mut c_void, remote: u64) -> bool {
    if let Some(shared) = BRIDGE.get() {
        let Ok(mut bridge) = shared.bridge.try_lock() else {
            return false;
        };
        if let Some(result) = bridge.close(remote, None) {
            return result;
        }
    }
    let f: unsafe extern "thiscall" fn(*mut c_void, u64) -> bool = transmute(original(4));
    f(this, remote)
}
unsafe extern "thiscall" fn close_channel(this: *mut c_void, remote: u64, channel: i32) -> bool {
    if let Some(shared) = BRIDGE.get() {
        let Ok(mut bridge) = shared.bridge.try_lock() else {
            return false;
        };
        if let Some(result) = bridge.close(remote, Some(channel)) {
            return result;
        }
    }
    let f: unsafe extern "thiscall" fn(*mut c_void, u64, i32) -> bool = transmute(original(5));
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
unsafe extern "thiscall" fn session(
    this: *mut c_void,
    remote: u64,
    result: *mut SessionState,
) -> bool {
    if result.is_null() {
        return false;
    }
    if let Some(shared) = BRIDGE.get() {
        let bridge = shared.bridge.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((active, bytes, packets)) = bridge.session(remote) {
            result.write(SessionState {
                active: u8::from(active),
                connecting: 0,
                error: if bridge.stopped || bridge.peer_failed(remote) {
                    4
                } else {
                    0
                },
                relay: 1,
                bytes: bytes as i32,
                packets: packets as i32,
                ip: 0,
                port: 0,
            });
            return true;
        }
    }
    let f: unsafe extern "thiscall" fn(*mut c_void, u64, *mut SessionState) -> bool =
        transmute(original(6));
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
        // Query a current interface on the game callback thread, never write a stale saved object.
        let module = GetModuleHandleW(wide("steam_api.dll").as_ptr());
        if let (Some(saved), Some((object, _))) = (STEAM.get(), steam_interface(module)) {
            if object != saved.object
                || (readable(object, 4) && (object as *const usize).read() != saved.table)
            {
                INTERFACE_CHANGED.store(true, Ordering::Relaxed);
            }
        }
    }
    // Steam owns callback registration, object lifetimes, and dispatch.
}
