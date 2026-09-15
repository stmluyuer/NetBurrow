//! Small Windows process boundary shared with the x86 helper; no Steam account files are read.
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

pub fn validate_x86_image(path: &Path) -> io::Result<()> {
    let mut file = File::open(path)?;
    let mut dos = [0u8; 64];
    file.read_exact(&mut dos)?;
    if &dos[..2] != b"MZ" {
        return Err(io::Error::other("not a PE executable"));
    }
    let offset = u32::from_le_bytes(dos[60..64].try_into().unwrap()) as u64;
    if offset > 16 * 1024 * 1024 {
        return Err(io::Error::other("invalid PE header offset"));
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut header = [0u8; 26];
    file.read_exact(&mut header)?;
    if &header[..4] != b"PE\0\0"
        || u16::from_le_bytes([header[4], header[5]]) != 0x14c
        || u16::from_le_bytes([header[24], header[25]]) != 0x10b
    {
        return Err(io::Error::other("expected x86 PE32"));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GameProcess {
    pub pid: u32,
    pub created: u64,
    pub path: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessState {
    Alive,
    Exited,
    Unknown(Option<i32>),
}

#[cfg(windows)]
pub use windows::*;

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{
        ffi::OsStr,
        mem::{size_of, zeroed},
        os::windows::ffi::OsStrExt,
        ptr,
    };

    /// An owned handle pins the exact process object, including after PID reuse.
    pub struct ProcessMonitor(std::os::windows::io::OwnedHandle);
    impl ProcessMonitor {
        pub fn open(expected: &GameProcess) -> io::Result<Self> {
            use std::os::windows::io::FromRawHandle;
            use windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE;
            let raw = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    0,
                    expected.pid,
                )
            };
            let handle = Handle::new(raw)?;
            let mut created: FILETIME = unsafe { zeroed() };
            let (mut exited, mut kernel, mut user) = (created, created, created);
            if unsafe {
                GetProcessTimes(handle.0, &mut created, &mut exited, &mut kernel, &mut user)
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            if (((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64)
                != expected.created
            {
                return Err(io::Error::other("process identity changed"));
            }
            std::mem::forget(handle);
            Ok(Self(unsafe {
                std::os::windows::io::OwnedHandle::from_raw_handle(raw)
            }))
        }
        pub fn state(&self) -> ProcessState {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::{
                Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
                System::Threading::WaitForSingleObject,
            };
            match unsafe { WaitForSingleObject(self.0.as_raw_handle(), 0) } {
                WAIT_OBJECT_0 => ProcessState::Exited,
                WAIT_TIMEOUT => ProcessState::Alive,
                _ => ProcessState::Unknown(io::Error::last_os_error().raw_os_error()),
            }
        }
    }
    #[cfg(test)]
    mod monitor_tests {
        use super::*;
        #[test]
        fn child_fixture() {
            if std::env::var_os("NETBURROW_MONITOR_FIXTURE").is_some() {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
        #[test]
        fn pinned_process_handle_survives_path_changes_and_confirms_exit() {
            use std::os::windows::process::CommandExt;
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "process::windows::monitor_tests::child_fixture"])
                .env("NETBURROW_MONITOR_FIXTURE", "1")
                .creation_flags(0x08000000)
                .spawn()
                .unwrap();
            let raw = Handle::new(unsafe {
                OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, child.id())
            })
            .unwrap();
            let mut created: FILETIME = unsafe { zeroed() };
            let (mut exited, mut kernel, mut user) = (created, created, created);
            assert_ne!(
                unsafe {
                    GetProcessTimes(raw.0, &mut created, &mut exited, &mut kernel, &mut user)
                },
                0
            );
            let game = GameProcess {
                pid: child.id(),
                created: ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64,
                path: PathBuf::from("path-no-longer-readable.exe"),
            };
            let monitor = ProcessMonitor::open(&game).unwrap();
            assert_eq!(monitor.state(), ProcessState::Alive);
            let mut wrong = game;
            wrong.created += 1;
            assert!(ProcessMonitor::open(&wrong).is_err());
            assert!(child.wait().unwrap().success());
            assert_eq!(monitor.state(), ProcessState::Exited);
        }
    }
    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE},
        Security::{EqualSid, GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
                TH32CS_SNAPPROCESS,
            },
            Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW},
            RemoteDesktop::ProcessIdToSessionId,
            Threading::{
                GetCurrentProcess, GetCurrentProcessId, GetExitCodeProcess, GetProcessTimes,
                IsWow64Process2, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
                QueryFullProcessImageNameW,
            },
        },
    };

    pub fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
        value.as_ref().encode_wide().chain(Some(0)).collect()
    }

    pub struct Handle(pub HANDLE);
    impl Handle {
        pub fn new(raw: HANDLE) -> io::Result<Self> {
            if raw.is_null() || raw == INVALID_HANDLE_VALUE {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self(raw))
            }
        }
    }
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn same_owner(process: HANDLE) -> io::Result<bool> {
        fn user_buffer(process: HANDLE) -> io::Result<Vec<usize>> {
            let mut raw = ptr::null_mut();
            if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut raw) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let token = Handle::new(raw)?;
            let mut size = 0;
            unsafe {
                GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut size);
            }
            if size == 0 || size > 64 * 1024 {
                return Err(io::Error::other("invalid token information"));
            }
            let mut data = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
            if unsafe {
                GetTokenInformation(
                    token.0,
                    TokenUser,
                    data.as_mut_ptr().cast(),
                    size,
                    &mut size,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(data)
        }
        let ours = user_buffer(unsafe { GetCurrentProcess() })?;
        let theirs = user_buffer(process)?;
        let a = unsafe { &*ours.as_ptr().cast::<TOKEN_USER>() };
        let b = unsafe { &*theirs.as_ptr().cast::<TOKEN_USER>() };
        Ok(unsafe { EqualSid(a.User.Sid, b.User.Sid) } != 0)
    }

    pub fn describe_process(pid: u32) -> io::Result<GameProcess> {
        let process =
            Handle::new(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) })?;
        let mut our_session = 0;
        let mut their_session = 0;
        if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut our_session) } == 0
            || unsafe { ProcessIdToSessionId(pid, &mut their_session) } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if our_session != their_session || !same_owner(process.0)? {
            return Err(io::Error::other("target belongs to another user/session"));
        }
        let (mut machine, mut native_machine) = (0, 0);
        if unsafe { IsWow64Process2(process.0, &mut machine, &mut native_machine) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if machine != 0x14c && !(machine == 0 && native_machine == 0x14c) {
            return Err(io::Error::other("target is not x86"));
        }
        let mut created: FILETIME = unsafe { zeroed() };
        let mut exited = created;
        let mut kernel = created;
        let mut user = created;
        if unsafe { GetProcessTimes(process.0, &mut created, &mut exited, &mut kernel, &mut user) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut path = vec![0u16; 32768];
        let mut count = path.len() as u32;
        if unsafe { QueryFullProcessImageNameW(process.0, 0, path.as_mut_ptr(), &mut count) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut code = 0;
        if unsafe { GetExitCodeProcess(process.0, &mut code) } == 0 || code != 259 {
            return Err(io::Error::other("target has exited"));
        }
        Ok(GameProcess {
            pid,
            created: ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64,
            path: PathBuf::from(String::from_utf16_lossy(&path[..count as usize])),
        })
    }

    pub fn same_path(a: &Path, b: &Path) -> bool {
        fn normalized(p: &Path) -> Option<String> {
            p.canonicalize().ok().map(|p| {
                p.to_string_lossy()
                    .trim_start_matches("\\\\?\\")
                    .to_lowercase()
            })
        }
        match (normalized(a), normalized(b)) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }

    pub fn find_games(path: &Path) -> io::Result<Vec<GameProcess>> {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| io::Error::other("missing executable filename"))?;
        let snapshot = Handle::new(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) })?;
        let mut entry: PROCESSENTRY32W = unsafe { zeroed() };
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut valid = unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0;
        let mut found = Vec::new();
        while valid {
            let end = entry
                .szExeFile
                .iter()
                .position(|v| *v == 0)
                .unwrap_or(entry.szExeFile.len());
            if String::from_utf16_lossy(&entry.szExeFile[..end]).eq_ignore_ascii_case(name) {
                if let Ok(candidate) = describe_process(entry.th32ProcessID) {
                    if same_path(&candidate.path, path) {
                        found.push(candidate);
                    }
                }
            }
            valid = unsafe { Process32NextW(snapshot.0, &mut entry) } != 0;
        }
        Ok(found)
    }

    pub fn steam_directory() -> Option<PathBuf> {
        let subkey = wide("Software\\Valve\\Steam");
        let value = wide("SteamPath");
        let mut data = vec![0u16; 32768];
        let mut bytes = (data.len() * 2) as u32;
        let error = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ,
                ptr::null_mut(),
                data.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if error != 0 {
            return None;
        }
        let end = data.iter().position(|v| *v == 0)?;
        Some(PathBuf::from(String::from_utf16_lossy(&data[..end])))
    }
}

#[cfg(not(windows))]
pub fn find_games(_: &Path) -> io::Result<Vec<GameProcess>> {
    Err(io::Error::other("Windows only"))
}
