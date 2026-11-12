import ctypes, sys, time
from ctypes import wintypes
from PIL import ImageGrab

user32 = ctypes.windll.user32
user32.SetProcessDPIAware()
title_part = sys.argv[1] if len(sys.argv) > 1 else 'O11A1'
out = sys.argv[2] if len(sys.argv) > 2 else r'D:\Dev\bilibili-electron\shot.png'

found = []
WNDENUMPROC = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
def cb(hwnd, lparam):
    if not user32.IsWindowVisible(hwnd):
        return True
    text = ctypes.create_unicode_buffer(512)
    user32.GetWindowTextW(hwnd, text, 512)
    cls = ctypes.create_unicode_buffer(128)
    user32.GetClassNameW(hwnd, cls, 128)
    if title_part.lower() in text.value.lower():
        found.append((hwnd, text.value, cls.value))
    return True
user32.EnumWindows(WNDENUMPROC(cb), 0)
print(found)
for hwnd, text, cls in found:
    rect = wintypes.RECT()
    user32.GetWindowRect(hwnd, ctypes.byref(rect))
    w, h = rect.right - rect.left, rect.bottom - rect.top
    print(hwnd, repr(text), cls, rect.left, rect.top, w, h)
    if w < 200 or h < 200:
        continue
    HWND_TOPMOST = -1
    HWND_NOTOPMOST = -2
    SWP_NOMOVE = 0x0002
    SWP_NOSIZE = 0x0001
    SWP_SHOWWINDOW = 0x0040
    user32.SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW)
    user32.ShowWindow(hwnd, 9)
    user32.SetForegroundWindow(hwnd)
    user32.BringWindowToTop(hwnd)
    time.sleep(1.0)
    img = ImageGrab.grab(bbox=(rect.left, rect.top, rect.right, rect.bottom), all_screens=True)
    img.save(out)
    print('saved', out, img.size)
    user32.SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE)
    break
