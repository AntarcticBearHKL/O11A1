import { useCallback, useEffect, useRef, useState } from 'react';

export function useAsync<T>(loader: () => Promise<T>, deps: any[], options: { immediate?: boolean } = {}) {
  const { immediate = true } = options;
  const [data, setData] = useState<T | null>(null);
  const [loading, setLoading] = useState(immediate);
  const [error, setError] = useState('');
  const seq = useRef(0);

  const reload = useCallback(async () => {
    const current = ++seq.current;
    setLoading(true);
    setError('');
    try {
      const result = await loader();
      if (current === seq.current) setData(result);
      return result;
    } catch (reason: any) {
      if (current === seq.current) setError(reason?.message || String(reason));
      return null;
    } finally {
      if (current === seq.current) setLoading(false);
    }
  }, deps);

  useEffect(() => {
    if (immediate) reload();
  }, [reload, immediate]);

  return { data, loading, error, reload, setData };
}

export function useDebounced<T>(value: T, delay = 250): T {
  const [debounced, setDebounced] = useState(value);
  useEffect(() => {
    const timer = setTimeout(() => setDebounced(value), delay);
    return () => clearTimeout(timer);
  }, [value, delay]);
  return debounced;
}

export function useToast() {
  const [toast, setToast] = useState<{ text: string; error?: boolean } | null>(null);
  const timer = useRef<any>(null);
  const show = useCallback((text: string, error = false) => {
    setToast({ text, error });
    clearTimeout(timer.current);
    timer.current = setTimeout(() => setToast(null), 2600);
  }, []);
  useEffect(() => () => clearTimeout(timer.current), []);
  return { toast, show };
}
