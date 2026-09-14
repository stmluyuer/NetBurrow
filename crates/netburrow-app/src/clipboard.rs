//! Minimal Windows clipboard bridge for the group paste button.

#[cfg(windows)]
pub fn read_text() -> Result<String, String> {
    use std::slice;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};

    const CF_UNICODETEXT: u32 = 13;

    // Clipboard ownership is transient. A short single attempt gives the user a useful
    // error instead of blocking the UI thread when another program owns it.
    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT) == 0 {
            return Err("剪贴板中没有可粘贴的文字".to_owned());
        }
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("无法读取剪贴板，请稍后重试".to_owned());
        }

        let handle = GetClipboardData(CF_UNICODETEXT);
        if handle.is_null() {
            let _ = CloseClipboard();
            return Err("剪贴板文字不可用".to_owned());
        }
        let pointer = GlobalLock(handle);
        if pointer.is_null() {
            let _ = CloseClipboard();
            return Err("无法读取剪贴板文字".to_owned());
        }

        let mut length = 0usize;
        let words = pointer.cast::<u16>();
        while *words.add(length) != 0 {
            length += 1;
        }
        let text = String::from_utf16_lossy(slice::from_raw_parts(words, length));
        let _ = GlobalUnlock(handle);
        let _ = CloseClipboard();
        Ok(text.trim().to_owned())
    }
}

#[cfg(not(windows))]
pub fn read_text() -> Result<String, String> {
    Err("当前平台不支持系统剪贴板".to_owned())
}
