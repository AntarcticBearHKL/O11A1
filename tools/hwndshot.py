import ctypes, sys
from ctypes import wintypes
from PIL import Image

user32 = ctypes.windll.user32
gdi32 = ctypes.windll.gdi32
user32.SetProcessDPIAware()
hwnd = int(sys.argv[1], 0)
out = sys.argv[2]
rect = wintypes.RECT()
user32.GetWindowRect(hwnd, ctypes.byref(rect))
w, h = rect.right - rect.left, rect.bottom - rect.top
print('capture', hwnd, w, h)
hdc_screen = user32.GetDC(0)
hdc = gdi32.CreateCompatibleDC(hdc_screen)
bmp = gdi32.CreateCompatibleBitmap(hdc_screen, w, h)
gdi32.SelectObject(hdc, bmp)
ok = user32.PrintWindow(hwnd, hdc, 2)
print('PrintWindow', ok)
class BITMAPINFOHEADER(ctypes.Structure):
    _fields_ = [('biSize', wintypes.DWORD), ('biWidth', wintypes.LONG), ('biHeight', wintypes.LONG),
                ('biPlanes', wintypes.WORD), ('biBitCount', wintypes.WORD), ('biCompression', wintypes.DWORD),
                ('biSizeImage', wintypes.DWORD), ('biXPelsPerMeter', wintypes.LONG), ('biYPelsPerMeter', wintypes.LONG),
                ('biClrUsed', wintypes.DWORD), ('biClrImportant', wintypes.DWORD)]
bi = BITMAPINFOHEADER()
bi.biSize = ctypes.sizeof(BITMAPINFOHEADER)
bi.biWidth = w
bi.biHeight = -h
bi.biPlanes = 1
bi.biBitCount = 32
data = ctypes.create_string_buffer(w * h * 4)
got = gdi32.GetDIBits(hdc, bmp, 0, h, data, ctypes.byref(bi), 0)
print('GetDIBits', got)
img = Image.frombuffer('RGBA', (w, h), data, 'raw', 'BGRA', 0, 1)
img.convert('RGB').save(out)
print('saved', out)
gdi32.DeleteObject(bmp)
gdi32.DeleteDC(hdc)
user32.ReleaseDC(0, hdc_screen)
