use crate::process::{self, Handle, wide};
use netburrow_protocol::{HOOK_INIT_VERSION, HookInit};
use std::{
    ffi::c_void,
    io::{self, Read},
    mem::{size_of, zeroed},
    path::{Path, PathBuf},
    ptr,
};
use windows_sys::Win32::{
    Foundation::{
        ERROR_BAD_LENGTH, ERROR_NO_MORE_FILES, ERROR_PARTIAL_COPY, FreeLibrary, HANDLE, HMODULE,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    System::{
        Diagnostics::{
            Debug::WriteProcessMemory,
            ToolHelp::{
                CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW,
                TH32CS_SNAPMODULE,
            },
        },
        LibraryLoader::{
            DONT_RESOLVE_DLL_REFERENCES, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleHandleExW, GetModuleHandleW,
            GetProcAddress, LoadLibraryExW,
        },
        Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAllocEx, VirtualFreeEx,
        },
        Threading::{
            CreateRemoteThread, GetExitCodeThread, OpenProcess, PROCESS_CREATE_THREAD,
            PROCESS_QUERY_INFORMATION, PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE,
            WaitForSingleObject,
        },
    },
};

pub fn run() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--help"] {
        println!(
            "netburrow-injector --pid N --created N --exe PATH --dll PATH --port N --epoch N (16-byte nonce on stdin)"
        );
        return Ok(());
    }
    let option = |key: &str| -> io::Result<&str> {
        args.chunks_exact(2)
            .find(|p| p[0] == key)
            .map(|p| p[1].as_str())
            .ok_or_else(|| io::Error::other(format!("missing {key}")))
    };
    if args.len() != 12 {
        return Err(io::Error::other("expected six named arguments; see --help"));
    }
    let number = |key| {
        option(key)?
            .parse::<u64>()
            .map_err(|_| io::Error::other("invalid numeric argument"))
    };
    let pid = u32::try_from(number("--pid")?).map_err(|_| io::Error::other("invalid pid"))?;
    let created = number("--created")?;
    let port =
        u16::try_from(number("--port")?).map_err(|_| io::Error::other("invalid IPC port"))?;
    let epoch = number("--epoch")?;
    if port == 0 || epoch == 0 {
        return Err(io::Error::other("zero port or instance"));
    }
    let exe = PathBuf::from(option("--exe")?).canonicalize()?;
    let dll = PathBuf::from(option("--dll")?).canonicalize()?;
    if !exe
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("isaac-ng.exe"))
    {
        return Err(io::Error::other("only isaac-ng.exe is supported"));
    }
    if !dll
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("netburrow_hook.dll"))
    {
        return Err(io::Error::other("expected netburrow_hook.dll"));
    }
    process::validate_x86_image(&exe)?;
    process::validate_x86_image(&dll)?;
    let target = step("validate target", process::describe_process(pid))?;
    crate::diagnostics::record(
        "INFO",
        "target",
        &format!("pid={pid}; x86 image, user/session verified"),
    );
    if target.created != created || !process::same_path(&target.path, &exe) {
        return Err(io::Error::other("target identity changed"));
    }
    let mut nonce = [0; 16];
    io::stdin().read_exact(&mut nonce)?;
    if nonce == [0; 16] {
        return Err(io::Error::other("invalid IPC nonce"));
    }
    let process = step(
        "OpenProcess",
        Handle::new(unsafe {
            OpenProcess(
                PROCESS_CREATE_THREAD
                    | PROCESS_QUERY_INFORMATION
                    | PROCESS_VM_OPERATION
                    | PROCESS_VM_READ
                    | PROCESS_VM_WRITE,
                0,
                pid,
            )
        }),
    )?;
    // Verify again after opening the process. PID reuse cannot redirect an already-open HANDLE.
    let load = prepare_loader(
        || {
            if process::describe_process(pid)? != target {
                return Err(io::Error::other("target changed during preparation"));
            }
            Ok(())
        },
        || remote_load_library(pid),
        std::thread::sleep,
    )?;
    let dll_name = wide(&dll);
    let bytes =
        unsafe { std::slice::from_raw_parts(dll_name.as_ptr().cast::<u8>(), dll_name.len() * 2) };
    let mut path_memory = step("prepare DLL path", RemoteMemory::new(process.0, bytes))?;
    // No retry may encompass this call: a started remote thread can outlive its timeout.
    let loaded = step(
        "execute LoadLibraryW",
        call_remote(process.0, load, &mut path_memory),
    )?;
    crate::diagnostics::record(
        "INFO",
        "LoadLibraryW",
        &format!("completed success={}", loaded != 0),
    );
    if loaded == 0 {
        return Err(io::Error::other("LoadLibraryW failed in game"));
    }
    let remote_base = step("find loaded Hook", modules(pid))?
        .into_iter()
        .find(|(_, p)| process::same_path(p, &dll))
        .map(|(base, _)| base)
        .ok_or_else(|| io::Error::other("loaded DLL not found in target"))?;
    let init_rva = step(
        "resolve NetBurrowInit",
        export_rva(&dll, b"NetBurrowInit\0"),
    )?;
    let init = HookInit {
        version: HOOK_INIT_VERSION,
        port: port as u32,
        pid,
        reserved: 0,
        epoch,
        nonce,
    };
    let bytes = unsafe {
        std::slice::from_raw_parts(
            (&init as *const HookInit).cast::<u8>(),
            size_of::<HookInit>(),
        )
    };
    let mut init_memory = step("prepare HookInit", RemoteMemory::new(process.0, bytes))?;
    let code = step(
        "execute NetBurrowInit",
        call_remote(
            process.0,
            remote_base
                .checked_add(init_rva)
                .ok_or_else(|| io::Error::other("invalid initialization address"))?,
            &mut init_memory,
        ),
    )?;
    if code != 0 {
        crate::diagnostics::record(
            "ERROR",
            "NetBurrowInit",
            &format!(
                "return={code}; 1=invalid parameters, 2=already initialized, 3=worker creation failed"
            ),
        );
        return Err(io::Error::other(format!(
            "DLL initialization rejected request ({code})"
        )));
    }
    println!("Hook loaded; waiting for IPC readiness.");
    crate::diagnostics::record(
        "INFO",
        "NetBurrowInit",
        "accepted; waiting for Hook worker and IPC",
    );
    Ok(())
}

