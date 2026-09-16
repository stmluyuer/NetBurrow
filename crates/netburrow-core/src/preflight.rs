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
            "服务器",
            CheckLevel::Passed,
            "TCP 握手与成员状态协议可用；UDP 通道在正式接入后确认。",
        ),
        Err(error) => report.add("服务器", CheckLevel::Failed, error),
    }
    report
}

fn local_checks(settings: &Settings, directory: Option<&Path>) -> PreflightReport {
    let mut report = PreflightReport::default();
    match settings.validate() {
        Ok(()) => report.add(
            "连接与游戏设置",
            CheckLevel::Passed,
            "地址与组码格式有效，游戏文件为 32 位程序。",
        ),
        Err(error) => report.add("连接与游戏设置", CheckLevel::Failed, error),
    }
    for name in ["netburrow-injector.exe", "netburrow_hook.dll"] {
        let result = directory
            .ok_or_else(|| "无法确定工具目录".to_owned())
            .and_then(|directory| {
                crate::process::validate_x86_image(&directory.join(name)).map_err(|_| {
                    format!("{name} 缺失、无法读取或不是 32 位文件，请完整解压工具包。")
                })
            });
        match result {
            Ok(()) => report.add(name, CheckLevel::Passed, "文件存在且架构正确。"),
            Err(error) => report.add(name, CheckLevel::Failed, error),
        }
    }
    let (level, detail) = match file_version(Path::new(settings.game_path.trim())) {
        Ok(Some(version)) => (CheckLevel::Info, format!("EXE 文件版本：{version}。")),
        Ok(None) => (
            CheckLevel::Info,
            "EXE 未提供可读取的文件版本信息，不表示游戏版本有误，不影响启用。".into(),
        ),
        Err(error) => (
            CheckLevel::Warning,
            format!("读取文件版本信息失败：{error}。此项不影响启用。"),
        ),
    };
    report.add(
        "文件版本信息",
        level,
        format!("{detail}本工具适用于忏悔+；文件版本信息不用于判断游戏兼容性。"),
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
                _ => Err(format!("查询失败（Windows 错误码 {error}）")),
            };
        }
        if size > 1024 * 1024 {
            return Err("版本信息大小超过读取上限".into());
        }
        let mut bytes = vec![0u8; size as usize];
        if GetFileVersionInfoW(path.as_ptr(), 0, size, bytes.as_mut_ptr().cast()) == 0 {
            return Err(format!("读取失败（Windows 错误码 {}）", GetLastError()));
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
            return Err("版本资源中缺少固定版本信息".into());
        }
        if value.is_null() || length < std::mem::size_of::<VS_FIXEDFILEINFO>() as u32 {
            return Err("固定版本信息不完整".into());
        }
        let info = value.cast::<VS_FIXEDFILEINFO>().read_unaligned();
        if info.dwSignature != 0xfeef04bd {
            return Err("固定版本信息签名无效".into());
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
    Err("当前平台不支持读取 Windows 文件版本信息".into())
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
                .any(|check| check.name == "文件版本信息"
                    && matches!(check.level, CheckLevel::Info | CheckLevel::Warning))
        );
        let mut report = PreflightReport::default();
        assert!(!report.can_start());
        report.add("version", CheckLevel::Warning, "unverified");
        assert!(report.can_start());
        report.add("version", CheckLevel::Info, "unverified");
        assert!(report.can_start());
    }

    #[cfg(windows)]
    #[test]
    fn missing_file_is_an_error_not_missing_version_metadata() {
        let missing = std::env::current_exe().unwrap().join("missing.exe");
        assert!(file_version(&missing).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn executable_without_version_resource_is_informational() {
        // This crate has no build script embedding a VERSIONINFO resource.
        let executable = std::env::current_exe().unwrap();
        assert_eq!(file_version(&executable), Ok(None));
        let settings = Settings {
            game_path: executable.to_string_lossy().into_owned(),
            ..Settings::default()
        };
        let report = local_checks(&settings, None);
        let check = report
            .checks
            .into_iter()
            .find(|check| check.name == "文件版本信息")
            .unwrap();
        assert_eq!(check.level, CheckLevel::Info);
        let version_only = PreflightReport {
            checks: vec![check],
        };
        assert!(version_only.can_start());
    }
}
