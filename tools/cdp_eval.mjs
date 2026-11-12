import WebSocket from 'ws';

const expression = process.argv[2];
const targets = await (await fetch(`http://127.0.0.1:${process.env.CDP_PORT || 9222}/json`)).json();
const page = targets.find((t) => t.type === 'page' && (process.argv[3] === 'main' ? !t.url.includes('#/player') : process.argv[3] ? t.url.includes(process.argv[3]) : !t.url.includes('devtools')));
if (!page) throw new Error('no page target: ' + JSON.stringify(targets.map((t) => t.title)));
const ws = new WebSocket(page.webSocketDebuggerUrl);
let id = 0;
const pending = new Map();
ws.on('message', (raw) => {
  const message = JSON.parse(raw.toString());
  if (message.id && pending.has(message.id)) {
    const { resolve, reject } = pending.get(message.id);
    pending.delete(message.id);
    if (message.error) reject(new Error(JSON.stringify(message.error)));
    else resolve(message.result);
  }
});
const send = (method, params = {}) => new Promise((resolve, reject) => {
  const callId = ++id;
  pending.set(callId, { resolve, reject });
  ws.send(JSON.stringify({ id: callId, method, params }));
});
await new Promise((resolve) => ws.once('open', resolve));
const result = await send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
console.log(JSON.stringify(result.result.value ?? result.result.description ?? result, null, 2));
ws.close();
