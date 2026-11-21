import React, { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { call, cover, relativeTime } from '../api';
import { useApp } from '../context';
import type { DmMessage, DmPage, DmSession, MsgPage } from '../types';
import { Empty, ErrorBox, LoadingText, PageHead, Tabs } from '../components/Common';
import { ImageViewer } from '../components/ImageViewer';

const TABS = ['回复我的', '@我的', '收到的赞', '私信'];
const KINDS = ['reply', 'at', 'like', 'dm'];

/// The session payload only carries a name for system conversations. Normal
/// user sessions are resolved through one card lookup on the engine side; if
/// that lookup fails, show a neutral label instead of inventing 用户<mid>.
function dmDisplayName(session: DmSession | null): string {
  return session?.name?.trim() || '未知用户';
}

function dmMessageText(message: DmMessage): string {
  if (message.msg_status === 1 || message.msg_status === 2) return '[消息已撤回]';
  if (message.msg_status === 50) return '[图片已失效]';
  if (message.text) return message.text;
  if (message.msg_type === 2) return '[图片]';
  return `[消息类型 ${message.msg_type}]`;
}

function dmMessageImage(message: DmMessage): string {
  if (message.msg_status === 1 || message.msg_status === 2 || message.msg_status === 50) return '';
  return message.image || '';
}

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
  const [thread, setThread] = useState<DmSession | null>(null);
  const [threadMessages, setThreadMessages] = useState<DmMessage[]>([]);
  const [threadMore, setThreadMore] = useState(false);
  const [threadError, setThreadError] = useState('');
  const [minSeqno, setMinSeqno] = useState(0);
  const [viewer, setViewer] = useState<{ images: string[]; index: number } | null>(null);

  const chatRef = useRef<HTMLDivElement>(null);
  const scrollToBottomRef = useRef(false);
  const prependRef = useRef<{ height: number; top: number } | null>(null);
  const threadRequestRef = useRef(0);

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
    setThreadMessages([]);
    setThreadError('');
    setViewer(null);
    setCursor(null);
    setHasMore(false);
    load(tab);
  }, [tab, load]);

  const openThread = async (session: DmSession) => {
    const token = ++threadRequestRef.current;
    prependRef.current = null;
    setViewer(null);
    setThread(session);
    setThreadMessages([]);
    setThreadMore(false);
    setThreadError('');
    setMinSeqno(0);
    setLoading(true);
    try {
      const page = await call<DmPage>('messages.dm.messages', { talker: session.talker_id });
      if (token !== threadRequestRef.current) return;
      scrollToBottomRef.current = true;
      setThreadMessages(page.messages);
      setThreadMore(page.has_more);
      setMinSeqno(page.min_seqno);
    } catch (reason: any) {
      if (token !== threadRequestRef.current) return;
      setThreadError(reason?.message || String(reason));
      toast(reason.message, true);
    } finally {
      if (token === threadRequestRef.current) setLoading(false);
    }
  };

  const loadEarlier = async () => {
    if (!thread || !minSeqno) return;
    const node = chatRef.current;
    if (node) prependRef.current = { height: node.scrollHeight, top: node.scrollTop };
    try {
      const page = await call<DmPage>('messages.dm.more', { talker: thread.talker_id, seqno: minSeqno });
      setThreadMessages((current) => [...page.messages, ...current]);
      setThreadMore(page.has_more);
      setMinSeqno(page.min_seqno);
    } catch (reason: any) {
      prependRef.current = null;
      toast(reason.message, true);
    }
  };

  // The list is the entry point; the right pane stays empty until the user
  // picks a conversation, so a system notice is never opened by accident.

  useLayoutEffect(() => {
    const node = chatRef.current;
    if (!node) return;
    if (prependRef.current) {
      const { height, top } = prependRef.current;
      prependRef.current = null;
      node.scrollTop = top + (node.scrollHeight - height);
      return;
    }
    if (scrollToBottomRef.current) {
      scrollToBottomRef.current = false;
      node.scrollTop = node.scrollHeight;
    }
  }, [threadMessages]);

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

  const threadImages = threadMessages
    .map((message) => dmMessageImage(message))
    .filter((url): url is string => Boolean(url));

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
        <div className="dm-layout">
          <div className="dm-sessions">
            {sessions.map((session) => {
              const active = thread?.talker_id === session.talker_id;
              return (
                <button
                  type="button"
                  className={`dm-session${active ? ' active' : ''}`}
                  key={session.talker_id}
                  onClick={() => { void openThread(session); }}
                >
                  <span className="dm-session-line">
                    <span className="dm-session-name">{dmDisplayName(session)}</span>
                    <span className="dm-session-time">{session.at_time ? relativeTime(session.at_time) : ''}</span>
                  </span>
                  <span className="dm-session-preview-line">
                    <span className="dm-session-preview">{session.preview || '暂无消息'}</span>
                    {session.unread ? <span className="dm-session-unread">{session.unread > 99 ? '99+' : session.unread}</span> : null}
                  </span>
                </button>
              );
            })}
            {!sessions.length ? <div className="dm-empty">没有私信会话</div> : null}
          </div>

          <div className="dm-chat">
            {thread ? (
              <>
                <div className="dm-chat-head">
                  <span className="dm-chat-name">{dmDisplayName(thread)}</span>
                  {threadMore ? <button type="button" className="dm-more" onClick={loadEarlier}>加载更早</button> : null}
                </div>
                <div className="dm-thread" ref={chatRef}>
                  {threadError ? <div className="dm-empty">{threadError}</div> : null}
                  {!threadError && loading && !threadMessages.length ? <div className="dm-empty">正在加载…</div> : null}
                  {!threadError ? threadMessages.map((message, index) => {
                    const mine = message.sender_uid === account.mid;
                    const previous = index > 0 ? threadMessages[index - 1] : null;
                    const firstOfGroup = !previous || (previous.sender_uid === account.mid) !== mine;
                    const image = dmMessageImage(message);
                    const imageIndex = image
                      ? threadMessages.slice(0, index).filter((row) => dmMessageImage(row)).length
                      : -1;
                    return (
                      <div className={`dm-line ${mine ? 'mine' : ''}`} key={message.seqno}>
                        {firstOfGroup ? <div className="dm-who">{mine ? '我' : dmDisplayName(thread)}</div> : null}
                        {image ? (
                          <button
                            type="button"
                            className="dm-image"
                            onClick={() => setViewer({ images: threadImages, index: imageIndex })}
                            aria-label="查看图片"
                          >
                            <img src={image} alt="" loading="lazy" referrerPolicy="no-referrer" draggable={false} />
                          </button>
                        ) : (
                          <div className="dm-text">{dmMessageText(message)}</div>
                        )}
                        <div className="dm-time">{message.timestamp ? relativeTime(message.timestamp) : ''}</div>
                      </div>
                    );
                  }) : null}
                  {!threadError && !loading && !threadMessages.length ? <div className="dm-empty">这个会话还没有消息</div> : null}
                </div>
              </>
            ) : (
              <div className="dm-empty">选择左侧会话查看私信</div>
            )}
          </div>
        </div>
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

      {viewer ? (
        <ImageViewer
          images={viewer.images}
          initialIndex={viewer.index}
          onClose={() => setViewer(null)}
        />
      ) : null}
    </>
  );
}
