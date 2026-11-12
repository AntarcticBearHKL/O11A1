import { call } from './api';

export interface LaunchOptions {
  bvid?: string;
  cid?: number;
  aid?: number;
  title?: string;
  cover?: string;
  startMs?: number;
  quality?: number;
  url?: string;
  audioUrl?: string;
  audioIndex?: number;
  parts?: Array<{ cid: number; title: string; index: number }>;
  partIndex?: number;
  seasonId?: number;
  epId?: number;
}

export async function launchPlayer(options: LaunchOptions): Promise<void> {
  await window.o11a1.player.launch(options);
}

/// Resolve one PGC episode: combined durl first, then DASH video + audio as a
/// fallback for episodes where `fnval=1` returns no durl.
export async function resolvePgcSource(epId: number, cid: number): Promise<{ url: string; audioUrl?: string }> {
  const combined = await call<any>('pgc.playurl', { epId, cid });
  if (combined.code !== 0) throw new Error(combined.message || `播放失败（code=${combined.code}）`);
  const url = combined.durl?.[0]?.url;
  if (url) return { url };
  const dash = await call<any>('pgc.playurl_dash', { epId, cid });
  if (dash.code !== 0) throw new Error(dash.message || `播放失败（code=${dash.code}）`);
  const videos = (dash.video || []).slice().sort((a: any, b: any) => {
    const score = (track: any) => (track.codecid === 7 ? 3 : track.codecid === 12 ? 2 : 1) * 1_000_000 + (track.bandwidth || 0);
    return score(b) - score(a);
  });
  const audios = (dash.audio || []).slice().sort((a: any, b: any) => (b.bandwidth || 0) - (a.bandwidth || 0));
  const videoUrl = videos[0]?.base_url || videos[0]?.backup_urls?.[0];
  const audioUrl = audios[0]?.base_url || audios[0]?.backup_urls?.[0];
  if (!videoUrl) throw new Error('没有可用的播放地址');
  return { url: videoUrl, audioUrl };
}

/// Merge one queue row the way the native client does: dedupe by bvid, move
/// the current marker instead of appending a duplicate, keep the newest 200.
async function queueWrite(entry: Record<string, any>, makeCurrent: boolean): Promise<void> {
  if (!entry?.bvid) return;
  const settings = await call<any>('settings.get');
  const playlist = Array.isArray(settings.playlist) ? settings.playlist.map((row: any) => ({ ...row })) : [];
  const index = playlist.findIndex((row: any) => row.bvid === entry.bvid);
  const merge = (base: any) => {
    const next = { ...base };
    for (const key of ['title', 'owner', 'duration', 'cover']) {
      if (String(entry[key] || '').trim()) next[key] = entry[key];
    }
    if (Number(entry.cid) > 0) next.cid = entry.cid;
    if (Number(entry.aid) > 0) next.aid = entry.aid;
    next.bvid = entry.bvid;
    return next;
  };
  let current = Number.isInteger(settings.playlist_current) ? settings.playlist_current : -1;
  if (index >= 0) {
    playlist[index] = merge(playlist[index]);
    if (makeCurrent) current = index;
  } else {
    playlist.push(merge({ bvid: entry.bvid, cid: 0, aid: 0, title: '', owner: '', duration: '', cover: '' }));
    if (playlist.length > 200) playlist.splice(0, playlist.length - 200);
    if (makeCurrent) current = playlist.length - 1;
  }
  await call('settings.patch', {
    playlist,
    playlist_current: current >= 0 && current < playlist.length ? current : null,
  });
}

export async function enqueuePlaylist(entry: Record<string, any>): Promise<void> {
  await queueWrite(entry, true);
}

export async function addToPlaylist(entry: Record<string, any>): Promise<void> {
  await queueWrite(entry, false);
}
