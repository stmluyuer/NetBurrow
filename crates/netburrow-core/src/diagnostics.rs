//! Small local, bounded diagnostic log. Call only at lifecycle boundaries, never in game calls.
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};
static COMPONENT: OnceLock<&'static str> = OnceLock::new();
static WRITE_LOCK: Mutex<()> = Mutex::new(());
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);
const LIMIT: u64 = 2 * 1024 * 1024;

pub fn directory() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("NetBurrow")
        .join("logs")
}
pub fn init(component: &'static str) {
    let _ = COMPONENT.set(component);
    record(
        "INFO",
        "session",
        &format!(
            "started version={} pid={}",
            env!("CARGO_PKG_VERSION"),
            std::process::id()
        ),
    );
}
pub fn last_error() -> Option<String> {
    LAST_ERROR.lock().unwrap_or_else(|p| p.into_inner()).clone()
}
pub fn record(level: &str, event: &str, message: &str) {
    record_batch(level, std::iter::once((event, message)));
}

/// Append a bounded caller-owned snapshot with one file open, retaining per-line cleaning.
pub fn record_batch<'a>(level: &str, records: impl IntoIterator<Item = (&'a str, &'a str)>) {
    let Some(component) = COMPONENT.get() else {
        return;
    };
    let _lock = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let stamp = timestamp();
    let mut lines = String::new();
    for (event, message) in records {
        lines.push_str(&format!(
            "{stamp} [{level}] pid={} {event}: {}\n",
            std::process::id(),
            clean(message)
        ));
    }
    let result = append(
        &directory().join(format!("{component}.log")),
        lines.as_bytes(),
        LIMIT,
    );
    if let Err(error) = result {
        *LAST_ERROR.lock().unwrap_or_else(|p| p.into_inner()) =
            Some(error.to_string());
    }
}
#[cfg(windows)]
pub(crate) fn network_history(lines: &[String]) {
    // A separate bounded file keeps per-second evidence out of the verbose client log.
    let _lock = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let stamp = timestamp();
    let mut text = String::new();
    for line in lines {
        text.push_str(&format!("{stamp} pid={} {}\n", std::process::id(), clean(line)));
    }
    if let Err(error) = append(&directory().join("network.log"), text.as_bytes(), 8 * 1024 * 1024) {
        *LAST_ERROR.lock().unwrap_or_else(|p| p.into_inner()) =
            Some(format!("Cannot write network log: {error}"));
    }
}
fn append(path: &Path, bytes: &[u8], limit: u64) -> io::Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    if fs::metadata(path).map(|m| m.len()).unwrap_or(0) + bytes.len() as u64 > limit {
        let previous = path.with_extension("previous.log");
        if previous.exists() {
            fs::remove_file(&previous)?;
        }
        fs::rename(path, previous)?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(bytes)
}
fn clean(text: &str) -> String {
    let mut text = text.replace(['\r', '\n', '\t'], " ");
    // Defense in depth: callers must never pass tokens, packet payloads or settings structs.
    while let Some(start) = text.find("NB1-") {
        let end = text[start..]
            .find(char::is_whitespace)
            .map_or(text.len(), |n| start + n);
        text.replace_range(start..end, "[group-redacted]");
    }
    for variable in ["USERPROFILE", "LOCALAPPDATA"] {
        if let Ok(home) = std::env::var(variable) {
            if !home.is_empty() {
                text = text.replace(&home, "[local-user]");
            }
        }
    }
    text.chars().take(2048).collect()
}
fn timestamp() -> String {
    #[cfg(windows)]
    {
        let mut now = unsafe { std::mem::zeroed() };
        unsafe {
            windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut now);
        }
        return format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
            now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute, now.wSecond, now.wMilliseconds
        );
    }
    #[cfg(not(windows))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string()
    }
}
