//! Своя шапка окна (п. 14 плана, замечание 7): раскладки Snap у своей кнопки «Развернуть».
//!
//! У системной кнопки Windows 11 показывает раскладки при наведении сама: окно отвечает на
//! `WM_NCHITTEST` кодом `HTMAXBUTTON`. В окне без рамки всю клиентскую область занимает WebView2 и
//! проверку попадания делает он, а не окно, — системного наведения нет. Поэтому кнопка окна по
//! наведению зовёт `window_snap_layouts`, а та нажимает Win+Z: Windows открывает те же раскладки у
//! правого верхнего угла окна на переднем плане.
//!
//! Ничего не нажимается, если окно не в фокусе (Win+Z досталось бы чужому окну), если раскладки
//! при наведении выключены в Параметрах Windows (`EnableSnapAssistFlyout` = 0) и до Windows 11
//! (сборка ниже 22000: у Windows 10 нет раскладок по Win+Z).

#[cfg(windows)]
use tracing::warn;

/// Первая сборка Windows 11.
#[cfg(windows)]
const FIRST_WINDOWS_11_BUILD: u32 = 22000;

/// Открыть раскладки Snap окна; `true` — клавиши нажаты.
#[tauri::command]
pub(crate) fn window_snap_layouts(window: tauri::WebviewWindow) -> bool {
    #[cfg(windows)]
    {
        if !window.is_focused().unwrap_or(false) || !snap_flyout_enabled() {
            return false;
        }
        press_win_z()
    }
    #[cfg(not(windows))]
    {
        let _ = window;
        false
    }
}

/// Раскладки при наведении доступны: Windows 11 и не выключены в Параметрах (значения нет —
/// включены, как по умолчанию в Windows).
#[cfg(windows)]
fn snap_flyout_enabled() -> bool {
    use windows::core::w;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ};

    let mut build = [0u16; 16];
    let mut size = std::mem::size_of_val(&build) as u32;
    // SAFETY: буфер и его размер в байтах переданы вместе; строка читается только при успехе.
    let read = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"), w!("CurrentBuildNumber"), RRF_RT_REG_SZ, None, Some(build.as_mut_ptr().cast()), Some(&mut size)) };
    let build = if read == ERROR_SUCCESS { windows_build(&build[..size as usize / 2]) } else { 0 };
    if build < FIRST_WINDOWS_11_BUILD {
        return false;
    }

    let mut enabled = 1u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: DWORD читается в u32 того же размера.
    let read = unsafe { RegGetValueW(HKEY_CURRENT_USER, w!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced"), w!("EnableSnapAssistFlyout"), RRF_RT_REG_DWORD, None, Some((&mut enabled as *mut u32).cast()), Some(&mut size)) };
    read != ERROR_SUCCESS || enabled != 0
}

/// Номер сборки из строки реестра (UTF-16, с нулём в конце); не число — 0.
#[cfg(windows)]
fn windows_build(utf16: &[u16]) -> u32 {
    String::from_utf16_lossy(utf16).trim_end_matches('\0').trim().parse().unwrap_or(0)
}

/// Нажать и отпустить Win+Z.
#[cfg(windows)]
fn press_win_z() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_LWIN};

    const VK_Z: VIRTUAL_KEY = VIRTUAL_KEY(0x5A);
    let key = |vk: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    };
    let inputs = [key(VK_LWIN, KEYBD_EVENT_FLAGS(0)), key(VK_Z, KEYBD_EVENT_FLAGS(0)), key(VK_Z, KEYEVENTF_KEYUP), key(VK_LWIN, KEYEVENTF_KEYUP)];
    // SAFETY: массив INPUT и размер одной структуры — как требует SendInput.
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        warn!(sent, "раскладки Snap: Windows приняла не все нажатия Win+Z");
        return false;
    }
    true
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn build_number_from_registry_string() {
        let utf16 = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        assert_eq!(windows_build(&utf16("26300\0")), 26300);
        assert_eq!(windows_build(&utf16("19045")), 19045);
        assert_eq!(windows_build(&utf16("")), 0);
        assert_eq!(windows_build(&utf16("abc\0")), 0);
    }
}
