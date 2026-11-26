import React, { useEffect, useMemo, useRef, useState } from 'react';
import { cardCover, cover } from '../api';

type Props = React.ImgHTMLAttributes<HTMLImageElement> & {
  src?: string | null;
  /// Above-the-fold cards load immediately; everything else stays lazy.
  eager?: boolean;
};

function retryUrl(url: string): string {
  const sep = url.includes('?') ? '&' : '?';
  return `${url}${sep}o11a1_retry=${Date.now()}`;
}

/// One resilient cover image for every video card.
///
/// The CDN rendition can fail independently of the original file (missing
/// @480w variant, transient CDN error, empty payload), so walk:
/// 480px thumbnail → original URL → one cache-busted retry → neutral fallback.
/// A failed image never leaves a broken icon or an empty box behind.
export function CoverImage({ src, eager, className, alt = '', onLoad, ...rest }: Props) {
  const full = cover(src || '');
  const thumb = cardCover(src || '');
  const candidates = useMemo(() => {
    if (!full) return [];
    const retryable = /^https?:\/\//i.test(full);
    if (thumb && thumb !== full) {
      return retryable ? [thumb, full, retryUrl(full)] : [thumb, full];
    }
    return retryable ? [full, retryUrl(full)] : [full];
  }, [full, thumb]);
  const [attempt, setAttempt] = useState<{ key: string; index: number }>({ key: full, index: 0 });
  const index = attempt.key === full ? attempt.index : 0;
  const failed = !candidates.length || index >= candidates.length;
  const autoRetried = useRef(false);

  useEffect(() => {
    autoRetried.current = false;
  }, [full]);

  // One delayed retry after every candidate has failed. This recovers a card
  // whose first request happened while the network was briefly unavailable,
  // without retrying forever when the URL is genuinely gone.
  useEffect(() => {
    if (!failed || !full || autoRetried.current) return;
    const timer = window.setTimeout(() => {
      autoRetried.current = true;
      setAttempt({ key: full, index: 0 });
    }, 4000);
    return () => window.clearTimeout(timer);
  }, [failed, full]);

  if (failed) {
    return <span className={`cover-fallback ${className ?? ''}`} style={rest.style} aria-hidden="true" />;
  }

  return (
    <img
      {...rest}
      className={className}
      src={candidates[index]}
      alt={alt}
      loading={eager ? 'eager' : 'lazy'}
      decoding="async"
      referrerPolicy="no-referrer"
      draggable={false}
      onLoad={(event) => {
        // Decode while the card is still offscreen; otherwise the first
        // scroll into a row pays the image decode on the presentation frame.
        event.currentTarget.decode?.().catch(() => {});
        onLoad?.(event);
      }}
      onError={() => setAttempt({ key: full, index: index + 1 })}
    />
  );
}
