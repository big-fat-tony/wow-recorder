//! Find the WoW game window and its client size (the "raw" capture resolution).

/// The WoW window, by class with the title as a fallback.
#[cfg(target_os = "windows")]
fn find_wow_window() -> Option<windows::Win32::Foundation::HWND> {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;

    unsafe {
        let hwnd = FindWindowW(&HSTRING::from(crate::recorder::WOW_WINDOW_CLASS), None)
            .or_else(|_| FindWindowW(None, &HSTRING::from(crate::recorder::WOW_WINDOW_TITLE)))
            .ok()?;
        (!hwnd.0.is_null()).then_some(hwnd)
    }
}

/// Is WoW running? True for a minimized window too — unlike
/// [`detect_wow_resolution`], which measures a client area that a minimized
/// window reports as empty.
#[cfg(target_os = "windows")]
pub fn wow_is_running() -> bool {
    find_wow_window().is_some()
}

#[cfg(not(target_os = "windows"))]
pub fn wow_is_running() -> bool {
    false
}

#[cfg(target_os = "windows")]
pub fn detect_wow_resolution() -> Option<(u32, u32)> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

    unsafe {
        let hwnd = find_wow_window()?;
        let mut rect = RECT::default();
        GetClientRect(hwnd, &mut rect).ok()?;
        let w = (rect.right - rect.left) as u32;
        let h = (rect.bottom - rect.top) as u32;
        (w > 0 && h > 0).then_some((w, h))
    }
}

#[cfg(not(target_os = "windows"))]
pub fn detect_wow_resolution() -> Option<(u32, u32)> {
    None
}