// Preserve the OS code for classification while recording the exact failing operation.
fn step<T>(name: &str, result: io::Result<T>) -> io::Result<T> {
    result.inspect_err(|error| {
        crate::diagnostics::record("ERROR", name, &error.to_string());
    })
}

fn prepare_loader(
    mut verify: impl FnMut() -> io::Result<()>,
    mut resolve: impl FnMut() -> io::Result<usize>,
    mut wait: impl FnMut(std::time::Duration),
) -> io::Result<usize> {
    let delays = [200, 500];
    for attempt in 0..=delays.len() {
        // Identity/permission/exit failures are terminal, including during a retry.
        step("revalidate target", verify())?;
        match step("resolve loader before remote thread", resolve()) {
            Ok(address) => return Ok(address),
            Err(error) => {
                let transient = matches!(
                    error.raw_os_error(),
                    Some(code) if code == ERROR_BAD_LENGTH as i32 || code == ERROR_PARTIAL_COPY as i32
                );
                if !transient || attempt == delays.len() {
                    return Err(error);
                }
                crate::diagnostics::record(
                    "WARN",
                    "prepare retry",
                    &format!(
                        "attempt={} next_attempt={} delay_ms={} remote_thread_started=false os_error={:?}",
                        attempt + 1,
                        attempt + 2,
                        delays[attempt],
                        error.raw_os_error()
                    ),
                );
                wait(std::time::Duration::from_millis(delays[attempt]));
            }
        }
    }
    unreachable!()
}

