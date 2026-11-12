import React, { useCallback, useEffect, useState } from 'react';
import { call, cover, formatCount } from '../api';
import { useApp } from '../context';
import type { FishState } from '../types';
import { Empty, ErrorBox, PageHead, Spinner, Tabs, Toggle } from '../components/Common';

const TABS = ['概览', '推荐', '设置', 'ASR'];

export default function Fish() {
  const { settings, saveSettings, toast } = useApp();
  const [tab, setTab] = useState(0);
  const [state, setState] = useState<FishState | null>(null);
  const [models, setModels] = useState<any>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [key, setKey] = useState('');
  const [instruction, setInstruction] = useState(settings.fish_instruction || '');
  const [asrStatus, setAsrStatus] = useState('');

  const load = useCallback(async (quiet = false) => {
    if (!quiet) setLoading(true);
    try {
      setState(await call<FishState>('fish.state'));
      setError('');
    } catch (reason: any) {
      if (!quiet) setError(reason?.message || String(reason));
    } finally {
      if (!quiet) setLoading(false);
    }
  }, []);

  useEffect(() => {
    load();
    const timer = setInterval(() => load(true), 2000);
    return () => clearInterval(timer);
  }, [load]);

  useEffect(() => {
    const onRefresh = () => load();
    window.addEventListener('o11a1:refresh', onRefresh);
    return () => window.removeEventListener('o11a1:refresh', onRefresh);
  }, [load]);

  useEffect(() => {
    if (tab === 3) call('fish.models').then(setModels).catch((reason) => toast(reason.message, true));
  }, [tab, toast]);

  useEffect(() => () => { call('fish.pause').catch(() => {}); }, []);

  const action = async (method: string, params: Record<string, any> = {}) => {
    try {
      await call(method, params);
      await load(true);
      toast('操作成功');
    } catch (reason: any) { toast(reason.message, true); }
  };

  const saveFish = async (patch: Record<string, any>) => {
    await saveSettings(patch as any);
  };

  const snapshot = state?.snapshot;
  const progress = snapshot?.progress;
  const dashboard = state?.dashboard;

  return (
    <>
      <PageHead title="养鱼" sub="AI 看片 · 自动判断值不值得看">
        <Tabs tabs={TABS} active={tab} onChange={setTab} />
      </PageHead>

      {loading && !state ? <Spinner label="读取养鱼引擎…" /> : error ? <ErrorBox error={error} onRetry={() => load()} /> : state ? (
        <>
          {tab === 0 ? (
            <div className="fish-grid">
              <div>
                <div className="card panel" style={{ marginBottom: 14 }}>
                  <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
                    <div className={`chip small ${snapshot.state === 'Running' ? 'active' : ''}`}>
                      {typeof snapshot.state === 'string' ? snapshot.state : snapshot.state?.Error || 'Idle'}
                    </div>
                    <div style={{ color: 'var(--text-dim)', fontSize: 12 }}>
                      队列 {snapshot.queued} · 完成 {snapshot.done} · 错误 {snapshot.errors} · tokens {formatCount(snapshot.tokens_in + snapshot.tokens_out)}
                    </div>
                    <div style={{ flex: 1 }} />
                    <button className="btn sm primary" onClick={() => action('fish.start')}>开始</button>
                    <button className="btn sm" onClick={() => action('fish.pause')}>暂停</button>
                    <button className="btn sm danger" onClick={() => action('fish.stop')}>停止</button>
                  </div>
                  {snapshot.current ? <div style={{ marginTop: 10, fontSize: 12.5 }}>正在看：{snapshot.current[1] || snapshot.current[0]}</div> : null}
                  {progress ? (
                    <div style={{ marginTop: 12 }}>
                      <div className="progress-steps">
                        {(progress.steps || []).map((step: any, index: number) => (
                          <div key={step.id || index} className={`step ${step.state === 'done' ? 'done' : progress.current === index ? 'current' : ''}`}>
                            <span style={{ width: 16 }}>{step.state === 'done' ? '✓' : progress.current === index ? '•' : '·'}</span>
                            <span style={{ flex: 1 }}>{step.label}</span>
                            <span>{step.detail || ''} {step.ms ? `${step.ms}ms` : ''}</span>
                          </div>
                        ))}
                      </div>
                    </div>
                  ) : null}
                  <div style={{ display: 'flex', gap: 8, marginTop: 14 }}>
                    <button className="btn sm" onClick={() => action('fish.enqueue')}>投喂首页推荐</button>
                    <button className="btn sm" onClick={() => action('fish.retry')}>重试错误</button>
                    <button className="btn sm danger" onClick={() => { if (window.confirm('清空推荐流？历史结论会保留。')) action('fish.clear'); }}>清空推荐流</button>
                  </div>
                </div>

                {snapshot.last ? (
                  <div className="card fish-verdict">
                    <div style={{ display: 'flex', gap: 12 }}>
                      <img src={cover(snapshot.last.cover)} referrerPolicy="no-referrer" alt="" style={{ width: 150, aspectRatio: '16/9', objectFit: 'cover', borderRadius: 10 }} />
                      <div style={{ flex: 1 }}>
                        <div style={{ display: 'flex', gap: 10, alignItems: 'baseline' }}>
                          <div className="score">{Math.round(snapshot.last.score)}</div>
                          <div style={{ fontWeight: 600 }}>{snapshot.last.verdict}</div>
                        </div>
                        <div style={{ fontSize: 12.5, marginTop: 6 }}>{snapshot.last.title}</div>
                        <div style={{ fontSize: 11.5, color: 'var(--text-faint)', marginTop: 4 }}>{snapshot.last.up_name}</div>
                      </div>
                    </div>
                    <div style={{ marginTop: 10, fontSize: 12.5, lineHeight: 1.6 }}>{snapshot.last.one_line}</div>
                  </div>
                ) : <Empty title="还没有结论" hint="点击「投喂首页推荐」开始第一轮" />}
              </div>

              <div>
                <div className="fish-stats" style={{ marginBottom: 14 }}>
                  <div className="fish-stat"><div className="n">{dashboard?.today?.total ?? 0}</div><div className="l">今天看了</div></div>
                  <div className="fish-stat"><div className="n">{dashboard?.week?.total ?? 0}</div><div className="l">本周</div></div>
                  <div className="fish-stat"><div className="n">{dashboard?.month?.total ?? 0}</div><div className="l">本月</div></div>
                </div>
                <div className="card panel">
                  <div className="panel-title">高频标签</div>
                  <div className="chips">
                    {(dashboard?.top_tags || []).map(([tag, count]: any) => <span className="chip small" key={tag}>{tag} · {count}</span>)}
                    {!dashboard?.top_tags?.length ? <span style={{ color: 'var(--text-faint)', fontSize: 12 }}>还没有统计</span> : null}
                  </div>
                  {state.errors.length ? (
                    <>
                      <div className="panel-title" style={{ marginTop: 16 }}>队列错误</div>
                      {state.errors.map((row: any) => (
                        <div key={row.bvid} style={{ fontSize: 11.5, color: 'var(--danger)', marginBottom: 5 }}>{row.title || row.bvid}：{row.last_error}</div>
                      ))}
                    </>
                  ) : null}
                </div>
              </div>
            </div>
          ) : null}

          {tab === 1 ? (
            <div>
              {state.verdicts.length ? state.verdicts.map((row: any) => (
                <div className="card fish-verdict" key={`${row.bvid}-${row.at}`}>
                  <div style={{ display: 'flex', gap: 12 }}>
                    <img src={cover(row.cover)} referrerPolicy="no-referrer" alt="" style={{ width: 140, aspectRatio: '16/9', objectFit: 'cover', borderRadius: 9 }} />
                    <div style={{ flex: 1 }}>
                      <div style={{ display: 'flex', gap: 10, alignItems: 'baseline' }}>
                        <div className="score" style={{ fontSize: 22 }}>{Math.round(row.score)}</div>
                        <div style={{ fontWeight: 600 }}>{row.verdict}</div>
                        <div style={{ flex: 1 }} />
                        <span style={{ fontSize: 11, color: 'var(--text-faint)' }}>{row.up_name}</span>
                      </div>
                      <div style={{ fontWeight: 600, fontSize: 13, marginTop: 4 }}>{row.title}</div>
                      <div style={{ fontSize: 12, marginTop: 6, color: 'var(--text-dim)' }}>{row.one_line}</div>
                      <div className="chips" style={{ marginTop: 8 }}>
                        {(row.tags || []).map((tag: string) => <span className="chip small" key={tag}>{tag}</span>)}
                      </div>
                    </div>
                  </div>
                </div>
              )) : <Empty title="还没有推荐结论" />}
            </div>
          ) : null}

          {tab === 2 ? (
            <div className="card panel" style={{ maxWidth: 860 }}>
              <div className="field">
                <label>DeepSeek API Key</label>
                <div style={{ display: 'flex', gap: 8 }}>
                  <input className="input" style={{ flex: 1 }} value={key} type="password" placeholder={state.settings.deepseek_key_set ? '已设置（留空保持）' : 'sk-…'} onChange={(event) => setKey(event.target.value)} />
                  <button className="btn sm primary" disabled={!key.trim()} onClick={async () => { await saveFish({ deepseek_key: key.trim() }); setKey(''); toast('Key 已保存'); load(true); }}>保存</button>
                </div>
              </div>
              <div className="field">
                <label>观看指令</label>
                <textarea className="textarea" value={instruction} onChange={(event) => setInstruction(event.target.value)} />
                <div style={{ display: 'flex', gap: 8 }}>
                  <button className="btn sm" onClick={async () => { await saveFish({ fish_instruction: instruction }); toast('已保存'); }}>保存指令</button>
                  {(state.presets || []).map((preset: any) => (
                    <button key={preset.id} className="chip small" title={preset.tagline} onClick={async () => { setInstruction(preset.body); await saveFish({ fish_instruction: preset.body }); toast(`已切换到 ${preset.name}`); }}>{preset.name}</button>
                  ))}
                </div>
              </div>
              <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 14 }}>
                <div className="field"><label>模型</label>
                  <select className="select" value={settings.fish_model} onChange={(event) => saveFish({ fish_model: event.target.value })}>
                    <option value="deepseek-flash">deepseek-flash</option>
                    <option value="deepseek-reasoner">deepseek-reasoner</option>
                  </select>
                </div>
                <div className="field"><label>预算（视频数）</label><input className="input" type="number" min={1} max={200} value={settings.fish_budget_videos} onChange={(event) => saveFish({ fish_budget_videos: Number(event.target.value) })} /></div>
                <div className="field"><label>间隔（毫秒）</label><input className="input" type="number" min={200} max={10000} value={settings.fish_interval_ms} onChange={(event) => saveFish({ fish_interval_ms: Number(event.target.value) })} /></div>
                <div className="field"><label>推荐来源</label>
                  <select className="select" value={settings.fish_sources} onChange={(event) => saveFish({ fish_sources: Number(event.target.value) })}>
                    <option value={1}>首页推荐</option>
                    <option value={2}>首页 + 分区</option>
                  </select>
                </div>
              </div>
              <div className="settings-row"><div className="label"><div className="name">启用养鱼</div><div className="desc">允许后台定时喂食</div></div><Toggle on={settings.fish_enabled} onChange={(value) => saveFish({ fish_enabled: value })} /></div>
              <div className="settings-row"><div className="label"><div className="name">自动喂食</div><div className="desc">{settings.fish_feed_time || '20:00'} 开始</div></div><Toggle on={settings.fish_auto_feed} onChange={(value) => saveFish({ fish_auto_feed: value })} /></div>
              <div className="settings-row"><div className="label"><div className="name">画面理解精度</div><div className="desc">low / high</div></div>
                <select className="select" value={settings.fish_vision_detail} onChange={(event) => saveFish({ fish_vision_detail: event.target.value })}><option value="low">low</option><option value="high">high</option></select>
              </div>
              <div className="panel-title" style={{ marginTop: 16 }}>步骤</div>
              {[
                ['fish_step_meta', '元数据'],
                ['fish_step_subtitle', '字幕'],
                ['fish_step_outline', '大纲'],
                ['fish_step_comments', '评论'],
                ['fish_step_asr', 'ASR'],
                ['fish_step_frames', '画面抽帧'],
              ].map(([field, label]) => (
                <div className="settings-row" key={field}>
                  <div className="label"><div className="name">{label}</div></div>
                  <Toggle on={Boolean((settings as any)[field])} onChange={(value) => saveFish({ [field]: value })} />
                </div>
              ))}
            </div>
          ) : null}

          {tab === 3 ? (
            <div className="card panel" style={{ maxWidth: 900 }}>
              <div style={{ display: 'flex', gap: 10, alignItems: 'center', marginBottom: 12 }}>
                <div className="panel-title" style={{ margin: 0 }}>语音识别模型</div>
                <div style={{ flex: 1 }} />
                <button className="btn sm" onClick={() => call('fish.models').then(setModels).catch((reason) => toast(reason.message, true))}>刷新</button>
                <button className="btn sm" onClick={() => call('fish.models.cancel').then(() => toast('已请求取消')).catch((reason) => toast(reason.message, true))}>取消下载</button>
                <button className="btn sm primary" onClick={() => { setAsrStatus('探测中…'); call<string>('fish.asr.status').then(setAsrStatus).catch((reason) => setAsrStatus(`探测失败：${reason.message}`)); }}>探测引擎</button>
              </div>
              {asrStatus ? <div className="banner">{asrStatus}</div> : null}
              {models?.running ? <div className="banner">正在下载：{models.running}</div> : null}
              {(models?.items || []).map((item: any) => (
                <div className="row" key={item.model.name}>
                  <div className="grow">
                    <div className="row-title">{item.model.name} <span style={{ color: 'var(--text-faint)', fontSize: 11 }}>{item.model.file}</span></div>
                    <div className="row-sub">{item.state} · {formatCount(item.have)} / {formatCount(item.model.bytes)}（{formatCount(item.model.bytes)} 字节）{item.verified ? ' · 已校验' : ''}</div>
                    {item.state === 'Partial' ? <div className="bar"><div style={{ width: `${item.pct || 0}%` }} /></div> : null}
                  </div>
                  <button className="btn sm" onClick={() => call('fish.models.download', { name: item.model.name }).then(() => toast('已开始下载')).catch((reason) => toast(reason.message, true))}>下载</button>
                  <button className="btn sm danger" onClick={() => { if (window.confirm(`删除 ${item.model.name}？`)) call('fish.models.delete', { name: item.model.name }).then(() => call('fish.models').then(setModels)).catch((reason) => toast(reason.message, true)); }}>删除</button>
                </div>
              ))}
              {!models?.items?.length ? <Empty title="没有模型信息" /> : null}
              <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 14, marginTop: 14 }}>
                <div className="field"><label>ASR 引擎</label>
                  <select className="select" value={settings.fish_asr_engine} onChange={(event) => saveFish({ fish_asr_engine: event.target.value })}>
                    <option value="auto">自动</option><option value="whisper.cpp">whisper.cpp</option><option value="faster-whisper">faster-whisper</option>
                  </select>
                </div>
                <div className="field"><label>设备</label>
                  <select className="select" value={settings.fish_asr_device} onChange={(event) => saveFish({ fish_asr_device: event.target.value })}>
                    <option value="cpu">CPU</option><option value="gpu">GPU</option>
                  </select>
                </div>
                <div className="field"><label>默认模型</label>
                  <select className="select" value={settings.fish_asr_model} onChange={(event) => saveFish({ fish_asr_model: event.target.value })}>
                    {['tiny', 'base', 'small', 'medium'].map((name) => <option key={name} value={name}>{name}</option>)}
                  </select>
                </div>
                <div className="field"><label>线程数</label><input className="input" type="number" min={1} max={32} value={settings.fish_asr_threads} onChange={(event) => saveFish({ fish_asr_threads: Number(event.target.value) })} /></div>
                <div className="field"><label>下载镜像</label>
                  <select className="select" value={settings.fish_model_mirror} onChange={(event) => saveFish({ fish_model_mirror: event.target.value })}>
                    <option value="auto">自动</option><option value="official">官方</option><option value="mirror">镜像</option>
                  </select>
                </div>
                <div className="field"><label>启用 ASR</label><Toggle on={settings.fish_asr_enabled} onChange={(value) => saveFish({ fish_asr_enabled: value })} /></div>
              </div>
            </div>
          ) : null}
        </>
      ) : null}
    </>
  );
}
