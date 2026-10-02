//! Current-user autorun. Launching the app never writes to the Run key.
use std::path::Path;

fn command_line(exe: &Path) -> Result<String, String> {
    let path = exe.to_str().ok_or(netburrow_core::text!("无法读取程序路径", "Cannot read application path"))?;
    if path.contains(['"', '\0']) {
        return Err(netburrow_core::text!("程序路径无效", "Invalid application path").into());
    }
    let command = format!("\"{path}\"");
    if command.encode_utf16().count() + 1 > 260 {
        return Err(netburrow_core::text!("程序路径过长，请移至较短路径后重试", "Application path is too long. Move the app to a shorter path and try again.").into());
    }
    Ok(command)
}

#[cfg(windows)]
mod native {
    use super::*;
    use windows_sys::Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS},
        System::Registry::*,
    };
    const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const NAME: &str = "NetBurrow";
    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }
    fn error(code: u32) -> String {
        netburrow_core::text_format!("无法更新启动项：{}", "Cannot access startup settings: {}",
            std::io::Error::from_raw_os_error(code as i32)
        )
    }

    pub fn is_enabled() -> Result<bool, String> {
        let key = wide(KEY);
        let name = wide(NAME);
        let mut size = 0;
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        match status {
            ERROR_SUCCESS => Ok(size > 2),
            ERROR_FILE_NOT_FOUND => Ok(false),
            _ => Err(error(status)),
        }
    }

    pub fn set_enabled(enabled: bool) -> Result<(), String> {
        let command = if enabled {
            Some(wide(&command_line(
                &std::env::current_exe().map_err(|e| e.to_string())?,
            )?))
        } else {
            None
        };
        let key = wide(KEY);
        let name = wide(NAME);
        let mut handle = std::ptr::null_mut();
        let status = unsafe {
            if enabled {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    0,
                    std::ptr::null(),
                    REG_OPTION_NON_VOLATILE,
                    KEY_SET_VALUE,
                    std::ptr::null(),
                    &mut handle,
                    std::ptr::null_mut(),
                )
            } else {
                RegOpenKeyExW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    0,
                    KEY_SET_VALUE,
                    &mut handle,
                )
            }
        };
        if !enabled && status == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        if status != ERROR_SUCCESS {
            return Err(error(status));
        }
        let result = unsafe {
            let result = if let Some(command) = command {
                RegSetValueExW(
                    handle,
                    name.as_ptr(),
                    0,
                    REG_SZ,
                    command.as_ptr().cast(),
                    (command.len() * 2) as u32,
                )
            } else {
                RegDeleteValueW(handle, name.as_ptr())
            };
            RegCloseKey(handle);
            result
        };
        if result == ERROR_SUCCESS || (!enabled && result == ERROR_FILE_NOT_FOUND) {
            Ok(())
        } else {
            Err(error(result))
        }
    }
}

#[cfg(windows)]
pub use native::{is_enabled, set_enabled};
#[cfg(not(windows))]
pub fn is_enabled() -> Result<bool, String> {
    Ok(false)
}
#[cfg(not(windows))]
pub fn set_enabled(_: bool) -> Result<(), String> {
    Err(netburrow_core::text!("仅 Windows 支持登录时启动", "Launch at login requires Windows").into())
}
