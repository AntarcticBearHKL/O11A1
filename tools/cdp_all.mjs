import WebSocket from 'ws';

const expression = process.argv[2];
const port = process.env.CDP_PORT || 9222;
const targets = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
const pages = targets.filter((t) => t.type === 'page' && t.url.includes('#/player'));
for (const page of pages) {
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  let id = 0;
  const pending = new Map();
  ws.on('message', (raw) => {
    const message = JSON.parse(raw.toString());
    if (message.id && pending.has(message.id)) {
      const { resolve } = pending.get(message.id);
      pending.delete(message.id);
      resolve(message.result);
    }
  });
  const send = (method, params = {}) => new Promise((resolve) => {
    const callId = ++id;
    pending.set(callId, { resolve });
    ws.send(JSON.stringify({ id: callId, method, params }));
  });
  await new Promise((resolve) => ws.once('open', resolve));
  const result = await send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
  console.log(page.id.slice(0, 8), JSON.stringify(result.result.value ?? result.result.description));
  ws.close();
}
