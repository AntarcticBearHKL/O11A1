import fs from 'fs';
import WebSocket from 'ws';

const args = process.argv.slice(2);
let expression;
let targetArg;
if (args[0] === '--file') {
  expression = fs.readFileSync(args[1], 'utf8');
  targetArg = args[2];
} else {
  expression = args[0];
  targetArg = args[1];
}
const targets = await (await fetch(`http://127.0.0.1:${process.env.CDP_PORT || 9222}/json`)).json();
const page = targets.find((t) => t.type === 'page' && (targetArg === 'main' ? !t.url.includes('#/player') : targetArg ? t.url.includes(targetArg) : !t.url.includes('devtools')));
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
