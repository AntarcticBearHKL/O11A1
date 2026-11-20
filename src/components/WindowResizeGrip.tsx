import React, { useCallback, useEffect, useRef } from 'react';

/// Bottom-right resize grip for the frameless main window. Chromium's native
/// sizing border is only a few pixels wide and sits outside the 22 px paper
/// rim, so the visible rounded corner is almost impossible to grab. This
/// transparent 36 px square covers the rim and the visible corner, shows the
/// diagonal cursor, and drives the main process's cursor-follow resize loop.
export function WindowResizeGrip({ hidden = false }: { hidden?: boolean }) {
  const active = useRef(false);
  const pointer = useRef<number | null>(null);
  const element = useRef<HTMLDivElement>(null);

  const end = useCallback(() => {
    if (!active.current) return;
    active.current = false;
    const id = pointer.current;
    pointer.current = null;
    try {
      if (id !== null) element.current?.releasePointerCapture?.(id);
    } catch {
      /* pointer capture is best-effort */
    }
    (window as any).o11a1?.window?.endResize?.().catch?.(() => {});
  }, []);

  useEffect(() => {
    // The grip keeps pointer capture, but these listeners also finish the
    // gesture if the window loses focus or the capture is lost mid-drag.
    window.addEventListener('pointerup', end, true);
    window.addEventListener('pointercancel', end, true);
    window.addEventListener('blur', end);
    return () => {
      window.removeEventListener('pointerup', end, true);
      window.removeEventListener('pointercancel', end, true);
      window.removeEventListener('blur', end);
    };
  }, [end]);

  if (hidden) return null;

  return (
    <div
      ref={element}
      className="window-resize-grip no-drag"
      title="拖动调整窗口大小"
      onPointerDown={(event) => {
        if (event.button !== 0) return;
        event.preventDefault();
        event.stopPropagation();
        active.current = true;
        pointer.current = event.pointerId;
        try {
          event.currentTarget.setPointerCapture?.(event.pointerId);
        } catch {
          /* pointer capture is best-effort */
        }
        (window as any).o11a1?.window?.beginResize?.().catch?.(() => {});
      }}
      onPointerUp={end}
      onPointerCancel={end}
      onLostPointerCapture={end}
    />
  );
}
