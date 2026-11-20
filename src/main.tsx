import React from 'react';
import { createRoot } from 'react-dom/client';
import Startup from './startup';
import './styles.css';
import './redesign.css';
import { installWindowDrag } from './windowDrag';

// No native selection box and no Chromium context menu anywhere in the app.
// Text selection is still available inside real text fields via CSS.
document.addEventListener('contextmenu', (event) => event.preventDefault());
document.addEventListener('dragstart', (event) => event.preventDefault());
// A right press must not move keyboard focus, otherwise buttons and tabs draw
// a focus ring under a plain right click. Capture phase so it runs before the
// button receives focus; left clicks and keyboard focus are untouched.
window.addEventListener('mousedown', (event) => {
  if (event.button === 2) event.preventDefault();
}, true);
installWindowDrag();

createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <Startup />
  </React.StrictMode>,
);
