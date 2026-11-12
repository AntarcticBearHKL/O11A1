import React, { useCallback, useEffect, useState } from 'react';
import { call, cover, relativeTime } from '../api';
import { useApp } from '../context';
import type { DmMessage, DmPage, DmSession, MsgPage } from '../types';
import { Empty, ErrorBox, PageHead, SkeletonGrid } from '../components/Common';

const TABS = ['回复我的', '@我的', '收到的赞', '私信'];
const KINDS = ['reply', 'at', 'like', 'dm'];

export default function Messages() {
  const { account, navigate, toast } = useApp();
  const [tab, setTab] = useState(0);
  const [unread, setUnread] = useState<any>(null);
  const [items, setItems] = useState<any[]>([]);
  const [cursor, setCursor] = useState<any>(null);
  const [hasMore, setHasMore] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const [sessions, setSessions] = useState<DmSession[]>([]);
  const [thread, setThread] = useState<any>(null);
  const [threadMessages, setThreadMessages] = useState<DmMessage[]>([]);
  const [threadMore, setThreadMore] = useState(false);
  const [minSeqno, setMinSeqno] = useState(0);

  const loadUnread = useCallback(() => {
    call('messages.unread').then(setUnread).catch(() => {});
  }, []);

  const load = useCallback(async (nextTab: number, pageCursor: any = null, append = false) => {
    if (!account) return;
    setLoading(true);
    setError('');
    try {
      if (nextTab === 3) {
        const rows = await call<DmSession[]>('messages.dm.sessions');
        setSessions(rows);
      } else {
        const page = pageCursor
          ? await call<MsgPage>('messages.page', { kind: KINDS[nextTab], cursor: pageCursor })
          : await call<MsgPage>('messages.list', { kind: KINDS[nextTab] });
        setItems((current) => append ? [...current, ...page.items] : page.items);
        setCursor(page.cursor);
        setHasMore(!page.is_end);
      }
    } catch (reason: any) { setError(reason?.message || String(reason)); }
    finally { setLoading(false); }
  }, [account]);

  useEffect(() => {
    loadUnread();
  }, [loadUnread]);

  useEffect(() => {
    setItems([]);
    setSessions([]);
    setThread(null);
    setCursor(null);
    setHasMore(false);
    load(tab);
  }, [tab, load]);

  const openThread = async (session: DmSession) => {
    setThread(session);
    setLoading(true);
    try {
      const page = await call<DmPage>('messages.dm.messages', { talker: session.talker_id });
      setThreadMessages(page.messages);
      setThreadMore(page.has_more);
      setMinSeqno(page.min_seqno);
    } catch (reason: any) { toast(reason.message, true); }
    finally { setLoading(false); }
  };

  const loadEarlier = async () => {
    if (!thread || !minSeqno) return;
    try {
      const page = await call<DmPage>('messages.dm.more', { talker: thread.talker_id, seqno: minSeqno });
      setThreadMessages((current) => [...page.messages, ...current]);
      setThreadMore(page.has_more);
      setMinSeqno(page.min_seqno);
    } catch (reason: any) { toast(reason.message, true); }
  };

  useEffect(() => {
    const onRefresh = () => {
      loadUnread();
      if (tab === 3 && thread) openThread(thread);
      else load(tab);
    };
    window.addEventListener('o11a1:refresh', onRefresh);
    return () => window.removeEventListener('o11a1:refresh', onRefresh);
    // openThread is intentionally omitted: it is recreated on every render.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [load, loadUnread, tab, thread]);

  if (!account) {
    return (
      <>
        <PageHead title="消息" sub="回复 / @ / 收到的赞 / 私信" />
        <div className="empty"><strong>请先登录</strong><button className="btn primary" onClick={() => navigate('member')}>去登录</button></div>
      </>
    );
  }

  return (
    <>
      <PageHead title="消息" sub={unread ? `未读 ${unread.total}` : '回复 / @ / 收到的赞 / 私信'}>
        <div className="tabs">
          {TABS.map((label, index) => (
            <button key={label} className={`tab ${tab === index ? 'active' : ''}`} onClick={() => setTab(index)}>
              {label}
              {index < 3 && unread ? ` ${[unread.reply, unread.at, unread.like][index]}` : ''}
            </button>
          ))}
        </div>
      </PageHead>

      {loading && !items.length && !sessions.length ? <SkeletonGrid count={3} /> : error ? <ErrorBox error={error} onRetry={() => load(tab)} /> : null}

      {tab === 3 ? (
        thread ? (
          <div className="card panel">
            <div style={{ display: 'flex', alignItems: 'center', gap: 10, marginBottom: 12 }}>
              <button className="btn sm" onClick={() => { setThread(null); setThreadMessages([]); }}>返回会话</button>
              <div style={{ fontWeight: 600 }}>会话 {thread.talker_id}</div>
              <div style={{ flex: 1 }} />
              {threadMore ? <button className="btn sm" onClick={loadEarlier}>加载更早的消息</button> : null}
            </div>
            <div style={{ display: 'flex', flexDirection: 'column', gap: 8, maxHeight: 520, overflow: 'auto', padding: '8px 2px' }}>
              {threadMessages.map((message) => (
                <div key={message.seqno} style={{ alignSelf: message.sender_uid === account.mid ? 'flex-end' : 'flex-start', maxWidth: '72%' }}>
                  <div style={{
                    padding: '8px 12px', borderRadius: 12, fontSize: 12.5, lineHeight: 1.5,
                    background: message.sender_uid === account.mid ? 'var(--accent)' : 'var(--card-solid)',
                    color: message.sender_uid === account.mid ? '#fff' : 'var(--text)',
                    border: message.sender_uid === account.mid ? 'none' : '1px solid var(--card-border)',
                  }}>{message.text || `[消息类型 ${message.msg_type}]`}</div>
                  <div style={{ fontSize: 10, color: 'var(--text-faint)', marginTop: 3, textAlign: message.sender_uid === account.mid ? 'right' : 'left' }}>
                    {message.timestamp ? relativeTime(message.timestamp) : ''}
                  </div>
                </div>
              ))}
              {!threadMessages.length ? <Empty title="这个会话还没有消息" /> : null}
            </div>
          </div>
        ) : (
          <div className="card panel">
            {sessions.map((session) => (
              <div className="msg-row" key={session.talker_id} onClick={() => openThread(session)}>
                <div className="avatar" style={{ display: 'grid', placeItems: 'center', fontSize: 12 }}>{session.talker_id % 100}</div>
                <div style={{ flex: 1, minWidth: 0 }}>
                  <div style={{ display: 'flex', gap: 8 }}>
                    <div style={{ fontWeight: 600, fontSize: 12.5 }}>用户 {session.talker_id}</div>
                    <div style={{ fontSize: 10.5, color: 'var(--text-faint)' }}>{relativeTime(session.at_time)}</div>
                  </div>
                  <div className="detail">{session.preview}</div>
                </div>
                {session.unread ? <span className="chip small active">{session.unread}</span> : null}
              </div>
            ))}
            {!sessions.length ? <Empty title="没有私信会话" /> : null}
          </div>
        )
      ) : (
        <div className="card panel">
          {items.map((item: any) => (
            <div className="msg-row" key={item.id} onClick={() => item.title && window.o11a1.shell.open(item.title.startsWith('http') ? item.title : '')}>
              <img className="avatar" src={cover(item.avatar)} referrerPolicy="no-referrer" alt="" onError={(event) => { (event.target as HTMLImageElement).style.display = 'none'; }} />
              <div style={{ flex: 1, minWidth: 0 }}>
                <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
                  <div style={{ fontWeight: 600, fontSize: 12.5 }}>{item.nickname}</div>
                  <div style={{ color: 'var(--text-faint)', fontSize: 10.5 }}>{relativeTime(item.time)}</div>
                  {item.count > 1 ? <span className="chip small">{item.count}</span> : null}
                </div>
                <div className="detail">{item.title || item.detail}</div>
              </div>
            </div>
          ))}
          {!items.length ? <Empty title="今天没有新消息" /> : null}
          {hasMore ? <div style={{ display: 'flex', justifyContent: 'center', paddingTop: 12 }}><button className="btn" onClick={() => load(tab, cursor, true)}>加载更多</button></div> : null}
        </div>
      )}
    </>
  );
}
