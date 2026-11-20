/**
 * One-shot page cache for data warmed in the background before the shell is
 * entered. `call()` consumes an entry the first time a page asks for the same
 * command + params, so the first paint has data without a second network wait;
 * later refreshes go straight to the engine.
 *
 * Entries are short-lived, memory-only and cleared on logout so one account's
 * favourites, history or messages can never be served to another.
 */
type Entry = { data: any; at: number };

const TTL = 15 * 60_000;
const cache = new Map<string, Entry>();

function stable(value: any): string {
  if (value === null || value === undefined) return String(value);
  if (Array.isArray(value)) return `[${value.map(stable).join(',')}]`;
  if (typeof value === 'object') {
    return `{${Object.keys(value).sort().map((key) => `${key}:${stable(value[key])}`).join(',')}}`;
  }
  return JSON.stringify(value);
}

function cacheKey(method: string, params: Record<string, any> = {}): string {
  return `${method}?${stable(params)}`;
}

export function putPageCache(method: string, params: Record<string, any>, data: any): void {
  if (data === undefined || data === null) return;
  cache.set(cacheKey(method, params), { data, at: Date.now() });
}

export function takePageCache<T>(
  method: string,
  params: Record<string, any> = {},
): { hit: boolean; data?: T } {
  const key = cacheKey(method, params);
  const entry = cache.get(key);
  if (!entry) return { hit: false };
  cache.delete(key);
  if (Date.now() - entry.at > TTL) return { hit: false };
  return { hit: true, data: entry.data as T };
}

export function clearPageCache(): void {
  cache.clear();
}
