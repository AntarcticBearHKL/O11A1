import json, os, subprocess, socket, sys, time
from pathlib import Path

exe = Path(r'D:\Dev\bilibili-slint\target\release\o11a1-engine.exe')
home = Path(r'D:\Dev\bilibili-electron\.test-home')
home.mkdir(exist_ok=True)
env = os.environ.copy()
env['O11A1_HOME'] = str(home)
env['BILI_FFMPEG_DIR'] = r'C:\Programs\ffmpeg-unpack\bin'
p = subprocess.Popen([str(exe)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
line = p.stdout.readline().decode().strip()
print('engine:', line)
port = int(line.split()[-1])
s = socket.create_connection(('127.0.0.1', port), timeout=10)
f = s.makefile('rwb', buffering=0)

def call(i, method, params=None):
    f.write((json.dumps({'id': i, 'method': method, 'params': params or {}}) + '\n').encode())
    while True:
        data = json.loads(f.readline().decode())
        if 'event' in data:
            print('event', data['event'], str(data['data'])[:200])
            continue
        print(method, '->', json.dumps(data, ensure_ascii=False)[:1200])
        return data

call(1, 'app.ping')
call(2, 'settings.get')
call(3, 'app.bootstrap')
call(4, 'feed.home', {'fresh': 0})
time.sleep(1)
s.close()
p.terminate()
