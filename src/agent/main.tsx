import React from 'react';
import { createRoot } from 'react-dom/client';
import AgentDev from './AgentDev';
import '../styles.css';
import '../redesign.css';
import './agent.css';

document.addEventListener('contextmenu', (event) => event.preventDefault());
document.addEventListener('dragstart', (event) => event.preventDefault());
window.addEventListener('mousedown', (event) => {
  if (event.button === 2) event.preventDefault();
}, true);

createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <AgentDev />
  </React.StrictMode>,
);
