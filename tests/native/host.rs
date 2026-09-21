//! A controlled x86 process named isaac-ng.exe, with our test ABI DLL only.
//! Exercises the real helper and Hook without opening Steam or touching game files.
#![allow(unsafe_op_in_unsafe_fn)]
use netburrow_protocol::{Message, Packet, Peer};
static SESSION:std::sync::Mutex<Option<netburrow_protocol::resume::Window>>=std::sync::Mutex::new(None);
fn write_message(socket:&mut std::net::TcpStream,message:&Message)->std::io::Result<()> {
    let mut session=SESSION.lock().unwrap();
    if matches!(message,Message::IpcAccepted(_)){*session=Some(Default::default());}
    let message=if netburrow_protocol::replayable(message)&&session.is_some(){
        let body=netburrow_protocol::encode(message)?;
        let sequence=session.as_mut().unwrap().retain(body.clone())?;
        Message::SessionFrame{sequence,body}
    }else{message.clone()};
    netburrow_protocol::write_message(socket,&message)
}
fn read_message(socket:&mut std::net::TcpStream)->std::io::Result<Message> {
    loop {
        let message=netburrow_protocol::read_message(socket)?;
        match message {
            Message::SessionAck(n)=>{SESSION.lock().unwrap().as_mut().unwrap().acknowledge(n)?;}
            Message::SessionFrame{sequence,body}=>{
                let mut session=SESSION.lock().unwrap();let session=session.as_mut().unwrap();
                let fresh=session.classify(sequence)?;
                let message=netburrow_protocol::decode_session_body(&body)?;
                session.received(sequence)?;
                netburrow_protocol::write_message(socket,&Message::SessionAck(session.received_through()))?;
                if fresh{return Ok(message);}
            }
            _=>return Ok(message),
        }
    }
}
use std::{
    ffi::c_void,
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
static NATIVE_READS: AtomicUsize = AtomicUsize::new(0);
static NATIVE_PACKETS: std::sync::Mutex<std::collections::VecDeque<(u64, i32, Vec<u8>)>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());
static REQUESTS: AtomicUsize = AtomicUsize::new(0);
static FAILURES: AtomicUsize = AtomicUsize::new(0);
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
    let packets = NATIVE_PACKETS.lock().unwrap();
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
    let mut packets = NATIVE_PACKETS.lock().unwrap();
    let Some(index) = packets.iter().position(|(_, c, _)| *c == channel) else {
        return false;
    };
    let (peer, _, payload) = packets.remove(index).unwrap();
    let length = payload.len().min(capacity as usize);
    std::ptr::copy_nonoverlapping(payload.as_ptr(), destination.cast::<u8>(), length);
    size.write(length as u32);
    remote.write_unaligned(peer);
    true
}
unsafe extern "thiscall" fn native_peer(_: *mut c_void, _: u64) -> bool {
    true
}
unsafe extern "thiscall" fn native_channel(_: *mut c_void, _: u64, _: i32) -> bool {
    true
}
unsafe extern "thiscall" fn native_session(_: *mut c_void, _: u64, _: *mut c_void) -> bool {
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
    let mut object = Box::new(table.as_ptr() as usize);
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
    println!("helper passed; waiting for IPC");
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
        &Message::Members(vec![Peer {
            client_id: 2,
            steam_id: 202,
            epoch: 22,
        }]),
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
    let patched = *object as *const usize;
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
    assert!(!available(object, &mut count, 4));
    eventually(|| available(object, &mut count, 3));
    assert_eq!(count, 6);
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
    // A cleared Hook reservation must fail without falling through to Steam.
    write_message(&mut socket, &Message::Data(incoming.clone())).unwrap();
    eventually(|| available(object, &mut count, 3));
    assert_eq!(count, 6);
    eventually(|| close(object, 202));
    let native_reads = NATIVE_READS.load(Ordering::SeqCst);
    assert!(!read(object, small.as_mut_ptr().cast(), 2, &mut count, &mut remote, 3));
    assert_eq!(NATIVE_READS.load(Ordering::SeqCst), native_reads);
    assert!(!read(object, small.as_mut_ptr().cast(), 2, &mut count, &mut remote, 3));
    assert_eq!(NATIVE_READS.load(Ordering::SeqCst), native_reads);
    assert!(!available(object, &mut count, 3));
    assert!(!read(object, small.as_mut_ptr().cast(), 2, &mut count, &mut remote, 3));
    assert_eq!(NATIVE_READS.load(Ordering::SeqCst), native_reads + 1);
    println!("PASS: invalidated Hook query blocks native fallback until a fresh query");
    // Keep the source of a native size query when Hook data arrives in between.
    NATIVE_PACKETS.lock().unwrap().push_back((303, 12, vec![7; 16]));
    assert!(available(object, &mut count, 12));
    assert_eq!(count, 16);
    let mut hook_packet = incoming.clone();
    hook_packet.channel = 12;
    hook_packet.payload = vec![8; 40];
    write_message(&mut socket, &Message::Data(hook_packet)).unwrap();
    let mut barrier = incoming.clone();
    barrier.channel = 13;
    barrier.payload = vec![0];
    write_message(&mut socket, &Message::Data(barrier)).unwrap();
    eventually(|| available(object, &mut count, 13));
    let mut buffer = [0u8; 40];
    assert!(read(object, buffer.as_mut_ptr().cast(), 40, &mut count, &mut remote, 13));
    assert!(read(object, buffer.as_mut_ptr().cast(), 16, &mut count, &mut remote, 12));
    let native_source_kept = remote == 303 && count == 16 && buffer[..16] == [7; 16];
    // Drain the remaining packet so both regressions can report before failing.
    while available(object, &mut count, 12) {
        assert!(read(object, buffer.as_mut_ptr().cast(), 40, &mut count, &mut remote, 12));
    }
    // Discarding an owned peer's stale native packet must not consume the next one.
    NATIVE_PACKETS.lock().unwrap().extend([(202, 14, vec![1; 8]), (303, 14, vec![9; 40])]);
    assert!(available(object, &mut count, 14));
    assert_eq!(count, 8);
    let discarded_without_replacement =
        !read(object, buffer.as_mut_ptr().cast(), 8, &mut count, &mut remote, 14);
    let native_reads = NATIVE_READS.load(Ordering::SeqCst);
    assert!(!read(object, buffer.as_mut_ptr().cast(), 8, &mut count, &mut remote, 14));
    assert_eq!(NATIVE_READS.load(Ordering::SeqCst), native_reads);
    let next_available = available(object, &mut count, 14);
    let larger_packet_retained = next_available && count == 40;
    if next_available {
        assert!(read(object, buffer.as_mut_ptr().cast(), 40, &mut count, &mut remote, 14));
        assert_eq!((remote, count, buffer), (303, 40, [9; 40]));
    }
    assert!(native_source_kept && discarded_without_replacement && larger_packet_retained,
        "native source kept={native_source_kept}, stale read rejected={discarded_without_replacement}, next packet retained={larger_packet_retained}");
    println!("PASS: native size queries keep their source and stale packets cannot substitute a larger packet");
    // Close the local session, then start solely with Isaac's no-delay send mode.
    eventually(|| close(object, 202));
    eventually(|| send(object, 202, b"out".as_ptr().cast(), 3, 1, 9));
    loop {
        match read_message(&mut socket).unwrap() {
            Message::Ping(n) => write_message(&mut socket, &Message::Pong(n)).unwrap(),
            Message::IpcHealth(_) => {}
            Message::Data(packet) => {
                assert_eq!(
                    (
                        packet.from,
                        packet.to,
                        packet.source_epoch,
                        packet.target_epoch,
                        packet.channel,
                        packet.send_type,
                        packet.payload
                    ),
                    (101, 202, 11, 22, 9, 1, b"out".to_vec())
                );
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(send(object, 303, b"native".as_ptr().cast(), 6, 2, 0));
    assert_eq!(NATIVE_SENDS.load(Ordering::SeqCst), 1);
    // Validate the actual x86 ABI and IPC path, including buffering across channels.
    for (payload, kind, channel) in [(b"a", 3, 4), (b"b", 3, 5), (b"c", 2, 4)] {
        assert!(send(object, 202, payload.as_ptr().cast(), 1, kind, channel));
    }
    for (expected, kind, channel) in [(b"a", 3, 4), (b"b", 3, 5), (b"c", 2, 4)] {
        loop {
            match read_message(&mut socket).unwrap() {
                Message::Ping(n) => write_message(&mut socket, &Message::Pong(n)).unwrap(),
                Message::IpcHealth(_) => {}
                Message::Data(packet) => {
                    assert_eq!(packet.payload, expected);
                    assert_eq!((packet.send_type, packet.channel), (kind, channel));
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }
    println!(
        "PASS: mixed buffered reliable packets preserve order across channels through x86 Hook/IPC"
    );
    // Real-time consumption pause: Hook stays live while the game does not call ReadP2PPacket.
    let mut retained=incoming.clone();retained.channel=7;retained.payload=b"r".to_vec();
    write_message(&mut socket,&Message::Data(retained)).unwrap();
    let duplicate=SESSION.lock().unwrap().as_ref().unwrap().pending().last().unwrap().clone();
    eventually(||available(object,&mut count,7));
    socket.shutdown(std::net::Shutdown::Both).unwrap();drop(socket);
    let mut replacement=None;
    eventually(||{replacement=listener.accept().ok().map(|p|p.0);replacement.is_some()});
    let mut socket=replacement.unwrap();socket.set_nonblocking(false).unwrap();socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    match netburrow_protocol::read_message(&mut socket).unwrap() {
        Message::IpcResume{nonce,pid,steam_id,epoch,received}=>{
            assert_eq!((nonce,pid,steam_id,epoch),([7;16],std::process::id(),101,11));
            SESSION.lock().unwrap().as_mut().unwrap().acknowledge(received).unwrap();
        }
        other=>panic!("expected original IPC session resume, got {other:?}"),
    }
    let through=SESSION.lock().unwrap().as_ref().unwrap().received_through();
    netburrow_protocol::write_message(&mut socket,&Message::SessionAck(through)).unwrap();
    for (sequence,body) in SESSION.lock().unwrap().as_ref().unwrap().pending(){netburrow_protocol::write_message(&mut socket,&Message::SessionFrame{sequence,body}).unwrap();}
    netburrow_protocol::write_message(&mut socket,&Message::SessionFrame{sequence:duplicate.0,body:duplicate.1}).unwrap();
    assert!(matches!(read_message(&mut socket).unwrap(),Message::Diagnostic(_)));
    assert_eq!(read_message(&mut socket).unwrap(),Message::IpcReady);
    let mut data=[0u8;1];
    assert!(read(object,data.as_mut_ptr().cast(),1,&mut count,&mut remote,7));assert_eq!(data,*b"r");
    // Wait for a ping after replay processing before asserting duplicate suppression.
    loop {match read_message(&mut socket).unwrap(){Message::Ping(n)=>{write_message(&mut socket,&Message::Pong(n)).unwrap();break;},Message::IpcHealth(_)=>{},other=>panic!("unexpected recovery message {other:?}")}}
    assert!(!available(object,&mut count,7));
    eventually(||{workers=hook_workers();workers.len()==2});
    assert!(send(object,202,b"resume".as_ptr().cast(),6,2,8));
    loop {match read_message(&mut socket).unwrap(){Message::Data(p)=>{assert_eq!(p.payload,b"resume");break;},Message::Ping(n)=>write_message(&mut socket,&Message::Pong(n)).unwrap(),Message::IpcHealth(_)=>{},other=>panic!("unexpected {other:?}")}}
    println!("PASS: original x86 Hook survives IPC reconnect, preserves queued reliable data and suppresses replay duplicates");
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
    let accept: unsafe extern "thiscall" fn(*mut c_void, u64) -> bool = transmute(*patched.add(3));
    let session: unsafe extern "thiscall" fn(*mut c_void, u64, *mut c_void) -> bool =
        transmute(*patched.add(6));
    for peer in [202, 302, 303] {
        let mut state = [0u64; 4];
        eventually(|| session(object, peer, state.as_mut_ptr().cast()));
        eventually(|| accept(object, peer));
    }
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
    write_message(&mut socket, &Message::Stop).unwrap();
    let session: unsafe extern "thiscall" fn(*mut c_void, u64, *mut u8) -> bool =
        transmute(*patched.add(6));
    let mut state = [0u32; 5];
    eventually(|| {
        SteamAPI_RunCallbacks();
        session(object, 202, state.as_mut_ptr().cast()) && state[0].to_le_bytes()[2] == 4
    });
    assert_eq!(state[0].to_le_bytes()[0], 0);
    assert_eq!(
        FAILURES.load(Ordering::SeqCst),
        0,
        "Hook must not synthesize failure callbacks"
    );
    assert!(!send(object, 202, b"stopped".as_ptr().cast(), 7, 2, 0));
    assert_eq!(
        NATIVE_SENDS.load(Ordering::SeqCst),
        1,
        "owned peer must not fall back after stop"
    );
    SteamAPI_UnregisterCallback((&mut request as *mut Callback).cast());
    SteamAPI_UnregisterCallback((&mut failure as *mut Callback).cast());
    for worker in workers {
        assert_eq!(
            WaitForSingleObject(worker.as_raw_handle(), 4_000),
            0,
            "Hook thread did not terminate after Stop"
        );
    }
    println!(
        "PASS: x86 helper identity rejection, DLL load, IPC identity, receive-first acceptance without synthetic callbacks, channel/truncated read, no-delay first-packet routing and stop without fallback"
    );
}
