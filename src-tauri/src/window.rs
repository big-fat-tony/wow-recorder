//! Detect the WoW game window's client size (the "raw" capture resolution).

#[cfg(target_os = "windows")]
pub fn detect_wow_resolution() -> Option<(u32, u32)> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, GetClientRect};

    unsafe {
        // Try the WoW window class first, then fall back to the title.
        let hwnd = FindWindowW(&HSTRING::from(crate::recorder::WOW_WINDOW_CLASS), None)
            .or_else(|_| FindWindowW(None, &HSTRING::from(crate::recorder::WOW_WINDOW_TITLE)))
            .ok()?;
        if hwnd.0.is_null() {
            return None;
        }
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
