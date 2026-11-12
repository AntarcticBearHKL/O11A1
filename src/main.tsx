import React from 'react';
import { createRoot } from 'react-dom/client';
import App from './App';
import PlayerApp from './PlayerApp';
import './styles.css';

const isPlayer = window.location.hash.startsWith('#/player');
createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    {isPlayer ? <PlayerApp /> : <App />}
  </React.StrictMode>,
);
