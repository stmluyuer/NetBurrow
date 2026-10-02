use crate::Settings;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckLevel {
    Passed,
    Info,
    Warning,
    Failed,
}

#[derive(Clone, Debug)]
pub struct Check {
    pub name: &'static str,
    pub level: CheckLevel,
    pub detail: String,
}

#[derive(Clone, Debug, Default)]
pub struct PreflightReport {
    pub checks: Vec<Check>,
}

impl PreflightReport {
    pub fn can_start(&self) -> bool {
        !self.checks.is_empty()
            && self
                .checks
                .iter()
                .all(|check| check.level != CheckLevel::Failed)
    }
    fn add(&mut self, name: &'static str, level: CheckLevel, detail: impl Into<String>) {
        self.checks.push(Check {
            name,
            level,
            detail: detail.into(),
        });
    }
}

/// Runs on the caller's background worker. Does not launch the game or inject a DLL.
pub fn preflight(settings: &Settings) -> PreflightReport {
    let directory = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_owned));
    let mut report = local_checks(settings, directory.as_deref());
    if !report.can_start() {
        return report;
    }
    match crate::client::probe_relay(settings) {
        Ok(()) => report.add(
            crate::text!("服务器", "Server"),
            CheckLevel::Passed,
            crate::text!("TCP 连接与成员状态正常；UDP 在连接后确认。", "TCP connection and member status are available. UDP is checked after connecting."),
        ),
        Err(error) => report.add(crate::text!("服务器", "Server"), CheckLevel::Failed, error),
    }
    report
}

fn local_checks(settings: &Settings, directory: Option<&Path>) -> PreflightReport {
    let mut report = PreflightReport::default();
    match settings.validate() {
        Ok(()) => report.add(
            crate::text!("连接与游戏", "Connection and game"),
            CheckLevel::Passed,
            crate::text!("地址与组码格式有效，游戏为 32 位程序。", "Address and group code are valid. Game executable is 32-bit."),
        ),
        Err(error) => report.add(crate::text!("连接与游戏", "Connection and game"), CheckLevel::Failed, error),
    }
    for name in ["netburrow-injector.exe", "netburrow_hook.dll"] {
        let result = directory
            .ok_or_else(|| crate::text!("无法确定程序目录", "Cannot locate the application folder").to_owned())
            .and_then(|directory| {
                crate::process::validate_x86_image(&directory.join(name)).map_err(|_| {
                    crate::text_format!("{name} 缺失、不可读或非 32 位，请完整解压程序包。", "{name} is missing, unreadable, or not 32-bit. Extract the complete package again.")
                })
            });
        match result {
            Ok(()) => report.add(name, CheckLevel::Passed, crate::text!("文件与架构正常。", "File and architecture are valid.")),
            Err(error) => report.add(name, CheckLevel::Failed, error),
        }
    }
    let (level, detail) = match file_version(Path::new(settings.game_path.trim())) {
        Ok(Some(version)) => (CheckLevel::Info, crate::text_format!("文件版本：{version}。", "File version: {version}.")),
        Ok(None) => (
            CheckLevel::Info,
            crate::text!("无可读的文件版本信息，不影响连接。", "No readable file version. This does not block connecting.").into(),
        ),
        Err(error) => (
            CheckLevel::Warning,
            crate::text_format!("无法读取文件版本：{error}。不影响连接。", "Cannot read file version: {error}. This does not block connecting."),
        ),
    };
    report.add(
        crate::text!("文件版本", "File version"),
        level,
        crate::text_format!("{detail} 仅支持忏悔+；文件版本不代表兼容性。", "{detail} Repentance+ only. File version does not establish compatibility."),
    );
    report
}

#[cfg(windows)]
fn file_version(path: &Path) -> Result<Option<String>, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{
        ERROR_RESOURCE_DATA_NOT_FOUND, ERROR_RESOURCE_LANG_NOT_FOUND,
        ERROR_RESOURCE_NAME_NOT_FOUND, ERROR_RESOURCE_TYPE_NOT_FOUND, GetLastError,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW,
    };
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let size = GetFileVersionInfoSizeW(path.as_ptr(), std::ptr::null_mut());
        if size == 0 {
            let error = GetLastError();
            return match error {
                ERROR_RESOURCE_DATA_NOT_FOUND
                | ERROR_RESOURCE_TYPE_NOT_FOUND
                | ERROR_RESOURCE_NAME_NOT_FOUND
                | ERROR_RESOURCE_LANG_NOT_FOUND => Ok(None),
                _ => Err(crate::text_format!("查询失败（Windows 错误 {error}）", "Query failed (Windows error {error})")),
            };
        }
        if size > 1024 * 1024 {
            return Err(crate::text!("版本信息超过大小限制", "Version information exceeds the size limit").into());
        }
        let mut bytes = vec![0u8; size as usize];
        if GetFileVersionInfoW(path.as_ptr(), 0, size, bytes.as_mut_ptr().cast()) == 0 {
            return Err(crate::text_format!("读取失败（Windows 错误 {}）", "Read failed (Windows error {})", GetLastError()));
        }
        let mut value = std::ptr::null_mut();
        let mut length = 0;
        if VerQueryValueW(
            bytes.as_ptr().cast(),
            [b'\\' as u16, 0].as_ptr(),
            &mut value,
            &mut length,
        ) == 0
        {
            return Err(crate::text!("缺少固定版本信息", "Fixed version information is missing").into());
        }
        if value.is_null() || length < std::mem::size_of::<VS_FIXEDFILEINFO>() as u32 {
            return Err(crate::text!("固定版本信息不完整", "Fixed version information is incomplete").into());
        }
        let info = value.cast::<VS_FIXEDFILEINFO>().read_unaligned();
        if info.dwSignature != 0xfeef04bd {
            return Err(crate::text!("固定版本信息签名无效", "Fixed version information has an invalid signature").into());
        }
        Ok(Some(format!(
            "{}.{}.{}.{}",
            info.dwFileVersionMS >> 16,
            info.dwFileVersionMS & 0xffff,
            info.dwFileVersionLS >> 16,
            info.dwFileVersionLS & 0xffff
        )))
    }
}
#[cfg(not(windows))]
fn file_version(_: &Path) -> Result<Option<String>, String> {
    Err(crate::text!("当前平台不支持读取 Windows 文件版本", "Reading Windows file versions is not supported on this platform").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_settings_and_missing_binaries_block_start() {
        let report = local_checks(&Settings::default(), None);
        assert!(!report.can_start());
        assert_eq!(
            report
                .checks
                .iter()
                .filter(|check| check.level == CheckLevel::Failed)
                .count(),
            3
        );
        assert!(
            report
                .checks
                .iter()
                .any(|check| check.name == crate::text!("文件版本", "File version")
                    && matches!(check.level, CheckLevel::Info | CheckLevel::Warning))
        );
        let mut report = PreflightReport::default();
        assert!(!report.can_start());
        report.add("version", CheckLevel::Warning, "unverified");
        assert!(report.can_start());
        report.add("version", CheckLevel::Info, "unverified");
        assert!(report.can_start());
    }

}
