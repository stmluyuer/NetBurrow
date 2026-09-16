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

fn compress(source: &Path, destination: &Path) -> io::Result<()> {
    let output = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(destination)?;
    let mut archive = zip::ZipWriter::new(output);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| io::Error::other("无法读取日志文件名"))?;
        archive.start_file(name, options)?;
        io::copy(&mut File::open(entry.path())?, &mut archive)?;
    }
    // Finalization writes the ZIP directory and must succeed before revealing the file.
    archive.finish()?.sync_all()?;
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

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
        let mut archive = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
        for index in 0..archive.len() {
            assert_eq!(
                archive.by_index(index).unwrap().compression(),
                zip::CompressionMethod::Deflated
            );
        }
        archive.extract(destination).unwrap();
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
