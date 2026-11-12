from pathlib import Path
old = '''    let under_target = exe.components().any(|part| {
        part.as_os_str().eq_ignore_ascii_case("target")
    });'''
new = '''    let under_target = exe.components().any(|part| {
        part.as_os_str()
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains("target")
    });'''
for p in [Path(r'D:\Dev\bilibili-electron\native\core\src\paths.rs'), Path(r'D:\Dev\bilibili-slint\core\src\paths.rs')]:
    t = p.read_text(encoding='utf-8')
    if old not in t:
        print('pattern missing', p)
    p.write_text(t.replace(old, new), encoding='utf-8')
print('done')
