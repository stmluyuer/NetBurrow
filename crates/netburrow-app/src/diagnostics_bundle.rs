use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub(super) struct Bundle {
    pub path: PathBuf,
    pub missing_logs: Vec<&'static str>,
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
    )
    .map_err(|error| format!("无法打包日志：{error}"))
}

fn bundle_to(destination: &Path, logs: &Path, report: &Path) -> io::Result<Bundle> {
    fs::create_dir_all(destination)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = format!("NetBurrow-logs-{stamp}");
    let staging = destination.join(format!(".{name}.tmp"));
    let path = destination.join(format!("{name}.zip"));
    // Only clean up paths created by this invocation; never reuse an old staging folder.
    fs::create_dir(&staging)?;
    let mut owns_archive = false;
    let result = (|| {
        fs::copy(report, staging.join("diagnostics.txt"))?;
        let mut missing_logs = Vec::new();
        for name in ["client.log", "injector.log", "hook.log"] {
            match File::open(logs.join(name)) {
                Ok(file) => {
                    // Capture a finite snapshot even while the process keeps appending.
                    let length = file.metadata()?.len();
                    let mut output = File::create(staging.join(name))?;
                    io::copy(&mut file.take(length), &mut output)?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => missing_logs.push(name),
                Err(error) => {
                    return Err(io::Error::new(
                        error.kind(),
                        format!("读取 {name} 失败：{error}"),
                    ));
                }
            }
        }
        if !missing_logs.is_empty() {
            fs::write(
                staging.join("missing-logs.txt"),
                format!(
                    "以下日志尚未生成，本次未包含：{}\n",
                    missing_logs.join("、")
                ),
            )?;
        }
        // Reserve the output so repeated exports cannot overwrite an existing ZIP.
        drop(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?,
        );
        owns_archive = true;
        compress(&staging, &path)?;
        Ok(Bundle {
            path: path.clone(),
            missing_logs,
        })
    })();
    if result.is_err() && owns_archive {
        let _ = fs::remove_file(&path);
    }
    let _ = fs::remove_dir_all(&staging);
    result
}

#[cfg(windows)]
fn compress(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::process::CommandExt;

    // Paths are data in environment variables, never interpolated into PowerShell code.
    // ASCII source also works with Windows PowerShell 5.1 irrespective of script encoding.
    let script = r#"
$ErrorActionPreference = 'Stop'
$OutputEncoding = [System.Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
[Console]::InputEncoding = [System.Text.UTF8Encoding]::new($false)
$PSDefaultParameterValues['*:Encoding'] = 'utf8'
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
$stream = [System.IO.File]::Open($env:NETBURROW_ZIP_DESTINATION, 'Open', 'Write', 'None')
try {
    $zip = [System.IO.Compression.ZipArchive]::new($stream, [System.IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($file in [System.IO.Directory]::GetFiles($env:NETBURROW_ZIP_SOURCE)) {
            [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, $file, [System.IO.Path]::GetFileName($file), [System.IO.Compression.CompressionLevel]::Optimal) | Out-Null
        }
    } finally { if ($null -ne $zip) { $zip.Dispose() } }
} finally { $stream.Dispose() }
"#;
    let powershell = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("未找到 Windows 系统目录"))?
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let output = std::process::Command::new(powershell)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            script,
        ])
        .env("NETBURROW_ZIP_SOURCE", source)
        .env("NETBURROW_ZIP_DESTINATION", destination)
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "压缩失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

#[cfg(not(windows))]
fn compress(_source: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::other("日志打包需要 Windows"))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::os::windows::process::CommandExt;

    fn fixture() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "netburrow-bundle-测试 [a] ' $-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        fs::create_dir_all(root.join("logs")).unwrap();
        fs::write(root.join("report.txt"), "最新诊断\n").unwrap();
        root
    }

    fn extract(archive: &Path, destination: &Path) {
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command",
                "$ErrorActionPreference = 'Stop'; $OutputEncoding = [System.Text.UTF8Encoding]::new($false); [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); [Console]::InputEncoding = [System.Text.UTF8Encoding]::new($false); $PSDefaultParameterValues['*:Encoding'] = 'utf8'; Add-Type -AssemblyName System.IO.Compression.FileSystem; [System.IO.Compression.ZipFile]::ExtractToDirectory($env:NETBURROW_TEST_ZIP, $env:NETBURROW_TEST_EXTRACT)"])
            .env("NETBURROW_TEST_ZIP", archive)
            .env("NETBURROW_TEST_EXTRACT", destination)
            .creation_flags(0x0800_0000)
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn zip_round_trip_has_only_three_logs_and_latest_report() {
        let root = fixture();
        let logs = root.join("logs");
        let destination = root.join("exports");
        let payload = "完整日志：测试\n".repeat(16000);
        for name in ["client.log", "injector.log", "hook.log"] {
            fs::write(logs.join(name), &payload).unwrap();
        }
        fs::write(logs.join("settings.json"), "must not export").unwrap();
        fs::write(logs.join("client.previous.log"), "outside requested scope").unwrap();
        // An active log writer must not prevent the snapshot from being packaged.
        let _writer = OpenOptions::new()
            .append(true)
            .open(logs.join("client.log"))
            .unwrap();
        let first = bundle_to(&destination, &logs, &root.join("report.txt")).unwrap();
        let first_bytes = fs::read(&first.path).unwrap();
        let second = bundle_to(&destination, &logs, &root.join("report.txt")).unwrap();
        assert_ne!(first.path, second.path);
        assert!(first.missing_logs.is_empty());
        assert_eq!(first_bytes, fs::read(&first.path).unwrap());
        assert!(first_bytes.len() < payload.len());
        let extracted = root.join("extracted");
        extract(&first.path, &extracted);
        assert_eq!(fs::read_dir(&extracted).unwrap().count(), 4);
        for name in ["client.log", "injector.log", "hook.log"] {
            assert_eq!(fs::read(extracted.join(name)).unwrap(), payload.as_bytes());
            assert_eq!(fs::read(logs.join(name)).unwrap(), payload.as_bytes());
        }
        assert_eq!(
            fs::read_to_string(extracted.join("diagnostics.txt")).unwrap(),
            "最新诊断\n"
        );
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 2);
        drop(_writer);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_logs_are_reported_and_read_failures_leave_no_archive() {
        let root = fixture();
        let logs = root.join("logs");
        let destination = root.join("exports");
        let bundle = bundle_to(&destination, &logs, &root.join("report.txt")).unwrap();
        assert_eq!(
            bundle.missing_logs,
            ["client.log", "injector.log", "hook.log"]
        );
        let extracted = root.join("extracted");
        extract(&bundle.path, &extracted);
        assert_eq!(fs::read_dir(&extracted).unwrap().count(), 2);
        let missing = fs::read_to_string(extracted.join("missing-logs.txt")).unwrap();
        for name in &bundle.missing_logs {
            assert!(missing.contains(name));
        }
        // A read error is not silently treated as a missing log.
        fs::create_dir(logs.join("hook.log")).unwrap();
        assert!(bundle_to(&destination, &logs, &root.join("report.txt")).is_err());
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 1);
        fs::remove_dir_all(root).unwrap();
    }
}
