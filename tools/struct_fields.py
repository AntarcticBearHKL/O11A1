import re, pathlib, sys
root = pathlib.Path(r'D:\Dev\bilibili-slint\core\src')
files = {
    'feed': 'bilibili/feed.rs',
    'account': 'bilibili/account.rs',
    'search': 'bilibili/search.rs',
    'pgc': 'bilibili/pgc.rs',
    'live': 'bilibili/live.rs',
    'settings': 'settings.rs',
    'fish': 'fish.rs',
}
want = sys.argv[1:]
for key, rel in files.items():
    t = (root / rel).read_text(encoding='utf-8')
    for name in want:
        m = re.search(r'pub (?:struct|enum) ' + name + r'\b[^\{]*\{.*?\n\}', t, re.S)
        if not m:
            continue
        body = m.group(0)
        if len(body) > 4000:
            body = body[:4000] + '...'
        # strip doc comments
        body = '\n'.join(l for l in body.splitlines() if not l.strip().startswith('//'))
        print('###', name)
        print(body)
