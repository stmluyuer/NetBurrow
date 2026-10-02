use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub(super) struct Bundle {
    pub path: PathBuf,
    pub missing_logs: Vec<&'static str>,
    pub crash_included: bool,
    pub crash_note: Option<String>,
}

pub(super) fn export(
    settings: &netburrow_core::Settings,
    snapshot: &netburrow_core::Snapshot,
) -> Result<Bundle, String> {
    let report = netburrow_core::export_report(settings, snapshot)?;
    bundle_to(
        report.parent().unwrap(),
        &netburrow_core::diagnostics::directory(),
        &report,
        &netburrow_core::config_directory().join("crashes"),
    )
    .map_err(|error| netburrow_core::text_format!("无法打包日志：{error}", "Cannot create log archive: {error}"))
}

fn bundle_to(destination: &Path, logs: &Path, report: &Path, crashes: &Path) -> io::Result<Bundle> {
    let crash = select_crash(crashes)?;
    fs::create_dir_all(destination)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = destination.join(format!("NetBurrow-logs-{stamp}.zip"));
    // Reserve the output so repeated exports cannot overwrite an existing ZIP.
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    let result = (|| {
        let mut archive = zip::ZipWriter::new(output);
        add_file(&mut archive, "diagnostics.txt", open_file(report, false)?)?;
        let mut missing_logs = Vec::new();
        for name in ["client.log", "injector.log", "hook.log"] {
            match open_file(&logs.join(name), false) {
                Ok(file) => add_file(&mut archive, name, file)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => missing_logs.push(name),
                Err(error) => {
                    return Err(io::Error::new(
                        error.kind(),
                        netburrow_core::text_format!("无法读取 {name}：{error}", "Cannot read {name}: {error}"),
                    ));
                }
            }
        }
        // Rotated files contain the minutes before an incident when export happens later.
        for name in ["client.previous.log", "injector.previous.log", "hook.previous.log", "network.log", "network.previous.log"] {
            match open_file(&logs.join(name), false) {
                Ok(file) => add_file(&mut archive, name, file)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {},
                Err(error) => return Err(error),
            }
        }
        if !missing_logs.is_empty() {
            add_text(
                &mut archive,
                "missing-logs.txt",
                &netburrow_core::text_format!("未生成的日志：{}\n", "Logs not yet available: {}\n",
                    missing_logs.join("、")
                ),
            )?;
        }
        let crash_included = !crash.files.is_empty();
        for (name, file) in crash.files {
            add_file(&mut archive, &name, file).map_err(|error| {
                io::Error::new(error.kind(), netburrow_core::text_format!("无法读取崩溃记录 {name}：{error}", "Cannot read crash record {name}: {error}"))
            })?;
        }
        add_text(&mut archive, "crash-capture.txt", &crash.note)?;
        // Finalization writes the ZIP directory and must succeed before revealing the file.
        archive.finish()?.sync_all()?;
        Ok(Bundle {
            path: path.clone(),
            missing_logs,
            crash_included,
            crash_note: Some(crash.note),
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&path);
    }
    result
}

fn add_file(archive: &mut zip::ZipWriter<File>, name: &str, file: File) -> io::Result<()> {
    let length = file.metadata()?.len();
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(true);
    archive.start_file(name, options)?;
    // Stream full dumps directly to ZIP64, and take a finite snapshot of active logs.
    let copied = io::copy(&mut file.take(length), archive)?;
    if copied != length {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            netburrow_core::text_format!("{name} 在打包期间被截断", "{name} was truncated while archiving"),
        ));
    }
    Ok(())
}

fn add_text(archive: &mut zip::ZipWriter<File>, name: &str, text: &str) -> io::Result<()> {
    archive.start_file(name, zip::write::SimpleFileOptions::default())?;
    archive.write_all(text.as_bytes())
}

struct CrashFiles {
    files: Vec<(String, File)>,
    note: String,
}

