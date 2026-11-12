import React, { useEffect, useState } from 'react';
import { call, cover } from '../api';
import { useApp } from '../context';
import { PageHead, SettingsRow, Toggle } from '../components/Common';

const CATEGORIES = ['账户', '外观', '防干扰', '播放', '快捷键', '系统', '弹幕', '屏蔽', '智能云屏蔽', '优化'];

const KEY_LABELS = ['播放 / 暂停', '快退', '快进', '音量 +', '音量 -', '静音', '全屏', '退出全屏', '弹幕开关', '点赞', '截图'];

const DEFAULT_KEYS = [0x20, 0x25, 0x27, 0x26, 0x28, 0x4d, 0x46, 0x1b, 0x44, 0x4c, 0x53];

function effectiveKey(stored: number | undefined, index: number): number {
  return stored && stored >= 0x20 ? stored : DEFAULT_KEYS[index] || 0;
}

const KEY_NAMES: Record<number, string> = {
  0x20: 'Space', 0x25: '←', 0x26: '↑', 0x27: '→', 0x28: '↓', 0x0d: 'Enter', 0x1b: 'Esc',
  0x09: 'Tab', 0x08: 'Backspace', 0x2d: 'Insert', 0x2e: 'Delete', 0x24: 'Home', 0x23: 'End',
  0x21: 'PageUp', 0x22: 'PageDown', 0x10: 'Shift', 0x11: 'Ctrl', 0x12: 'Alt',
};

function keyName(code: number): string {
  if (KEY_NAMES[code]) return KEY_NAMES[code];
  if (code >= 0x41 && code <= 0x5a) return String.fromCharCode(code);
  if (code >= 0x30 && code <= 0x39) return String.fromCharCode(code);
  if (code >= 0x70 && code <= 0x87) return `F${code - 0x6f}`;
  return `0x${code.toString(16).toUpperCase()}`;
}

