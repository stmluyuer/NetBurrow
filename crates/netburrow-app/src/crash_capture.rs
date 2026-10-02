//! Client-owned background capture. Only the script attaches to the game;
//! cancellation asks it to detach, never kills the debugger or target process.
use serde::Deserialize;
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
const CAPTURE_SCRIPT: &[u8] = include_bytes!("../../../scripts/Capture-Crash.ps1");

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum State {
    #[default]
    Disabled,
    Preparing,
    Waiting,
    Attaching,
    Monitoring,
    Captured,
    Failed,
    Stopping,
    Stopped,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub(super) struct Status {
    pub state: State,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub capture_path: Option<PathBuf>,
}

impl Status {
    /// The capture worker reports stable state codes; retain its original detail
    /// for troubleshooting without making the UI depend on script wording.
    pub fn display_message(&self) -> &'static str {
        use netburrow_core::text;
        match self.state {
            State::Disabled => text!("连接后开始监视", "Monitoring starts when connected"),
            State::Preparing => text!("正在准备监视，首次使用可能需要下载", "Preparing monitoring; first use may require a download"),
            State::Waiting => text!("等待游戏启动", "Waiting for the game"),
            State::Attaching => text!("正在接入游戏…", "Attaching to the game…"),
            State::Monitoring => text!("正在监视游戏", "Monitoring the game"),
            State::Captured => text!("崩溃记录已保存", "Crash capture saved"),
            State::Failed => text!("监视失败，请查看详情后重试", "Monitoring failed. Check the details and retry"),
            State::Stopping => text!("正在停止监视…", "Stopping monitoring…"),
            State::Stopped => text!("监视已停止", "Monitoring stopped"),
        }
    }

    fn message(state: State, message: impl Into<String>) -> Self {
        Self {
            state,
            message: message.into(),
            capture_path: None,
        }
    }
}

struct Running {
    game_path: String,
    stop: Arc<AtomicBool>,
    updates: Receiver<Status>,
    worker: JoinHandle<()>,
}

#[derive(Default)]
pub(super) struct Capture {
    running: Option<Running>,
    attempted: Option<String>,
    pub status: Status,
    pub latest_capture: Option<PathBuf>,
}

impl Capture {
    /// Returns a newly completed capture once, so the UI can enqueue its ZIP.
    pub fn sync(&mut self, wanted: Option<&str>) -> Option<PathBuf> {
        let mut captured = None;
        let finished = self
            .running
            .as_ref()
            .is_some_and(|run| run.worker.is_finished());
        if let Some(running) = &self.running {
            while let Ok(update) = running.updates.try_recv() {
                if let Some(path) = &update.capture_path {
                    if self.latest_capture.as_ref() != Some(path) {
                        self.latest_capture = Some(path.clone());
                        captured = Some(path.clone());
                    }
                }
                self.status = update;
            }
            // When finished was true before draining, every final update was
            // already queued. Otherwise retain the receiver for the next tick.
            if finished {
                let running = self.running.take().unwrap();
                if running.worker.join().is_err() {
                    self.status = Status::message(State::Failed, netburrow_core::text!("自动记录中断，请重试", "Crash recording interrupted. Try again"));
                }
            }
        }
        if self
            .running
            .as_ref()
            .is_some_and(|run| wanted != Some(run.game_path.as_str()))
        {
            self.stop();
        }
        if wanted.is_none() {
            self.attempted = None;
            if self.running.is_none() {
                self.status = Status::message(State::Disabled, netburrow_core::text!("启用联机后开始监视", "Monitoring starts when connected"));
            }
        } else if let Some(path) = wanted {
            if self.running.is_none() && self.attempted.as_deref() != Some(path) {
                self.start(path.to_owned());
            }
        }
        captured
    }

    fn start(&mut self, game_path: String) {
        self.attempted = Some(game_path.clone());
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let (sender, updates) = mpsc::channel();
        let path = game_path.clone();
        match thread::Builder::new()
            .name("crash-capture".into())
            .spawn(move || {
                if let Err(error) = launch(&path, worker_stop, sender.clone()) {
                    let _ = sender.send(Status::message(
                        State::Failed,
                        netburrow_core::text_format!("无法自动记录：{error}", "Cannot record crashes: {error}"),
                    ));
                }
            }) {
            Ok(worker) => {
                self.status =
                    Status::message(State::Preparing, netburrow_core::text!("正在准备监视，首次使用可能需要下载", "Preparing monitoring; a download may be needed on first use"));
                self.running = Some(Running {
                    game_path,
                    stop,
                    updates,
                    worker,
                });
            }
            Err(error) => {
                self.status = Status::message(State::Failed, netburrow_core::text_format!("无法启动监视：{error}", "Cannot start monitoring: {error}"))
            }
        }
    }

