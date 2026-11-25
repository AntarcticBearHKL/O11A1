/// The startup preloader and the 换一换 refresh buffer are sized as seven-row
/// batches. The home feed itself is now an infinite sliding window; this batch
/// only covers the first paint and each refresh.

export const FEED_ROWS = 7;
export const MAX_FEED_COLUMNS = 8;

/// Match the CSS grid breakpoints in redesign.css:
/// - <=720px: two fixed columns
/// - <=1260px: 28px page padding, 206px min card, 16px gap
/// - above: 36px page padding, 210px min card, 22px gap
/// `.page-inner` caps the track at 1460px; the scrollbar allowance avoids
/// asking for a column that does not actually fit.
export function feedColumns(viewport = typeof window === 'undefined' ? 1280 : window.innerWidth): number {
  if (viewport <= 720) return 2;
  const compact = viewport <= 1260;
  const padding = compact ? 28 : 36;
  const gap = compact ? 16 : 22;
  const min = compact ? 206 : 210;
  const grid = Math.min(viewport - padding * 2 - 14, 1460);
  const columns = Math.floor((grid + gap) / (min + gap));
  return Math.max(1, Math.min(MAX_FEED_COLUMNS, columns));
}

/// Number of recommendations to request for seven rows at this width.
export function feedBatchCount(viewport?: number): number {
  return FEED_ROWS * feedColumns(viewport);
}
