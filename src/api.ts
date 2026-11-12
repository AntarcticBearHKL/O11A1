declare global {
  interface Window {
    o11a1: any;
  }
}

export async function call<T = any>(method: string, params: Record<string, any> = {}): Promise<T> {
  const response = await window.o11a1.call(method, params);
  if (!response?.ok) throw new Error(response?.error || '引擎调用失败');
  return response.data as T;
}

export function formatCount(value: number | string | undefined | null): string {
  const number = Number(value || 0);
  if (number >= 100_000_000) return `${(number / 100_000_000).toFixed(1)}亿`;
  if (number >= 10_000) return `${(number / 10_000).toFixed(1)}万`;
  if (number >= 1_000) return `${(number / 1_000).toFixed(1)}k`;
  return String(number);
}

export function formatDuration(seconds: number | undefined | null): string {
  const total = Math.max(0, Math.floor(Number(seconds || 0)));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  return h > 0 ? `${h}:${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}` : `${m}:${String(s).padStart(2, '0')}`;
}

export function formatTime(ms: number | undefined | null): string {
  return formatDuration(Math.floor(Number(ms || 0) / 1000));
}

export function formatDate(ts: number | undefined | null): string {
  if (!ts) return '';
  const date = new Date(Number(ts) * 1000);
  const now = new Date();
  const sameYear = date.getFullYear() === now.getFullYear();
  const month = date.getMonth() + 1;
  const day = date.getDate();
  if (sameYear) return `${month}月${day}日`;
  return `${date.getFullYear()}年${month}月${day}日`;
}

export function relativeTime(ts: number | undefined | null): string {
  if (!ts) return '';
  const diff = Date.now() / 1000 - Number(ts);
  if (diff < 60) return '刚刚';
  if (diff < 3600) return `${Math.floor(diff / 60)} 分钟前`;
  if (diff < 86400) return `${Math.floor(diff / 3600)} 小时前`;
  if (diff < 86400 * 7) return `${Math.floor(diff / 86400)} 天前`;
  return formatDate(ts);
}

export function cover(url: string | undefined | null): string {
  if (!url) return '';
  if (url.startsWith('//')) return `https:${url}`;
  if (url.startsWith('http://')) return `https://${url.slice(7)}`;
  return url;
}

export function stripHtml(value: string): string {
  return String(value || '').replace(/<[^>]*>/g, '');
}