fn select_crash(root: &Path) -> io::Result<CrashFiles> {
    match fs::symlink_metadata(root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(CrashFiles {
                files: Vec::new(),
                note: netburrow_core::text!("暂无完整崩溃记录，仅包含日志。", "No completed crash capture available. Logs only.").into(),
            });
        }
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    // Keep directories open without delete sharing on Windows until every source is open.
    // This prevents a checked directory from being replaced by a junction during traversal.
    let _root_guard = open_directory(root)?;
    let mut completed = Vec::new();
    let mut unfinished = 0;
    let mut unsafe_entries = 0;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if is_link(&metadata) {
            unsafe_entries += 1;
            continue;
        }
        if !metadata.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|name| safe_name(name)) else {
            unsafe_entries += 1;
            continue;
        };
        let session = entry.path();
        let _session_guard = open_directory(&session)?;
        match open_file(&session.join("capture.complete"), true) {
            Ok(marker) if marker.metadata()?.len() == 0 => {
                completed.push((marker.metadata()?.modified()?, name.to_owned(), session));
            }
            Ok(_) => unfinished += 1,
            Err(error) if error.kind() == io::ErrorKind::NotFound => unfinished += 1,
            Err(error) => return Err(error),
        }
    }
    completed.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));
    let total = completed.len();
    let mut files = Vec::new();
    let mut note = if let Some((_, name, session)) = completed.pop() {
        let _session_guard = open_directory(&session)?;
        // Reopen the marker under the directory guard; only sealed captures are eligible.
        let marker = open_file(&session.join("capture.complete"), true)?;
        if marker.metadata()?.len() != 0 {
            return Err(io::Error::other(netburrow_core::text!("崩溃记录完成标记无效", "Invalid crash capture completion marker")));
        }
        let mut metadata_file = open_file(&session.join("capture.json"), true)?;
        if metadata_file.metadata()?.len() > 1024 * 1024 {
            return Err(io::Error::other(netburrow_core::text!("崩溃记录元数据过大，打包已取消", "Crash metadata exceeds the size limit. Archive cancelled")));
        }
        let mut contents = Vec::new();
        metadata_file.read_to_end(&mut contents)?;
        // Windows PowerShell 5.1 writes UTF-8 with a BOM.
        let contents = contents
            .strip_prefix(&[0xef, 0xbb, 0xbf])
            .unwrap_or(&contents);
        let metadata: serde_json::Value = serde_json::from_slice(contents)
            .map_err(|error| io::Error::other(netburrow_core::text_format!("崩溃记录元数据损坏：{error}", "Damaged crash metadata: {error}")))?;
        if metadata.get("status").and_then(serde_json::Value::as_str) != Some("exception_captured")
        {
            return Err(io::Error::other(netburrow_core::text!("崩溃记录未完成，打包已取消", "Crash capture is incomplete. Archive cancelled")));
        }
        metadata_file.rewind()?;
        files.push((format!("crashes/{name}/capture.json"), metadata_file));
        let mut dumps = 0;
        for entry in fs::read_dir(&session)? {
            let entry = entry?;
            let filename = entry.file_name();
            let Some(filename) = filename.to_str().filter(|name| safe_name(name)) else {
                continue;
            };
            let is_dump = filename.to_ascii_lowercase().ends_with(".dmp");
            if is_dump
                || matches!(
                    filename,
                    "procdump.log" | "procdump-error.log" | "collection-warnings.txt"
                )
            {
                let file = open_file(&entry.path(), true).map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        netburrow_core::text_format!("无法读取崩溃记录 {filename}：{error}", "Cannot read crash record {filename}: {error}"),
                    )
                })?;
                dumps += usize::from(is_dump);
                files.push((format!("crashes/{name}/{filename}"), file));
            }
        }
        if dumps == 0 {
            return Err(io::Error::other(
                netburrow_core::text!("崩溃记录缺少转储文件，打包已取消", "Crash dump is missing. Archive cancelled"),
            ));
        }
        for phase in ["before", "after"] {
            let directory = session.join(phase);
            let _phase_guard = match open_directory(&directory) {
                Ok(guard) => guard,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            for filename in [
                "client.log",
                "client.previous.log",
                "injector.log",
                "injector.previous.log",
                "hook.log",
                "hook.previous.log",
                "network.log",
                "network.previous.log",
                "isaac-Repentance+.log",
                "isaac-Repentance.log",
                "isaac-Rebirth.log",
            ] {
                match open_file(&directory.join(filename), true) {
                    Ok(file) => files.push((format!("crashes/{name}/{phase}/{filename}"), file)),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
        }
        let mut note = netburrow_core::text_format!("已包含最近的崩溃记录 {name}（完整转储和采集日志）。", "Included latest crash capture {name} (full dump and capture logs).");
        if total > 1 {
            note.push_str(&netburrow_core::text_format!("另有 {} 份历史现场未加入，以控制文件大小。", "Omitted {} older captures to limit archive size.",
                total - 1
            ));
        }
        note
    } else {
        netburrow_core::text!("暂无完整崩溃记录，仅包含日志。", "No completed crash capture available. Logs only.").into()
    };
    if unfinished > 0 {
        note.push_str(&netburrow_core::text_format!("{unfinished} 个尚未确认完成的记录未加入。请等待采集完成后重试；旧版记录需重新采集。", "Omitted {unfinished} unconfirmed captures. Retry after capture finishes; legacy records need to be captured again."
        ));
    }
    if unsafe_entries > 0 {
        note.push_str(&netburrow_core::text_format!("已跳过 {unsafe_entries} 个链接或名称不安全的目录。", "Skipped {unsafe_entries} linked or unsafe capture folders."
        ));
    }
    Ok(CrashFiles { files, note })
}

fn safe_name(name: &str) -> bool {
    !matches!(name, "" | "." | "..")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+'))
}

fn is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    metadata.file_type().is_symlink()
}

