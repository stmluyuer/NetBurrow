//! Public release metadata only. Never downloads or installs executable files.
use std::{
    cmp::Ordering,
    sync::mpsc,
    time::{Duration, Instant},
};

pub const REPOSITORY: &str = "https://github.com/stmluyuer/NetBurrow-Releases";
pub const RELEASES: &str = "https://github.com/stmluyuer/NetBurrow-Releases/releases";
pub const ISSUES: &str = "https://github.com/stmluyuer/NetBurrow-Releases/issues/new";
const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_NOTES_CHARS: usize = 12_000;
const REQUEST_BUDGET: Duration = Duration::from_secs(20);

#[derive(Debug, serde::Deserialize)]
pub struct Release {
    pub version: String,
    pub notes: String,
}

impl Release {
    pub fn download_url(&self) -> String {
        format!(
            "{REPOSITORY}/releases/download/v{0}/NetBurrow-{0}-win-x64.zip",
            self.version
        )
    }

    pub fn page_url(&self) -> String {
        format!("{REPOSITORY}/releases/tag/v{}", self.version)
    }
}

#[derive(Debug)]
pub struct CheckedRelease {
    pub release: Release,
    /// Remote version compared to this executable's version.
    pub comparison: Ordering,
}

fn version_parts(version: &str) -> Result<[u64; 3], String> {
    let mut parts = version.split('.');
    let mut parsed = [0; 3];
    for value in &mut parsed {
        let part = parts.next().ok_or("版本号格式无效")?;
        if part.is_empty()
            || !part.bytes().all(|b| b.is_ascii_digit())
            || (part.len() > 1 && part.starts_with('0'))
        {
            return Err("版本号格式无效".into());
        }
        *value = part.parse().map_err(|_| "版本号数值超出范围")?;
    }
    if parts.next().is_some() {
        return Err("版本号格式无效".into());
    }
    Ok(parsed)
}

fn parse_manifest(bytes: &[u8], current: &str) -> Result<CheckedRelease, String> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err("更新清单超过大小限制".into());
    }
    let release: Release = serde_json::from_slice(bytes).map_err(|_| "更新清单格式无效")?;
    let comparison = version_parts(&release.version)?.cmp(&version_parts(current)?);
    if release.notes.trim().is_empty() || release.notes.chars().count() > MAX_NOTES_CHARS {
        return Err("更新说明为空或超过长度限制".into());
    }
    Ok(CheckedRelease {
        release,
        comparison,
    })
}

#[derive(Default)]
pub struct UpdateCheck {
    pending: Option<mpsc::Receiver<Result<CheckedRelease, String>>>,
    pub result: Option<Result<CheckedRelease, String>>,
}

impl UpdateCheck {
    pub fn is_checking(&self) -> bool {
        self.pending.is_some()
    }

    pub fn newer_release(&self) -> Option<&Release> {
        self.result
            .as_ref()?
            .as_ref()
            .ok()
            .filter(|checked| checked.comparison == Ordering::Greater)
            .map(|checked| &checked.release)
    }

    pub fn begin(&mut self) {
        self.begin_with(|| {
            let bytes = download_manifest()?;
            parse_manifest(&bytes, env!("CARGO_PKG_VERSION"))
        });
    }

    fn begin_with(
        &mut self,
        work: impl FnOnce() -> Result<CheckedRelease, String> + Send + 'static,
    ) {
        if self.is_checking() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        match std::thread::Builder::new()
            .name("netburrow-update-check".into())
            .spawn(move || {
                let _ = sender.send(work());
            }) {
            Ok(_) => self.pending = Some(receiver),
            Err(_) => self.result = Some(Err("无法启动更新检查任务".into())),
        }
    }

    pub fn poll(&mut self) {
        let Some(receiver) = &self.pending else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("更新检查任务未完成，请重试".into()),
        };
        self.pending = None;
        self.result = Some(result);
    }
}

fn require_success(status: u32) -> Result<(), String> {
    match status {
        200 => Ok(()),
        404 => Err("尚未找到公开更新清单，请稍后重试或查看发布页".into()),
        _ => Err(format!("更新服务器返回 HTTP {status}")),
    }
}

fn check_deadline(started: Instant) -> Result<(), String> {
    if started.elapsed() >= REQUEST_BUDGET {
        Err("更新检查超时，请稍后重试".into())
    } else {
        Ok(())
    }
}

fn read_body(
    mut read: impl FnMut(&mut [u8]) -> Result<usize, String>,
    started: Instant,
) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        check_deadline(started)?;
        let count = read(&mut chunk)?;
        check_deadline(started)?;
        if count == 0 {
            return Ok(bytes);
        }
        if count > chunk.len() || bytes.len() + count > MAX_MANIFEST_BYTES {
            return Err("更新清单超过大小限制".into());
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
}

#[cfg(windows)]
fn download_manifest() -> Result<Vec<u8>, String> {
    windows::download("/stmluyuer/NetBurrow-Releases/releases/latest/download/latest.json")
}

