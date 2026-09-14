//! A controlled x86 process named isaac-ng.exe, with our test ABI DLL only.
//! Exercises the real helper and Hook without opening Steam or touching game files.
#![allow(unsafe_op_in_unsafe_fn)]
use netburrow_protocol::{Message, Packet, Peer, read_message, write_message};
use std::{
    ffi::c_void,
    io::Write,
    mem::transmute,
    net::TcpListener,
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
    fn GetCurrentProcess() -> *mut c_void;
    fn GetProcessTimes(
        process: *mut c_void,
        created: *mut u64,
        exit: *mut u64,
        kernel: *mut u64,
        user: *mut u64,
    ) -> i32;
}
static NATIVE_SENDS: AtomicUsize = AtomicUsize::new(0);
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
unsafe extern "thiscall" fn native_available(_: *mut c_void, _: *mut u32, _: i32) -> bool {
    false
}
unsafe extern "thiscall" fn native_read(
    _: *mut c_void,
    _: *mut c_void,
    _: u32,
    _: *mut u32,
    _: *mut u64,
    _: i32,
) -> bool {
    false
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
        Message::IpcHello {
            nonce: [7; 16],
            pid: std::process::id(),
            steam_id: 101,
            epoch: 11
        }
    );
    assert!(matches!(
        read_message(&mut socket).unwrap(),
        Message::Diagnostic(_)
    ));
    assert_eq!(read_message(&mut socket).unwrap(), Message::IpcReady);
    println!("IPC ready; waiting for request callback");
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
        from: 202,
        to: 101,
        source_epoch: 22,
        target_epoch: 11,
        channel: 3,
        send_type: 2,
        payload: b"abcdef".to_vec(),
    };
    write_message(&mut socket, &Message::Data(incoming.clone())).unwrap();
    eventually(|| {
        SteamAPI_RunCallbacks();
        REQUESTS.load(Ordering::SeqCst) == 1
    });
    println!("request callback passed");
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
    // Clear callback acceptance, then start solely with Isaac's no-delay send mode.
    eventually(|| close(object, 202));
    eventually(|| send(object, 202, b"out".as_ptr().cast(), 3, 1, 9));
    loop {
        match read_message(&mut socket).unwrap() {
            Message::Ping(n) => write_message(&mut socket, &Message::Pong(n)).unwrap(),
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
    write_message(&mut socket, &Message::Stop).unwrap();
    eventually(|| {
        SteamAPI_RunCallbacks();
        FAILURES.load(Ordering::SeqCst) == 1
    });
    assert!(!send(object, 202, b"stopped".as_ptr().cast(), 7, 2, 0));
    assert_eq!(
        NATIVE_SENDS.load(Ordering::SeqCst),
        1,
        "owned peer must not fall back after stop"
    );
    SteamAPI_UnregisterCallback((&mut request as *mut Callback).cast());
    SteamAPI_UnregisterCallback((&mut failure as *mut Callback).cast());
    println!(
        "PASS: x86 helper identity rejection, DLL load, IPC identity, callback acceptance, channel/truncated read, no-delay first-packet routing and stop without fallback"
    );
}
