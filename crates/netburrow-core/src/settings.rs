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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
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
    pub window_placement: Option<WindowPlacement>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
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
            window_placement: None,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<(), String> {
        if self.display_name.chars().count() > 24 || self.display_name.chars().any(char::is_control)
        {
            return Err("显示名最多 24 个字符，不能含换行或控制字符".into());
        }
        let server = self.server.trim();
        if server.is_empty() || server.contains('/') || server.chars().any(char::is_whitespace) {
            return Err("服务器请填写主机名或 IP 加端口，例如 relay.example.com:24872".into());
        }
        let (host, port) = server
            .rsplit_once(':')
            .ok_or("服务器缺少端口，例如 :24872")?;
        if host.is_empty() || port.parse::<u16>().ok().filter(|p| *p != 0).is_none() {
            return Err("服务器地址或端口无效".into());
        }
        parse_group(&self.group)?;
        let path = Path::new(self.game_path.trim());
        if !path.is_file() {
            return Err("请选择实际存在的 isaac-ng.exe".into());
        }
        if !path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("isaac-ng.exe"))
        {
            return Err("游戏路径必须指向以撒的 isaac-ng.exe".into());
        }
        super::process::validate_x86_image(path)
            .map_err(|e| format!("无法识别 32 位以撒程序：{e}"))?;
        Ok(())
    }
}

pub fn parse_group(input: &str) -> Result<netburrow_protocol::Group, String> {
    let input = input.trim();
    let hex = input
        .strip_prefix("NB1-")
        .ok_or("联机组格式无效，请创建或导入 NB1- 开头的组码")?;
    if hex.len() != 64 {
        return Err("联机组不完整：应为 NB1- 加 64 位十六进制字符，请重新复制朋友的完整组码".into());
    }
    let mut group = [0; 32];
    for (slot, pair) in group.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
        let digit = |b: u8| {
            (b as char)
                .to_digit(16)
                .map(|n| n as u8)
                .ok_or("联机组含无效字符，请重新复制朋友的完整组码，或创建新组并分享")
        };
        *slot = digit(pair[0])? * 16 + digit(pair[1])?;
    }
    if group == [0; 32] {
        return Err("全零组码无效，请重新创建联机组".into());
    }
    Ok(group)
}

pub fn new_group() -> Result<String, String> {
    let group =
        netburrow_protocol::random_group().map_err(|_| "系统随机数不可用，无法创建联机组")?;
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
            .map_err(|_| "设置文件损坏或版本不匹配。请重新填写；不会自动更换联机组。".into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(format!("无法读取设置：{e}")),
    }
}

pub fn load_settings() -> Result<Settings, String> {
    read_settings(&config_directory().join("settings.json"))
}

pub fn save_settings(settings: &Settings) -> Result<(), String> {
    save_settings_in(&config_directory(), settings)
}

pub fn save_minimize_on_close(enabled: bool) -> Result<(), String> {
    save_minimize_on_close_in(&config_directory(), enabled)
}

pub fn save_notifications_enabled(enabled: bool) -> Result<(), String> {
    update_preferences_in(&config_directory(), |settings| settings.notifications_enabled = enabled)
}

pub fn save_window_placement(placement: WindowPlacement) -> Result<(), String> {
    update_preferences_in(&config_directory(), |settings| settings.window_placement = Some(placement))
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
    fs::create_dir_all(&dir).map_err(|e| format!("无法创建设置目录：{e}"))?;
    let bytes = serde_json::to_vec_pretty(settings).map_err(|e| e.to_string())?;
    let staging = dir.join("settings.new");
    fs::write(&staging, bytes).map_err(|e| format!("无法写入设置：{e}"))?;
    fs::rename(staging, dir.join("settings.json")).map_err(|e| format!("无法保存设置：{e}"))
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
        let loaded = read_settings(&dir.join("settings.json")).unwrap();
        assert!(loaded.minimize_on_close);
        assert_eq!(loaded.server, saved.server);
        assert_eq!(loaded.group, saved.group);
        let placement = WindowPlacement { normal: [50, 60, 690, 880], maximized: true };
        update_preferences_in(&dir, |settings| {
            settings.notifications_enabled = false;
            settings.window_placement = Some(placement.clone());
        }).unwrap();
        let loaded = read_settings(&dir.join("settings.json")).unwrap();
        assert!(!loaded.notifications_enabled);
        assert_eq!(loaded.window_placement, Some(placement));
        assert!(loaded.minimize_on_close);
        assert_eq!(loaded.group, saved.group);
        assert_eq!(loaded.server, saved.server);
        fs::write(dir.join("settings.json"), b"{broken}").unwrap();
        assert!(save_minimize_on_close_in(&dir, false).is_err());
        assert_eq!(fs::read(dir.join("settings.json")).unwrap(), b"{broken}");
        fs::remove_file(dir.join("settings.json")).unwrap();
        save_minimize_on_close_in(&dir, true).unwrap();
        assert!(read_settings(&dir.join("settings.json")).unwrap().minimize_on_close);
        fs::remove_file(dir.join("settings.json")).unwrap();
        fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn group_roundtrip_and_invalid_input() {
        let value = new_group().unwrap();
        assert_ne!(parse_group(&value).unwrap(), [0; 32]);
        assert_eq!(
            parse_group(&format!("  {value}\n")).unwrap(),
            parse_group(&value).unwrap()
        );
        for invalid in [
            "",
            "NB1-你好",
            "old-room-code",
            &format!("NB1-{}", "0".repeat(64)),
        ] {
            assert!(parse_group(invalid).is_err());
        }
    }
    #[test]
    fn settings_preserve_group_and_reject_corruption() {
        let value = Settings {
            server: "localhost:24872".into(),
            group: new_group().unwrap(),
            game_path: "example/isaac-ng.exe".into(),
            transport: Transport::Udp,
            allow_late_hook: true,
            display_name: "测试玩家".into(),
            minimize_on_close: true,
            notifications_enabled: false,
            window_placement: Some(WindowPlacement { normal: [50, 60, 690, 880], maximized: false }),
        };
        let encoded = serde_json::to_vec(&value).unwrap();
        let decoded: Settings = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.group, value.group);
        assert_eq!(decoded.transport, Transport::Udp);
        assert!(decoded.allow_late_hook);
        assert!(decoded.minimize_on_close);
        assert!(!decoded.notifications_enabled);
        assert_eq!(decoded.window_placement, value.window_placement);
        assert_eq!(decoded.display_name, "测试玩家");
        let mut legacy = serde_json::to_value(&value).unwrap();
        legacy.as_object_mut().unwrap().remove("allow_late_hook");
        legacy.as_object_mut().unwrap().remove("display_name");
        legacy.as_object_mut().unwrap().remove("minimize_on_close");
        legacy.as_object_mut().unwrap().remove("notifications_enabled");
        legacy.as_object_mut().unwrap().remove("window_placement");
        let legacy: Settings = serde_json::from_value(legacy).unwrap();
        assert!(!legacy.allow_late_hook);
        assert!(!legacy.minimize_on_close);
        assert!(legacy.notifications_enabled);
        assert!(legacy.window_placement.is_none());
        assert!(!Settings::default().minimize_on_close);
        assert!(legacy.display_name.is_empty());
        assert_eq!(legacy.group, value.group);
        assert!(serde_json::from_slice::<Settings>(b"{broken}").is_err());
        assert!(serde_json::from_str::<Settings>("{}").is_err());
    }
}
