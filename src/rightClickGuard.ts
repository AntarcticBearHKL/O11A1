/// A right click on a video card opens a new player window. The old shell
/// hold-to-refresh gesture is gone, so there is no longer a long-press
/// arbitration to wait for: the card action can run immediately.
export function scheduleCardRightClick(open: () => void): void {
  open();
}
