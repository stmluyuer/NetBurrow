mod client;
pub mod diagnostics;
pub mod process;
mod settings;

pub use client::{Client, PeerInfo, Phase, Snapshot};
pub use netburrow_protocol::MemberStatus;
pub use settings::{
    Settings, Transport, autodetect_game, config_directory, load_settings, new_group, save_settings,
};

#[cfg(windows)]
pub struct SingleInstance {
    _guard: process::Handle,
}

#[cfg(windows)]
impl SingleInstance {
    pub fn acquire() -> Result<Self, String> {
        use windows_sys::Win32::{
            Foundation::{ERROR_ALREADY_EXISTS, GetLastError},
            System::Threading::CreateMutexW,
        };
        let name = process::wide("Local\\NetBurrow.Desktop.1");
        let raw = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let handle = process::Handle::new(raw).map_err(|e| e.to_string())?;
        if already_exists {
            return Err("NetBurrow 已在运行，请从托盘打开现有窗口。".into());
        }
        Ok(Self { _guard: handle })
    }
}

#[cfg(not(windows))]
pub struct SingleInstance;
#[cfg(not(windows))]
impl SingleInstance {
    pub fn acquire() -> Result<Self, String> {
        Err("Windows 客户端只能在 Windows 上运行".into())
    }
}
