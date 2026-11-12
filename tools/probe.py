import json, os, subprocess, socket, sys
from pathlib import Path

exe = Path(r'D:\Dev\bilibili-electron\runtime\o11a1-engine.exe')
env = os.environ.copy()
env['O11A1_HOME'] = str(Path(os.environ['APPDATA']) / 'o11a1-electron' / 'data')
env['BILI_FFMPEG_DIR'] = r'C:\Programs\ffmpeg-unpack\bin'
p = subprocess.Popen([str(exe)], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env)
line = p.stdout.readline().decode().strip()
port = int(line.split()[-1])
s = socket.create_connection(('127.0.0.1', port), timeout=10)
f = s.makefile('rwb', buffering=0)
method = sys.argv[1]
params = json.loads(sys.argv[2]) if len(sys.argv) > 2 else {}
f.write((json.dumps({'id': 1, 'method': method, 'params': params}) + '\n').encode())
while True:
    data = json.loads(f.readline().decode())
    if 'event' in data:
        continue
    print(json.dumps(data, ensure_ascii=False)[:4000])
    break
s.close(); p.terminate(); p.wait(timeout=5)
