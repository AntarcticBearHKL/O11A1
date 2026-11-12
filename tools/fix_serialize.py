import re, pathlib
root = pathlib.Path(r'D:\Dev\bilibili-slint\core\src')

# 1. duplicate imports
for rel in ['settings.rs', 'fish.rs']:
    p = root / rel
    t = p.read_text(encoding='utf-8')
    lines = t.splitlines()
    seen = False
    out = []
    for line in lines:
        if line.strip() == 'use serde::Serialize;':
            if seen:
                continue
            seen = True
        out.append(line)
    p.write_text('\n'.join(out) + '\n', encoding='utf-8')
    print('imports', rel)

# 2. merge adjacent derive lines and drop Default on enums
for rel in ['bilibili/feed.rs', 'bilibili/account.rs', 'bilibili/search.rs', 'bilibili/pgc.rs', 'bilibili/live.rs', 'settings.rs', 'accounts.rs', 'fish.rs', 'danmaku.rs']:
    p = root / rel
    t = p.read_text(encoding='utf-8')
    def merge(m):
        a = [x.strip() for x in m.group(1).split(',')]
        b = [x.strip() for x in m.group(2).split(',')]
        out = []
        for x in a + b:
            if x and x not in out:
                out.append(x)
        return '#[derive(' + ', '.join(out) + ')]\n'
    t = re.sub(r'#\[derive\(([^\)]*)\)\]\n#\[derive\(([^\)]*)\)\]\n', merge, t)
    # Default on enum -> remove
    def enum_fix(m):
        attrs = [x.strip() for x in m.group(1).split(',') if x.strip() != 'Default']
        return '#[derive(' + ', '.join(attrs) + ')]\npub enum ' + m.group(2)
    t = re.sub(r'#\[derive\(([^\)]*)\)\]\npub enum (\w+)', enum_fix, t)
    p.write_text(t, encoding='utf-8')
    print('merged', rel)