struct RemoteMemory {
    process: HANDLE,
    address: *mut c_void,
    release: bool,
}
impl RemoteMemory {
    fn new(process: HANDLE, bytes: &[u8]) -> io::Result<Self> {
        let address = unsafe {
            VirtualAllocEx(
                process,
                ptr::null(),
                bytes.len(),
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        if address.is_null() {
            return step("VirtualAllocEx", Err(io::Error::last_os_error()));
        }
        let value = Self {
            process,
            address,
            release: true,
        };
        let mut written = 0;
        if unsafe {
            WriteProcessMemory(
                process,
                address,
                bytes.as_ptr().cast(),
                bytes.len(),
                &mut written,
            )
        } == 0
        {
            return step("WriteProcessMemory", Err(io::Error::last_os_error()));
        }
        if written != bytes.len() {
            return step(
                "WriteProcessMemory",
                Err(io::Error::from_raw_os_error(ERROR_PARTIAL_COPY as i32)),
            );
        }
        Ok(value)
    }
}
impl Drop for RemoteMemory {
    fn drop(&mut self) {
        if self.release {
            unsafe {
                VirtualFreeEx(self.process, self.address, 0, MEM_RELEASE);
            }
        }
    }
}

fn call_remote(process: HANDLE, entry: usize, argument: &mut RemoteMemory) -> io::Result<u32> {
    let function: unsafe extern "system" fn(*mut c_void) -> u32 =
        unsafe { std::mem::transmute(entry) };
    let thread = step(
        "CreateRemoteThread",
        Handle::new(unsafe {
            CreateRemoteThread(
                process,
                ptr::null(),
                0,
                Some(function),
                argument.address,
                0,
                ptr::null_mut(),
            )
        }),
    )?;
    crate::diagnostics::record(
        "INFO",
        "remote thread",
        "started; automatic reinjection disabled",
    );
    let wait = unsafe { WaitForSingleObject(thread.0, 10_000) };
    if wait != WAIT_OBJECT_0 {
        // A running remote thread may still be reading these bytes. Leave this tiny allocation
        // to the game's normal process teardown, rather than freeing beneath it or killing it.
        argument.release = false;
        return step(
            "WaitForSingleObject",
            Err(if wait == WAIT_TIMEOUT {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "remote initialization timed out; restart the game",
                )
            } else {
                io::Error::last_os_error()
            }),
        );
    }
    let mut result = 0;
    if unsafe { GetExitCodeThread(thread.0, &mut result) } == 0 {
        return step("GetExitCodeThread", Err(io::Error::last_os_error()));
    }
    Ok(result)
}

fn modules(pid: u32) -> io::Result<Vec<(usize, PathBuf)>> {
    let snapshot = step(
        "CreateToolhelp32Snapshot",
        Handle::new(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, pid) }),
    )?;
    let mut entry: MODULEENTRY32W = unsafe { zeroed() };
    entry.dwSize = size_of::<MODULEENTRY32W>() as u32;
    let mut next = unsafe { Module32FirstW(snapshot.0, &mut entry) };
    let mut operation = "Module32FirstW";
    let mut out = Vec::new();
    loop {
        if next == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                break;
            }
            return step(operation, Err(error));
        }
        let end = entry
            .szExePath
            .iter()
            .position(|v| *v == 0)
            .unwrap_or(entry.szExePath.len());
        out.push((
            entry.modBaseAddr as usize,
            PathBuf::from(String::from_utf16_lossy(&entry.szExePath[..end])),
        ));
        operation = "Module32NextW";
        next = unsafe { Module32NextW(snapshot.0, &mut entry) };
    }
    Ok(out)
}

