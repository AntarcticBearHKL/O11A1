import ctypes
from ctypes import wintypes
user32 = ctypes.windll.user32
WNDENUMPROC = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
def cb(hwnd, lparam):
    if not user32.IsWindowVisible(hwnd):
        return True
    text = ctypes.create_unicode_buffer(512)
    user32.GetWindowTextW(hwnd, text, 512)
    cls = ctypes.create_unicode_buffer(128)
    user32.GetClassNameW(hwnd, cls, 128)
    if 'O11A1' in text.value or 'O11A1' in cls.value:
        rect = wintypes.RECT()
        user32.GetWindowRect(hwnd, ctypes.byref(rect))
        print(hwnd, hex(hwnd), cls.value, repr(text.value), rect.left, rect.top, rect.right-rect.left, rect.bottom-rect.top)
    return True
user32.EnumWindows(WNDENUMPROC(cb), 0)
