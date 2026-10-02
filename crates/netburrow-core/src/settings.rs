use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    #[default]
    Tcp,
    Udp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowPlacement {
    /// Normal window rectangle in Windows workspace coordinates.
    pub normal: [i32; 4],
    pub maximized: bool,
}

fn enabled_by_default() -> bool { true }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentConnection {
    pub server: String,
    pub group: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default)]
    pub language: crate::i18n::Language,
    pub server: String,
    pub group: String,
    pub game_path: String,
    pub transport: Transport,
    #[serde(default)]
    pub allow_late_hook: bool,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub minimize_on_close: bool,
    #[serde(default = "enabled_by_default")]
    pub notifications_enabled: bool,
    #[serde(default)]
    pub auto_check_updates: bool,
    #[serde(default)]
    pub auto_crash_capture: bool,
    #[serde(default)]
    pub crash_capture_consent: bool,
    #[serde(default)]
    pub window_placement: Option<WindowPlacement>,
    #[serde(default)]
    pub start_minimized: bool,
    #[serde(default)]
    pub recent_connections: Vec<RecentConnection>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            language: crate::i18n::Language::default(),
            server: String::new(),
            group: String::new(),
            game_path: autodetect_game()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            transport: Transport::Tcp,
            allow_late_hook: false,
            display_name: String::new(),
            minimize_on_close: false,
            notifications_enabled: true,
            auto_check_updates: false,
            auto_crash_capture: false,
            crash_capture_consent: false,
            window_placement: None,
            start_minimized: false,
            recent_connections: Vec::new(),
        }
    }
}

impl Settings {
    pub fn same_connection(&self, other: &Self) -> bool {
        self.server.trim().eq_ignore_ascii_case(other.server.trim())
            && self.group.trim().eq_ignore_ascii_case(other.group.trim())
            && self.game_path.trim() == other.game_path.trim()
            && self.transport == other.transport
            && self.allow_late_hook == other.allow_late_hook
            && self.display_name.trim() == other.display_name.trim()
    }

    pub fn remember_connection(&mut self) {
        let recent = RecentConnection { server: self.server.trim().into(), group: self.group.trim().into() };
        self.recent_connections.retain(|entry| !entry.server.eq_ignore_ascii_case(&recent.server) || !entry.group.eq_ignore_ascii_case(&recent.group));
        self.recent_connections.insert(0, recent);
        self.recent_connections.truncate(5);
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.display_name.chars().count() > 24 || self.display_name.chars().any(char::is_control)
        {
            return Err(crate::text!("显示名最多 24 个字符，不支持换行或控制字符", "Use up to 24 characters, without line breaks or control characters").into());
        }
        let server = self.server.trim();
        if server.is_empty() || server.contains('/') || server.chars().any(char::is_whitespace) {
            return Err(crate::text!("请输入服务器地址和端口，如 relay.example.com:24872", "Enter a server address and port, e.g. relay.example.com:24872").into());
        }
        let (host, port) = server
            .rsplit_once(':')
            .ok_or(crate::text!("请补全服务器端口，如 :24872", "Add the server port, e.g. :24872"))?;
        if host.is_empty() || port.parse::<u16>().ok().filter(|p| *p != 0).is_none() {
            return Err(crate::text!("服务器地址或端口无效", "Invalid server address or port").into());
        }
        parse_group(&self.group)?;
        self.validate_game()
    }

    pub fn validate_game(&self) -> Result<(), String> {
        let path = Path::new(self.game_path.trim());
        if !path.is_file() {
            return Err(crate::text!("请选择有效的 isaac-ng.exe", "Select an existing isaac-ng.exe file").into());
        }
        if !path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("isaac-ng.exe"))
        {
            return Err(crate::text!("请选择 isaac-ng.exe", "Select isaac-ng.exe").into());
        }
        super::process::validate_x86_image(path)
            .map_err(|e| crate::text_format!("无法识别 32 位游戏程序：{e}", "Cannot identify the 32-bit game executable: {e}"))?;
        Ok(())
    }
}

pub fn parse_group(input: &str) -> Result<netburrow_protocol::Group, String> {
    let input = input.trim();
    let hex = input
        .strip_prefix("NB1-")
        .ok_or(crate::text!("组码无效，请创建组或粘贴 NB1- 开头的组码", "Invalid group code. Create a group or paste a code starting with NB1-"))?;
    if hex.len() != 64 {
        return Err(crate::text!("组码不完整，应为 NB1- 加 64 位十六进制字符。请重新复制", "Incomplete group code. Copy the full code: NB1- followed by 64 hexadecimal characters").into());
    }
    let mut group = [0; 32];
    for (slot, pair) in group.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
        let digit = |b: u8| {
            (b as char)
                .to_digit(16)
                .map(|n| n as u8)
                .ok_or(crate::text!("组码含无效字符，请重新复制或创建组", "Invalid characters in group code. Copy it again or create a group"))
        };
        *slot = digit(pair[0])? * 16 + digit(pair[1])?;
    }
    if group == [0; 32] {
        return Err(crate::text!("组码不能全为零，请重新创建组", "An all-zero group code is invalid. Create a new group").into());
    }
    Ok(group)
}

