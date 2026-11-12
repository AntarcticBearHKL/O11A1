//! Minimal Unicode clipboard writer for 复制内容.
//!
//! Only one tiny operation is needed, so the player does not link a clipboard
//! crate: open, replace with one `CF_UNICODETEXT` global block, close. The
//! window owner is `None` because the player thread owns a message queue and
//! the clipboard is opened and closed inside one call.

use anyhow::{anyhow, Result};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

/// `CF_UNICODETEXT` from `Win32_System_Ole`; kept as a literal so the player
/// does not need that whole feature for one clipboard format id.
const CF_UNICODETEXT: u32 = 13;

/// Replace the clipboard text with `text` (UTF-16, NUL terminated).
pub fn set_text(text: &str) -> Result<()> {
    unsafe {
        OpenClipboard(None).map_err(|error| anyhow!("打开剪贴板失败：{error}"))?;
        // A guard would be nicer, but every path below has to return through
        // CloseClipboard and the body is three Win32 calls long.
        let result = (|| -> Result<()> {
            EmptyClipboard().map_err(|error| anyhow!("清空剪贴板失败：{error}"))?;
            let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes = utf16.len() * std::mem::size_of::<u16>();
            let handle: HGLOBAL =
                GlobalAlloc(GMEM_MOVEABLE, bytes).map_err(|error| anyhow!("分配剪贴板内存失败：{error}"))?;
            let pointer = GlobalLock(handle);
            if pointer.is_null() {
                let _ = GlobalFree(Some(handle));
                return Err(anyhow!("锁定剪贴板内存失败"));
            }
            std::ptr::copy_nonoverlapping(utf16.as_ptr() as *const u8, pointer as *mut u8, bytes);
            let _ = GlobalUnlock(handle);
            // Ownership of `handle` moves to the clipboard on success.
            match SetClipboardData(CF_UNICODETEXT, Some(HANDLE(handle.0))) {
                Ok(_) => Ok(()),
                Err(error) => {
                    let _ = GlobalFree(Some(handle));
                    Err(anyhow!("写入剪贴板失败：{error}"))
                }
            }
        })();
        let _ = CloseClipboard();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The clipboard is a process-global OS resource; reading it back needs a
    // message loop in some desktop states, so the test only proves the writer
    // accepts text (and, when the desktop session allows it, replaces it).
    // A headless CI session may reject OpenClipboard; that is reported as an
    // error by the writer, not a panic.
    #[test]
    fn writer_does_not_panic_on_empty_or_unicode_text() {
        let _ = set_text("");
        let _ = set_text("弹幕 复制 test");
    }
}
