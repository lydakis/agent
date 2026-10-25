// Diagram drawing cost in headless Chromium: app/ui served on a local port, each diagram sent to the
// app's diagram worker (Pele) as rich.js sends it, timed from post to answer, while the page measures
// its longest frame gap. The inputs are small ones a reply holds and large ones within Pele's bounds.
// Run from the repository: `node app/bench/diagrams.cjs` with playwright-core resolvable
// (`NODE_PATH=<dir>/node_modules`) and CHROME set to a Chromium binary. Prints JSON.
const fs = require('fs');
const http = require('http');
const path = require('path');
const { chromium } = require('playwright-core');
const ui = path.join(__dirname, '..', 'ui');
const R = (n, f) => Array.from({ length: n }, (_, i) => f(i)).join('\n');
const CASES = {
  'flowchart, 5 nodes': 'flowchart LR\n  login --> rotate --> commit --> cookie\n  refresh --> rotate',
  'sequence, 5 messages': 'sequenceDiagram\n  C->>S: POST /refresh\n  S->>DB: rotate(id)\n  DB-->>S: new token\n  S->>DB: commit\n  S-->>C: Set-Cookie',
  'block-beta, space:99999': 'block-beta\n  columns 3\n  a space:99999 b',
  'block-beta, space:500000': 'block-beta\n  columns 3\n  a space:500000 b',
  'flowchart, 4,999 edges on 71 nodes': 'flowchart TD\n' + R(4999, (i) => `n${i % 70}-->n${(i * 7 + 3) % 71}`),
  'state, 80 states': 'stateDiagram-v2\n' + R(3000, (i) => `s${i % 80} --> s${(i * 11) % 80}`).slice(0, 49000),
  'er, 80 entities': 'erDiagram\n' + R(2000, (i) => `E${i % 80} ||--o{ E${(i * 3) % 81} : r`).slice(0, 49000),
  'sequence, 40 actors': 'sequenceDiagram\n' + R(2000, (i) => `A${i % 40}->>A${(i * 3) % 40}: m${i}`).slice(0, 49000),
};
(async () => {
  const server = http.createServer((req, res) => {
    const file = path.join(ui, decodeURIComponent(new URL(req.url, 'http://x').pathname));
    if (!file.startsWith(ui) || !fs.existsSync(file)) { res.writeHead(404); res.end(); return; }
    res.writeHead(200, { 'content-type': file.endsWith('.js') ? 'text/javascript' : 'text/html' }); res.end(fs.readFileSync(file));
  }).listen(0);
  const b = await chromium.launch({ executablePath: process.env.CHROME });
  const p = await b.newPage();
  await p.goto(`http://localhost:${server.address().port}/index.html`);
  const out = {};
  for (const [name, src] of Object.entries(CASES)) {
    const runs = [];
    for (let i = 0; i < 5; i++) runs.push(await p.evaluate((src) => new Promise((resolve) => {
      const w = new Worker('diagram-worker.js'); let gap = 0, last = performance.now(), on = true;
      const frame = () => { const t = performance.now(); gap = Math.max(gap, t - last); last = t; if (on) requestAnimationFrame(frame); };
      requestAnimationFrame(frame);
      // The first message also loads Pele; time a warm one.
      w.onmessage = () => {
        const t0 = performance.now();
        w.onmessage = ({ data }) => { on = false; w.terminate(); resolve({ ms: performance.now() - t0, gap, chars: data.svg?.length ?? 0, error: data.error?.split('\n')[0] }); };
        w.postMessage({ src, font: 'monospace', width: 680 });
      };
      w.postMessage({ src: 'graph TD\nA-->B', font: 'monospace', width: 680 });
    }), src));
    runs.sort((x, y) => x.ms - y.ms);
    const m = runs[2];
    out[name] = { median_ms: +m.ms.toFixed(1), longest_frame_gap_ms: +Math.max(...runs.map((r) => r.gap)).toFixed(1), svg_chars: m.chars, ...(m.error ? { error: m.error } : {}) };
  }
  console.log(JSON.stringify(out, null, 2));
  await b.close(); server.close();
})();
