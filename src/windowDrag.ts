/**
 * Window dragging without `-webkit-app-region`. Native drag regions swallow
 * right-click events, which would break the 0.3 s refresh gesture. Here every
 * non-interactive area starts a main-process cursor-follow loop on left press,
 * while normal DOM events stay intact.
 */
const INTERACTIVE = [
  'button',
  'a',
  'input',
  'textarea',
  'select',
  'option',
  'label',
  '[role="button"]',
  '[role="tab"]',
  '[role="slider"]',
  '[contenteditable="true"]',
  '.video-card',
  '.pgc-card',
  '.msg-row',
  '.queue-row',
  '.folder-item',
  '.row',
  '.search-hit-card',
  '.dynamic-card .head',
  '.dynamic-archive',
  '.search-dock',
  '.tabs',
  '.chips',
  '.settings-row .control',
  '.account-menu',
  '.playlist-popover',
  '.queue-foot',
  '.folder-actions',
  '.window-controls',
  '.login-window-controls',
  '.no-drag',
].join(',');

function interactive(target: EventTarget | null): boolean {
  const element = target as Element | null;
  return !!element?.closest?.(INTERACTIVE);
}

function onScrollbar(event: PointerEvent): boolean {
  const element = event.target as HTMLElement | null;
  if (!element || element.scrollHeight <= element.clientHeight) return false;
  const rect = element.getBoundingClientRect();
  // Chromium scrollbars are 9 px wide in this app.
  return event.clientX >= rect.left + element.clientWidth;
}

export function installWindowDrag() {
  if ((window as any).__o11a1WindowDrag) return;
  (window as any).__o11a1WindowDrag = true;
  let dragging = false;
  let pointer = -1;
  let captureEl: Element | null = null;

  const stop = () => {
    if (!dragging) return;
    dragging = false;
    (window as any).o11a1?.window?.endDrag?.().catch?.(() => {});
  };

  const down = (event: PointerEvent) => {
    if (event.button !== 0 || event.defaultPrevented) return;
    if (interactive(event.target) || onScrollbar(event)) return;
    dragging = true;
    pointer = event.pointerId;
    captureEl = event.target as Element | null;
    try {
      captureEl?.setPointerCapture?.(event.pointerId);
    } catch {
      /* capture is best-effort */
    }
    (window as any).o11a1?.window?.beginDrag?.().catch?.(() => {});
  };

  const up = () => {
    if (!dragging) return;
    try {
      if (pointer >= 0) captureEl?.releasePointerCapture?.(pointer);
    } catch {
      /* ignore */
    }
    captureEl = null;
    stop();
  };

  window.addEventListener('pointerdown', down, true);
  window.addEventListener('pointerup', up, true);
  window.addEventListener('pointercancel', up, true);
  window.addEventListener('blur', stop);
  window.addEventListener('mousedown', (event) => {
    // Keep CSS ::selection from starting a drag inside non-interactive text.
    if (event.button === 0 && !interactive(event.target)) event.preventDefault();
  }, true);
}
