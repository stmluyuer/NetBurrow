//! A controlled x86 process named isaac-ng.exe, with our test ABI DLL only.
//! Exercises the real helper and Hook without opening Steam or touching game files.
#![allow(unsafe_op_in_unsafe_fn)]
use netburrow_protocol::{Message, Packet, Peer};
static SESSION: std::sync::Mutex<Option<netburrow_protocol::resume::Window>> =
    std::sync::Mutex::new(None);
fn write_message(socket: &mut std::net::TcpStream, message: &Message) -> std::io::Result<()> {
    let mut session = SESSION.lock().unwrap();
    if matches!(message, Message::IpcAccepted(_)) {
        *session = Some(Default::default());
    }
    let message = if netburrow_protocol::replayable(message) && session.is_some() {
        let body = netburrow_protocol::encode(message)?;
        let sequence = session.as_mut().unwrap().retain(body.clone())?;
        Message::SessionFrame { sequence, body }
    } else {
        message.clone()
    };
    netburrow_protocol::write_message(socket, &message)
}
fn read_message(socket: &mut std::net::TcpStream) -> std::io::Result<Message> {
    loop {
        let message = netburrow_protocol::read_message(socket)?;
        match message {
            Message::SessionAck(n) => {
                SESSION.lock().unwrap().as_mut().unwrap().acknowledge(n)?;
            }
            Message::SessionFrame { sequence, body } => {
                let mut session = SESSION.lock().unwrap();
                let session = session.as_mut().unwrap();
                let fresh = session.classify(sequence)?;
                let message = netburrow_protocol::decode_session_body(&body)?;
                session.received(sequence)?;
                netburrow_protocol::write_message(
                    socket,
                    &Message::SessionAck(session.received_through()),
                )?;
                if fresh {
                    return Ok(message);
                }
            }
            _ => return Ok(message),
        }
    }
}
fn read_data(socket: &mut std::net::TcpStream) -> Packet {
    loop {
        match read_message(socket).unwrap() {
            Message::Data(packet) => return packet,
            Message::Ping(n) => write_message(socket, &Message::Pong(n)).unwrap(),
            Message::IpcHealth(_) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}
use std::{
    ffi::{c_char, c_void},
    io::Write,
    mem::transmute,
    net::TcpListener,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};
#[link(name = "steam_api", kind = "raw-dylib")]
unsafe extern "C" {
    fn FixtureSetObject(value: usize);
    fn FixtureFindCalls() -> usize;
    fn FixtureNetworkingCalls() -> usize;
    fn FixtureCallbackCalls() -> usize;
    fn FixtureSetUserReady();
    fn FixtureUserChecks() -> usize;
    fn FixtureEarlyUserCalls() -> usize;
    fn SteamInternal_FindOrCreateUserInterface(user: i32, version: *const c_char) -> *mut c_void;
    fn SteamAPI_RegisterCallback(object: *mut c_void, id: i32);
    fn SteamAPI_UnregisterCallback(object: *mut c_void);
    fn SteamAPI_RunCallbacks();
}
#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> *mut c_void;
    fn Thread32First(snapshot: *mut c_void, entry: *mut ThreadEntry) -> i32;
    fn Thread32Next(snapshot: *mut c_void, entry: *mut ThreadEntry) -> i32;
    fn OpenThread(access: u32, inherit: i32, id: u32) -> *mut c_void;
    fn GetThreadDescription(thread: *mut c_void, description: *mut *mut u16) -> i32;
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
    fn WaitForSingleObject(handle: *mut c_void, millis: u32) -> u32;
    fn GetCurrentProcess() -> *mut c_void;
    fn GetProcessTimes(
        process: *mut c_void,
        created: *mut u64,
        exit: *mut u64,
        kernel: *mut u64,
        user: *mut u64,
    ) -> i32;
}
#[repr(C)]
struct ThreadEntry {
    size: u32,
    usage: u32,
    id: u32,
    owner: u32,
    base_priority: i32,
    delta_priority: i32,
    flags: u32,
}

// Capture existing, named Hook threads. Waiting on their OS handles tests actual
// completion; a stopped session alone does not mean DLL threads have finished.
unsafe fn hook_workers() -> Vec<OwnedHandle> {
    let raw = CreateToolhelp32Snapshot(4, 0);
    assert_ne!(raw as isize, -1);
    let snapshot = OwnedHandle::from_raw_handle(raw);
    let mut entry: ThreadEntry = std::mem::zeroed();
    entry.size = std::mem::size_of::<ThreadEntry>() as u32;
    let mut found = Vec::new();
    let mut valid = Thread32First(snapshot.as_raw_handle(), &mut entry);
    while valid != 0 {
        if entry.owner == std::process::id() {
            let raw = OpenThread(0x0010_0800, 0, entry.id);
            if !raw.is_null() {
                let thread = OwnedHandle::from_raw_handle(raw);
                let mut description = std::ptr::null_mut();
                if GetThreadDescription(thread.as_raw_handle(), &mut description) >= 0
                    && !description.is_null()
                {
                    let mut len = 0;
                    while *description.add(len) != 0 {
                        len += 1;
                    }
                    let name =
                        String::from_utf16_lossy(std::slice::from_raw_parts(description, len));
                    LocalFree(description.cast());
                    if matches!(name.as_str(), "netburrow-hook" | "netburrow-ipc-read") {
                        found.push(thread);
                    }
                }
            }
        }
        valid = Thread32Next(snapshot.as_raw_handle(), &mut entry);
    }
    found
}
static NATIVE_SENDS: AtomicUsize = AtomicUsize::new(0);
static NATIVE_AVAILABLE: AtomicUsize = AtomicUsize::new(0);
static NATIVE_READS: AtomicUsize = AtomicUsize::new(0);
static NATIVE_PACKETS: std::sync::Mutex<std::collections::VecDeque<(u64, i32, Vec<u8>)>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());
static SHARED_OBJECT: AtomicUsize = AtomicUsize::new(0);
static SHARED_PEERS: AtomicUsize = AtomicUsize::new(0);
static SHARED_CHANNELS: AtomicUsize = AtomicUsize::new(0);
static SHARED_SESSIONS: AtomicUsize = AtomicUsize::new(0);
static SHARED_ABI_FAILURES: AtomicUsize = AtomicUsize::new(0);
static REQUESTS: AtomicUsize = AtomicUsize::new(0);
static FAILURES: AtomicUsize = AtomicUsize::new(0);
const SHARED_REMOTE: u64 = 0x0000_0002_0000_00ca;
const SHARED_CHANNEL: i32 = 17;
const SHARED_PAYLOAD: &[u8] = b"shared";
const SHARED_SESSION: [u8; 20] = [
    1, 0, 0, 1, 0x44, 0x33, 0x22, 0x11, 0x88, 0x77, 0x66, 0x55, 0, 0, 0, 0, 0x34, 0x12, 0, 0,
];
fn shared_this(this: *mut c_void) -> bool {
    this as usize == SHARED_OBJECT.load(Ordering::Acquire)
}
fn shared_abi(ok: bool) {
    if !ok {
        SHARED_ABI_FAILURES.fetch_add(1, Ordering::SeqCst);
    }
}
unsafe extern "thiscall" fn unused(_: *mut c_void) -> bool {
    false
}
unsafe extern "thiscall" fn native_send(
    _: *mut c_void,
    _: u64,
    _: *const c_void,
    _: u32,
    _: i32,
    _: i32,
) -> bool {
    NATIVE_SENDS.fetch_add(1, Ordering::SeqCst);
    true
}
unsafe extern "thiscall" fn native_available(_: *mut c_void, size: *mut u32, channel: i32) -> bool {
    NATIVE_AVAILABLE.fetch_add(1, Ordering::SeqCst);
    let packets = NATIVE_PACKETS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some((_, _, payload)) = packets.iter().find(|(_, c, _)| *c == channel) else {
        return false;
    };
    size.write(payload.len() as u32);
    true
}
unsafe extern "thiscall" fn native_read(
    _: *mut c_void,
    destination: *mut c_void,
    capacity: u32,
    size: *mut u32,
    remote: *mut u64,
    channel: i32,
) -> bool {
    NATIVE_READS.fetch_add(1, Ordering::SeqCst);
    let mut packets = NATIVE_PACKETS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(index) = packets.iter().position(|(_, c, _)| *c == channel) else {
        return false;
    };
    let Some((peer, _, payload)) = packets.remove(index) else {
        return false;
    };
    let length = payload.len().min(capacity as usize);
    std::ptr::copy_nonoverlapping(payload.as_ptr(), destination.cast::<u8>(), length);
    size.write(length as u32);
    remote.write_unaligned(peer);
    true
}
unsafe extern "thiscall" fn native_peer(this: *mut c_void, remote: u64) -> bool {
    if shared_this(this) {
        shared_abi(remote == SHARED_REMOTE && remote >> 32 != 0);
        SHARED_PEERS.fetch_add(1, Ordering::SeqCst);
    }
    true
}
unsafe extern "thiscall" fn native_channel(this: *mut c_void, remote: u64, channel: i32) -> bool {
    if shared_this(this) {
        shared_abi(remote == SHARED_REMOTE && remote >> 32 != 0 && channel == SHARED_CHANNEL);
        SHARED_CHANNELS.fetch_add(1, Ordering::SeqCst);
    }
    true
}
unsafe extern "thiscall" fn native_session(
    this: *mut c_void,
    remote: u64,
    result: *mut c_void,
) -> bool {
    if shared_this(this) {
        shared_abi(!result.is_null() && remote == SHARED_REMOTE && remote >> 32 != 0);
        if !result.is_null() {
            std::ptr::copy_nonoverlapping(
                SHARED_SESSION.as_ptr(),
                result.cast::<u8>(),
                SHARED_SESSION.len(),
            );
        }
        SHARED_SESSIONS.fetch_add(1, Ordering::SeqCst);
        return true;
    }
    false
}
unsafe extern "thiscall" fn replacement_session(
    _: *mut c_void,
    remote: u64,
    result: *mut c_void,
) -> bool {
    assert_eq!(remote, SHARED_REMOTE);
    std::ptr::write_bytes(result.cast::<u8>(), 0x3c, 20);
    false
}
#[repr(C)]
struct Callback {
    table: *const usize,
    flags: u32,
    id: i32,
    network: *mut usize,
}
unsafe extern "thiscall" fn callback_size(this: *mut Callback) -> i32 {
    if (*this).id == 1202 { 8 } else { 12 }
}
unsafe extern "thiscall" fn callback_run(this: *mut Callback, data: *mut c_void) {
    if (*this).id == 1202 {
        REQUESTS.fetch_add(1, Ordering::SeqCst);
        let remote = (data as *const u64).read_unaligned();
        let table = *(*this).network as *const usize;
        let accept: unsafe extern "thiscall" fn(*mut c_void, u64) -> bool =
            transmute(*table.add(3));
        assert!(accept((*this).network.cast(), remote));
    } else {
        FAILURES.fetch_add(1, Ordering::SeqCst);
    }
}
fn eventually(mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(4);
    while !condition() {
        assert!(Instant::now() < until, "native assertion timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn main() {
    unsafe {
        run();
    }
}
unsafe fn run() {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    assert_eq!(arguments.len(), 2, "host needs helper and hook paths");
    let mut table = Box::new([unused as *const () as usize; 22]);
    table[..7].copy_from_slice(&[
        native_send as *const () as usize,
        native_available as *const () as usize,
        native_read as *const () as usize,
        native_peer as *const () as usize,
        native_peer as *const () as usize,
        native_channel as *const () as usize,
        native_session as *const () as usize,
    ]);
    let original_table = table.as_ptr() as usize;
    let original_slots = *table;
    let mut object = Box::new(original_table);
    let mut shared_object = Box::new(original_table);
    FixtureSetObject((&mut *object) as *mut usize as usize);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let (mut created, mut exited, mut kernel, mut user) = (0, 0, 0, 0);
    assert_ne!(
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user
        ),
        0
    );
    let make_helper = |time: u64| {
        let mut command = Command::new(&arguments[0]);
        command
            .args([
                "--pid",
                &std::process::id().to_string(),
                "--created",
                &time.to_string(),
                "--exe",
            ])
            .arg(std::env::current_exe().unwrap())
            .arg("--dll")
            .arg(&arguments[1])
            .args([
                "--port",
                &listener.local_addr().unwrap().port().to_string(),
                "--epoch",
                "11",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    };
    let wrong = make_helper(created + 1).output().unwrap();
    assert!(
        !wrong.status.success(),
        "helper must reject a stale process identity"
    );
    let mut child = make_helper(created).spawn().unwrap();
    child.stdin.take().unwrap().write_all(&[7; 16]).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    eventually(|| {
        assert_eq!(
            FixtureEarlyUserCalls(), 0,
            "must wait for Steam user readiness before requesting identity"
        );
        FixtureUserChecks() >= 3
    });
    let callbacks = FixtureCallbackCalls();
    SteamAPI_RunCallbacks();
    assert_eq!(FixtureCallbackCalls(), callbacks + 1);
    assert_eq!(
        SteamInternal_FindOrCreateUserInterface(0, c"SteamNetworking006".as_ptr()),
        (&mut *object) as *mut usize as *mut c_void
    );
    assert_eq!(
        *table, original_slots,
        "waiting for Steam must preserve native networking"
    );
    assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    FixtureSetUserReady();
    println!("helper passed; Steam ready; waiting for IPC");
    let mut socket = None;
    eventually(|| {
        socket = listener.accept().ok().map(|p| p.0);
        socket.is_some()
    });
    let mut socket = socket.unwrap();
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    assert_eq!(
        read_message(&mut socket).unwrap(),
        Message::IpcHelloV2 {
            nonce: [7; 16],
            pid: std::process::id(),
            steam_id: 101,
            epoch: 11,
            capabilities: netburrow_protocol::IPC_CAPABILITIES,
        }
    );
    write_message(
        &mut socket,
        &Message::IpcAccepted(netburrow_protocol::IPC_CAPABILITIES),
    )
    .unwrap();
    assert!(matches!(
        read_message(&mut socket).unwrap(),
        Message::Diagnostic(_)
    ));
    assert_eq!(read_message(&mut socket).unwrap(), Message::IpcReady);
    let mut workers = Vec::new();
    eventually(|| {
        workers = hook_workers();
        workers.len() == 2
    });
    assert_eq!(
        *object, original_table,
        "target object must retain its original vtable pointer"
    );
    assert_eq!(
        *shared_object, original_table,
        "a second object sharing the table must retain its original vtable pointer"
    );
    for slot in [0, 1, 2, 6] {
        assert_ne!(
            table[slot], original_slots[slot],
            "replacement transport must patch slot {slot} in the shared table"
        );
    }
    assert_eq!(
        &table[3..6],
        &original_slots[3..6],
        "accept, close and close-channel must remain native Steam functions"
    );
    assert_eq!(
        &table[7..],
        &original_slots[7..],
        "uncontrolled SteamNetworking006 slots must stay untouched"
    );
    let hooked_slots = *table;
    let finds = FixtureFindCalls();
    assert_eq!(
        SteamInternal_FindOrCreateUserInterface(1, c"SteamNetworking006".as_ptr()),
        (&mut *object as *mut usize).cast(),
        "the imported interface factory must preserve the native result"
    );
    assert!(SteamInternal_FindOrCreateUserInterface(1, c"SteamNetworking005".as_ptr()).is_null());
    assert_eq!(
        FixtureFindCalls(),
        finds + 2,
        "both interface queries must reach Steam"
    );
    let lookups = FixtureNetworkingCalls();
    let callbacks = FixtureCallbackCalls();
    table[0] = original_slots[0];
    SteamAPI_RunCallbacks();
    assert_eq!(
        table[0], hooked_slots[0],
        "the first callback pass must restore the cached hook"
    );
    table[0] = original_slots[0];
    for _ in 0..60 {
        SteamAPI_RunCallbacks();
    }
    assert_eq!(
        table[0], hooked_slots[0],
        "the callback cadence must reinstall a restored slot"
    );
    assert_eq!(
        FixtureCallbackCalls(),
        callbacks + 61,
        "every callback call must reach Steam"
    );
    assert_eq!(
        FixtureNetworkingCalls(),
        lookups,
        "cached-table checks must not recreate the interface"
    );
    assert_eq!(&table[3..6], &original_slots[3..6]);
    println!("IPC ready; verifying receive without synthetic callbacks");
    let callback_table = [
        callback_run as *const () as usize,
        unused as *const () as usize,
        callback_size as *const () as usize,
    ];
    let mut request = Callback {
        table: callback_table.as_ptr(),
        flags: 0,
        id: 1202,
        network: &mut *object,
    };
    let mut failure = Callback {
        table: callback_table.as_ptr(),
        flags: 0,
        id: 1203,
        network: &mut *object,
    };
    SteamAPI_RegisterCallback((&mut request as *mut Callback).cast(), 1202);
    SteamAPI_RegisterCallback((&mut failure as *mut Callback).cast(), 1203);
    write_message(
        &mut socket,
        &Message::Members(vec![
            Peer {
                client_id: 2,
                steam_id: 202,
                epoch: 22,
            },
            Peer {
                client_id: 9,
                steam_id: SHARED_REMOTE,
                epoch: 99,
            },
        ]),
    )
    .unwrap();
    write_message(&mut socket, &Message::IpcReady).unwrap();
    let incoming = Packet {
        delivery: None,
        from: 202,
        to: 101,
        source_epoch: 22,
        target_epoch: 11,
        channel: 3,
        send_type: 2,
        payload: b"abcdef".to_vec(),
    };
    write_message(&mut socket, &Message::Data(incoming.clone())).unwrap();
    let patched = original_table as *const usize;
    let send: unsafe extern "thiscall" fn(*mut c_void, u64, *const c_void, u32, i32, i32) -> bool =
        transmute(*patched);
    let available: unsafe extern "thiscall" fn(*mut c_void, *mut u32, i32) -> bool =
        transmute(*patched.add(1));
    let read: unsafe extern "thiscall" fn(
        *mut c_void,
        *mut c_void,
        u32,
        *mut u32,
        *mut u64,
        i32,
    ) -> bool = transmute(*patched.add(2));
    let close: unsafe extern "thiscall" fn(*mut c_void, u64) -> bool = transmute(*patched.add(4));
    let object = (&mut *object as *mut usize).cast();
    let mut count = 0;
    eventually(|| available(object, &mut count, 3));
    assert_eq!(count, 6);
    let shared = (&mut *shared_object as *mut usize).cast();
    SHARED_OBJECT.store(shared as usize, Ordering::Release);
    let shared_send: unsafe extern "thiscall" fn(
        *mut c_void,
        u64,
        *const c_void,
        u32,
        i32,
        i32,
    ) -> bool = transmute(*patched);
    let shared_available: unsafe extern "thiscall" fn(*mut c_void, *mut u32, i32) -> bool =
        transmute(*patched.add(1));
    let shared_read: unsafe extern "thiscall" fn(
        *mut c_void,
        *mut c_void,
        u32,
        *mut u32,
        *mut u64,
        i32,
    ) -> bool = transmute(*patched.add(2));
    let shared_accept: unsafe extern "thiscall" fn(*mut c_void, u64) -> bool =
        transmute(*patched.add(3));
    let shared_close: unsafe extern "thiscall" fn(*mut c_void, u64) -> bool =
        transmute(*patched.add(4));
    let shared_close_channel: unsafe extern "thiscall" fn(*mut c_void, u64, i32) -> bool =
        transmute(*patched.add(5));
    let shared_session: unsafe extern "thiscall" fn(*mut c_void, u64, *mut c_void) -> bool =
        transmute(*patched.add(6));
    // Shared vtables route every object through the replacement transport. A native
    // packet cannot leak into the game, even if its size query would otherwise win.
    let mut shared_count = 0;
    assert!(shared_available(shared, &mut shared_count, 3));
    assert_eq!(shared_count, 6);
    NATIVE_PACKETS
        .lock()
        .unwrap()
        .push_back((SHARED_REMOTE, SHARED_CHANNEL, b"native".to_vec()));
    assert!(!shared_available(shared, &mut shared_count, SHARED_CHANNEL));
    let mut shared_packet = incoming.clone();
    shared_packet.from = SHARED_REMOTE;
    shared_packet.source_epoch = 99;
    shared_packet.channel = SHARED_CHANNEL;
    shared_packet.payload = SHARED_PAYLOAD.to_vec();
    write_message(&mut socket, &Message::Data(shared_packet)).unwrap();
    eventually(|| shared_available(shared, &mut shared_count, SHARED_CHANNEL));
    assert_eq!(shared_count, SHARED_PAYLOAD.len() as u32);
    let mut shared_bytes = [0u8; SHARED_PAYLOAD.len()];
    let mut shared_remote = 0;
    assert!(shared_read(
        shared,
        shared_bytes.as_mut_ptr().cast(),
        shared_bytes.len() as u32,
        &mut shared_count,
        &mut shared_remote,
        SHARED_CHANNEL,
    ));
    assert_eq!(shared_bytes.as_slice(), SHARED_PAYLOAD);
    assert_eq!(
        (shared_count, shared_remote),
        (SHARED_PAYLOAD.len() as u32, SHARED_REMOTE)
    );
    assert!(shared_send(
        shared,
        SHARED_REMOTE,
        SHARED_PAYLOAD.as_ptr().cast(),
        SHARED_PAYLOAD.len() as u32,
        2,
        SHARED_CHANNEL,
    ));
    let sent = read_data(&mut socket);
    assert_eq!(
        (sent.from, sent.to, sent.source_epoch, sent.target_epoch),
        (101, SHARED_REMOTE, 11, 99)
    );
    assert_eq!(
        (sent.payload.as_slice(), sent.channel, sent.send_type),
        (SHARED_PAYLOAD, SHARED_CHANNEL, 2)
    );
    assert!(shared_accept(shared, SHARED_REMOTE));
    assert!(shared_close(shared, SHARED_REMOTE));
    assert!(shared_close_channel(shared, SHARED_REMOTE, SHARED_CHANNEL));
    let mut session_with_canaries = [0xa5u8; 28];
    assert!(shared_session(
        shared,
        SHARED_REMOTE,
        session_with_canaries[4..24].as_mut_ptr().cast(),
    ));
    assert_eq!(&session_with_canaries[..4], &[0xa5; 4]);
    assert_eq!(&session_with_canaries[4..24], &SHARED_SESSION);
    assert_eq!(&session_with_canaries[24..], &[0xa5; 4]);
    // The other object's original function returns false and leaves its output
    // untouched. Hook must preserve both details instead of fabricating state.
    let session: unsafe extern "thiscall" fn(*mut c_void, u64, *mut c_void) -> bool =
        transmute(*patched.add(6));
    let mut untouched = [0xa5u8; 20];
    assert!(!session(object, 202, untouched.as_mut_ptr().cast()));
    assert_eq!(untouched, [0xa5; 20]);
    assert_eq!(
        (
            SHARED_PEERS.load(Ordering::SeqCst),
            SHARED_CHANNELS.load(Ordering::SeqCst),
            SHARED_SESSIONS.load(Ordering::SeqCst),
            SHARED_ABI_FAILURES.load(Ordering::SeqCst),
        ),
        (2, 1, 1, 0),
        "native session calls must preserve the shared object's x86 thiscall/u64 ABI"
    );
    assert!(!available(object, &mut count, 4));
    SteamAPI_RunCallbacks();
    assert_eq!(
        REQUESTS.load(Ordering::SeqCst),
        0,
        "Hook must not invoke game callback objects"
    );
    let mut small = [0u8; 2];
    let mut remote = 0;
    assert!(read(
        object,
        small.as_mut_ptr().cast(),
        2,
        &mut count,
        &mut remote,
        3
    ));
    assert_eq!((small, count, remote), (*b"ab", 2, 202));
    assert!(!available(object, &mut count, 3));
    // Native close/accept state must not gate or discard replacement traffic.
    write_message(&mut socket, &Message::Data(incoming.clone())).unwrap();
    let mut second = incoming.clone();
    second.payload = b"second".to_vec();
    write_message(&mut socket, &Message::Data(second)).unwrap();
    eventually(|| available(object, &mut count, 3));
    assert!(close(object, 202));
    let mut buffer = [0u8; 6];
    assert!(read(
        object,
        buffer.as_mut_ptr().cast(),
        6,
        &mut count,
        &mut remote,
        3
    ));
    assert_eq!((buffer, count, remote), (*b"abcdef", 6, 202));
    eventually(|| {
        read(
            object,
            buffer.as_mut_ptr().cast(),
            6,
            &mut count,
            &mut remote,
            3,
        )
    });
    assert_eq!((buffer, count, remote), (*b"second", 6, 202));
    assert!(!available(object, &mut count, SHARED_CHANNEL));
    assert!(!read(
        object,
        buffer.as_mut_ptr().cast(),
        6,
        &mut count,
        &mut remote,
        SHARED_CHANNEL
    ));
    assert!(!send(object, 404, b"unknown".as_ptr().cast(), 7, 2, 0));
    assert_eq!(
        (
            NATIVE_SENDS.load(Ordering::SeqCst),
            NATIVE_AVAILABLE.load(Ordering::SeqCst),
            NATIVE_READS.load(Ordering::SeqCst)
        ),
        (0, 0, 0),
        "replacement transport must never fall through to native send, available or read"
    );
    assert_eq!(NATIVE_PACKETS.lock().unwrap().len(), 1);
    println!(
        "PASS: shared-table replacement, native session ABI, receive-first FIFO and no native data fallback"
    );
    assert!(send(object, 202, b"out".as_ptr().cast(), 3, 1, 9));
    let sent = read_data(&mut socket);
    assert_eq!(
        (
            sent.from,
            sent.to,
            sent.source_epoch,
            sent.target_epoch,
            sent.channel,
            sent.send_type,
            sent.payload
        ),
        (101, 202, 11, 22, 9, 1, b"out".to_vec())
    );
    // The queue test covers immediate dequeue without wall-clock assumptions.
    // Here a lone mode-3 packet reaches IPC without a second send to flush it.
    assert!(send(object, 202, b"a".as_ptr().cast(), 1, 3, 4));
    let first = read_data(&mut socket);
    assert_eq!(
        (first.payload, first.send_type, first.channel),
        (b"a".to_vec(), 3, 4)
    );
    for (payload, kind, channel) in [(b"b", 3, 5), (b"c", 2, 4)] {
        assert!(send(object, 202, payload.as_ptr().cast(), 1, kind, channel));
    }
    for (expected, kind, channel) in [(b"b", 3, 5), (b"c", 2, 4)] {
        let packet = read_data(&mut socket);
        assert_eq!(packet.payload, expected);
        assert_eq!((packet.send_type, packet.channel), (kind, channel));
    }
    println!("PASS: no-delay first send and immediate reliable FIFO through x86 Hook/IPC");
    // Real-time consumption pause: Hook stays live while the game does not call ReadP2PPacket.
    let mut retained = incoming.clone();
    retained.channel = 7;
    retained.payload = b"r".to_vec();
    write_message(&mut socket, &Message::Data(retained)).unwrap();
    let duplicate = SESSION
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .pending()
        .last()
        .unwrap()
        .clone();
    eventually(|| available(object, &mut count, 7));
    socket.shutdown(std::net::Shutdown::Both).unwrap();
    drop(socket);
    let mut replacement = None;
    eventually(|| {
        replacement = listener.accept().ok().map(|p| p.0);
        replacement.is_some()
    });
    let mut socket = replacement.unwrap();
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    match netburrow_protocol::read_message(&mut socket).unwrap() {
        Message::IpcResume {
            nonce,
            pid,
            steam_id,
            epoch,
            received,
        } => {
            assert_eq!(
                (nonce, pid, steam_id, epoch),
                ([7; 16], std::process::id(), 101, 11)
            );
            SESSION
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .acknowledge(received)
                .unwrap();
        }
        other => panic!("expected original IPC session resume, got {other:?}"),
    }
    let through = SESSION.lock().unwrap().as_ref().unwrap().received_through();
    netburrow_protocol::write_message(&mut socket, &Message::SessionAck(through)).unwrap();
    for (sequence, body) in SESSION.lock().unwrap().as_ref().unwrap().pending() {
        netburrow_protocol::write_message(&mut socket, &Message::SessionFrame { sequence, body })
            .unwrap();
    }
    netburrow_protocol::write_message(
        &mut socket,
        &Message::SessionFrame {
            sequence: duplicate.0,
            body: duplicate.1,
        },
    )
    .unwrap();
    assert!(matches!(
        read_message(&mut socket).unwrap(),
        Message::Diagnostic(_)
    ));
    assert_eq!(read_message(&mut socket).unwrap(), Message::IpcReady);
    let mut data = [0u8; 1];
    assert!(read(
        object,
        data.as_mut_ptr().cast(),
        1,
        &mut count,
        &mut remote,
        7
    ));
    assert_eq!(data, *b"r");
    // Wait for a ping after replay processing before asserting duplicate suppression.
    loop {
        match read_message(&mut socket).unwrap() {
            Message::Ping(n) => {
                write_message(&mut socket, &Message::Pong(n)).unwrap();
                break;
            }
            Message::IpcHealth(_) => {}
            other => panic!("unexpected recovery message {other:?}"),
        }
    }
    assert!(!available(object, &mut count, 7));
    eventually(|| {
        workers = hook_workers();
        workers.len() == 2
    });
    assert!(send(object, 202, b"resume".as_ptr().cast(), 6, 2, 8));
    loop {
        match read_message(&mut socket).unwrap() {
            Message::Data(p) => {
                assert_eq!(p.payload, b"resume");
                break;
            }
            Message::Ping(n) => write_message(&mut socket, &Message::Pong(n)).unwrap(),
            Message::IpcHealth(_) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    println!(
        "PASS: original x86 Hook survives IPC reconnect, preserves queued reliable data and suppresses replay duplicates"
    );
    println!("Starting 20s no-read backlog test (three peers, 90% dominant traffic)");
    write_message(
        &mut socket,
        &Message::Members(vec![
            Peer {
                client_id: 2,
                steam_id: 202,
                epoch: 22,
            },
            Peer {
                client_id: 3,
                steam_id: 302,
                epoch: 32,
            },
            Peer {
                client_id: 4,
                steam_id: 303,
                epoch: 33,
            },
        ]),
    )
    .unwrap();
    let worker = std::thread::spawn(move || {
        let started = std::time::Instant::now();
        for index in 0u32..3000 {
            let (peer, epoch, channel) = match index % 30 {
                0 => (303, 33, 5),
                1 | 2 => (302, 32, 4),
                _ => (202, 22, 3),
            };
            let mut payload = vec![0; 256];
            payload[..4].copy_from_slice(&index.to_le_bytes());
            write_message(
                &mut socket,
                &Message::Data(Packet {
                    delivery: None,
                    from: peer,
                    to: 101,
                    source_epoch: epoch,
                    target_epoch: 11,
                    channel,
                    send_type: 2,
                    payload,
                }),
            )
            .unwrap();
            if index % 150 == 0 {
                write_message(&mut socket, &Message::Pong(0)).unwrap();
            }
            let due = Duration::from_micros((u64::from(index) + 1) * 1_000_000 / 150);
            if let Some(wait) = due.checked_sub(started.elapsed()) {
                std::thread::sleep(wait);
            }
        }
        socket
    });
    let mut socket = worker.join().unwrap();
    let mut buffer = [0u8; 256];
    for channel in 3..=5 {
        for index in (0u32..3000).filter(|index| match index % 30 {
            0 => channel == 5,
            1 | 2 => channel == 4,
            _ => channel == 3,
        }) {
            eventually(|| {
                read(
                    object,
                    buffer.as_mut_ptr().cast(),
                    256,
                    &mut count,
                    &mut remote,
                    channel,
                )
            });
            assert_eq!(count, 256);
            assert_eq!(&buffer[..4], &index.to_le_bytes());
        }
        assert!(!available(object, &mut count, channel));
    }
    println!("PASS: 20s real-time 3000-packet reliable backlog consumed without gaps/duplicates");
    // The imported factory can expose another table after initialization. Its
    // originals must be saved independently and looked up using this's current table.
    let mut replacement_table = Box::new(original_slots);
    replacement_table[6] = replacement_session as *const () as usize;
    let replacement_originals = *replacement_table;
    let replacement_address = replacement_table.as_ptr() as usize;
    let mut replacement_object = Box::new(replacement_address);
    let replacement = (&mut *replacement_object as *mut usize).cast();
    FixtureSetObject(replacement as usize);
    assert_eq!(
        SteamInternal_FindOrCreateUserInterface(1, c"SteamNetworking006".as_ptr()),
        replacement
    );
    assert_eq!(*replacement_object, replacement_address);
    for slot in [0, 1, 2, 6] {
        assert_ne!(replacement_table[slot], replacement_originals[slot]);
    }
    assert_eq!(&replacement_table[3..6], &replacement_originals[3..6]);
    assert_eq!(&replacement_table[7..], &replacement_originals[7..]);
    let mut second_state = [0xa5u8; 28];
    assert!(!session(
        replacement,
        SHARED_REMOTE,
        second_state[4..24].as_mut_ptr().cast()
    ));
    assert_eq!(&second_state[..4], &[0xa5; 4]);
    assert_eq!(&second_state[4..24], &[0x3c; 20]);
    assert_eq!(&second_state[24..], &[0xa5; 4]);
    *replacement_object = original_table;
    second_state.fill(0xa5);
    assert!(!session(
        replacement,
        SHARED_REMOTE,
        second_state[4..24].as_mut_ptr().cast()
    ));
    assert_eq!(second_state, [0xa5; 28]);
    *replacement_object = replacement_address;
    assert!(!session(
        replacement,
        SHARED_REMOTE,
        second_state[4..24].as_mut_ptr().cast()
    ));
    assert_eq!(&second_state[4..24], &[0x3c; 20]);
    assert!(shared_session(
        shared,
        SHARED_REMOTE,
        session_with_canaries[4..24].as_mut_ptr().cast()
    ));
    assert_eq!(&session_with_canaries[4..24], &SHARED_SESSION);
    let replacement_send: unsafe extern "thiscall" fn(
        *mut c_void,
        u64,
        *const c_void,
        u32,
        i32,
        i32,
    ) -> bool = transmute(replacement_table[0]);
    assert!(replacement_send(
        replacement,
        202,
        b"new-table".as_ptr().cast(),
        9,
        2,
        8
    ));
    assert_eq!(read_data(&mut socket).payload, b"new-table");
    let lookups = FixtureNetworkingCalls();
    replacement_table[0] = replacement_originals[0];
    for _ in 0..60 {
        SteamAPI_RunCallbacks();
    }
    assert_eq!(replacement_table[0], hooked_slots[0]);
    assert_eq!(FixtureNetworkingCalls(), lookups);
    println!(
        "PASS: imported interface factory installs new tables and preserves each table's native session implementation"
    );
    write_message(&mut socket, &Message::Stop).unwrap();
    for worker in &workers {
        assert_eq!(
            WaitForSingleObject(worker.as_raw_handle(), 4_000),
            0,
            "Hook thread did not terminate after Stop"
        );
    }
    assert!(!send(object, 202, b"stopped".as_ptr().cast(), 7, 2, 0));
    assert!(!send(object, 404, b"unknown".as_ptr().cast(), 7, 2, 0));
    assert!(!available(object, &mut count, SHARED_CHANNEL));
    assert!(!read(
        object,
        buffer.as_mut_ptr().cast(),
        256,
        &mut count,
        &mut remote,
        SHARED_CHANNEL
    ));
    assert_eq!(
        (
            NATIVE_SENDS.load(Ordering::SeqCst),
            NATIVE_AVAILABLE.load(Ordering::SeqCst),
            NATIVE_READS.load(Ordering::SeqCst)
        ),
        (0, 0, 0),
        "stopped replacement transport must not fall back to native data functions"
    );
    assert!(shared_accept(shared, SHARED_REMOTE));
    assert!(shared_close(shared, SHARED_REMOTE));
    assert!(shared_close_channel(shared, SHARED_REMOTE, SHARED_CHANNEL));
    session_with_canaries.fill(0xa5);
    assert!(shared_session(
        shared,
        SHARED_REMOTE,
        session_with_canaries[4..24].as_mut_ptr().cast()
    ));
    assert_eq!(&session_with_canaries[4..24], &SHARED_SESSION);
    assert_eq!(&session_with_canaries[..4], &[0xa5; 4]);
    assert_eq!(&session_with_canaries[24..], &[0xa5; 4]);
    second_state.fill(0xa5);
    assert!(!session(
        replacement,
        SHARED_REMOTE,
        second_state[4..24].as_mut_ptr().cast()
    ));
    assert_eq!(&second_state[4..24], &[0x3c; 20]);
    SteamAPI_RunCallbacks();
    assert_eq!(
        (
            REQUESTS.load(Ordering::SeqCst),
            FAILURES.load(Ordering::SeqCst)
        ),
        (0, 0)
    );
    assert_eq!(SHARED_ABI_FAILURES.load(Ordering::SeqCst), 0);
    SteamAPI_UnregisterCallback((&mut request as *mut Callback).cast());
    SteamAPI_UnregisterCallback((&mut failure as *mut Callback).cast());
    println!(
        "PASS: x86 helper identity rejection, DLL load, IPC identity, native callbacks and sessions, channel/truncated read, replacement transport and stop without native data fallback"
    );
}