    pub fn stop(&mut self) {
        if let Some(run) = &self.running {
            if !run.stop.swap(true, Ordering::AcqRel) {
                self.status = Status::message(State::Stopping, netburrow_core::text!("正在停止监视…", "Stopping monitoring…"));
            }
        }
        self.attempted = None;
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    pub fn retry(&mut self) {
        self.stop();
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // Normal shutdown signals cancellation. If the app exits before the
        // worker can publish it, the script also watches a pinned parent handle.
        self.stop();
    }
}

#[cfg(windows)]
fn launch(game_path: &str, stop: Arc<AtomicBool>, updates: Sender<Status>) -> io::Result<()> {
    let executable = std::env::current_exe()?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let root = netburrow_core::config_directory().join("crash-monitor");
    fs::create_dir_all(&root)?;
    let control = root.join(format!("{}-{stamp}", std::process::id()));
    fs::create_dir(&control)?;
    let child = spawn_worker(game_path, &control, executable.parent().unwrap())?;
    supervise(child, &control, stop, updates)
}

#[cfg(windows)]
fn spawn_worker(game_path: &str, control: &Path, package_directory: &Path) -> io::Result<Child> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::Threading::{GetCurrentProcess, GetProcessTimes},
    };
    // Materialize the exact embedded version in this session's private working
    // directory. The installation directory may be read-only or contain old files.
    // include_bytes preserves the BOM required by Windows PowerShell 5.1.
    let script = control.join("Capture-Crash.ps1");
    fs::write(&script, CAPTURE_SCRIPT)?;
    let mut created = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut exited, mut kernel, mut user) = (created, created, created);
    if unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let parent_created =
        (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    let log = fs::File::create(control.join("worker.log"))?;
    let system = std::env::var_os("SystemRoot")
        .ok_or_else(|| io::Error::other(netburrow_core::text!("找不到 Windows 系统目录", "Windows system folder not found")))?;
    let powershell = PathBuf::from(system).join("System32/WindowsPowerShell/v1.0/powershell.exe");
    Command::new(powershell)
        .env_remove("PSModulePath")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(script)
        .args(["-Managed", "-AcceptEula", "-GamePath"])
        .arg(game_path)
        .arg("-PackageDirectory")
        .arg(package_directory)
        .arg("-ControlDirectory")
        .arg(control)
        .arg("-ParentPid")
        .arg(std::process::id().to_string())
        .arg("-ParentCreated")
        .arg(parent_created.to_string())
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .creation_flags(0x08000000)
        .spawn()
}

#[cfg(not(windows))]
fn launch(_: &str, _: Arc<AtomicBool>, _: Sender<Status>) -> io::Result<()> {
    Err(io::Error::other(netburrow_core::text!("自动记录仅支持 Windows", "Crash recording requires Windows")))
}

fn read_status(control: &Path) -> io::Result<Status> {
    let mut bytes = Vec::new();
    fs::File::open(control.join("status.json"))?
        .take(64 * 1024)
        .read_to_end(&mut bytes)?;
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
    serde_json::from_slice(bytes).map_err(io::Error::other)
}

