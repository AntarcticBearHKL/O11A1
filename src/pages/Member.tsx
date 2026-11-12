import React, { useCallback, useEffect, useRef, useState } from 'react';
import QRCode from 'qrcode';
import { call, cover, formatCount } from '../api';
import { useApp } from '../context';
import { Empty, ErrorBox, PageHead, Spinner } from '../components/Common';

export default function Member() {
  const { account, refreshAccount, toast, navigate } = useApp();
  const [qr, setQr] = useState<{ url: string; key: string } | null>(null);
  const [qrError, setQrError] = useState('');
  const [qrState, setQrState] = useState('等待扫码');
  const [polling, setPolling] = useState(false);
  const [vip, setVip] = useState<any>(null);
  const [nav, setNav] = useState<any>(null);
  const [vipLoading, setVipLoading] = useState(false);
  const [vipError, setVipError] = useState('');
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const pollRef = useRef<any>(null);

  const loadQr = useCallback(async () => {
    setQrError('');
    setQrState('正在生成二维码…');
    try {
      const data = await call<{ url: string; key: string }>('login.qr');
      setQr(data);
      setQrState('请使用哔哩哔哩手机客户端扫码');
      setPolling(true);
    } catch (reason: any) {
      setQrError(reason?.message || String(reason));
      setQrState('生成失败');
    }
  }, []);

  useEffect(() => {
    if (!qr || !canvasRef.current) return;
    QRCode.toCanvas(canvasRef.current, qr.url, { width: 180, margin: 1, color: { dark: '#1a1a1a', light: '#ffffff' } }).catch((error) => setQrError(error.message));
  }, [qr]);

  useEffect(() => {
    if (!polling || !qr) return;
    let polls = 0;
    const tick = async () => {
      polls += 1;
      if (polls > 90) {
        setPolling(false);
        setQrState('二维码已过期，请刷新');
        return;
      }
      try {
        const result = await call<{ stateCode: number; account?: any }>('login.poll', { key: qr.key });
        if (result.stateCode === 0) {
          setPolling(false);
          setQrState('登录成功');
          await refreshAccount();
          toast('登录成功');
          return;
        }
        if (result.stateCode === 86038) {
          setPolling(false);
          setQrState('二维码已过期，请刷新');
        } else if (result.stateCode === 86090) {
          setQrState('已扫码，请在手机上确认');
        } else if (result.stateCode === 86101) {
          setQrState('已扫码，等待确认');
        }
      } catch {
        /* transient poll errors are ignored */
      }
      pollRef.current = setTimeout(tick, 2000);
    };
    pollRef.current = setTimeout(tick, 1200);
    return () => clearTimeout(pollRef.current);
  }, [polling, qr, refreshAccount, toast]);

  const loadVip = useCallback(async () => {
    if (!account) return;
    setVipLoading(true);
    setVipError('');
    try {
      const data = await call<any>('member.vip');
      setVip(data);
    } catch (reason: any) { setVipError(reason?.message || String(reason)); }
    finally { setVipLoading(false); }
  }, [account]);

  useEffect(() => {
    if (!account) return;
    call('account.nav').then(setNav).catch(() => {});
    loadVip();
  }, [account, loadVip]);

  useEffect(() => {
    const onRefresh = () => {
      if (!account) return;
      call('account.nav').then(setNav).catch(() => {});
      loadVip();
    };
    window.addEventListener('o11a1:refresh', onRefresh);
    return () => window.removeEventListener('o11a1:refresh', onRefresh);
  }, [account, loadVip]);

  const doVip = async (method: string) => {
    try {
      const result = await call<any>(method);
      toast(result?.message || `code=${result?.code}`);
      if (result?.code === 0) loadVip();
    } catch (reason: any) { toast(reason.message, true); }
  };

  const logout = async () => {
    if (!window.confirm('退出当前账号？')) return;
    try {
      await call('login.logout');
      await refreshAccount();
      setVip(null);
      setNav(null);
      toast('已退出登录');
    } catch (reason: any) { toast(reason.message, true); }
  };

  if (!account) {
    return (
      <>
        <PageHead title="账户" sub="扫码登录后可以同步收藏、历史、弹幕与大会员信息" />
        <div className="card panel" style={{ maxWidth: 520, margin: '40px auto', textAlign: 'center' }}>
          <div className="panel-title">二维码登录</div>
          {qrError ? <ErrorBox error={qrError} onRetry={loadQr} /> : qr ? (
            <>
              <div className="qr-wrap" style={{ margin: '10px auto 12px' }}><canvas ref={canvasRef} /></div>
              <div style={{ color: 'var(--text-dim)', fontSize: 12.5 }}>{qrState}</div>
              <div style={{ marginTop: 12, display: 'flex', gap: 8, justifyContent: 'center' }}>
                <button className="btn sm" onClick={loadQr}>刷新二维码</button>
                <button className="btn sm ghost" onClick={() => { setPolling(false); setQr(null); }}>取消</button>
              </div>
            </>
          ) : <Spinner label={qrState} />}
          <div style={{ color: 'var(--text-faint)', fontSize: 11.5, marginTop: 16 }}>二维码由哔哩哔哩官方接口生成，不会保存在本地。</div>
        </div>
      </>
    );
  }

  const page = vip?.page;
  const tasks = vip?.tasks?.tasks || vip?.tasks?.list || [];
  const catalog = vip?.catalog || [];

  return (
    <>
      <PageHead title="账户 / 大会员" sub="扫码登录、权益、任务与经验">
        <button className="btn sm" onClick={() => navigate('settings')}>账户设置</button>
        <button className="btn sm danger" onClick={logout}>退出登录</button>
      </PageHead>

      <div className="card panel" style={{ display: 'flex', gap: 18, alignItems: 'center', marginBottom: 16 }}>
        <img className="avatar" style={{ width: 84, height: 84 }} src={cover(account.face)} referrerPolicy="no-referrer" alt="" />
        <div style={{ flex: 1 }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
            <h2 style={{ margin: 0, fontSize: 22 }}>{account.uname}</h2>
            {nav?.LoggedIn ? <span className="chip small">LV{nav.LoggedIn.level}</span> : null}
            {account.vip ? <span className="chip small" style={{ color: 'var(--accent)' }}>大会员</span> : null}
          </div>
          <div style={{ fontSize: 12.5, color: 'var(--text-dim)', marginTop: 8 }}>UID {account.mid}</div>
          {nav?.LoggedIn ? (
            <div style={{ display: 'flex', gap: 16, marginTop: 8, fontSize: 12, color: 'var(--text-faint)' }}>
              <span>硬币 {nav.LoggedIn.money}</span>
              <span>节操 {nav.LoggedIn.moral}</span>
              <span>{nav.LoggedIn.vip_text}</span>
            </div>
          ) : null}
        </div>
      </div>

      <div className="card panel" style={{ marginBottom: 16 }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
          <div className="panel-title" style={{ margin: 0 }}>大会员</div>
          <div style={{ flex: 1 }} />
          <button className="btn sm" onClick={loadVip} disabled={vipLoading}>{vipLoading ? '刷新中…' : '刷新'}</button>
        </div>
        {vipError ? <div style={{ color: 'var(--danger)', fontSize: 12 }}>{vipError}</div> : null}
        {page ? (
          <>
            <div className="kv" style={{ marginTop: 12 }}>
              <div className="k">状态</div><div className="v">{page.is_vip ? '大会员生效中' : '未开通 / 已过期'}</div>
              <div className="k">权益</div><div className="v">{page.summary || '暂无'}</div>
            </div>
            <div style={{ display: 'flex', gap: 8, marginTop: 12 }}>
              <button className="btn sm" onClick={() => doVip('member.vip.sign')}>签到</button>
              <button className="btn sm" onClick={() => doVip('member.vip.privilege')}>领取权益</button>
              <button className="btn sm" onClick={() => doVip('member.vip.experience')}>领取经验</button>
            </div>
            <div className="panel-title" style={{ marginTop: 16 }}>本月权益</div>
            <div className="list">
              {(page.list || []).map((item: any) => (
                <div className="row" key={item.kind}>
                  <div className="grow">
                    <div className="row-title">{privilegeLabel(item.kind)}</div>
                    <div className="row-sub">{item.expire_time ? `到期 ${new Date(item.expire_time * 1000).toLocaleDateString()}` : '本月有效'} · {privilegeState(item.state)}</div>
                  </div>
                  <span className="chip small">{item.state === 1 ? '已领取' : item.state === 2 ? '未完成' : '可领取'}</span>
                </div>
              ))}
              {!page.list?.length ? <div style={{ color: 'var(--text-faint)', fontSize: 12, padding: 8 }}>没有权益记录</div> : null}
            </div>
          </>
        ) : !vipError ? <Spinner label="读取大会员信息…" /> : null}
      </div>

      <div className="card panel" style={{ marginBottom: 16 }}>
        <div className="panel-title">会员任务</div>
        <div className="list">
          {tasks.map((task: any) => (
            <div className="row" key={task.code || task.title}>
              <img src={cover(task.icon)} referrerPolicy="no-referrer" alt="" style={{ width: 30, height: 30, borderRadius: 8, display: task.icon ? 'block' : 'none' }} />
              <div className="grow">
                <div className="row-title">{task.title}</div>
                <div className="row-sub">{task.subtitle || task.explain} · {`${task.complete_times || 0}/${task.max_times || 0}`}</div>
              </div>
              <span className="chip small">{task.state === 1 ? '已完成' : task.state === 2 ? '已领取' : '进行中'}</span>
            </div>
          ))}
          {!tasks.length ? <div style={{ color: 'var(--text-faint)', fontSize: 12, padding: 8 }}>服务端没有返回任务</div> : null}
        </div>
      </div>

      <div className="card panel">
        <div className="panel-title">特权目录</div>
        {catalog.map((group: any) => (
          <div key={group.id} style={{ marginBottom: 14 }}>
            <div style={{ fontWeight: 600, fontSize: 13, marginBottom: 6 }}>{group.name}</div>
            <div style={{ display: 'flex', flexWrap: 'wrap', gap: 8 }}>
              {(group.items || []).map((item: any) => (
                <span className="chip small" key={item.id} title={item.desc}>{item.name}{item.new ? ' · NEW' : ''}{item.hot ? ' · 热门' : ''}</span>
              ))}
            </div>
          </div>
        ))}
        {!catalog.length && !vipLoading ? <Empty title="暂无特权目录" /> : null}
      </div>
    </>
  );
}

function privilegeLabel(kind: number): string {
  const labels: Record<number, string> = {
    1: 'B币券', 2: '会员购优惠券', 3: '漫画福利券', 4: '会员购包邮券', 5: '漫画商城优惠券',
    6: '装扮体验卡', 7: '课堂优惠券', 8: '每日礼盒', 9: '每日经验',
  };
  return labels[kind] || `特权 ${kind}`;
}

function privilegeState(state: number): string {
  return state === 1 ? '已兑换' : state === 2 ? '未完成' : '未兑换';
}
