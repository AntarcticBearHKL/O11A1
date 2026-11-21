/// Right-button gesture arbitration shared by the shell refresh gesture and
/// video cards.
///
/// A short right click on a card opens a new player window. Holding the right
/// button for the shell's 400 ms refresh ring owns the gesture instead, so
/// releasing over a card must not also launch it. Chromium can deliver
/// `contextmenu` before or after `mouseup`, so the card action is held until
/// mouseup proves the gesture was short.

let holdRefreshFired = false;
let gestureMoved = false;
let rightButtonHeld = false;
let pendingOpen: (() => void) | null = null;

export function beginRightClickGesture(): void {
  holdRefreshFired = false;
  gestureMoved = false;
  rightButtonHeld = true;
  pendingOpen = null;
}

export function markRightClickMoved(): void {
  if (holdRefreshFired) return;
  gestureMoved = true;
  pendingOpen = null;
}

export function markHoldRefreshFired(): void {
  holdRefreshFired = true;
  pendingOpen = null;
}

/// Called from a card's context menu handler. If the menu arrived while the
/// right button was still down, the open waits for mouseup or the hold timer.
/// If it arrived after release, the gesture is already known to be short.
export function scheduleCardRightClick(open: () => void): void {
  if (holdRefreshFired || gestureMoved) return;
  if (!rightButtonHeld) {
    open();
    return;
  }
  pendingOpen = open;
}

/// Mouseup resolves a contextmenu that was delivered while the button was
/// still held. A long press has already cancelled the pending action.
export function flushCardRightClick(): void {
  rightButtonHeld = false;
  const action = pendingOpen;
  pendingOpen = null;
  if (action && !holdRefreshFired && !gestureMoved) action();
}
