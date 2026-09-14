use crate::Settings;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckLevel {
    Passed,
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
    let version = file_version(Path::new(settings.game_path.trim()));
    report.add("游戏版本", CheckLevel::Warning, match version {
        Some(version) => format!("检测到文件版本 {version}。当前没有版本白名单；请确认使用忏悔+，实际兼容性由接入检查确认。"),
        None => "未读取到游戏文件版本。请确认使用忏悔+；此项不代表已验证游戏兼容性。".into(),
    });
    report
}

#[cfg(windows)]
fn file_version(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW,
    };
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let size = GetFileVersionInfoSizeW(path.as_ptr(), std::ptr::null_mut());
        if size == 0 || size > 1024 * 1024 {
            return None;
        }
        let mut bytes = vec![0u8; size as usize];
        if GetFileVersionInfoW(path.as_ptr(), 0, size, bytes.as_mut_ptr().cast()) == 0 {
            return None;
        }
        let mut value = std::ptr::null_mut();
        let mut length = 0;
        if VerQueryValueW(
            bytes.as_ptr().cast(),
            [b'\\' as u16, 0].as_ptr(),
            &mut value,
            &mut length,
        ) == 0
            || value.is_null()
            || length < std::mem::size_of::<VS_FIXEDFILEINFO>() as u32
        {
            return None;
        }
        let info = value.cast::<VS_FIXEDFILEINFO>().read_unaligned();
        if info.dwSignature != 0xfeef04bd {
            return None;
        }
        Some(format!(
            "{}.{}.{}.{}",
            info.dwFileVersionMS >> 16,
            info.dwFileVersionMS & 0xffff,
            info.dwFileVersionLS >> 16,
            info.dwFileVersionLS & 0xffff
        ))
    }
}
#[cfg(not(windows))]
fn file_version(_: &Path) -> Option<String> {
    None
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
                .any(|check| check.name == "游戏版本" && check.level == CheckLevel::Warning)
        );
        let mut report = PreflightReport::default();
        assert!(!report.can_start());
        report.add("version", CheckLevel::Warning, "unverified");
        assert!(report.can_start());
    }
}
