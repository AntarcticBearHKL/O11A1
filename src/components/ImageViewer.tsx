import React, { useEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { IconBack, IconClose } from './Icons';

type Props = {
  images: string[];
  initialIndex?: number;
  onClose: () => void;
};

/// A window-sized rendition for Bilibili CDN images. Walking a post should not
/// decode a 4K original for every arrow press; the original is only requested
/// when the user asks for 原始大小.
function viewerSource(url: string, original: boolean): string {
  if (original || !/hdslb\.com\/bfs\//i.test(url)) return url;
  const [path, query] = url.split('?');
  if (/@[^/]+$/.test(path) || /\.gif$/i.test(path)) return url;
  return `${path}@1600w_1600h_1c.webp${query ? `?${query}` : ''}`;
}

/// Full-window image viewer for dynamic image posts and private-message
/// images. The image fits the window until it is clicked; then it switches to
/// its natural size inside a scroll surface so fine details can be inspected.
/// Arrow keys / buttons walk the image list without lifting the index into the
/// page, so switching images never re-renders the feed behind the viewer.
export function ImageViewer({ images, initialIndex = 0, onClose }: Props) {
  const count = images.length;
  const [active, setActive] = useState(() => {
    const value = Math.min(Math.max(initialIndex, 0), Math.max(count - 1, 0));
    return Number.isFinite(value) ? value : 0;
  });
  const [zoomed, setZoomed] = useState(false);
  const preloaded = useRef<Set<string>>(new Set());
  const current = images[active] || '';
  const multiple = count > 1;

  useEffect(() => {
    const value = Math.min(Math.max(initialIndex, 0), Math.max(count - 1, 0));
    setActive(Number.isFinite(value) ? value : 0);
    setZoomed(false);
  }, [initialIndex, count]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.preventDefault();
        onClose();
        return;
      }
      if (!multiple) return;
      if (event.key === 'ArrowLeft') {
        event.preventDefault();
        setActive((value) => (value - 1 + count) % count);
        setZoomed(false);
      } else if (event.key === 'ArrowRight') {
        event.preventDefault();
        setActive((value) => (value + 1) % count);
        setZoomed(false);
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [count, multiple, onClose]);

  // Keep the current image and its neighbours warm. The set makes the effect
  // idempotent even when the parent re-renders with a new array identity.
  useEffect(() => {
    if (!count) return;
    for (const offset of [0, 1, 2, -1]) {
      const url = viewerSource(images[(active + offset + count) % count] || '', false);
      if (!url || preloaded.current.has(url)) continue;
      preloaded.current.add(url);
      const image = new Image();
      image.decoding = 'async';
      image.referrerPolicy = 'no-referrer';
      image.src = url;
    }
  }, [active, count, images]);

  if (!current) return null;

  const step = (delta: number) => {
    if (!multiple) return;
    setActive((value) => (value + delta + count) % count);
    setZoomed(false);
  };

  const host = document.querySelector('.app') || document.body;
  const source = zoomed ? current : viewerSource(current, false);

  return createPortal(
    <div className="image-viewer" role="dialog" aria-modal="true" aria-label="图片查看">
      <div className="image-viewer-top">
        <span className="image-viewer-count">{active + 1} / {count}</span>
        <div className="image-viewer-actions">
          <button type="button" className="image-viewer-button" onClick={() => setZoomed((value) => !value)}>
            {zoomed ? '适应窗口' : '原始大小'}
          </button>
          <button type="button" className="image-viewer-button icon" onClick={onClose} aria-label="关闭图片查看">
            <IconClose size={17} />
          </button>
        </div>
      </div>

      {multiple ? (
        <button type="button" className="image-viewer-nav prev" onClick={() => step(-1)} aria-label="上一张">
          <IconBack size={22} />
        </button>
      ) : null}

      <div
        className={`image-viewer-stage ${zoomed ? 'zoomed' : ''}`}
        onClick={(event) => { if (event.target === event.currentTarget) onClose(); }}
      >
        <img
          key={active}
          src={source}
          alt={`图片 ${active + 1}`}
          referrerPolicy="no-referrer"
          decoding="async"
          loading="eager"
          draggable={false}
          title={zoomed ? '单击恢复适应窗口' : '单击查看原始大小'}
          onClick={(event) => {
            event.stopPropagation();
            setZoomed((value) => !value);
          }}
          onError={(event) => {
            const image = event.currentTarget;
            if (zoomed || image.dataset.fallback === '1') return;
            image.dataset.fallback = '1';
            image.src = current;
          }}
        />
      </div>

      {multiple ? (
        <button type="button" className="image-viewer-nav next" onClick={() => step(1)} aria-label="下一张">
          <IconBack size={22} />
        </button>
      ) : null}

      <div className="image-viewer-hint">
        {zoomed ? '滚动查看细节 · 单击图片恢复适应窗口' : '单击图片查看原始大小 · Esc 关闭'}
      </div>
    </div>,
    host,
  );
}