export default function SettingsPage() {
  const { settings, saveSettings, reloadSettings, account, navigate, toast } = useApp();
  const [category, setCategory] = useState('账户');
  const [capturing, setCapturing] = useState(-1);
  const [keyword, setKeyword] = useState('');
  const [regex, setRegex] = useState('');
  const [sender, setSender] = useState('');
  const [updateState, setUpdateState] = useState('');
  const [theme, setTheme] = useState<0 | 1 | 2>(settings.theme === 2 ? 1 : 0);

  useEffect(() => {
    if (theme !== 2) return;
    const dark = window.matchMedia?.('(prefers-color-scheme: dark)')?.matches;
    document.documentElement.dataset.theme = dark ? 'dark' : 'light';
  }, [theme]);

  useEffect(() => {
    const onRefresh = () => reloadSettings();
    window.addEventListener('o11a1:refresh', onRefresh);
    return () => window.removeEventListener('o11a1:refresh', onRefresh);
  }, [reloadSettings]);

  useEffect(() => {
    if (capturing < 0) return;
    const listener = (event: KeyboardEvent) => {
      event.preventDefault();
      const code = event.keyCode || event.which;
      const keys = Array.isArray(settings.electron_player_keys) ? settings.electron_player_keys.slice() : [];
      while (keys.length < KEY_LABELS.length) keys.push(0);
      keys[capturing] = code;
      saveSettings({ electron_player_keys: keys });
      setCapturing(-1);
    };
    window.addEventListener('keydown', listener, true);
    return () => window.removeEventListener('keydown', listener, true);
  }, [capturing, settings.electron_player_keys, saveSettings]);

  const setThemeMode = async (mode: 0 | 1 | 2) => {
    setTheme(mode);
    const resolved = mode === 2
      ? (window.matchMedia?.('(prefers-color-scheme: dark)')?.matches ? 2 : 1)
      : mode === 1 ? 2 : 1;
    await saveSettings({ theme: resolved });
    document.documentElement.dataset.theme = resolved === 2 ? 'dark' : 'light';
  };

  const addBlocked = async (key: 'blocked_keywords' | 'blocked_regex' | 'blocked_senders', value: string) => {
    const trimmed = value.trim();
    if (!trimmed) return;
    const list = Array.isArray(settings[key]) ? settings[key].slice() : [];
    if (!list.includes(trimmed)) list.push(trimmed);
    await saveSettings({ [key]: list } as any);
  };

  const removeBlocked = async (key: 'blocked_keywords' | 'blocked_regex' | 'blocked_senders', value: string) => {
    const list = (settings[key] as string[]).filter((item) => item !== value);
    await saveSettings({ [key]: list } as any);
  };

  const checkUpdate = async () => {
    setUpdateState('检查中…');
    try {
      const result = await window.o11a1.update.check();
      setUpdateState(result?.update ? `发现 ${result.update.version}` : `已是最新版本（${result?.current || ''}）`);
    } catch (error: any) {
      setUpdateState(`检查失败：${error.message}`);
    }
  };

  return (
    <>
      <PageHead title="设置" sub="10 个分类 · 修改后立即保存" />
      <div className="settings-layout">
        <div className="card settings-nav">
          {CATEGORIES.map((name) => (
            <button key={name} className={category === name ? 'active' : ''} onClick={() => setCategory(name)}>{name}</button>
          ))}
        </div>
        <div className="settings-content">
          {category === '账户' ? (
            <div className="card panel">
              <div className="panel-title">账户</div>
              {account ? (
                <>
                  <div style={{ display: 'flex', gap: 14, alignItems: 'center', marginBottom: 16 }}>
                    <img className="avatar lg" src={cover(account.face)} referrerPolicy="no-referrer" alt="" />
                    <div>
                      <div style={{ fontSize: 17, fontWeight: 600 }}>{account.uname}</div>
                      <div style={{ fontSize: 12, color: 'var(--text-faint)', marginTop: 4 }}>UID {account.mid}</div>
                    </div>
                    <div style={{ flex: 1 }} />
                    <button className="btn" onClick={() => navigate('member')}>账户 / 大会员</button>
                  </div>
                  <div style={{ color: 'var(--text-dim)', fontSize: 12 }}>登录状态、二维码、大会员权益和退出登录都在「账户 / 大会员」页。</div>
                </>
              ) : (
                <div>
                  <div style={{ color: 'var(--text-dim)', fontSize: 12.5, marginBottom: 12 }}>当前未登录。登录后可同步收藏、历史、弹幕与大会员信息。</div>
                  <button className="btn primary" onClick={() => navigate('member')}>扫码登录</button>
                </div>
              )}
            </div>
          ) : null}

          {category === '外观' ? (
            <div className="card panel">
              <div className="panel-title">主题</div>
              <div className="chips" style={{ marginBottom: 16 }}>
                {['浅色', '深色', '跟随系统'].map((label, index) => (
                  <button key={label} className={`chip ${theme === index ? 'active' : ''}`} onClick={() => setThemeMode(index as 0 | 1 | 2)}>{label}</button>
                ))}
              </div>
              <SettingsRow name="背景透明度" desc="0 = 完全不透明白底，100 = 无白底">
                <input type="range" min={0} max={100} value={Number(settings.glass_transparency)} onChange={(event) => saveSettings({ glass_transparency: Number(event.target.value) })} />
                <span className="time" style={{ width: 38, textAlign: 'right' }}>{Math.round(Number(settings.glass_transparency))}</span>
              </SettingsRow>
              <SettingsRow name="玻璃模糊" desc="主窗口卡片的背景模糊强度">
                <input type="range" min={0} max={100} value={Number(settings.glass_blur)} onChange={(event) => saveSettings({ glass_blur: Number(event.target.value) })} />
                <span style={{ width: 38, textAlign: 'right' }}>{Math.round(Number(settings.glass_blur))}</span>
              </SettingsRow>
              <SettingsRow name="背景模糊" desc="壁纸色团的扩散半径（0–160）">
                <input type="range" min={0} max={160} value={Number(settings.backdrop_blur)} onChange={(event) => saveSettings({ backdrop_blur: Number(event.target.value) })} />
                <span style={{ width: 38, textAlign: 'right' }}>{settings.backdrop_blur}</span>
              </SettingsRow>
            </div>
          ) : null}

          {category === '防干扰' ? (
            <div className="card panel">
              <div className="panel-title">防干扰</div>
              <SettingsRow name="隐藏直播入口" desc="底栏不再显示「直播」，启动页也跳过直播加载">
                <Toggle on={settings.hide_live_tab} onChange={(value) => saveSettings({ hide_live_tab: value })} />
              </SettingsRow>
              <SettingsRow name="隐藏热搜" desc="搜索页不显示热门搜索">
                <Toggle on={settings.hide_hot_search} onChange={(value) => saveSettings({ hide_hot_search: value })} />
              </SettingsRow>
            </div>
          ) : null}

          {category === '播放' ? (
            <div className="card panel">
              <div className="panel-title">播放</div>
              <SettingsRow name="默认画质" desc="0 为自动（不超过 1080P）">
                <select className="select" value={settings.player_quality} onChange={(event) => saveSettings({ player_quality: Number(event.target.value) })}>
                  <option value={0}>自动</option>
                  <option value={80}>1080P</option>
                  <option value={64}>720P</option>
                  <option value={32}>480P</option>
                </select>
              </SettingsRow>
              <SettingsRow name="画质增强" desc="0 关闭 / 1 通用 CAS / 2 番剧 Anime4K">
                <select className="select" value={settings.player_enhance} onChange={(event) => saveSettings({ player_enhance: Number(event.target.value) })}>
                  <option value={0}>关闭</option>
                  <option value={1}>通用（CAS 锐化）</option>
                  <option value={2}>番剧（Anime4K）</option>
                </select>
              </SettingsRow>
              <SettingsRow name="插帧" desc="光流补帧；需要显示刷新率高于片源">
                <select className="select" value={settings.interp_mode} onChange={(event) => saveSettings({ interp_mode: Number(event.target.value) })}>
                  <option value={0}>关闭</option>
                  <option value={1}>补到显示器刷新率</option>
                  <option value={2}>固定 60fps</option>
                </select>
              </SettingsRow>
              <SettingsRow name="默认速度" desc="0.5×–2.0×">
                <select className="select" value={settings.player_speed} onChange={(event) => saveSettings({ player_speed: Number(event.target.value) })}>
                  {[500, 750, 1000, 1250, 1500, 2000].map((value) => <option key={value} value={value}>{(value / 1000).toFixed(2)}×</option>)}
                </select>
              </SettingsRow>
              <SettingsRow name="编码偏好" desc="自动 / AVC / HEVC / AV1">
                <select className="select" value={settings.codec_pref} onChange={(event) => saveSettings({ codec_pref: Number(event.target.value) })}>
                  <option value={0}>自动</option>
                  <option value={7}>AVC / H.264</option>
                  <option value={12}>HEVC / H.265</option>
                  <option value={13}>AV1</option>
                </select>
              </SettingsRow>
              <SettingsRow name="原始分辨率解码" desc="关闭时按窗口尺寸解码，更省带宽">
                <Toggle on={settings.player_native_size} onChange={(value) => saveSettings({ player_native_size: value })} />
              </SettingsRow>
              <SettingsRow name="硬件解码" desc="使用 D3D11VA / Media Foundation">
                <Toggle on={settings.hwaccel} onChange={(value) => saveSettings({ hwaccel: value })} />
              </SettingsRow>
            </div>
          ) : null}

          {category === '快捷键' ? (
            <div className="card panel">
              <div className="panel-title">播放器快捷键（点击右侧按键后按新键）</div>
              {KEY_LABELS.map((label, index) => (
                <SettingsRow key={label} name={label} desc={`当前：${keyName(effectiveKey(settings.electron_player_keys?.[index], index))}`}>
                  <button className={`btn sm ${capturing === index ? 'primary' : ''}`} onClick={() => setCapturing(index)}>
                    {capturing === index ? '按键…' : '修改'}
                  </button>
                  <button className="btn sm ghost" onClick={() => {
                    const keys = Array.isArray(settings.electron_player_keys) ? settings.electron_player_keys.slice() : [];
                    while (keys.length < KEY_LABELS.length) keys.push(0);
                    keys[index] = 0;
                    saveSettings({ electron_player_keys: keys });
                  }}>清除</button>
                </SettingsRow>
              ))}
              <button className="btn sm" style={{ marginTop: 12 }} onClick={() => saveSettings({ electron_player_keys: [] })}>恢复默认</button>
            </div>
          ) : null}

          {category === '系统' ? (
            <div className="card panel">
              <div className="panel-title">系统</div>
              <SettingsRow name="开机自启" desc="写入 Windows 登录启动项">
                <Toggle on={settings.autostart} onChange={(value) => saveSettings({ autostart: value })} />
              </SettingsRow>
              <SettingsRow name="关闭按钮" desc="关闭主窗口时的行为">
                <select className="select" value={settings.close_action} onChange={(event) => saveSettings({ close_action: Number(event.target.value) })}>
                  <option value={1}>最小化到托盘</option>
                  <option value={2}>退出程序</option>
                </select>
              </SettingsRow>
              <SettingsRow name="检查更新" desc={updateState || 'GitHub Release'}>
                <button className="btn sm" onClick={checkUpdate}>检查更新</button>
              </SettingsRow>
              <SettingsRow name="数据目录" desc="设置、账号、缓存与下载">
                <button className="btn sm" onClick={() => window.o11a1.info().then((info: any) => window.o11a1.shell.openPath(info.dataRoot))}>打开</button>
              </SettingsRow>
              <SettingsRow name="版本" desc="Electron + Rust sidecar">
                <span style={{ color: 'var(--text-faint)', fontSize: 12 }}>O11A1 0.1.0</span>
              </SettingsRow>
            </div>
          ) : null}

          {category === '弹幕' ? (
            <div className="card panel">
              <div className="panel-title">弹幕</div>
              <SettingsRow name="不透明度" desc="0–100">
                <input type="range" min={0} max={100} value={settings.danmaku_opacity} onChange={(event) => saveSettings({ danmaku_opacity: Number(event.target.value) })} />
                <span style={{ width: 38, textAlign: 'right' }}>{settings.danmaku_opacity}</span>
              </SettingsRow>
              <SettingsRow name="字号" desc="小 / 标准 / 大">
                <select className="select" value={settings.danmaku_font} onChange={(event) => saveSettings({ danmaku_font: Number(event.target.value) })}>
                  <option value={0}>小</option>
                  <option value={1}>标准</option>
                  <option value={2}>大</option>
                </select>
              </SettingsRow>
              <SettingsRow name="速度" desc="滚动速度百分比（50–200）">
                <input type="range" min={50} max={200} value={settings.danmaku_speed} onChange={(event) => saveSettings({ danmaku_speed: Number(event.target.value) })} />
                <span style={{ width: 38, textAlign: 'right' }}>{settings.danmaku_speed}</span>
              </SettingsRow>
              <SettingsRow name="显示区域" desc="弹幕最多占视频顶部百分比">
                <select className="select" value={settings.danmaku_area} onChange={(event) => saveSettings({ danmaku_area: Number(event.target.value) })}>
                  <option value={25}>25%</option>
                  <option value={50}>50%</option>
                  <option value={75}>75%</option>
                  <option value={100}>100%</option>
                </select>
              </SettingsRow>
              <div style={{ color: 'var(--text-faint)', fontSize: 11.5, marginTop: 8 }}>弹幕由 Rust 原生层绘制，修改后在下次打开播放器时生效。</div>
            </div>
          ) : null}

          {category === '屏蔽' ? (
            <div className="card panel">
              <div className="panel-title">屏蔽</div>
              <SettingsRow name="屏蔽滚动弹幕"><Toggle on={settings.block_scroll} onChange={(value) => saveSettings({ block_scroll: value })} /></SettingsRow>
              <SettingsRow name="屏蔽顶部弹幕"><Toggle on={settings.block_top} onChange={(value) => saveSettings({ block_top: value })} /></SettingsRow>
              <SettingsRow name="屏蔽底部弹幕"><Toggle on={settings.block_bottom} onChange={(value) => saveSettings({ block_bottom: value })} /></SettingsRow>
              <div className="panel-title" style={{ marginTop: 16 }}>关键词</div>
              <div style={{ display: 'flex', gap: 8, marginBottom: 10 }}>
                <input className="input" style={{ flex: 1 }} value={keyword} placeholder="包含即屏蔽" onChange={(event) => setKeyword(event.target.value)} />
                <button className="btn sm" onClick={() => { addBlocked('blocked_keywords', keyword); setKeyword(''); }}>添加</button>
              </div>
              <div className="chips" style={{ marginBottom: 16 }}>
                {settings.blocked_keywords?.map((item) => <button className="chip small" key={item} onClick={() => removeBlocked('blocked_keywords', item)}>{item} ×</button>)}
                {settings.blocked_keywords?.length ? <button className="btn sm ghost" onClick={() => saveSettings({ blocked_keywords: [] })}>清空</button> : null}
              </div>
              <div className="panel-title">正则</div>
              <div style={{ display: 'flex', gap: 8, marginBottom: 10 }}>
                <input className="input" style={{ flex: 1 }} value={regex} placeholder="正则表达式" onChange={(event) => setRegex(event.target.value)} />
                <button className="btn sm" onClick={() => { addBlocked('blocked_regex', regex); setRegex(''); }}>添加</button>
              </div>
              <div className="chips" style={{ marginBottom: 16 }}>
                {settings.blocked_regex?.map((item) => <button className="chip small" key={item} onClick={() => removeBlocked('blocked_regex', item)}>{item} ×</button>)}
                {settings.blocked_regex?.length ? <button className="btn sm ghost" onClick={() => saveSettings({ blocked_regex: [] })}>清空</button> : null}
              </div>
              <div className="panel-title">用户</div>
              <div style={{ display: 'flex', gap: 8, marginBottom: 10 }}>
                <input className="input" style={{ flex: 1 }} value={sender} placeholder="UID 或昵称" onChange={(event) => setSender(event.target.value)} />
                <button className="btn sm" onClick={() => { addBlocked('blocked_senders', sender); setSender(''); }}>添加</button>
              </div>
              <div className="chips">
                {settings.blocked_senders?.map((item) => <button className="chip small" key={item} onClick={() => removeBlocked('blocked_senders', item)}>{item} ×</button>)}
                {settings.blocked_senders?.length ? <button className="btn sm ghost" onClick={() => saveSettings({ blocked_senders: [] })}>清空</button> : null}
              </div>
            </div>
          ) : null}

          {category === '智能云屏蔽' ? (
            <div className="card panel">
              <div className="panel-title">智能云屏蔽</div>
              <SettingsRow name="启用 AI 云屏蔽" desc="按 B 站服务端的 AI 等级过滤弹幕">
                <Toggle on={settings.ai_on} onChange={(value) => saveSettings({ ai_on: value })} />
              </SettingsRow>
              <SettingsRow name="AI 等级" desc="0–10，越高过滤越严格">
                <input type="range" min={0} max={10} value={settings.ai_level} onChange={(event) => saveSettings({ ai_level: Number(event.target.value) })} />
                <span style={{ width: 24, textAlign: 'right' }}>{settings.ai_level}</span>
              </SettingsRow>
            </div>
          ) : null}

          {category === '优化' ? (
            <div className="card panel">
              <div className="panel-title">优化</div>
              <SettingsRow name="每页卡片数" desc="12 / 24 / 36 / 48">
                <select className="select" value={settings.page_size} onChange={(event) => saveSettings({ page_size: Number(event.target.value) })}>
                  {[12, 24, 36, 48].map((value) => <option key={value} value={value}>{value}</option>)}
                </select>
              </SettingsRow>
              <SettingsRow name="首页预取批数" desc="0–8，0 关闭">
                <input className="input" type="number" min={0} max={8} value={settings.feed_prefetch_batches} onChange={(event) => saveSettings({ feed_prefetch_batches: Number(event.target.value) })} />
              </SettingsRow>
              <SettingsRow name="CDN 择优" desc="播放前在多个 CDN 中测速">
                <Toggle on={settings.cdn_select} onChange={(value) => saveSettings({ cdn_select: value })} />
              </SettingsRow>
              <SettingsRow name="本地预取代理" desc="在播放前预先缓存分段">
                <Toggle on={settings.prefetch_proxy} onChange={(value) => saveSettings({ prefetch_proxy: value })} />
              </SettingsRow>
              <SettingsRow name="预取定位点" desc="4–16">
                <input className="input" type="number" min={4} max={16} value={settings.seek_points} onChange={(event) => saveSettings({ seek_points: Number(event.target.value) })} />
              </SettingsRow>
              <SettingsRow name="预取连接数" desc="4–32">
                <input className="input" type="number" min={4} max={32} value={settings.prefetch_connections} onChange={(event) => saveSettings({ prefetch_connections: Number(event.target.value) })} />
              </SettingsRow>
              <SettingsRow name="网络线程" desc="1–16">
                <input className="input" type="number" min={1} max={16} value={settings.net_threads} onChange={(event) => saveSettings({ net_threads: Number(event.target.value) })} />
              </SettingsRow>
              <SettingsRow name="预加载评论" desc="0 = 关闭">
                <Toggle on={settings.preload_enabled} onChange={(value) => saveSettings({ preload_enabled: value })} />
              </SettingsRow>
            </div>
          ) : null}
        </div>
      </div>
    </>
  );
}
