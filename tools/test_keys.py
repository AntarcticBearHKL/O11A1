import json, os, subprocess, socket
from pathlib import Path

exe = Path(r'D:\Dev\bilibili-electron\runtime\o11a1-engine.exe')
home = Path(r'D:\Dev\bilibili-electron\.keys-home')
home.mkdir(exist_ok=True)
env = os.environ.copy()
env['O11A1_HOME'] = str(home)
env['BILI_FFMPEG_DIR'] = r'C:\Programs\ffmpeg-unpack\bin'
keys = [32, 37, 39, 38, 40, 77, 70, 27, 68, 76, 83]

def run(expr):
    p = subprocess.Popen([str(exe)], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env)
    line = p.stdout.readline().decode().strip()
    port = int(line.split()[-1])
    s = socket.create_connection(('127.0.0.1', port), timeout=10)
    f = s.makefile('rwb', buffering=0)
    f.write((json.dumps({'id': 1, 'method': expr[0], 'params': expr[1]}) + '\n').encode())
    while True:
        data = json.loads(f.readline().decode())
        if 'event' in data:
            continue
        s.close(); p.terminate(); p.wait(timeout=5)
        return data

print(run(('settings.patch', {'electron_player_keys': keys}))['data']['electron_player_keys'])
print(run(('settings.get', {}))['data']['electron_player_keys'])