#[cfg(not(windows))]
fn download_manifest() -> Result<Vec<u8>, String> {
    Err("当前平台不支持更新检查，请查看发布页".into())
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{ffi::c_void, ptr};
    use windows_sys::Win32::{Foundation::GetLastError, Networking::WinHttp::*};

    struct Handle(*mut c_void);
    impl Handle {
        fn new(raw: *mut c_void) -> Result<Self, String> {
            if raw.is_null() {
                Err(last_error())
            } else {
                Ok(Self(raw))
            }
        }
        fn option(&self, option: u32, value: u32) -> Result<(), String> {
            // WinHTTP copies this DWORD during the synchronous call.
            succeeded(unsafe { WinHttpSetOption(self.0, option, (&value as *const u32).cast(), 4) })
        }
    }
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                WinHttpCloseHandle(self.0);
            }
        }
    }

    fn error_message(code: u32) -> String {
        match code {
            ERROR_WINHTTP_TIMEOUT => "更新检查超时，请稍后重试".into(),
            ERROR_WINHTTP_SECURE_FAILURE => {
                "无法验证更新服务器的安全连接，请检查系统时间和网络".into()
            }
            _ => format!("无法读取更新信息（Windows 错误码 {code}），请检查网络后重试"),
        }
    }
    fn last_error() -> String {
        error_message(unsafe { GetLastError() })
    }
    fn succeeded(result: i32) -> Result<(), String> {
        if result == 0 {
            Err(last_error())
        } else {
            Ok(())
        }
    }
    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    fn configure_request(request: &Handle) -> Result<(), String> {
        request.option(
            WINHTTP_OPTION_REDIRECT_POLICY,
            WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP,
        )?;
        request.option(WINHTTP_OPTION_MAX_HTTP_AUTOMATIC_REDIRECTS, 5)?;
        request.option(
            WINHTTP_OPTION_DISABLE_FEATURE,
            WINHTTP_DISABLE_COOKIES | WINHTTP_DISABLE_AUTHENTICATION,
        )?;
        Ok(())
    }

    pub(super) fn download(path: &str) -> Result<Vec<u8>, String> {
        let started = Instant::now();
        let agent = wide(concat!("NetBurrow/", env!("CARGO_PKG_VERSION")));
        let host = wide("github.com");
        let verb = wide("GET");
        let path = wide(path);
        // All handles stay on this worker and are dropped in reverse creation order.
        unsafe {
            let session = Handle::new(WinHttpOpen(
                agent.as_ptr(),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                ptr::null(),
                ptr::null(),
                0,
            ))?;
            succeeded(WinHttpSetTimeouts(session.0, 5000, 5000, 5000, 5000))?;
            let connection = Handle::new(WinHttpConnect(session.0, host.as_ptr(), 443, 0))?;
            let request = Handle::new(WinHttpOpenRequest(
                connection.0,
                verb.as_ptr(),
                path.as_ptr(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                WINHTTP_FLAG_SECURE,
            ))?;
            configure_request(&request)?;
            check_deadline(started)?;
            succeeded(WinHttpSendRequest(
                request.0,
                ptr::null(),
                0,
                ptr::null(),
                0,
                0,
                0,
            ))?;
            check_deadline(started)?;
            succeeded(WinHttpReceiveResponse(request.0, ptr::null_mut()))?;
            check_deadline(started)?;
            let mut status = 0u32;
            let mut length = 4u32;
            succeeded(WinHttpQueryHeaders(
                request.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                ptr::null(),
                (&mut status as *mut u32).cast(),
                &mut length,
                ptr::null_mut(),
            ))?;
            require_success(status)?;
            read_body(
                |chunk| {
                    let mut count = 0;
                    succeeded(WinHttpReadData(
                        request.0,
                        chunk.as_mut_ptr().cast(),
                        chunk.len() as u32,
                        &mut count,
                    ))?;
                    Ok(count as usize)
                },
                started,
            )
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(version: &str, notes: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"version": version, "notes": notes})).unwrap()
    }

    #[test]
    fn numeric_versions_and_fixed_download_links() {
        for (remote, expected) in [
            ("0.1.9", Ordering::Less),
            ("0.1.10", Ordering::Equal),
            ("0.1.11", Ordering::Greater),
            ("1.0.0", Ordering::Greater),
        ] {
            let checked = parse_manifest(&manifest(remote, "中文更新\n第二行"), "0.1.10").unwrap();
            assert_eq!(checked.comparison, expected);
            assert_eq!(checked.release.notes, "中文更新\n第二行");
            assert_eq!(
                checked.release.download_url(),
                format!("{REPOSITORY}/releases/download/v{remote}/NetBurrow-{remote}-win-x64.zip")
            );
            assert_eq!(
                checked.release.page_url(),
                format!("{REPOSITORY}/releases/tag/v{remote}")
            );
        }
        for version in [
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "v1.2.3",
            "1.2.3-beta",
            "1.2.3/evil",
            "1.2.-3",
            "1.2.18446744073709551616",
        ] {
            assert!(
                parse_manifest(&manifest(version, "说明"), "0.1.10").is_err(),
                "{version}"
            );
        }
    }

    #[test]
    fn response_errors_limits_and_timeouts_do_not_become_success() {
        use std::io::Read;
        assert!(require_success(200).is_ok());
        assert!(require_success(404).unwrap_err().contains("尚未找到"));
        assert!(require_success(500).is_err());
        for length in [0, MAX_MANIFEST_BYTES, MAX_MANIFEST_BYTES + 1] {
            let mut source = std::io::Cursor::new(vec![b'x'; length]);
            let result = read_body(
                |chunk| source.read(chunk).map_err(|e| e.to_string()),
                Instant::now(),
            );
            assert_eq!(result.is_ok(), length <= MAX_MANIFEST_BYTES);
        }
        assert!(
            read_body(
                |_| panic!("Expired request must not read"),
                Instant::now() - REQUEST_BUDGET
            )
            .is_err()
        );
        assert!(read_body(|_| Err("certificate failure".into()), Instant::now()).is_err());
    }

}