fn supervise(
    mut child: Child,
    control: &Path,
    stop: Arc<AtomicBool>,
    updates: Sender<Status>,
) -> io::Result<()> {
    let mut previous = None;
    let mut cancelled = false;
    loop {
        if stop.load(Ordering::Acquire) && !cancelled {
            match fs::write(control.join("stop"), b"") {
                Ok(()) => cancelled = true,
                Err(error) => {
                    let _ = updates.send(Status::message(
                        State::Failed,
                        netburrow_core::text_format!("无法请求停止监视：{error}", "Cannot request monitoring to stop: {error}"),
                    ));
                }
            }
        }
        // An atomic file replacement can be momentarily unavailable. Retain
        // the last known state; a worker exit is checked independently below.
        if let Ok(status) = read_status(control) {
            if previous.as_ref() != Some(&status) {
                previous = Some(status.clone());
                let _ = updates.send(status);
            }
        }
        if let Some(exit) = child.try_wait()? {
            // Re-read after process exit so its final publication cannot race
            // the polling interval, especially capture -> cancelled/exit.
            if let Ok(status) = read_status(control) {
                if previous.as_ref() != Some(&status) {
                    previous = Some(status.clone());
                    let _ = updates.send(status);
                }
            }
            if !exit.success() && !previous.as_ref().is_some_and(|s| s.state == State::Failed) {
                let _ = updates.send(Status::message(
                    State::Failed,
                    netburrow_core::text_format!("监视已结束（{exit}），日志：{}", "Monitor exited ({exit}). Log: {}",
                        control.join("worker.log").display()
                    ),
                ));
            } else if !cancelled && !previous.as_ref().is_some_and(|s| s.state == State::Failed) {
                let _ = updates.send(Status::message(
                    State::Failed,
                    netburrow_core::text!("监视中断，请重新监视", "Monitoring interrupted. Restart monitoring"),
                ));
            }
            // Keep error details for diagnosis; successful cancellation needs
            // no persistent control files. Never recursively delete a path.
            if exit.success() && cancelled {
                for name in [
                    "status.json",
                    "status.tmp",
                    "stop",
                    "worker.log",
                    "Capture-Crash.ps1",
                ] {
                    let _ = fs::remove_file(control.join(name));
                }
                let _ = fs::remove_dir(control);
            }
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::os::windows::process::CommandExt;

    #[test]
    fn embedded_worker_starts_and_stops_without_adjacent_script() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("netburrow 内置采集 {}-{stamp}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let control = root.join("control");
        fs::create_dir(&control).unwrap();
        // Cancel before path resolution/download: exercise the real embedded
        // PowerShell worker and launch arguments without attaching to any game.
        fs::write(control.join("stop"), b"").unwrap();
        let child = spawn_worker("unused", &control, &root).unwrap();
        let (tx, rx) = mpsc::channel();
        supervise(child, &control, Arc::new(AtomicBool::new(true)), tx).unwrap();
        let statuses: Vec<_> = rx.try_iter().collect();
        assert!(
            !statuses.iter().any(|status| status.state == State::Failed),
            "{statuses:?}"
        );
        assert_eq!(statuses.last().unwrap().state, State::Stopped);
        assert!(
            !control.exists(),
            "session script and control files must be removed"
        );
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn cancellation_keeps_failure_and_delivers_final_capture_once() {
        let (tx, updates) = mpsc::channel();
        let (finish, done) = mpsc::channel();
        let worker = thread::spawn(move || {
            let _ = done.recv();
        });
        let mut capture = Capture::default();
        capture.running = Some(Running {
            game_path: "fixture".into(),
            stop: Arc::new(AtomicBool::new(false)),
            updates,
            worker,
        });
        capture.sync(None);
        tx.send(Status::message(State::Failed, "detach still pending"))
            .unwrap();
        capture.sync(None);
        assert_eq!(capture.status.state, State::Failed);
        assert_eq!(capture.status.message, "detach still pending");
        let path = PathBuf::from("completed-fixture");
        tx.send(Status {
            state: State::Stopped,
            message: String::new(),
            capture_path: Some(path.clone()),
        })
        .unwrap();
        finish.send(()).unwrap();
        while !capture.running.as_ref().unwrap().worker.is_finished() {
            thread::yield_now();
        }
        assert_eq!(capture.sync(None), Some(path));
        assert!(!capture.is_running());
        assert_eq!(capture.sync(None), None);
    }

    #[test]
    fn background_supervisor_forwards_status_and_requests_graceful_stop() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("netburrow-monitor-{}-{stamp}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let control = root.join("control");
        fs::create_dir(&control).unwrap();
        let script = root.join("fixture.ps1");
        fs::write(&script, br#"param([string]$Control)
$ErrorActionPreference = 'Stop'
Set-Content -LiteralPath (Join-Path $Control 'status.json') -Value '{"state":"monitoring","message":"ready"}' -Encoding UTF8
while (-not (Test-Path -LiteralPath (Join-Path $Control 'stop'))) { Start-Sleep -Milliseconds 50 }
Set-Content -LiteralPath (Join-Path $Control 'status.json') -Value '{"state":"stopped","message":"detached"}' -Encoding UTF8
"#).unwrap();
        let child = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-File"])
            .arg(&script)
            .arg("-Control")
            .arg(&control)
            .creation_flags(0x08000000)
            .spawn()
            .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || supervise(child, &control, worker_stop, tx));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(10)).unwrap().state,
            State::Monitoring
        );
        stop.store(true, Ordering::Release);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(10)).unwrap().state,
            State::Stopped
        );
        worker.join().unwrap().unwrap();
        fs::remove_file(script).unwrap();
        fs::remove_dir(root).unwrap();
    }
}