pub fn new_group() -> Result<String, String> {
    let group =
        netburrow_protocol::random_group().map_err(|_| crate::text!("系统随机数不可用，无法创建组", "Cannot create a group: system random number generator unavailable"))?;
    Ok(format!(
        "NB1-{}",
        group.iter().map(|v| format!("{v:02x}")).collect::<String>()
    ))
}

pub fn config_directory() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("NetBurrow")
}

fn read_settings(path: &Path) -> Result<Settings, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|_| crate::text!("设置文件损坏或版本不兼容。请重新填写，联机组不会自动更换", "Settings are damaged or incompatible. Enter them again; your group will not be replaced automatically").into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(crate::text_format!("无法读取设置：{e}", "Cannot read settings: {e}")),
    }
}

pub fn load_settings() -> Result<Settings, String> {
    read_settings(&config_directory().join("settings.json"))
}

pub fn save_settings(settings: &Settings) -> Result<(), String> {
    save_settings_in(&config_directory(), settings)
}

/// Save only the settings page fields, preserving home-page connection values and preferences.
pub fn save_game_settings(draft: &Settings) -> Result<(), String> {
    draft.validate_game()?;
    save_game_settings_in(&config_directory(), draft)
}

fn save_game_settings_in(dir: &Path, draft: &Settings) -> Result<(), String> {
    update_preferences_in(dir, |saved| {
        saved.game_path = draft.game_path.trim().into();
        saved.transport = draft.transport;
        saved.allow_late_hook = draft.allow_late_hook;
    })
}

pub fn save_minimize_on_close(enabled: bool) -> Result<(), String> {
    save_minimize_on_close_in(&config_directory(), enabled)
}

pub fn save_notifications_enabled(enabled: bool) -> Result<(), String> {
    update_preferences_in(&config_directory(), |settings| settings.notifications_enabled = enabled)
}

pub fn save_auto_check_updates(enabled: bool) -> Result<(), String> {
    save_auto_check_updates_in(&config_directory(), enabled)
}

pub fn save_language(language: crate::i18n::Language) -> Result<(), String> {
    update_preferences_in(&config_directory(), |settings| settings.language = language)
}

pub fn save_crash_capture(enabled: bool, consent: bool) -> Result<(), String> {
    save_crash_capture_in(&config_directory(), enabled, consent)
}

fn save_crash_capture_in(dir: &Path, enabled: bool, consent: bool) -> Result<(), String> {
    if enabled && !consent {
        return Err(crate::text!("请先阅读自动记录说明并同意 ProcDump 许可", "Read the crash recording notice and accept the ProcDump license first").into());
    }
    update_preferences_in(dir, |settings| {
        settings.auto_crash_capture = enabled;
        settings.crash_capture_consent = consent;
    })
}

fn save_auto_check_updates_in(dir: &Path, enabled: bool) -> Result<(), String> {
    update_preferences_in(dir, |settings| settings.auto_check_updates = enabled)
}

pub fn save_window_placement(placement: WindowPlacement) -> Result<(), String> {
    update_preferences_in(&config_directory(), |settings| settings.window_placement = Some(placement))
}

pub fn save_start_minimized(enabled: bool) -> Result<(), String> {
    update_preferences_in(&config_directory(), |settings| settings.start_minimized = enabled)
}

pub fn save_recent_connections(recent: &[RecentConnection]) -> Result<(), String> {
    update_preferences_in(&config_directory(), |settings| settings.recent_connections = recent.iter().take(5).cloned().collect())
}

fn save_minimize_on_close_in(dir: &Path, enabled: bool) -> Result<(), String> {
    update_preferences_in(dir, |settings| settings.minimize_on_close = enabled)
}

fn update_preferences_in(dir: &Path, update: impl FnOnce(&mut Settings)) -> Result<(), String> {
    // Read the saved connection values, never the UI's unconfirmed edits.
    // A corrupt configuration must remain untouched for explicit recovery.
    let mut settings = read_settings(&dir.join("settings.json"))?;
    update(&mut settings);
    save_settings_in(dir, &settings)
}

fn save_settings_in(dir: &Path, settings: &Settings) -> Result<(), String> {
    fs::create_dir_all(&dir).map_err(|e| crate::text_format!("无法创建设置目录：{e}", "Cannot create the settings folder: {e}"))?;
    let bytes = serde_json::to_vec_pretty(settings).map_err(|e| e.to_string())?;
    let staging = dir.join("settings.new");
    fs::write(&staging, bytes).map_err(|e| crate::text_format!("无法写入设置：{e}", "Cannot write settings: {e}"))?;
    fs::rename(staging, dir.join("settings.json")).map_err(|e| crate::text_format!("无法保存设置：{e}", "Cannot save settings: {e}"))
}