fn open_file(path: &Path, sealed: bool) -> io::Result<File> {
    let metadata = fs::symlink_metadata(path)?;
    if is_link(&metadata) || !metadata.is_file() {
        return Err(io::Error::other(netburrow_core::text_format!("拒绝读取链接或非普通文件：{}", "Cannot read a link or non-regular file: {}",
            path.display()
        )));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // OPEN_REPARSE_POINT; completed files cannot be modified or replaced while read.
        options
            .custom_flags(0x0020_0000)
            .share_mode(if sealed { 1 } else { 7 });
    }
    #[cfg(not(windows))]
    let _ = sealed;
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if is_link(&metadata) || !metadata.is_file() {
        return Err(io::Error::other(netburrow_core::text!("拒绝读取链接或非普通文件", "Cannot read a link or non-regular file")));
    }
    Ok(file)
}

fn open_directory(path: &Path) -> io::Result<File> {
    let metadata = fs::symlink_metadata(path)?;
    if is_link(&metadata) || !metadata.is_dir() {
        return Err(io::Error::other(netburrow_core::text_format!("拒绝读取链接或非目录：{}", "Cannot read a link or non-directory: {}",
            path.display()
        )));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_READ_ATTRIBUTES is sufficient to pin and inspect ancestors; requesting
        // GENERIC_READ would also require permission to list every parent directory.
        // BACKUP_SEMANTICS | OPEN_REPARSE_POINT, without FILE_SHARE_DELETE.
        options
            .access_mode(0x0080)
            .custom_flags(0x0220_0000)
            .share_mode(3);
    }
    let directory = options.open(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            netburrow_core::text_format!("无法检查采集目录 {}：{error}", "Cannot inspect capture folder {}: {error}", path.display()),
        )
    })?;
    let metadata = directory.metadata()?;
    if is_link(&metadata) || !metadata.is_dir() {
        return Err(io::Error::other(netburrow_core::text!("拒绝读取链接或非目录", "Cannot read a link or non-directory")));
    }
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs::FileTimes, time::Duration};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("netburrow-bundle-{}-{stamp}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn capture(&self, name: &str, completed: Option<u64>) -> PathBuf {
            let path = self.0.join("crashes").join(name);
            fs::create_dir_all(&path).unwrap();
            fs::write(
                path.join("capture.json"),
                b"\xef\xbb\xbf{\"status\":\"exception_captured\"}",
            )
            .unwrap();
            fs::write(path.join("isaac.dmp"), b"fixture dump contents").unwrap();
            if let Some(seconds) = completed {
                let marker = File::create(path.join("capture.complete")).unwrap();
                marker
                    .set_times(
                        FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)),
                    )
                    .unwrap();
            }
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn archive_contains_only_latest_sealed_capture_and_allowed_files() {
        let fixture = Fixture::new();
        fixture.capture("older", Some(100));
        let latest = fixture.capture("latest", Some(200));
        fixture.capture("unfinished", None);
        fs::write(latest.join("settings.json"), b"private configuration").unwrap();
        fs::create_dir(latest.join("before")).unwrap();
        fs::write(latest.join("before/client.log"), b"before log").unwrap();
        fs::write(
            latest.join("before/settings.json"),
            b"private configuration",
        )
        .unwrap();
        let logs = fixture.0.join("logs");
        fs::create_dir(&logs).unwrap();
        fs::write(logs.join("client.log"), b"current log").unwrap();
        fs::write(logs.join("client.previous.log"), b"before incident").unwrap();
        fs::write(logs.join("network.log"), b"network now").unwrap();
        fs::write(logs.join("network.previous.log"), b"network before").unwrap();
        let report = fixture.0.join("report.txt");
        fs::write(&report, b"diagnostics").unwrap();
        let result = bundle_to(&fixture.0, &logs, &report, &fixture.0.join("crashes")).unwrap();
        assert!(result.crash_included);
        assert_eq!(result.missing_logs, ["injector.log", "hook.log"]);
        let note = result.crash_note.unwrap();
        assert!(note.contains("1 份历史现场未加入"));
        assert!(note.contains("1 个尚未确认完成"));
        let mut archive = zip::ZipArchive::new(File::open(&result.path).unwrap()).unwrap();
        let mut names = archive.file_names().map(str::to_owned).collect::<Vec<_>>();
        names.sort();
        assert_eq!(
            names,
            [
                "client.log",
                "client.previous.log",
                "crash-capture.txt",
                "crashes/latest/before/client.log",
                "crashes/latest/capture.json",
                "crashes/latest/isaac.dmp",
                "diagnostics.txt",
                "missing-logs.txt",
                "network.log",
                "network.previous.log"
            ]
        );
        let mut contents = Vec::new();
        archive
            .by_name("crashes/latest/isaac.dmp")
            .unwrap()
            .read_to_end(&mut contents)
            .unwrap();
        assert_eq!(contents, b"fixture dump contents");
    }

    #[test]
    fn unfinished_capture_is_reported_and_broken_completed_capture_fails() {
        let fixture = Fixture::new();
        let session = fixture.capture("unfinished", None);
        let selected = select_crash(&fixture.0.join("crashes")).unwrap();
        assert!(selected.files.is_empty());
        assert!(selected.note.contains("1 个尚未确认完成"));
        fs::write(session.join("capture.complete"), b"").unwrap();
        fs::remove_file(session.join("isaac.dmp")).unwrap();
        assert!(select_crash(&fixture.0.join("crashes")).is_err());
        assert!(!safe_name("../outside"));
        assert!(!safe_name("..\\outside"));
        assert!(!safe_name("file.dmp:private"));
    }

    #[cfg(windows)]
    #[test]
    fn junction_capture_cannot_include_outside_files() {
        use std::os::windows::process::CommandExt;
        let fixture = Fixture::new();
        let session = fixture.capture("capture", Some(100));
        let outside = fixture.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("client.log"), b"outside secret").unwrap();
        // A junction needs no symlink privilege; construct it with native PowerShell.
        let link = session.join("before");
        let script = "New-Item -ItemType Junction -Path $env:NETBURROW_TEST_LINK -Target $env:NETBURROW_TEST_TARGET -ErrorAction Stop | Out-Null";
        let status = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env("NETBURROW_TEST_LINK", &link)
            .env("NETBURROW_TEST_TARGET", &outside)
            .creation_flags(0x0800_0000)
            .status()
            .unwrap();
        assert!(status.success());
        let error = select_crash(&fixture.0.join("crashes")).err().unwrap();
        assert!(error.to_string().contains("拒绝读取链接"), "{error}");
        fs::remove_dir(&link).unwrap();
    }
}
