//! Local support report. Never serialize Settings or peer identities into an export.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{Settings, Snapshot};

const LOG_TAIL: u64 = 128 * 1024;

pub fn export_report(settings: &Settings, snapshot: &Snapshot) -> Result<PathBuf, String> {
    let destination = crate::config_directory().join("diagnostics");
    export_to(
        &destination,
        &crate::diagnostics::directory(),
        settings,
        snapshot,
    )
    .map_err(|error| format!("无法导出诊断信息：{error}"))
}

fn export_to(
    destination: &Path,
    logs: &Path,
    settings: &Settings,
    snapshot: &Snapshot,
) -> io::Result<PathBuf> {
    let report = build_report(logs, settings, snapshot);
    fs::create_dir_all(destination)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = destination.join(format!("NetBurrow-diagnostics-{stamp}.txt"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(report.as_bytes())?;
    Ok(path)
}

fn build_report(logs: &Path, settings: &Settings, snapshot: &Snapshot) -> String {
    let mut secrets = vec![
        settings.group.clone(),
        settings.server.clone(),
        settings.display_name.clone(),
        settings.game_path.clone(),
    ];
    if let Some(hex) = settings.group.strip_prefix("NB1-") {
        secrets.push(hex.to_owned());
    }
    for peer in &snapshot.peers {
        if let Some(report) = &peer.status {
            secrets.push(report.name.clone());
        }
    }
    secrets.retain(|value| !value.is_empty());
    secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
    let mut result = format!(
        "NetBurrow 诊断信息\n版本：{}\n平台：{} / {}\n状态：{:?}\n成员数：{}\n发送：{}\n接收：{}\nUDP 发送：{}\nUDP 接收：{}\n延迟毫秒：{:?}\n\n仅包含状态与最近日志；未导出配置文件、组码、成员身份或数据包内容。\n疑似凭据、长标识符及包含本机路径的日志行会脱敏或省略。\n每份日志最多取末尾 128 KiB。文件仅保存在本机，不会自动上传。\n\n状态详情：{}\n\n最近状态\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        snapshot.phase,
        snapshot.peers.len(),
        snapshot.sent,
        snapshot.received,
        snapshot.udp_sent,
        snapshot.udp_received,
        snapshot.ping_ms,
        redact(&snapshot.detail, &secrets),
    );
    for line in snapshot
        .logs
        .iter()
        .rev()
        .take(64)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        result.push_str(&redact(line, &secrets));
        result.push('\n');
    }
    for name in [
        "client.previous.log",
        "client.log",
        "injector.previous.log",
        "injector.log",
        "hook.previous.log",
        "hook.log",
    ] {
        result.push_str(&format!("\n--- {name} ---\n"));
        match read_tail(&logs.join(name)) {
            Ok(text) => {
                for line in text.lines() {
                    result.push_str(&redact(line, &secrets));
                    result.push('\n');
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                result.push_str("尚无此日志。\n")
            }
            Err(error) => result.push_str(&format!("读取失败：{:?}\n", error.kind())),
        }
    }
    result
}

fn read_tail(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let offset = length.saturating_sub(LOG_TAIL);
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(LOG_TAIL).read_to_end(&mut bytes)?;
    // Drop the first partial line, which may start in the middle of a UTF-8 character or secret.
    let start = if offset > 0 {
        bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |index| index + 1)
    } else {
        0
    };
    Ok(String::from_utf8_lossy(&bytes[start..]).into_owned())
}

fn redact(text: &str, secrets: &[String]) -> String {
    text.lines()
        .map(|line| {
            let lower = line.to_ascii_lowercase();
            if [
                "token",
                "password",
                "secret",
                "nonce",
                "authorization",
                "cookie",
                "credential",
            ]
            .iter()
            .any(|key| lower.contains(key))
            {
                return "[疑似凭据的日志行已省略]".to_owned();
            }
            if line.as_bytes().windows(3).any(|part| {
                part[0].is_ascii_alphabetic() && part[1] == b':' && matches!(part[2], b'\\' | b'/')
            }) || line.contains("\\\\")
            {
                return "[包含本机路径的日志行已省略]".to_owned();
            }
            let mut line = line.to_owned();
            for secret in secrets {
                line = line.replace(secret, "[已隐藏]");
            }
            while let Some(start) = line.to_ascii_uppercase().find("NB1-") {
                let end = line[start..]
                    .find(char::is_whitespace)
                    .map_or(line.len(), |end| start + end);
                line.replace_range(start..end, "[组码已隐藏]");
            }
            // Covers raw group keys, nonces/tokens and decimal Steam IDs even from older logs.
            let mut output = String::new();
            let mut run = String::new();
            let flush = |run: &mut String, output: &mut String| {
                if run.len() >= 32 || (run.len() >= 15 && run.bytes().all(|b| b.is_ascii_digit())) {
                    output.push_str("[标识符已隐藏]");
                } else {
                    output.push_str(run);
                }
                run.clear();
            };
            for character in line.chars() {
                if character.is_ascii_hexdigit() {
                    run.push(character);
                } else {
                    flush(&mut run, &mut output);
                    output.push(character);
                }
            }
            flush(&mut run, &mut output);
            output
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_masks_secrets_and_exports_without_logs() {
        let settings = Settings {
            group: format!("NB1-{}", "a1".repeat(32)),
            display_name: "Example Player".into(),
            server: "private.example:24872".into(),
            ..Settings::default()
        };
        let snapshot = Snapshot {
            detail: format!(
                "{} {} {}",
                settings.group, settings.display_name, settings.server
            ),
            logs: vec![
                "token=never-export-me".into(),
                "old NB1-previous-secret".into(),
                "id=76561198012345678".into(),
                r"failed at D:\Private User\game.exe".into(),
                format!("raw {}", "b2".repeat(32)),
            ],
            ..Snapshot::default()
        };
        let dir = std::env::temp_dir().join(format!(
            "netburrow-export-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let file = export_to(&dir, &dir.join("missing-logs"), &settings, &snapshot).unwrap();
        let report = fs::read_to_string(&file).unwrap();
        for private in [
            &settings.group,
            &settings.display_name,
            &settings.server,
            "never-export-me",
            "previous-secret",
            "76561198012345678",
            "Private User",
            &"b2".repeat(32),
        ] {
            assert!(!report.contains(private));
        }
        assert!(report.contains(env!("CARGO_PKG_VERSION")));
        assert!(report.contains("尚无此日志"));
        fs::remove_file(file).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn tail_is_bounded_and_drops_partial_secret() {
        let file = std::env::temp_dir().join(format!("netburrow-tail-{}.log", std::process::id()));
        let mut bytes = vec![b'x'; LOG_TAIL as usize + 50];
        bytes.extend_from_slice(b"\nlast safe line\n");
        fs::write(&file, bytes).unwrap();
        assert_eq!(read_tail(&file).unwrap(), "last safe line\n");
        fs::remove_file(file).unwrap();
    }
}
