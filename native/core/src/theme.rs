//! App theme resolution.
//!
//! The theme setting has exactly two explicit modes: [`THEME_LIGHT`] (1) and
//! [`THEME_DARK`] (2). There is no stored 跟随系统 state any more. The Windows
//! light/dark setting (`AppsUseLightTheme` under
//! `HKCU\SOFTWARE\Microsoft\Windows\CurrentVersion\Themes\Personalize`, the
//! exact value the shell uses) is read exactly once: when a legacy `0`
//! (跟随系统) value is migrated to a concrete mode at load (see
//! `settings::normalise_theme`), and on the first run with no stored value.
//!
//! The read is a single synchronous registry query (microseconds, no process
//! spawn, no network), so it can never block the UI thread in practice.
//!
//! The Win32 calls are declared inline, matching `win.rs`: this avoids adding
//! a registry crate dependency for two function calls.

/// HKEY_CURRENT_USER (winreg.h).
const HKEY_CURRENT_USER: isize = 0x8000_0001u32 as isize;
const KEY_READ: u32 = 0x2001_9;
const REG_DWORD: u32 = 4;
const ERROR_SUCCESS: i32 = 0;

/// The two explicit theme modes persisted in `settings.json`.
pub const THEME_LIGHT: u32 = 1;
pub const THEME_DARK: u32 = 2;

#[link(name = "advapi32")]
extern "system" {
    fn RegOpenKeyExW(
        hkey: isize,
        sub_key: *const u16,
        options: u32,
        sam: u32,
        result: *mut isize,
    ) -> i32;

    fn RegQueryValueExW(
        hkey: isize,
        value_name: *const u16,
        reserved: *mut u32,
        ty: *mut u32,
        data: *mut u8,
        cb_data: *mut u32,
    ) -> i32;

    fn RegCloseKey(hkey: isize) -> i32;
}

/// NUL-terminated UTF-16 for the `W` registry APIs.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `Some(true)` while Windows is in dark app mode, `Some(false)` in light
/// mode, `None` when the value cannot be read.
fn system_is_dark() -> Option<bool> {
    let sub_key = wide(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let value_name = wide("AppsUseLightTheme");
    let mut key: isize = 0;
    // SAFETY: `sub_key` is a NUL-terminated UTF-16 buffer that outlives the
    // call; `key` is only used after a success status.
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            sub_key.as_ptr(),
            0,
            KEY_READ,
            &mut key,
        )
    };
    if opened != ERROR_SUCCESS {
        return None;
    }
    let mut data: u32 = 0;
    let mut ty: u32 = 0;
    let mut size: u32 = std::mem::size_of::<u32>() as u32;
    // SAFETY: `data`/`size` describe the 4-byte DWORD buffer being filled;
    // `value_name` is NUL-terminated and outlives the call.
    let queried = unsafe {
        RegQueryValueExW(
            key,
            value_name.as_ptr(),
            std::ptr::null_mut(),
            &mut ty,
            (&mut data as *mut u32).cast::<u8>(),
            &mut size,
        )
    };
    // SAFETY: `key` was returned by a successful RegOpenKeyExW and is closed
    // exactly once here.
    unsafe { RegCloseKey(key) };
    if queried != ERROR_SUCCESS || ty != REG_DWORD || size < 4 {
        return None;
    }
    Some(data == 0)
}

/// The concrete mode matching the Windows light/dark setting.
///
/// Used for the one-time migration of a legacy 跟随系统 `0` value and for the
/// first run with no stored value. Falls back to [`THEME_LIGHT`] when the
/// registry value is unreadable, which preserves the original all-light
/// rendering on systems without it.
pub fn system_mode() -> u32 {
    if system_is_dark().unwrap_or(false) {
        THEME_DARK
    } else {
        THEME_LIGHT
    }
}

/// Effective dark flag for a stored theme mode.
///
/// Only the two explicit modes are meaningful: [`THEME_DARK`] (2) is dark and
/// [`THEME_LIGHT`] (1) is light. Any other value resolves to light; the stored
/// value is guaranteed to be one of the two by `settings::normalise_theme`,
/// which migrates the legacy 跟随系统 `0` at load.
pub fn resolve(mode: u32) -> bool {
    mode == THEME_DARK
}
