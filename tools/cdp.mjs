import WebSocket from 'ws';
import fs from 'fs';

const route = process.argv[2] || 'home';
const out = process.argv[3] || `D:/Dev/bilibili-electron/shots/${route.replace(/[^a-z0-9]/gi, '_')}.png`;
const wait = Number(process.argv[4] || 3500);

const targets = await (await fetch(`http://127.0.0.1:${process.env.CDP_PORT || 9222}/json`)).json();
const page = targets.find((t) => t.type === 'page' && !t.url.includes('devtools') && !t.url.includes('#/player'));
if (!page) throw new Error('no page target');
const ws = new WebSocket(page.webSocketDebuggerUrl);
let id = 0;
const pending = new Map();
const errors = [];
ws.on('message', (raw) => {
  const message = JSON.parse(raw.toString());
  if (message.id && pending.has(message.id)) {
    const { resolve, reject } = pending.get(message.id);
    pending.delete(message.id);
    if (message.error) reject(new Error(JSON.stringify(message.error)));
    else resolve(message.result);
  }
  if (message.method === 'Runtime.exceptionThrown') {
    errors.push(message.params.exceptionDetails?.exception?.description || message.params.exceptionDetails?.text || 'exception');
  }
  if (message.method === 'Runtime.consoleAPICalled' && message.params.type === 'error') {
    errors.push(message.params.args.map((a) => a.value ?? a.description).join(' '));
  }
  if (message.method === 'Log.entryAdded' && message.params.entry.level === 'error') {
    errors.push(message.params.entry.text);
  }
});
const send = (method, params = {}) => new Promise((resolve, reject) => {
  const callId = ++id;
  pending.set(callId, { resolve, reject });
  ws.send(JSON.stringify({ id: callId, method, params }));
});
await new Promise((resolve) => ws.once('open', resolve));
await send('Runtime.enable');
await send('Log.enable');
await send('Page.enable');
await send('Runtime.evaluate', { expression: `location.hash = '#/${route}'` });
await new Promise((r) => setTimeout(r, wait));
const text = await send('Runtime.evaluate', { expression: 'document.body.innerText.slice(0,1200)', returnByValue: true });
console.log('TEXT:', text.result.value);
if (errors.length) console.log('ERRORS:', errors.slice(0, 20));
const shot = await send('Page.captureScreenshot', { format: 'png' });
fs.mkdirSync('D:/Dev/bilibili-electron/shots', { recursive: true });
fs.writeFileSync(out, Buffer.from(shot.data, 'base64'));
console.log('saved', out);
ws.close();
