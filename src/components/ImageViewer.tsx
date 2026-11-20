import React, { useEffect, useState } from 'react';
import { createPortal } from 'react-dom';
import { IconBack, IconClose } from './Icons';

type Props = {
  images: string[];
  index: number;
  onClose: () => void;
  onIndexChange: (index: number) => void;
};

/// Full-window image viewer for dynamic image posts. The image fits the window
/// until it is clicked; then it switches to its natural size inside a scroll
/// surface so fine details can be inspected. Arrow keys / buttons walk the
/// post's full image list.
export function ImageViewer({ images, index, onClose, onIndexChange }: Props) {
  const [zoomed, setZoomed] = useState(false);
  const count = images.length;
  const current = images[index] || '';
  const multiple = count > 1;

  useEffect(() => {
    setZoomed(false);
  }, [index]);

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
        onIndexChange((index - 1 + count) % count);
      } else if (event.key === 'ArrowRight') {
        event.preventDefault();
        onIndexChange((index + 1) % count);
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [count, index, multiple, onClose, onIndexChange]);

  if (!current) return null;

  const step = (delta: number) => onIndexChange((index + delta + count) % count);

  const host = document.querySelector('.app') || document.body;

  return createPortal(
    <div className="image-viewer" role="dialog" aria-modal="true" aria-label="图片查看">
      <div className="image-viewer-top">
        <span className="image-viewer-count">{index + 1} / {count}</span>
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
          src={current}
          alt={`图片 ${index + 1}`}
          referrerPolicy="no-referrer"
          draggable={false}
          title={zoomed ? '单击恢复适应窗口' : '单击查看原始大小'}
          onClick={(event) => {
            event.stopPropagation();
            setZoomed((value) => !value);
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
