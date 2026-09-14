#[cfg(windows)]
pub fn choose_game() -> Result<Option<String>, String> {
    use windows_sys::Win32::UI::{Controls::Dialogs::*, WindowsAndMessaging::GetForegroundWindow};
    let mut buffer = vec![0u16; 32768];
    let filter: Vec<u16> = "Isaac (isaac-ng.exe)\0isaac-ng.exe\0\0"
        .encode_utf16()
        .collect();
    let title: Vec<u16> = "选择 isaac-ng.exe".encode_utf16().chain(Some(0)).collect();
    let mut dialog: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    dialog.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    dialog.hwndOwner = unsafe { GetForegroundWindow() };
    dialog.lpstrFilter = filter.as_ptr();
    dialog.lpstrFile = buffer.as_mut_ptr();
    dialog.nMaxFile = buffer.len() as u32;
    dialog.lpstrTitle = title.as_ptr();
    dialog.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_EXPLORER;
    if unsafe { GetOpenFileNameW(&mut dialog) } != 0 {
        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        let path = String::from_utf16(&buffer[..end]).map_err(|_| "无法读取所选文件路径")?;
        if !std::path::Path::new(&path)
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case("isaac-ng.exe"))
        {
            return Err("请选择 isaac-ng.exe".into());
        }
        Ok(Some(path))
    } else {
        let error = unsafe { CommDlgExtendedError() };
        if error == 0 {
            Ok(None)
        } else {
            Err(format!("无法打开文件选择器：{error}"))
        }
    }
}
#[cfg(not(windows))]
pub fn choose_game() -> Result<Option<String>, String> {
    Err("文件选择需要 Windows；也可以直接输入路径。".into())
}
