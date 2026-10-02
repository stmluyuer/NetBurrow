//! Local support report. Never serialize Settings or peer identities into an export.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{Settings, Snapshot};

const LOG_TAIL: u64 = 128 * 1024;

/// A user-observed stall, not an automatic assertion that the game is deadlocked.
pub fn export_freeze_report(settings: &Settings, snapshot: &Snapshot) -> Result<PathBuf, String> {
    crate::diagnostics::record(
        "INFO",
        "freeze marker",
        "user reported gameplay not advancing; capturing diagnostics",
    );
    let mut snapshot = snapshot.clone();
    snapshot
        .logs
        .push(crate::text!(
            "现场标记：用户观察到对局画面不推进；此标记不代表已确认死锁或断网。",
            "Freeze marker: the user observed gameplay not advancing; this does not confirm a deadlock or network outage."
        ).into());
    export_report(settings, &snapshot)
}

pub fn export_report(settings: &Settings, snapshot: &Snapshot) -> Result<PathBuf, String> {
    let destination = crate::config_directory().join("diagnostics");
    export_to(
        &destination,
        &crate::diagnostics::directory(),
        settings,
        snapshot,
    )
    .map_err(|error| crate::text_format!("无法导出诊断：{error}", "Cannot export diagnostics: {error}"))
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
    for recent in &settings.recent_connections {
        secrets.push(recent.server.clone());
        secrets.push(recent.group.clone());
    }
    for peer in &snapshot.peers {
        if let Some(report) = &peer.status {
            secrets.push(report.name.clone());
        }
    }
    secrets.retain(|value| !value.is_empty());
    secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
    let mut result = crate::text_format!(
        "NetBurrow 诊断信息\n版本：{}\n平台：{} / {}\n状态：{:?}\n成员数：{}\n发送：{}\n接收：{}\nUDP 发送：{}\nUDP 接收：{}\n延迟毫秒：{:?}\n\n仅包含状态与最近日志；未导出配置文件、组码、成员身份或数据包内容。\n疑似凭据、长标识符及包含本机路径的日志行会脱敏或省略。\n每份日志最多取末尾 128 KiB。文件仅保存在本机，不会自动上传。\n\n状态详情：{}\n\n最近状态\n",
        "NetBurrow diagnostics\nVersion: {}\nPlatform: {} / {}\nState: {:?}\nMembers: {}\nSent: {}\nReceived: {}\nUDP sent: {}\nUDP received: {}\nLatency (ms): {:?}\n\nContains status and recent logs only; configuration files, group codes, member identities and packet contents are not exported.\nPossible credentials, long identifiers and log lines containing local paths are redacted or omitted.\nIncludes at most the last 128 KiB of each log. Files stay on this computer and are not uploaded automatically.\n\nStatus details: {}\n\nRecent status\n",
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
    result.push_str(&crate::text_format!(
        "导出时间（Unix 毫秒）：{}\n",
        "Export time (Unix milliseconds): {}\n",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    ));
    result.push_str(crate::text!(
        "诊断说明：连接在线及持续收发不代表对局正在推进。Hook 的 game diagnostics 每约 10 秒记录累计统计；member 是 Relay 临时编号，kind 0/1 为不可靠发送、2/3 为可靠发送，sent 表示 Hook 接受入队，不表示对端收到；received 包含被拒绝的入站包；consumed 表示从 Hook 队列取走，缓冲区截断另见 read_truncated。age_ms 表示距最近一次该事件的毫秒数，never 表示尚未发生。poll 只统计取得 Hook 锁后的查询；锁竞争、原生路径调用见 game api。省略计数非零时，细分统计不完整。\n",
        "Diagnostic notes: an active connection and ongoing traffic do not prove that gameplay is advancing. Hook game diagnostics record cumulative counters about every 10 seconds. member is a temporary Relay identifier; kind 0/1 means unreliable traffic and 2/3 means reliable traffic. sent means accepted into the Hook queue, not received by the peer; received includes rejected inbound packets; consumed means removed from the Hook queue. See read_truncated for buffer truncation. age_ms is the time since the last event in milliseconds; never means it has not occurred. poll counts queries made after acquiring the Hook lock; see game api for lock contention and native-path calls. Nonzero omitted counters mean detailed statistics are incomplete.\n"
    ));
    result.push_str(&crate::text_format!(
        "本地接入：ipc_slow={} process_unknown={} peer_faults={}\n",
        "Local integration: ipc_slow={} process_unknown={} peer_faults={}\n",
        snapshot.ipc_slow, snapshot.process_unknown, snapshot.peer_faults
    ));
    result.push_str(&crate::text_format!(
        "会话恢复：relay_recovering={} relay_recoveries={} ipc_recoveries={}\n",
        "Session recovery: relay_recovering={} relay_recoveries={} ipc_recoveries={}\n",
        snapshot.relay_recovering, snapshot.relay_recoveries, snapshot.ipc_recoveries
    ));
    if let Some(h) = &snapshot.hook_health {
        result.push_str(&crate::text_format!(
            "Hook：send_calls={} rejected={} read_calls={} consumed={} dropped={} lock_busy={} queue_packets={} queue_bytes={} oldest_ms={} interface_changed={}\n",
            "Hook: send_calls={} rejected={} read_calls={} consumed={} dropped={} lock_busy={} queue_packets={} queue_bytes={} oldest_ms={} interface_changed={}\n",
            h.send_calls, h.send_rejected, h.read_calls, h.consumed, h.dropped, h.lock_busy, h.queued_packets, h.queued_bytes, h.oldest_ms, h.interface_changed
        ));
    }
    result.push_str(crate::text!(
        "\n分成员通信（member 为本次 Relay 临时编号；Hook 接收不代表游戏已读取，discarded 为入站会话清理/失败包数）\n",
        "\nPer-member traffic (member is a temporary identifier for this Relay session; Hook reception does not mean the game has read the packet; discarded counts packets cleared from or rejected by inbound sessions)\n"
    ));
    result.push_str(crate::text!(
        "\n端到端诊断：TCP 探测经过本机客户端 → Relay → 队友客户端 → Relay → 本机，不经过游戏 Hook，也不代表 UDP 路径或游戏逻辑正常。未协商支持时不发送扩展数据。序号在本机客户端收到 Hook 数据后、网络发送入队前分配，接收统计位于对端客户端入站队列出队后、交给 Hook 前；assigned 不代表发送完成。每个目标的可靠/不可靠数据分别编号，可靠类型 2/3 共用序号。gaps_detected 为累计观察到的跳号，乱序补到后不回减；missing_window 仅统计最近 256 个序号仍缺失的数量，too_old 无法精确区分迟到与重复。没有后续数据时不能仅凭序号发现末尾缺包。stream_changes 表示当前会话中流编号变化；成员/游戏实例或 Relay 连接变化后重置统计。最多跟踪 32 位已绑定成员，超出部分见 omitted。\n",
        "\nEnd-to-end diagnostics: TCP probes travel from this client through the Relay to the peer client and back via the Relay. They bypass the game Hook and do not verify the UDP path or game logic. Extension data is sent only after support is negotiated. Sequence numbers are assigned after this client receives Hook data and before network enqueue; receive counters are updated after dequeue at the peer client and before delivery to its Hook. assigned does not mean transmission completed. Reliable and unreliable traffic have separate sequences for each destination; reliable kinds 2/3 share one sequence. gaps_detected counts observed sequence jumps and does not decrease when late packets arrive; missing_window counts missing numbers in the latest 256-number window; too_old cannot precisely distinguish late packets from duplicates. Without later packets, sequence numbers alone cannot reveal missing tail packets. stream_changes counts stream identifier changes in the current session. Counters reset when the member, game instance or Relay connection changes. At most 32 bound members are tracked; see omitted for additional members.\n"
    ));
    for line in snapshot.path_diagnostics.lines() {
        result.push_str(&line);
        result.push('\n');
    }
    for line in crate::client::peer_diagnostic_lines(snapshot) {
        result.push_str(&line);
        result.push('\n');
    }
    if let Some(h) = &snapshot.hook_health {
        if h.peers_omitted > 0 {
            result.push_str(&crate::text_format!(
                "摘要容量限制，省略 {} 个成员\n",
                "Summary capacity reached; {} members omitted\n",
                h.peers_omitted
            ));
        }
    }
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
        "network.previous.log",
        "network.log",
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
                result.push_str(crate::text!("尚无此日志。\n", "No log available yet.\n"))
            }
            Err(error) => result.push_str(&crate::text_format!("读取失败：{:?}\n", "Read failed: {:?}\n", error.kind())),
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
                return crate::text!("[疑似凭据的日志行已省略]", "[Log line with possible credentials omitted]").to_owned();
            }
            if line.as_bytes().windows(3).any(|part| {
                part[0].is_ascii_alphabetic() && part[1] == b':' && matches!(part[2], b'\\' | b'/')
            }) || line.contains("\\\\")
            {
                return crate::text!("[包含本机路径的日志行已省略]", "[Log line containing a local path omitted]").to_owned();
            }
            let mut line = line.to_owned();
            for secret in secrets {
                line = line.replace(secret, crate::text!("[已隐藏]", "[Redacted]"));
            }
            while let Some(start) = line.to_ascii_uppercase().find("NB1-") {
                let end = line[start..]
                    .find(char::is_whitespace)
                    .map_or(line.len(), |end| start + end);
                line.replace_range(start..end, crate::text!("[组码已隐藏]", "[Group code redacted]"));
            }
            // Covers raw group keys, nonces/tokens and decimal Steam IDs even from older logs.
            let mut output = String::new();
            let mut run = String::new();
            let flush = |run: &mut String, output: &mut String| {
                if run.len() >= 32 || (run.len() >= 15 && run.bytes().all(|b| b.is_ascii_digit())) {
                    output.push_str(crate::text!("[标识符已隐藏]", "[Identifier redacted]"));
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
                "现场标记：用户观察到对局画面不推进；此标记不代表已确认死锁或断网。".into(),
                "flow member=7 channel=3 kind=2 sent=90 received=80 consumed=80 dropped=0 stale_received=0 cleared_in=0 cleared_out=0".into(),
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
        assert!(report.contains("现场标记：用户观察到对局画面不推进"));
        assert!(report.contains("flow member=7 channel=3 kind=2 sent=90 received=80 consumed=80"));
        assert!(report.contains("导出时间（Unix 毫秒）"));
        fs::remove_file(file).unwrap();
        fs::remove_dir(dir).unwrap();
    }

}
