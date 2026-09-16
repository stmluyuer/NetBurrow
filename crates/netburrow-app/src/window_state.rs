use netburrow_core::WindowPlacement;

fn valid(placement: &WindowPlacement) -> bool {
    let [left, top, right, bottom] = placement.normal;
    let width = i64::from(right) - i64::from(left);
    let height = i64::from(bottom) - i64::from(top);
    (100..=32_000).contains(&width) && (100..=32_000).contains(&height)
}

#[cfg(windows)]
mod native {
    use super::*;
    use windows_sys::Win32::{
        Foundation::{HWND, POINT, RECT},
        System::Threading::GetCurrentProcessId,
        UI::WindowsAndMessaging::{
            FindWindowW, GetWindowPlacement, GetWindowThreadProcessId, SW_MINIMIZE,
            SW_SHOWMAXIMIZED, SW_SHOWMINIMIZED, SW_SHOWMINNOACTIVE, SW_SHOWNORMAL,
            SetWindowPlacement, WINDOWPLACEMENT, WPF_RESTORETOMAXIMIZED,
        },
    };

    fn window() -> Option<HWND> {
        let title: Vec<u16> = super::super::WINDOW_TITLE
            .encode_utf16()
            .chain(Some(0))
            .collect();
        unsafe {
            let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
            let mut pid = 0;
            if hwnd.is_null() {
                return None;
            }
            GetWindowThreadProcessId(hwnd, &mut pid);
            (pid == GetCurrentProcessId()).then_some(hwnd)
        }
    }

    pub fn capture() -> Option<WindowPlacement> {
        capture_from(window()?)
    }

    fn capture_from(hwnd: HWND) -> Option<WindowPlacement> {
        let mut placement: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
        placement.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
        if unsafe { GetWindowPlacement(hwnd, &mut placement) } == 0 {
            return None;
        }
        let rect = placement.rcNormalPosition;
        let minimized = [SW_SHOWMINIMIZED, SW_SHOWMINNOACTIVE, SW_MINIMIZE]
            .contains(&(placement.showCmd as i32));
        let saved = WindowPlacement {
            normal: [rect.left, rect.top, rect.right, rect.bottom],
            maximized: placement.showCmd == SW_SHOWMAXIMIZED as u32
                || (minimized && placement.flags & WPF_RESTORETOMAXIMIZED != 0),
        };
        valid(&saved).then_some(saved)
    }

    pub fn restore(saved: &WindowPlacement) -> Result<(), String> {
        if !valid(saved) {
            return Err("保存的窗口尺寸无效，已使用默认窗口。".into());
        }
        let hwnd = window().ok_or("暂时无法读取窗口，已使用默认窗口位置。")?;
        let placement = native_placement(saved);
        // Keep workspace coordinates end-to-end. Windows relocates a saved window
        // that is off-screen after a resolution or monitor configuration change.
        if unsafe { SetWindowPlacement(hwnd, &placement) } == 0 {
            return Err(format!(
                "无法恢复窗口位置：{}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    fn native_placement(saved: &WindowPlacement) -> WINDOWPLACEMENT {
        let [left, top, right, bottom] = saved.normal;
        WINDOWPLACEMENT {
            length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
            flags: 0,
            showCmd: if saved.maximized {
                SW_SHOWMAXIMIZED
            } else {
                SW_SHOWNORMAL
            } as u32,
            ptMinPosition: POINT { x: -1, y: -1 },
            ptMaxPosition: POINT { x: -1, y: -1 },
            rcNormalPosition: RECT {
                left,
                top,
                right,
                bottom,
            },
        }
    }

}

#[cfg(windows)]
pub use native::{capture, restore};

#[cfg(not(windows))]
pub fn capture() -> Option<WindowPlacement> {
    None
}
#[cfg(not(windows))]
pub fn restore(_: &WindowPlacement) -> Result<(), String> {
    Ok(())
}
