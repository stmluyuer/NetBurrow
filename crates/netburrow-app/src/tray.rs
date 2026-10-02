//! Windows notification-area integration. The tray window only owns the icon and menu;
//! it signals the egui window through the thread-safe context.

use std::sync::{Arc, OnceLock};

use eframe::egui;

#[cfg(windows)]
static TRAY_WINDOW: std::sync::atomic::AtomicPtr<std::ffi::c_void> = std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

pub fn notify(notification: &crate::notifications::Notification) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Shell::{NIF_INFO, NIF_REALTIME, NIIF_INFO, NIIF_WARNING, NIIF_NOSOUND, NIIF_RESPECT_QUIET_TIME, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW};
        let hwnd = TRAY_WINDOW.load(std::sync::atomic::Ordering::Acquire);
        if hwnd.is_null() { return false; }
        let mut icon: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        icon.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        icon.hWnd = hwnd;
        icon.uID = 1;
        icon.uFlags = NIF_INFO | NIF_REALTIME;
        icon.dwInfoFlags = (if notification.warning { NIIF_WARNING } else { NIIF_INFO }) | NIIF_NOSOUND | NIIF_RESPECT_QUIET_TIME;
        for (target, value) in icon.szInfoTitle.iter_mut().take(63).zip(notification.title.encode_utf16()) { *target = value; }
        for (target, value) in icon.szInfo.iter_mut().take(255).zip(notification.body.encode_utf16()) { *target = value; }
        return unsafe { Shell_NotifyIconW(NIM_MODIFY, &icon) != 0 };
    }
    #[cfg(not(windows))]
    { let _ = notification; false }
}

pub fn dismiss_notification() {
    let _ = notify(&crate::notifications::Notification { title: "", body: "", warning: false });
}

pub fn shutdown() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{SendMessageW, WM_CLOSE};
        let hwnd = TRAY_WINDOW.swap(std::ptr::null_mut(), std::sync::atomic::Ordering::AcqRel);
        if !hwnd.is_null() {
            use windows_sys::Win32::UI::Shell::{NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW};
            let mut icon: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
            icon.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            icon.hWnd = hwnd;
            icon.uID = 1;
            unsafe { Shell_NotifyIconW(NIM_DELETE, &icon); SendMessageW(hwnd, WM_CLOSE, 0, 0); }
        }
    }
}

static ACTIONS: OnceLock<TrayActions> = OnceLock::new();

#[derive(Clone)]
struct TrayActions {
    context: egui::Context,
    stop_requested: Arc<std::sync::atomic::AtomicBool>,
    quit_requested: Arc<std::sync::atomic::AtomicBool>,
}

pub fn install(
    context: egui::Context,
    stop_requested: Arc<std::sync::atomic::AtomicBool>,
    quit_requested: Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), String> {
    let actions = TrayActions {
        context,
        stop_requested,
        quit_requested,
    };
    ACTIONS
        .set(actions)
        .map_err(|_| netburrow_core::text!("托盘已初始化", "Tray is already initialized").to_owned())?;

    std::thread::Builder::new()
        .name("netburrow-tray".to_owned())
        .spawn(run)
        .map_err(|error| netburrow_core::text_format!("无法启动托盘：{error}", "Could not start system tray: {error}"))?;
    Ok(())
}

#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

#[cfg(windows)]
fn run() {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Shell::{
        NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIN_BALLOONUSERCLICK, NOTIFYICONDATAW, Shell_NotifyIconW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
        DispatchMessageW, GetCursorPos, GetMessageW, LoadIconW, MF_SEPARATOR, MF_STRING,
        PostQuitMessage, RegisterClassW, SetForegroundWindow, TPM_BOTTOMALIGN, TPM_LEFTALIGN,
        TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP, WM_COMMAND, WM_DESTROY,
        WM_LBUTTONUP, WM_RBUTTONUP, WNDCLASSW,
    };

    const TRAY_MESSAGE: u32 = WM_APP + 42;
    const SHOW_WINDOW: usize = 1;
    const STOP_NETWORKING: usize = 2;
    const EXIT_APP: usize = 3;

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            TRAY_MESSAGE if lparam as u32 == WM_LBUTTONUP || lparam as u32 == NIN_BALLOONUSERCLICK => {
                show_window();
                0
            }
            TRAY_MESSAGE if lparam as u32 == WM_RBUTTONUP => {
                show_menu(hwnd);
                0
            }
            WM_COMMAND => {
                match wparam & 0xffff {
                    SHOW_WINDOW => show_window(),
                    STOP_NETWORKING => {
                        if let Some(actions) = ACTIONS.get() {
                            actions
                                .stop_requested
                                .store(true, std::sync::atomic::Ordering::Release);
                            show_window();
                        }
                    }
                    EXIT_APP => {
                        if let Some(actions) = ACTIONS.get() {
                            actions
                                .quit_requested
                                .store(true, std::sync::atomic::Ordering::Release);
                            actions
                                .context
                                .send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    }
                    _ => {}
                }
                0
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                0
            }
            _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
        }
    }

    fn show_window() {
        if let Some(actions) = ACTIONS.get() {
            actions
                .context
                .send_viewport_cmd(egui::ViewportCommand::Visible(true));
            actions
                .context
                .send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            actions
                .context
                .send_viewport_cmd(egui::ViewportCommand::Focus);
        }
    }

    fn show_menu(hwnd: HWND) {
        unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return;
            }
            let show = wide(netburrow_core::text!("显示 NetBurrow", "Show NetBurrow"));
            let stop = wide(netburrow_core::text!("断开", "Disconnect"));
            let exit = wide(netburrow_core::text!("退出", "Quit"));
            let _ = AppendMenuW(menu, MF_STRING, SHOW_WINDOW, show.as_ptr());
            let _ = AppendMenuW(menu, MF_STRING, STOP_NETWORKING, stop.as_ptr());
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            let _ = AppendMenuW(menu, MF_STRING, EXIT_APP, exit.as_ptr());
            let mut point: POINT = zeroed();
            let _ = GetCursorPos(&mut point);
            let _ = SetForegroundWindow(hwnd);
            let _ = TrackPopupMenu(
                menu,
                TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                0,
                hwnd,
                std::ptr::null(),
            );
            let _ = DestroyMenu(menu);
        }
    }

    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class_name = wide("NetBurrowTrayWindow");
        let window_class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class_name.as_ptr(),
            ..zeroed()
        };
        let _ = RegisterClassW(&window_class);
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return;
        }

        let mut icon: NOTIFYICONDATAW = zeroed();
        icon.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        icon.hWnd = hwnd;
        icon.uID = 1;
        icon.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        icon.uCallbackMessage = TRAY_MESSAGE;
        icon.hIcon = LoadIconW(instance, 1usize as *const u16);
        let tip = wide(netburrow_core::text!("NetBurrow 正在后台运行", "NetBurrow is running in the background"));
        for (destination, source) in icon.szTip.iter_mut().zip(tip.iter()) {
            *destination = *source;
        }
        if Shell_NotifyIconW(NIM_ADD, &icon) != 0 {
            TRAY_WINDOW.store(hwnd, std::sync::atomic::Ordering::Release);
        }

        let mut message = zeroed();
        while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
            let _ = TranslateMessage(&message);
            let _ = DispatchMessageW(&message);
        }
        let _ = Shell_NotifyIconW(NIM_DELETE, &icon);
        TRAY_WINDOW.store(std::ptr::null_mut(), std::sync::atomic::Ordering::Release);
    }
}

#[cfg(not(windows))]
fn run() {}