fn remote_load_library(pid: u32) -> io::Result<usize> {
    let kernel = unsafe { GetModuleHandleW(wide("kernel32.dll").as_ptr()) };
    let load = unsafe { GetProcAddress(kernel, c"LoadLibraryW".as_ptr().cast()) }
        .ok_or_else(io::Error::last_os_error)? as usize;
    let mut owner: HMODULE = ptr::null_mut();
    if unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            load as *const u16,
            &mut owner,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let local = modules(std::process::id())?
        .into_iter()
        .find(|(base, _)| *base == owner as usize)
        .ok_or_else(|| io::Error::other("loader module missing"))?;
    let target = modules(pid)?
        .into_iter()
        .find(|(_, path)| {
            path.file_name().is_some_and(|n| {
                n.to_string_lossy()
                    .eq_ignore_ascii_case(&local.1.file_name().unwrap().to_string_lossy())
            })
        })
        .ok_or_else(|| io::Error::other("target loader module missing"))?;
    Ok(target.0 + (load - owner as usize))
}

fn export_rva(path: &Path, name: &[u8]) -> io::Result<usize> {
    // Mapping without resolution avoids running our own Hook DLL inside the helper.
    let local = unsafe {
        LoadLibraryExW(
            wide(path).as_ptr(),
            ptr::null_mut(),
            DONT_RESOLVE_DLL_REFERENCES,
        )
    };
    if local.is_null() {
        return Err(io::Error::last_os_error());
    }
    let address = unsafe { GetProcAddress(local, name.as_ptr()) };
    let result = address
        .map(|p| p as usize - local as usize)
        .ok_or_else(|| io::Error::other("NetBurrowInit export missing"));
    unsafe {
        FreeLibrary(local);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, time::Duration};

    #[test]
    fn loader_preparation_revalidates_after_each_delay_and_recovers() {
        let events = RefCell::new(Vec::new());
        let mut results = [
            Err(io::Error::from_raw_os_error(ERROR_PARTIAL_COPY as i32)),
            Err(io::Error::from_raw_os_error(ERROR_BAD_LENGTH as i32)),
            Ok(123),
        ]
        .into_iter();
        let address = prepare_loader(
            || {
                events.borrow_mut().push("verify");
                Ok(())
            },
            || {
                events.borrow_mut().push("resolve");
                results.next().unwrap()
            },
            |delay| {
                events.borrow_mut().push(match delay.as_millis() {
                    200 => "wait 200",
                    500 => "wait 500",
                    _ => panic!("unexpected delay"),
                })
            },
        )
        .unwrap();
        assert_eq!(address, 123);
        assert_eq!(
            *events.borrow(),
            [
                "verify", "resolve", "wait 200", "verify", "resolve", "wait 500", "verify",
                "resolve"
            ]
        );
    }

    #[test]
    fn loader_preparation_caps_transient_failures_and_does_not_retry_access_denied() {
        for (code, expected_calls) in [(ERROR_PARTIAL_COPY as i32, 3), (5, 1)] {
            let mut calls = 0;
            let mut delays = Vec::new();
            let error = prepare_loader(
                || Ok(()),
                || {
                    calls += 1;
                    Err(io::Error::from_raw_os_error(code))
                },
                |delay| delays.push(delay),
            )
            .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(code));
            assert_eq!(calls, expected_calls);
            assert_eq!(
                delays,
                if expected_calls == 3 {
                    vec![Duration::from_millis(200), Duration::from_millis(500)]
                } else {
                    vec![]
                }
            );
        }
    }

    #[test]
    fn loader_preparation_stops_if_target_changes_or_exits_during_delay() {
        for code in [None, Some(87)] {
            let mut verified = false;
            let mut calls = 0;
            let error = prepare_loader(
                || {
                    if verified {
                        return Err(code.map(io::Error::from_raw_os_error).unwrap_or_else(|| {
                            io::Error::other("target changed during preparation")
                        }));
                    }
                    verified = true;
                    Ok(())
                },
                || {
                    calls += 1;
                    Err(io::Error::from_raw_os_error(ERROR_PARTIAL_COPY as i32))
                },
                |_| {},
            )
            .unwrap_err();
            assert_eq!(calls, 1);
            assert_eq!(error.raw_os_error(), code);
        }
    }
}
