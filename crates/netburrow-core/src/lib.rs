mod client;
mod path_diagnostics;
pub use path_diagnostics::{PathDiagnostics, PathPeer, SequenceReport};
#[cfg(windows)]
mod client_io;
pub mod diagnostics;
mod diagnostics_report;
pub mod process;
mod settings;
mod preflight;
mod quality;

pub use client::{Client, PeerInfo, Phase, Snapshot};
pub use diagnostics_report::{export_freeze_report, export_report};
pub use preflight::{preflight, Check, CheckLevel, PreflightReport};
pub use quality::{connection_quality, Quality};
pub use netburrow_protocol::MemberStatus;
pub use settings::{
    Settings, Transport, WindowPlacement, RecentConnection, autodetect_game, config_directory, load_settings, new_group,
    save_minimize_on_close, save_notifications_enabled, save_window_placement, save_settings,
    save_start_minimized, save_recent_connections, save_game_settings, save_auto_check_updates,
};

#[cfg(windows)]
pub struct SingleInstance {
    _guard: process::Handle,
    activate: process::Handle,
}

#[cfg(windows)]
impl SingleInstance {
    pub fn acquire() -> Result<Option<Self>, String> {
        Self::acquire_named(
            "Local\\NetBurrow.Desktop.1",
            "Local\\NetBurrow.Desktop.Activate.1",
        )
    }

    fn acquire_named(mutex_name: &str, activation_name: &str) -> Result<Option<Self>, String> {
        use windows_sys::Win32::{
            Foundation::{ERROR_ALREADY_EXISTS, GetLastError},
            System::Threading::{CreateEventW, CreateMutexW, SetEvent},
            UI::WindowsAndMessaging::{
                AllowSetForegroundWindow, FindWindowW, GetWindowThreadProcessId,
            },
        };
        let event_name = process::wide(activation_name);
        let activate = process::Handle::new(unsafe {
            CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr())
        })
        .map_err(|e| format!("无法创建窗口唤回事件：{e}"))?;
        let name = process::wide(mutex_name);
        let raw = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let handle = process::Handle::new(raw).map_err(|e| e.to_string())?;
        if already_exists {
            unsafe {
                let title = process::wide("NetBurrow");
                let window = FindWindowW(std::ptr::null(), title.as_ptr());
                if !window.is_null() {
                    let mut pid = 0;
                    GetWindowThreadProcessId(window, &mut pid);
                    if pid != 0 {
                        AllowSetForegroundWindow(pid);
                    }
                }
                if SetEvent(activate.0) == 0 {
                    return Err(format!(
                        "无法唤回现有窗口：{}",
                        std::io::Error::last_os_error()
                    ));
                }
            }
            return Ok(None);
        }
        Ok(Some(Self {
            _guard: handle,
            activate,
        }))
    }

    pub fn activation_requested(&self) -> bool {
        unsafe {
            windows_sys::Win32::System::Threading::WaitForSingleObject(self.activate.0, 0)
                == windows_sys::Win32::Foundation::WAIT_OBJECT_0
        }
    }
}

#[cfg(all(test, windows))]
mod instance_tests {
    use super::SingleInstance;

    #[test]
    fn duplicate_activation_survives_until_polled_and_releases_on_exit() {
        let name = format!("Local\\NetBurrow.Test.{}", std::process::id());
        let event = format!("{name}.Activate");
        let first = SingleInstance::acquire_named(&name, &event)
            .unwrap()
            .unwrap();
        assert!(!first.activation_requested());
        assert!(
            SingleInstance::acquire_named(&name, &event)
                .unwrap()
                .is_none()
        );
        assert!(first.activation_requested());
        assert!(!first.activation_requested());
        drop(first);
        assert!(
            SingleInstance::acquire_named(&name, &event)
                .unwrap()
                .is_some()
        );
    }
}

#[cfg(not(windows))]
pub struct SingleInstance;
#[cfg(not(windows))]
impl SingleInstance {
    pub fn acquire() -> Result<Option<Self>, String> {
        Err("Windows 客户端只能在 Windows 上运行".into())
    }
    pub fn activation_requested(&self) -> bool {
        false
    }
}