pub fn autodetect_game() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let steam = super::process::steam_directory()?;
        let mut libraries = vec![steam.clone()];
        if let Ok(vdf) = fs::read_to_string(steam.join("steamapps/libraryfolders.vdf")) {
            for line in vdf.lines() {
                let mut parts = line.split('"');
                if parts.nth(1) == Some("path") {
                    if let Some(path) = parts.nth(1) {
                        libraries.push(PathBuf::from(path.replace("\\\\", "\\")));
                    }
                }
            }
        }
        let mut games: Vec<_> = libraries
            .into_iter()
            .map(|p| p.join("steamapps/common/The Binding of Isaac Rebirth/isaac-ng.exe"))
            .filter(|p| p.is_file())
            .collect();
        games.sort();
        games.dedup();
        return games.into_iter().next();
    }
    #[cfg(not(windows))]
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preference_save_preserves_connection_and_corrupt_file() {
        let dir = std::env::temp_dir().join(format!("netburrow-preferences-{}-{}", std::process::id(), new_group().unwrap()));
        let saved = Settings { server: "saved.example:24872".into(), group: new_group().unwrap(), ..Settings::default() };
        save_settings_in(&dir, &saved).unwrap();
        save_minimize_on_close_in(&dir, true).unwrap();
        save_auto_check_updates_in(&dir, true).unwrap();
        update_preferences_in(&dir, |settings| settings.language = crate::i18n::Language::En).unwrap();
        assert!(save_crash_capture_in(&dir, true, false).is_err());
        save_crash_capture_in(&dir, true, true).unwrap();
        let loaded = read_settings(&dir.join("settings.json")).unwrap();
        assert!(loaded.minimize_on_close);
        assert!(loaded.auto_check_updates);
        assert_eq!(loaded.language, crate::i18n::Language::En);
        assert!(loaded.auto_crash_capture && loaded.crash_capture_consent);
        assert_eq!(loaded.server, saved.server);
        assert_eq!(loaded.group, saved.group);
        let draft = Settings { server: "unsaved.example:24872".into(), group: new_group().unwrap(), game_path: "new-game-path".into(), transport: Transport::Udp, allow_late_hook: true, ..saved.clone() };
        save_game_settings_in(&dir, &draft).unwrap();
        let merged = read_settings(&dir.join("settings.json")).unwrap();
        assert_eq!(merged.server, saved.server);
        assert_eq!(merged.group, saved.group);
        assert_eq!(merged.game_path, draft.game_path);
        assert_eq!(merged.transport, Transport::Udp);
        assert!(merged.allow_late_hook && merged.minimize_on_close);
        assert!(merged.auto_check_updates);
        assert_eq!(merged.language, crate::i18n::Language::En);
        assert!(merged.auto_crash_capture && merged.crash_capture_consent);
        save_crash_capture_in(&dir, false, true).unwrap();
        let disabled = read_settings(&dir.join("settings.json")).unwrap();
        assert!(!disabled.auto_crash_capture && disabled.crash_capture_consent);
        let placement = WindowPlacement { normal: [50, 60, 690, 880], maximized: true };
        update_preferences_in(&dir, |settings| {
            settings.notifications_enabled = false;
            settings.window_placement = Some(placement.clone());
            settings.start_minimized = true;
            settings.recent_connections = vec![RecentConnection { server: "history.example:24872".into(), group: new_group().unwrap() }];
        }).unwrap();
        let loaded = read_settings(&dir.join("settings.json")).unwrap();
        assert!(!loaded.notifications_enabled);
        assert!(loaded.start_minimized);
        assert_eq!(loaded.recent_connections.len(), 1);
        assert_eq!(loaded.window_placement, Some(placement));
        assert!(loaded.minimize_on_close);
        assert_eq!(loaded.group, saved.group);
        assert_eq!(loaded.server, saved.server);
        update_preferences_in(&dir, |settings| settings.recent_connections.clear()).unwrap();
        let after_remove = read_settings(&dir.join("settings.json")).unwrap();
        assert!(after_remove.recent_connections.is_empty());
        assert_eq!(after_remove.group, saved.group);
        assert!(after_remove.start_minimized);
        fs::write(dir.join("settings.json"), b"{broken}").unwrap();
        assert!(save_minimize_on_close_in(&dir, false).is_err());
        assert!(save_auto_check_updates_in(&dir, false).is_err());
        assert!(save_crash_capture_in(&dir, true, true).is_err());
        assert!(save_game_settings_in(&dir, &draft).is_err());
        assert_eq!(fs::read(dir.join("settings.json")).unwrap(), b"{broken}");
        fs::remove_file(dir.join("settings.json")).unwrap();
        save_minimize_on_close_in(&dir, true).unwrap();
        assert!(read_settings(&dir.join("settings.json")).unwrap().minimize_on_close);
        let legacy: Settings = serde_json::from_str(r#"{"server":"","group":"","game_path":"","transport":"tcp"}"#).unwrap();
        assert!(!legacy.auto_crash_capture && !legacy.crash_capture_consent);
        assert_eq!(legacy.language, crate::i18n::Language::ZhCn);
        fs::remove_file(dir.join("settings.json")).unwrap();
        fs::remove_dir(dir).unwrap();
    }
}
