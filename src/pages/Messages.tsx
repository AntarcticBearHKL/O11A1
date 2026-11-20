import React, { useCallback, useEffect, useState } from 'react';
import { call, cover, relativeTime } from '../api';
import { useApp } from '../context';
import type { DmMessage, DmPage, DmSession, MsgPage } from '../types';
import { Empty, ErrorBox, LoadingText, PageHead, Tabs } from '../components/Common';

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
        <PageHead eyebrow="社区" title="消息" sub="回复 / @ / 收到的赞 / 私信" />
        <div className="empty"><strong>请先登录</strong><button className="btn primary" onClick={() => navigate('member')}>去登录</button></div>
      </>
    );
  }

  return (
    <>
      <PageHead eyebrow="社区" title="消息" sub={unread ? `未读 ${unread.total}` : '回复 / @ / 收到的赞 / 私信'}>
        <Tabs
          tabs={TABS.map((label, index) => (
            index < 3 && unread && [unread.reply, unread.at, unread.like][index]
              ? `${label} ${[unread.reply, unread.at, unread.like][index]}`
              : label
          ))}
          active={tab}
          onChange={setTab}
        />
      </PageHead>

      {loading && !items.length && !sessions.length ? <LoadingText label={tab === 3 ? '正在加载私信' : '正在加载消息'} /> : error ? <ErrorBox error={error} onRetry={() => load(tab)} /> : null}

      {!error ? (tab === 3 ? (
        thread ? (
          <div className="dm-view">
            <div className="dm-head">
              <button type="button" className="btn sm" onClick={() => { setThread(null); setThreadMessages([]); }}>返回会话</button>
              <div className="dm-head-title">用户 {thread.talker_id}</div>
              <span className="dm-head-spacer" />
              {threadMore ? <button type="button" className="btn sm" onClick={loadEarlier}>加载更早</button> : null}
            </div>
            <div className="dm-thread">
              {threadMessages.map((message, index) => {
                const mine = message.sender_uid === account.mid;
                const previous = index > 0 ? threadMessages[index - 1] : null;
                const firstOfGroup = !previous || (previous.sender_uid === account.mid) !== mine;
                return (
                  <div className={`dm-line ${mine ? 'mine' : ''}`} key={message.seqno}>
                    {firstOfGroup ? <div className="dm-who">{mine ? '我' : `用户 ${thread.talker_id}`}</div> : null}
                    <div className="dm-text">{message.text || `[消息类型 ${message.msg_type}]`}</div>
                    <div className="dm-time">{message.timestamp ? relativeTime(message.timestamp) : ''}</div>
                  </div>
                );
              })}
              {!threadMessages.length ? <Empty title="这个会话还没有消息" /> : null}
            </div>
          </div>
        ) : (
          <div className="msg-list">
            {sessions.map((session) => (
              <div className="msg-row" key={session.talker_id} onClick={() => openThread(session)}>
                <div className="avatar msg-avatar">{session.talker_id % 100}</div>
                <div className="msg-main">
                  <div className="msg-line1">
                    <span className="msg-name">用户 {session.talker_id}</span>
                    <span className="msg-time">{relativeTime(session.at_time)}</span>
                  </div>
                  <div className="detail">{session.preview}</div>
                </div>
                {session.unread ? <span className="msg-unread">{session.unread}</span> : null}
              </div>
            ))}
            {!sessions.length ? <Empty title="没有私信会话" /> : null}
          </div>
        )
      ) : (
        <div className="msg-list">
          {items.map((item: any) => (
            <div className="msg-row" key={item.id} onClick={() => item.title && window.o11a1.shell.open(item.title.startsWith('http') ? item.title : '')}>
              <img className="avatar" src={cover(item.avatar)} referrerPolicy="no-referrer" alt="" onError={(event) => { (event.target as HTMLImageElement).style.display = 'none'; }} />
              <div className="msg-main">
                <div className="msg-line1">
                  <span className="msg-name">{item.nickname}</span>
                  <span className="msg-time">{relativeTime(item.time)}</span>
                  {item.count > 1 ? <span className="msg-count">{item.count}</span> : null}
                </div>
                <div className="detail">{item.title || item.detail}</div>
              </div>
            </div>
          ))}
          {!items.length ? <Empty title="今天没有新消息" /> : null}
          {hasMore ? <div className="page-more"><button className="btn" onClick={() => load(tab, cursor, true)}>加载更多</button></div> : null}
        </div>
      )) : null}
    </>
  );
}
