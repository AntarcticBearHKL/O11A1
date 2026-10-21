//! Windows autostart through the per-user Run key.
//!
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` gets a value named
//! `bilibili-slint` whose data is the quoted current executable path. The
//! user's choice is persisted in settings.json and applied on toggle.

use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
};

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE_NAME: &str = "bilibili-slint";

fn wide(text: &str) -> Vec<u16> {
    let mut buffer: Vec<u16> = text.encode_utf16().collect();
    buffer.push(0);
    buffer
}

/// Add or remove the Run value; returns `true` on success.
pub fn set_enabled(enabled: bool) -> bool {
    let subkey = wide(RUN_KEY);
    let mut hkey: HKEY = std::ptr::null_mut();
    let status = unsafe {
        RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_SET_VALUE, &mut hkey)
    };
    if status != 0 {
        crate::debug_log(&format!("autostart: RegOpenKeyExW failed status={status}"));
        return false;
    }
    let name = wide(VALUE_NAME);
    let result = if enabled {
        match std::env::current_exe() {
            Ok(path) => {
                let command = format!("\"{}\"", path.display());
                let data = wide(&command);
                let bytes = data.len() * std::mem::size_of::<u16>();
                let status = unsafe {
                    RegSetValueExW(
                        hkey,
                        name.as_ptr(),
                        0,
                        REG_SZ,
                        data.as_ptr() as *const u8,
                        bytes as u32,
                    )
                };
                status == 0
            }
            Err(err) => {
                crate::debug_log(&format!("autostart: current_exe err={err}"));
                false
            }
        }
    } else {
        let status = unsafe { RegDeleteValueW(hkey, name.as_ptr()) };
        // 2 = ERROR_FILE_NOT_FOUND, which also means "already disabled".
        status == 0 || status == 2
    };
    unsafe { RegCloseKey(hkey) };
    result
}

/// Whether the Run value currently exists.
pub fn is_enabled() -> bool {
    let subkey = wide(RUN_KEY);
    let mut hkey: HKEY = std::ptr::null_mut();
    let status = unsafe {
        RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_QUERY_VALUE, &mut hkey)
    };
    if status != 0 {
        return false;
    }
    let name = wide(VALUE_NAME);
    let status = unsafe {
        RegQueryValueExW(
            hkey,
            name.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    unsafe { RegCloseKey(hkey) };
    status == 0
}
