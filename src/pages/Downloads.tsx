import React, { useCallback, useEffect, useState } from 'react';
import { call } from '../api';
import { CoverImage } from '../components/CoverImage';
import { useApp } from '../context';
import type { DownloadTask } from '../types';
import { Empty } from '../components/Common';
import { IconDownload, IconTrash } from '../components/Icons';

export default function Downloads() {
  const { toast } = useApp();
  const [tasks, setTasks] = useState<DownloadTask[]>([]);
  const [loading, setLoading] = useState(true);
  const previous = React.useRef<Record<string, string>>({});

  const load = useCallback(async () => {
    try {
      const next = await call<DownloadTask[]>('downloads.list');
      for (const task of next) {
        if (previous.current[task.id] && previous.current[task.id] !== 'done' && task.status === 'done') {
          toast(`下载完成：${task.title}`);
          window.o11a1.tray?.balloon('O11A1 下载完成', task.title);
        }
        previous.current[task.id] = task.status;
      }
      setTasks(next);
    } catch (reason: any) {
      toast(reason.message, true);
    } finally {
      setLoading(false);
    }
  }, [toast]);

  useEffect(() => {
    load();
    const timer = setInterval(load, 1000);
    return () => clearInterval(timer);
  }, [load]);

  const cancel = async (task: DownloadTask) => {
    try {
      await call('downloads.cancel', { id: task.id });
      toast('已取消下载');
      load();
    } catch (reason: any) { toast(reason.message, true); }
  };

  const remove = async (task: DownloadTask) => {
    if (!window.confirm(`删除「${task.title}」的下载记录和文件？`)) return;
    try {
      await call('downloads.remove', { id: task.id });
      load();
    } catch (reason: any) { toast(reason.message, true); }
  };

  const statusText = (task: DownloadTask) => {
    if (task.status === 'done') return '已完成';
    if (task.status === 'error') return `失败：${task.error || '未知错误'}`;
    if (task.status === 'cancelled') return '已取消';
    if (task.status === 'queued') return '排队中';
    return `下载中 ${task.progress.toFixed(1)}%${task.speed ? ` · ${task.speed}` : ''}`;
  };

  return (
    <>
      {!tasks.length && !loading ? <Empty title="还没有下载任务" hint="在视频详情页点击「下载」加入队列" /> : null}
      {tasks.length || loading ? (
        <div className="card panel">
          {tasks.map((task) => (
            <div className="download-row" key={task.id}>
              <CoverImage src={task.cover} alt="" style={{ width: 112, aspectRatio: '16/9', objectFit: 'cover', borderRadius: 9 }} />
              <div className="download-copy">
                <div className="download-title">{task.title || task.bvid}</div>
                <div className={`download-state ${task.status === 'error' ? 'error' : ''}`}>{statusText(task)}</div>
                {task.status === 'running' || task.status === 'queued' ? (
                  <div className="bar"><div style={{ width: `${Math.max(0, Math.min(100, task.progress))}%` }} /></div>
                ) : null}
              </div>
              {task.status === 'running' || task.status === 'queued' ? (
                <button className="btn sm" onClick={() => cancel(task)}>取消</button>
              ) : null}
              {task.status === 'done' ? (
                <button className="btn sm" onClick={() => window.o11a1.shell.openPath(task.path)}><IconDownload size={13} /> 打开</button>
              ) : null}
              <button className="btn sm ghost" onClick={() => remove(task)}><IconTrash size={13} /></button>
            </div>
          ))}
          {loading && !tasks.length ? <div className="empty"><span className="spinner" /></div> : null}
        </div>
      ) : null}
    </>
  );
}
