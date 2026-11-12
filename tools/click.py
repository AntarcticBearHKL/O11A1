import ctypes, sys, time
user32 = ctypes.windll.user32
class POINT(ctypes.Structure):
    _fields_ = [('x', ctypes.c_long), ('y', ctypes.c_long)]

x, y = int(sys.argv[1]), int(sys.argv[2])
user32.SetCursorPos(x, y)
time.sleep(0.2)
user32.mouse_event(0x0002, 0, 0, 0, 0)
time.sleep(0.08)
user32.mouse_event(0x0004, 0, 0, 0, 0)
print('clicked', x, y)
