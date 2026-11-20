import React, { useCallback, useEffect, useRef, useState } from 'react';
import QRCode from 'qrcode';
import { call, cover } from '../api';
import { clearPreloadedData } from '../preload';
import { useApp } from '../context';
import { Empty, Spinner } from '../components/Common';
import { LoginQrPlaceholder, LoginSurface, loginQrColors, readLastAccount } from '../components/LoginSurface';
import type { Account } from '../types';

export default function Member() {
  const { account, refreshAccount, toast, navigate } = useApp();
  const [qr, setQr] = useState<{ url: string; key: string } | null>(null);
  const [qrError, setQrError] = useState('');
  const [qrState, setQrState] = useState('正在生成二维码…');
  const [polling, setPolling] = useState(false);
  const [entering, setEntering] = useState(false);
  const [scanned, setScanned] = useState<Account | null>(null);
  const [last, setLast] = useState<Account | null>(() => readLastAccount());
  const [vip, setVip] = useState<any>(null);
  const [nav, setNav] = useState<any>(null);
  const [vipLoading, setVipLoading] = useState(false);
  const [vipError, setVipError] = useState('');
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const pollRef = useRef<any>(null);

  // Keep the in-app login portrait in step with the account that just left.
  useEffect(() => {
    if (account?.mid) setLast(account);
  }, [account]);

  const loadQr = useCallback(async () => {
    setQrError('');
    setQrState('正在生成二维码…');
    setPolling(false);
    try {
      const data = await call<{ url: string; key: string }>('login.qr');
      setQr(data);
      setQrState('打开哔哩哔哩扫一扫');
      setPolling(true);
    } catch {
      setQrError('二维码生成失败，点击重试');
    }
  }, []);

  // Generate on mount, and again after an expired code is cleared.
  useEffect(() => {
    if (account || qr || qrError || entering) return;
    loadQr();
  }, [account, qr, qrError, entering, loadQr]);

  useEffect(() => {
    if (!qr || !canvasRef.current) return;
    QRCode.toCanvas(canvasRef.current, qr.url, {
      width: 240,
      margin: 2,
      color: loginQrColors(),
    }).catch(() => setQrError('二维码生成失败，点击重试'));
  }, [qr]);

  useEffect(() => {
    if (!polling || !qr || entering) return;
    let stopped = false;
    let polls = 0;
    const tick = async () => {
      polls += 1;
      if (polls > 90) {
        setPolling(false);
        setQrState('二维码已过期，正在刷新…');
        setQr(null);
        setQrError('');
        return;
      }
      try {
        const result = await call<{ stateCode: number; account?: Account }>('login.poll', { key: qr.key });
        if (result.stateCode === 0) {
          setPolling(false);
          setEntering(true);
          setScanned(result.account ?? null);
          try {
            await refreshAccount();
            toast('登录成功');
          } catch {
            setEntering(false);
            setQrError('登录成功但没有返回账号信息，点击重试');
          }
          return;
        }
        if (result.stateCode === 86038) {
          setPolling(false);
          setQrState('二维码已过期，正在刷新…');
          setQr(null);
          setQrError('');
        } else if (result.stateCode === 86090) {
          setQrState('已扫码，请在手机上确认');
        } else if (result.stateCode === 86101) {
          setQrState('打开哔哩哔哩扫一扫');
        }
      } catch {
        /* transient poll errors are ignored */
      }
      if (!stopped) pollRef.current = setTimeout(tick, 2000);
    };
    pollRef.current = setTimeout(tick, 1200);
    return () => {
      stopped = true;
      clearTimeout(pollRef.current);
    };
  }, [polling, qr, entering, refreshAccount, toast]);

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
      clearPreloadedData();
      await refreshAccount();
      setVip(null);
      setNav(null);
      toast('已退出登录');
    } catch (reason: any) { toast(reason.message, true); }
  };

  if (!account) {
    if (entering) {
      return (
        <div className="login-embed">
          <LoginSurface account={scanned} progress={null} status="登录成功，正在进入主页…" />
        </div>
      );
    }
    return (
      <div className="login-embed">
        <LoginSurface
          account={last}
          progress={null}
          status={qrError || qrState}
          tone={qrError ? 'error' : 'normal'}
        >
          <div
            className={`login-qr-frame ${qrError ? 'error' : ''}`}
            onClick={() => {
              if (!polling || qrError) loadQr();
            }}
            title={qrError ? '点击重试' : '点击刷新二维码'}
          >
            {qr ? (
              <canvas ref={canvasRef} />
            ) : (
              <LoginQrPlaceholder />
            )}
          </div>
        </LoginSurface>
      </div>
    );
  }

  const page = vip?.page;
  const tasks = vip?.tasks?.tasks || vip?.tasks?.list || [];
  const catalog = vip?.catalog || [];

  return (
    <>
      <section className="profile-hero member-hero">
        <div className="profile-avatar-wrap">
          <img className="profile-avatar" src={cover(account.face)} referrerPolicy="no-referrer" alt="" />
        </div>
        <div className="profile-copy">
          <div className="profile-name">
            <h1>{account.uname}</h1>
            {nav?.LoggedIn ? <span className="chip small">LV{nav.LoggedIn.level}</span> : null}
            {account.vip ? <span className="chip small accent-chip">大会员</span> : null}
          </div>
          <p className="profile-sign">UID {account.mid}</p>
          {nav?.LoggedIn ? (
            <div className="profile-stats">
              <span><b>{nav.LoggedIn.money}</b>硬币</span>
              <span><b>{nav.LoggedIn.moral}</b>节操</span>
              <span><b>{nav.LoggedIn.vip_text}</b>会员状态</span>
            </div>
          ) : null}
          <div className="member-actions">
            <button className="btn sm" onClick={() => navigate('settings')}>账户设置</button>
            <button className="btn sm danger" onClick={logout}>退出登录</button>
          </div>
        </div>
      </section>

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
