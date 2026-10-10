const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

// Run the actual page state machine without booting a provider or a webview.
function page(daemon = {}, storage = null) {
  const elements = new Map(), timers = new Map();
  let timer = 0;
  const element = () => ({
    children: [], replaceChildren(...nodes) { this.children = nodes; }, append(...nodes) { this.children.push(...nodes); }, get childNodes() { return this.innerHTML ? [{ html: this.innerHTML }] : []; },
    dataset: {}, style: {}, innerHTML: '', value: '', scrollHeight: 0, scrollTop: 0, clientHeight: 0,
    classList: { add() {}, remove() {}, toggle() {}, contains() { return false; } },
    listeners: {}, addEventListener(type,fn) { this.listeners[type]=fn; }, querySelector() { return null; }, querySelectorAll() { return []; }, focus() {},
  });
  const transport = {...daemon, request:async(op,params)=> {
    if(op!=='history_items') return daemon.request(op,params);
    if(daemon.batch) return daemon.batch(params);
    const items=[];let bytes=0;
    for(const node of params.nodes) {
      const item=await daemon.request('item',{bot:params.bot,node});
      const size=JSON.stringify(item).length;
      if(items.length && bytes+size>768*1024)break;
      items.push({node,item});bytes+=size;
    }
    return {items};
  }};
  const context = vm.createContext({
    Daemon: transport, console, queueMicrotask, crypto: require('node:crypto').webcrypto, TextDecoder,
    document: { getElementById(id) { if (!elements.has(id)) elements.set(id, element()); return elements.get(id); }, listeners: {}, addEventListener(type, fn) { this.listeners[type] = fn; },
      createElement: element, createTextNode: () => ({ data: '', appended: 0, appendData(s) { this.data += s; this.appended += s.length; } }) },
    window: { listeners: {}, addEventListener(type, fn) { this.listeners[type] = fn; } }, localStorage: storage ? { getItem: k => storage.get(k) ?? null, setItem: (k, v) => storage.set(k, String(v)) } : { getItem() { return null; } },
    setTimeout(fn) { const id = ++timer; timers.set(id, fn); return id; }, clearTimeout(id) { timers.delete(id); }, setInterval() {},
  });
  for (const file of ['../ui/vendor/markdown-it.js', '../ui/rich.js']) vm.runInContext(fs.readFileSync(require.resolve(file), 'utf8'), context);
  context.Rich = context.window.Rich;
  let source = fs.readFileSync(require.resolve('../ui/app.js'), 'utf8');
  source = source.slice(0, source.indexOf('// ---------- boot ----------')) +
    'globalThis.app = { setRender: fn => { render = fn; }, S, joinPath, textHTML, waitsForHighlight, openFile, openFileFrom, dropFile, releaseDrawn, rail, renderRail, transcript, upsert, onEvent, handle, pump, loadBatch, evict, itemsHTML, renderTranscript, attach, lost, enqueue, load, cssEsc, esc, submit, interrupt, seat, botRowHTML, renderTail, tree, shortName, runStart, runHTML, botMenuItems, modelChoices, modelMenuItems, sendMenuItems, setSend, setModel, setEffort, fork, remove, createProject, save, restore, showMenu, refreshMenu, entries, pickerRows, waitSummary, nextBeside, sideChat, renderHead, followDrafts, openSetup, connectProvider, removeProvider, providerSpecs, act, setupHTML, renderSetup, refreshModels, modelMenu, learnSwarm, createSwarm, addAgent, stopSwarm, readBoard, renderSwarm, renderSwarmHead, postHTML, mixRows, nextRow, openSwarmSheet, readUsage, tally, forgetBot, setupState, readTriggers, renderTriggers, openTriggerSheet, renderTriggerSheet, trigSheet, soonTriggers, tellLead, markSeen, renderFile, go, upOf, crumbsHTML, railRows, renderTabs, triggerAct, turnNews, openProjectSheet, parsePlan, loadPlans, renderPlan, taskCard, mainBot, closeSheet, openPicker, pickerMode, renderPicker, keyLabel };\n})();';
  vm.runInContext(source, context);
  return { ...context.app, context, elements, async tick() { const jobs = [...timers.values()]; timers.clear(); jobs.forEach(fn => fn()); await settle(); } };
}
// A turn that ran ended: a steer finishes too, as its own turn taken into another.
const ended = (e) => e.event === 'turn_finished' && e.data?.status !== 'steered';
const settle = async () => { for (let i = 0; i < 20; i++) await Promise.resolve(); };
const deferred = () => { let resolve; const promise = new Promise(r => { resolve = r; }); return { promise, resolve }; };

test('schema-invalid tool arguments do not stop subsequent events', async () => {
  const p = page();
  for (const args of ['null', '{"handles":"proc:1"}', '{"handles":[null,12,"turn:Bob:1"]}']) {
    await p.onEvent({ event: 'tool_started', bot: 'Bob', data: { call_id: args, name: 'wait', arguments: args } });
  }
  await p.onEvent({ event: 'text_delta', bot: 'Bob', text: 'still running' });
  assert.equal(p.transcript('Bob').text, 'still running');
  assert.equal(p.transcript('Bob').items.length, 3);
});

test('historical process completion survives newest-first loads across batches', async () => {
  const outputs = { 1: { type:'function_call_output', output: '{"handle":"proc:1"}' }, 2: { type:'function_call_output', output: '{"results":{"proc:1":{"exit_code":0,"stdout":"finished"}}}' } };
  const p = page({ request: async (_, { node }) => outputs[node] });
  const t = p.transcript('Bob');
  t.items = [{ kind: 'tool', callId: 'shell', background: true, summary: 'build' }, { kind: 'node', node: 1, callId: 'shell' },
    { kind: 'tool', callId: 'wait', name: 'wait' }, { kind: 'node', node: 2, callId: 'wait' }];
  t.nodes = 2;
  await p.loadBatch('Bob');
  assert.equal(t.items.find(it => it.kind === 'proc').done, 'finished');

  const q = page({ request: async (_, { node }) => outputs[node] });
  const u = q.transcript('Bob');
  u.items = [{ kind: 'tool', callId: 'shell', background: true, summary: 'build' }, { kind: 'node', node: 1, callId: 'shell' },
    ...Array.from({ length: 1200 }, () => ({ kind: 'note', text: '' })),
    { kind: 'tool', callId: 'wait', name: 'wait' }, { kind: 'node', node: 2, callId: 'wait' }];
  u.nodes = 2;
  await q.loadBatch('Bob'); // The start lies outside the tail window.
  u.anchor = 'top';
  await q.loadBatch('Bob');
  const cards = u.items.filter(it => it.kind === 'proc');
  assert.equal(cards.length, 1);
  assert.equal(cards[0].done, 'finished');
  assert.equal(cards[0].cmd, 'build');
});

test('top-window eviction never leaves part of a decoded node beside its bare node', () => {
  const p = page(); const t = p.transcript('Bob'); t.anchor = 'top';
  t.items = Array.from({ length: 1700 }, (_, i) => ({ kind: 'text', text: String(i), from: i }));
  t.items[1200].from = 1200; t.items[1201].from = 1200;
  p.evict(t);
  assert.equal(t.items.some(it => it.from === 1200 || it.node === 1200), false);
  const range = t.items.find(it => it.kind === 'history' && it.min <= 1200 && it.next >= 1200);
  assert.ok(range, 'the whole node is recoverable through one range');
});

test('large fan-out keeps rendered peer cards and retained peer entries bounded', async () => {
  const p = page(); p.upsert({ name: 'parent', bot_id: 1 });
  for (let i = 0; i < 10000; i++) await p.onEvent({ event: 'created', bot: `child${i}`, data: { bot_id: i + 2, provider: 'test', created_by: 'parent', created_by_id: 1 } });
  const t = p.transcript('parent');
  assert.ok(t.items.filter(it => it.kind === 'peer').length <= 600);
  assert.ok(t.peers.length <= 600);
  assert.ok((p.itemsHTML(t).match(/data-task=/g) || []).length <= 600);
  assert.equal(p.S.bots.size, 10001, 'older peers remain reachable through the fleet');
  await p.onEvent({ event: 'deleted', bot: 'child9999' });
  assert.ok(!t.items.some(it => it.kind === 'peer' && it.who === 'child9999'));
});

test('retries cannot overlap a slow snapshot and a lost attachment retries after settling', async () => {
  const snapshot = deferred(); let attaches = 0;
  const p = page({ setup: async () => ({}), attach: async () => ({ session: ++attaches }), pull: () => new Promise(() => {}), request: () => snapshot.promise });
  const first = p.attach(); await settle();
  p.lost('closed during snapshot');
  await p.tick();
  assert.equal(attaches, 1);
  snapshot.resolve({ bots: [] }); await first;
  await p.tick();
  assert.equal(attaches, 2);
  await settle();
  assert.equal(p.S.attached, true);
});

test('stream rendering appends only new characters and resets between messages', () => {
  const p = page(); assert.equal(typeof p.renderTail, 'function');
  const t = p.transcript('Bob'); t.streamingTurn = 1;
  const el = { dataset: {}, children: [], replaceChildren(...nodes) { this.children = nodes; } };
  for (const field of ['text', 'thinking']) {
    t.text = ''; t.thinking = ''; t.streamGen = (t.streamGen || 0) + 1;
    for (let i = 0; i < 1000; i++) { t[field] += '<& chunk >'; p.renderTail(el, 'Bob', t); }
    const line = el.children.at(-1), text = line.children[0];
    assert.equal(text.data, t[field]);
    assert.equal(text.appended, t[field].length);
    t[field] = 'new'; t.streamGen += 1;
    p.renderTail(el, 'Bob', t);
    assert.equal(el.children.at(-1).children[0].data, 'new');
  }
});


test('messages draw as Markdown with raw HTML, scripts and remote fetches kept out', () => {
  const p = page(), Rich = p.context.Rich;
  const html = Rich.html('# Title\n\n**bold** <img src=x onerror=alert(1)> [ok](https://example.com) [bad](javascript:alert(1)) ![pic](https://example.com/p.png)\n\n<script>alert(1)</script>\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n- [x] done');
  assert.match(html, /<h1>Title<\/h1>/);
  assert.match(html, /<strong>bold<\/strong>/);
  assert.match(html, /&lt;img src=x onerror=alert\(1\)&gt;/);
  assert.match(html, /&lt;script&gt;alert\(1\)&lt;\/script&gt;/);
  assert.match(html, /<a href="#" data-href="https:\/\/example.com">ok<\/a>/);
  assert.doesNotMatch(html, /javascript:/);
  assert.doesNotMatch(html, /<img/);
  assert.match(html, /<a href="#" data-href="https:\/\/example.com\/p.png">pic<\/a>/);
  assert.match(html, /<table>/);
  assert.match(html, /type="checkbox"/);
});

test('fenced blocks become code, previews, diagrams and images by their language', () => {
  const p = page(), Rich = p.context.Rich;
  assert.match(Rich.html('```rust\nfn main() {}\n```'), /data-kind="code".*<span class="lang">rust<\/span>.*fn main\(\) \{\}/s);
  // A page in a message runs only when asked: its scripts would share the window's thread.
  assert.match(Rich.html('```html\n<!doctype html><body><b>hi</b></body>\n```'), /data-kind="html" data-view="code".*&lt;b&gt;hi&lt;\/b&gt;/s);
  assert.match(Rich.html('```html\n<b>hi</b>\n```'), /data-kind="html" data-view="code"/);
  assert.match(Rich.html('```mermaid\ngraph TD\nA-->B\n```'), /data-kind="mermaid".*A--&gt;B/s);
  // A diagram in a message draws when asked; one in a file someone opened draws at once.
  assert.match(Rich.html('```mermaid\ngraph TD\n```'), /data-kind="mermaid" data-lazy data-view="code"/);
  assert.match(Rich.file('/w/flow.mmd', new TextEncoder().encode('graph TD')).html, /data-kind="mermaid" data-lazy data-page/);
  const svg = Rich.html('```svg\n<svg xmlns="http://www.w3.org/2000/svg"><circle r="4"/></svg>\n```');
  // An SVG draws as an image, and in a message only when asked: its filters and animations take CPU.
  assert.match(svg, /data-kind="svg" data-view="code"/);
  assert.doesNotMatch(svg, /<img/);
  assert.doesNotMatch(svg.split('<pre')[0], /<svg/);
  assert.match(Rich.file('/w/a.svg', new TextEncoder().encode('<svg/>')).html, /data-kind="svg" data-view="view"/);
  assert.doesNotMatch(Rich.html('![x](data:image/svg+xml,%3Csvg%2F%3E)'), /<img/);
  // A raster image the message carries draws on a click; a local one opens beside; a remote one is a link.
  const png = Rich.html('![dot](data:image/png;base64,iVBORw0KGgo=)');
  assert.doesNotMatch(png, /<img/);
  assert.match(png, /<button type="button" class="img" data-img="data:image\/png;base64,iVBORw0KGgo=" title="dot">image: dot<\/button>/);
  assert.match(Rich.html('![flow](docs/flow.png)'), /<a class="file" href="#" data-file="docs\/flow.png">flow<\/a>/);
  assert.match(Rich.html('![r](https://example.com/r.png)'), /<a href="#" data-href="https:\/\/example.com\/r.png">r<\/a>/);
});

test('a table too wide or too large shows as its source; a modest one draws', () => {
  const Rich = page().context.Rich;
  assert.match(Rich.html('| a | b |\n|---|---|\n| 1 | 2 |'), /<table>/);
  const wide = '|' + 'a|'.repeat(300) + '\n|' + '-|'.repeat(300) + '\n|1|';
  assert.doesNotMatch(Rich.html(wide), /<table>|<th>/);
  assert.match(Rich.html(wide), /data-kind="code"/);
  const tall = '| a | b | c | d |\n|---|---|---|---|\n' + 'x\n'.repeat(3000);
  assert.doesNotMatch(Rich.html(tall), /<td>/);
});

test('a message highlights at most 256 KiB of code in all', () => {
  const p = page(), Rich = p.context.Rich; let calls = 0, bytes = 0;
  p.context.hljs = { getLanguage: () => true, highlight: (text) => { calls++; bytes += text.length; return { value: text }; } };
  const fence = '```rust\n' + 'x'.repeat(60 * 1024) + '\n```\n\n';
  Rich.html(fence.repeat(10));
  assert.equal(calls, 4); assert.ok(bytes <= 256 * 1024);
  Rich.html(fence); assert.equal(calls, 5);
  // A streamed reply's blocks share one budget.
  calls = 0; const used = { lines: 0, tags: 0 };
  Rich.html(fence.repeat(3), used); Rich.html(fence.repeat(3), used);
  assert.equal(calls, 4);
});

test('a message past 100,000 tags shows as its text', () => {
  const Rich = page().context.Rich;
  const list = '- *x*\n'.repeat(30000);
  assert.doesNotMatch(Rich.html(list), /<li>/);
  assert.match(Rich.html(list), /data-kind="code"/);
  assert.match(Rich.html('- x\n'.repeat(100)), /<li>/);
  assert.doesNotMatch(Rich.html('x\n'.repeat(60000)), /<br>/);
  // One long line of inline marks is not parsed: emphasis, links written bare.
  for (const line of ['*x* '.repeat(60000), 'www.a.io '.repeat(120000)]) {
    const out = Rich.html(line);
    assert.doesNotMatch(out, /<em>|<a /); assert.match(out, /data-kind="code"/);
  }
  assert.match(Rich.html('*x* '.repeat(100)), /<em>/);
  // A streamed reply's blocks share the bounds: each piece alone would draw, together they are text.
  const used = { lines: 0, tags: 0 }, piece = '- x\n'.repeat(30000);
  assert.match(Rich.html(piece, used), /<li>/);
  assert.doesNotMatch(Rich.html(piece, used), /<li>/);
  assert.equal(used.over, true);
});

test('the text blocks of one stored message share its bounds', () => {
  const p = page(), list = '- x\n'.repeat(30000);
  const es = p.entries({ role: 'assistant', content: [{ type: 'text', text: list }, { type: 'tool_use', id: 'c1', name: 'read', input: {} }, { type: 'text', text: list }] });
  const texts = es.filter((e) => e.kind === 'text');
  assert.equal(texts.length, 2);
  assert.match(p.textHTML(texts[0]), /<li>/);
  assert.doesNotMatch(p.textHTML(texts[1]), /<li>/);
  // Drawn again, the first block's share is counted once.
  texts[0].htmlOf = null; assert.match(p.textHTML(texts[0]), /<li>/);
});

test('highlighting arriving redraws only messages whose code waited for it', () => {
  const p = page(), R = p.context.Rich; let v = 0, calls = 0;
  p.context.Rich = { html: (x) => { calls++; return R.html(x); }, get version() { return v; }, get waited() { return R.waited; } };
  const prose = { text: 'just words' }, code = { text: '```rust\nfn a() {}\n```' };
  p.textHTML(prose); p.textHTML(code); assert.equal(calls, 2);
  v = 1; p.textHTML(prose); p.textHTML(code);
  assert.equal(calls, 3);
});

test('a diagram draws when clicked, and a pane drawn anew asks again, drawing from the cache', async () => {
  const p = page(), c = p.context, Rich = c.Rich; let renders = 0;
  c.document.head = { append(s) { s.onload(); } };
  c.getComputedStyle = () => ({ getPropertyValue: () => '' });
  c.mermaid = { initialize() {}, render: async (id, src) => { renders++; return { svg: `<svg>${src}</svg>` }; } };
  const box = (src = 'graph TD') => { const view = { innerHTML: '' }, pre = { textContent: src };
    return { dataset: { kind: 'mermaid', lazy: '', view: 'code' }, clientWidth: 0, view, closest: () => null, querySelector: (s) => s === 'pre' ? pre : s === '.view' ? view : null }; };
  const click = (b) => { const btn = { dataset: { rich: 'view' }, closest: () => b }; Rich.click({ target: { closest: (s) => s === '[data-rich]' ? btn : null }, preventDefault() {} }); };
  const asked = box(); click(asked);
  await new Promise((r) => setImmediate(r));
  assert.equal(asked.view.innerHTML, '<svg>graph TD</svg>'); assert.equal(renders, 1);
  // The pane drawn anew: nothing remembers the click, so the block is code until clicked again,
  // which draws it from the cache at once.
  const again = box(); Rich.hydrate({ querySelectorAll: () => [again] });
  assert.equal(again.view.innerHTML, '');
  click(again); assert.equal(again.view.innerHTML, '<svg>graph TD</svg>'); assert.equal(renders, 1);
  // A file someone opened draws at once.
  const page_ = box('graph LR'); page_.dataset.page = ''; Rich.hydrate({ querySelectorAll: () => [page_] });
  await new Promise((r) => setImmediate(r));
  assert.equal(page_.view.innerHTML, '<svg>graph LR</svg>'); assert.equal(renders, 2);
  // A reader below a block that draws keeps their place.
  let h = 1000; const pane = { scrollTop: 500, clientHeight: 100, get scrollHeight() { return h; }, getBoundingClientRect: () => ({ top: 0 }) };
  const b = box(); b.closest = (s) => s === '.scroll' ? pane : null; b.getBoundingClientRect = () => ({ bottom: -10 });
  let html = ''; Object.defineProperty(b.view, 'innerHTML', { get: () => html, set: (v) => { html = v; h += 300; } });
  click(b); assert.equal(html, '<svg>graph TD</svg>'); assert.equal(pane.scrollTop, 800);
});

test('a reply\'s diagram is code while it streams, and can be drawn once it is in', async () => {
  const reply = 'First:\n\n```mermaid\ngraph TD\n```\n\nand on.';
  const p = page({ request: async () => ({ type: 'message', role: 'assistant', content: [{ type: 'text', text: reply }] }) }), t = p.transcript('Bob');
  t.streamingTurn = 7; t.streamGen = 1; t.text = '';
  const el = { dataset: {}, children: [], replaceChildren(...nodes) { this.children = nodes; } };
  for (const ch of reply) { t.text += ch; p.renderTail(el, 'Bob', t); }
  const streamed = el.children[0].children.map((c) => c.html).join('');
  assert.match(streamed, /data-kind="code"><div class="rh"><span class="lang">mermaid/); assert.doesNotMatch(streamed, /data-lazy|data-rich="view"/);
  await p.onEvent({ event: 'message', bot: 'Bob', turn: 7, data: { node: 3 } }); await p.loadBatch('Bob');
  const text = t.items.find((it) => it.kind === 'text');
  assert.match(p.textHTML(text), /data-kind="mermaid" data-lazy data-view="code">.*data-rich="view"/);
});

test('a message\'s later blocks are drawn anew when an earlier one\'s share of its bounds changes', () => {
  const p = page(), list = '- x\n'.repeat(20000);
  const es = p.entries({ role: 'assistant', content: [{ type: 'text', text: 'short' }, { type: 'tool_use', id: 'c1', name: 'read', input: {} }, { type: 'text', text: list }] });
  const [first, later] = es.filter((e) => e.kind === 'text');
  p.textHTML(first); assert.match(p.textHTML(later), /<li>/);
  // The first block draws again with far more tags (as highlighting arriving can make it): the later one is past the bound.
  first.text = '- y\n'.repeat(40000); p.textHTML(first);
  assert.doesNotMatch(p.textHTML(later), /<li>/);
});

test('Markdown parses in time that grows with its length, for runs of markers too', () => {
  const p = page(), Rich = p.context.Rich;
  const took = (s) => { const t0 = process.hrtime.bigint(); Rich.html(s); return Number(process.hrtime.bigint() - t0) / 1e6; };
  for (const unit of ['!', '![', '[', '*x', '_a', '`a``']) {
    took(unit.repeat(2000));
    const small = took(unit.repeat(10000)), large = took(unit.repeat(40000));
    assert.ok(large < 12 * Math.max(small, 2), `${unit}: ${small.toFixed(1)} ms at 10k, ${large.toFixed(1)} ms at 40k`);
  }
  // A line of them past the bound is not parsed at all.
  assert.match(Rich.html('!'.repeat(200000)), /<span class="lang">text<\/span>/);
});

test('a message\'s parsing counts toward the window\'s bound', () => {
  const p = page(), it = { kind: 'text', text: '*x* '.repeat(1000) };
  p.textHTML(it);
  assert.ok(it.bytes >= 16 * 2000, `${it.bytes} bytes for 2,000 marks`);
});

test('a reference used many times copies at most 1 Mi characters of targets into the page', () => {
  const p = page(), Rich = p.context.Rich, target = 'https://example.com/' + 'a'.repeat(100000);
  const out = Rich.html(`[x][a] `.repeat(5000) + `\n\n[a]: ${target}`);
  assert.ok(out.length < 2 * 1024 * 1024, `${out.length} characters`);
  assert.equal((out.match(/data-href=/g) ?? []).length, 10);
  // The budget is shared by the pieces of one message, as a streamed reply's are.
  const used = { lines: 0, tags: 0, code: 0 }, piece = `[x][a]\n\n[a]: ${target}\n\n`;
  let links = 0; for (let i = 0; i < 20; i++) links += (Rich.html(piece, used).match(/data-href=/g) ?? []).length;
  assert.equal(links, 10);
  // Targets are charged as written into the page, escaped.
  const quoted = Rich.html(`[x][q] `.repeat(10) + `\n\n[q]: https://example.com/${"'".repeat(100000)}`);
  assert.ok(quoted.length < 2 * 1024 * 1024, `${quoted.length} characters`);
  assert.equal((quoted.match(/data-href=/g) ?? []).length, 2);
});

test('the text blocks of one message share the bound on marks parsed', () => {
  const p = page(), bang = '!'.repeat(99000), content = [];
  for (let i = 0; i < 7; i++) content.push({ type: 'text', text: bang }, { type: 'tool_use', id: `c${i}`, name: 'read', input: {} });
  const texts = p.entries({ role: 'assistant', content }).filter((e) => e.kind === 'text');
  assert.equal(texts.length, 7);
  assert.doesNotMatch(p.textHTML(texts[0]), /<span class="lang">text<\/span>/);
  for (const t of texts.slice(1)) assert.match(p.textHTML(t), /<span class="lang">text<\/span>/);
});

test('a diagram that failed to draw asks again before it is tried again', async () => {
  const p = page(), c = p.context, Rich = c.Rich; let renders = 0;
  c.document.head = { append(s) { s.onload(); } };
  c.getComputedStyle = () => ({ getPropertyValue: () => '' });
  c.mermaid = { initialize() {}, render: async () => { renders++; throw new Error('too big'); } };
  const box = () => { const view = { innerHTML: '' }, pre = { textContent: 'graph TD' }, lang = { textContent: 'mermaid' };
    return { dataset: { kind: 'mermaid', lazy: '', view: 'code', id: 'Bob|1|mermaid|x' }, clientWidth: 0, view, lang, closest: () => null, querySelector: (s) => s === 'pre' ? pre : s === '.view' ? view : s === '.rh .lang' ? lang : null }; };
  const asked = box(), button = { dataset: { rich: 'view' }, closest: () => asked };
  Rich.click({ target: { closest: (s) => s === '[data-rich]' ? button : null }, preventDefault() {} });
  await new Promise((r) => setImmediate(r)); await new Promise((r) => setImmediate(r));
  assert.equal(renders, 1); assert.match(asked.lang.textContent, /too big/);
  Rich.hydrate({ querySelectorAll: () => [box()] }); await new Promise((r) => setImmediate(r));
  assert.equal(renders, 1);
});

test('a file opened while the side pane opens is drawn once the pane has its width', async () => {
  const p = page(), c = p.context, R = c.Rich, opening = deferred(); let hydrated = 0;
  c.Rich = { file: R.file, get version() { return R.version; }, hydrate: () => { hydrated++; } };
  p.elements.set('app', { getAnimations: () => [{ finished: opening.promise }] });
  p.S.ui.file = { bot: 'Bob', full: '/w/c.vl.json', gen: 1, state: 'ok', bytes: new TextEncoder().encode('{}'), more: false, url: null };
  p.renderFile(); await settle();
  assert.equal(hydrated, 0);
  opening.resolve(); await settle();
  assert.equal(hydrated, 1);
});

test('drawn HTML is kept for the transcripts on screen and the most recent others up to 16 MiB', () => {
  const p = page(), MiB = 1024 * 1024;
  const fill = (name) => { const t = p.transcript(name), it = { kind: 'text', text: 'x', html: '<p>x</p>', htmlOf: 'x', drawnBytes: 9 * MiB, bytes: 9 * MiB + 1 }; t.items.push(it); t.bytes = it.bytes; return it; };
  const a = fill('A'), b = fill('B'), c = fill('C');
  p.releaseDrawn(['A']); p.releaseDrawn(['B']); p.releaseDrawn(['C', 'A']);
  // A is on screen again, B the most recent other: all kept.
  assert.ok(a.html && b.html && c.html);
  p.releaseDrawn(['C']);
  // Off screen now: A (shown last, kept) and B (past 16 MiB, let go, its bytes returned).
  assert.ok(a.html && c.html); assert.equal(b.html, undefined);
  assert.equal(b.bytes, 1); assert.equal(p.transcript('B').bytes, 1);
  // Shown again, it is drawn anew.
  assert.match(p.textHTML(b, p.transcript('B')), /<p>x<\/p>/);
});

test('highlighting arriving redraws only a pane with code waiting for it', () => {
  const p = page(), t = p.transcript('Bob'), el = { dataset: { who: 'Bob' }, lastElementChild: null };
  t.items.push({ kind: 'text', text: 'just words' }); p.textHTML(t.items[0]);
  assert.equal(p.waitsForHighlight(el), false);
  t.items.push({ kind: 'text', text: '```rust\nfn a() {}\n```' }); p.textHTML(t.items[1]);
  assert.equal(p.waitsForHighlight(el), true);
  // A streamed reply's code waits too.
  const u = p.transcript('Ann'), tail = { dataset: {}, replaceChildren() {} }, side = { dataset: { who: 'Ann' }, lastElementChild: tail };
  u.streamingTurn = 1; u.streamGen = 1; u.text = 'words\n\n'; p.renderTail(tail, 'Ann', u);
  assert.equal(p.waitsForHighlight(side), false);
  u.text += '```rust\nfn a() {}\n```\n\n'; p.renderTail(tail, 'Ann', u);
  assert.equal(p.waitsForHighlight(side), true);
});

test('a link\'s own menu is not offered, as it would follow the link in the window', () => {
  const p = page(); let prevented = 0;
  const target = { closest: (s) => s === 'a' ? {} : null };
  p.context.document.listeners.contextmenu({ target, preventDefault() { prevented++; } });
  assert.equal(prevented, 1);
});

test('highlighting arriving redraws a file beside only when its code waited for it', () => {
  const p = page(), c = p.context, R = c.Rich, enc = (s) => new TextEncoder().encode(s); let v = 0, files = 0;
  c.Rich = { file: (...a) => { files++; return R.file(...a); }, get version() { return v; }, hydrate() {} };
  const show = (full, text) => { p.S.ui.file = { bot: 'Bob', full, gen: 2, state: 'ok', bytes: enc(text), more: false, url: null }; p.renderFile(); };
  show('/w/page.html', '<p>hi</p>'); assert.equal(files, 1);
  v = 1; p.renderFile(); assert.equal(files, 1);
  show('/w/a.rs', 'fn a() {}'); assert.equal(files, 2);
  v = 2; p.renderFile(); assert.equal(files, 3);
});

test('a file drawn again keeps the reader\'s place; another file starts at its top', () => {
  const p = page(), enc = (s) => new TextEncoder().encode(s), el = p.elements.get('side') ?? p.context.document.getElementById('side');
  p.S.ui.file = { bot: 'Bob', full: '/w/a.rs', gen: 2, state: 'ok', bytes: enc('fn a() {}'), more: false, url: null }; p.renderFile();
  el.scrollTop = 300; p.S.ui.file.gen = 3; p.renderFile();
  assert.equal(el.scrollTop, 300);
  p.S.ui.file = { bot: 'Bob', full: '/w/b.rs', gen: 4, state: 'ok', bytes: enc('fn b() {}'), more: false, url: null }; p.renderFile();
  assert.equal(el.scrollTop, 0);
});

test('⌘P lists the repository in view, names first, and opens the pick in a file tab', async () => {
  const listed = [];
  const p = page({ request: async () => ({ nodes: [], workspaces: [], next_from: null }), listFiles: async (dir) => { listed.push(dir); return { root: '/w', files: ['docs/session.md', 'src/auth/session.rs', 'src/sessions/mod.rs', 'README.md'], more: false }; }, readFile: async (path) => new TextEncoder().encode(`# ${path}`) });
  p.S.session = 1; p.S.config = { workspace: '/synthetic' };
  p.upsert({ name: 'Bob', bot_id: 1, provider: 'alpha', model: 'one', workspace: '/w/src' });
  await p.go('Bob');
  const doc = p.context.document, q = doc.getElementById('pickerq');
  p.openPicker('files'); await settle();
  assert.deepEqual(listed, ['/w/src'], "the agent's folder names the repository");
  q.value = 'sess';
  // A name that starts with it, then a name holding it, then a folder that does.
  assert.deepEqual(Array.from(p.pickerRows(), (r) => r.path), ['docs/session.md', 'src/auth/session.rs', 'src/sessions/mod.rs']);
  p.renderPicker();
  assert.match(doc.getElementById('pickerlist').innerHTML, /<span class="n"><span class="hit">sess<\/span>ion\.md<\/span><span class="h">docs<\/span>/);
  // Tab trades lists and keeps what was typed.
  await q.listeners.keydown({ key: 'Tab', preventDefault() {} });
  assert.equal(p.S.ui.pickerMode, 'agents'); assert.equal(q.value, 'sess');
  await q.listeners.keydown({ key: 'Tab', preventDefault() {} }); await settle();
  assert.deepEqual(listed, ['/w/src'], 'trading lists keeps the listing: no second git');
  p.S.ui.pickerSel = 1;
  await q.listeners.keydown({ key: 'Enter', preventDefault() {} }); await settle();
  assert.equal(p.S.selected, '▤/w/src/auth/session.rs'); assert.deepEqual([...p.S.ui.tabs], ['Bob', '▤/w/src/auth/session.rs']);
  assert.equal(p.keyLabel(p.S.selected), 'session.rs');
  // The file is drawn in the main pane, which has no composer for it, and the sidebar shows Home's list.
  assert.match(doc.getElementById('log').innerHTML, /class="fview"/); assert.match(doc.getElementById('log').innerHTML, /\/w\/src\/auth\/session\.rs/);
  assert.equal(doc.getElementById('form').hidden, true);
  assert.equal(p.rail.open, '');
  // Back on the agent, the tab keeps only its path; shown again, it is read again.
  await p.go('Bob');
  assert.equal(p.S.ui.tabFile, null); assert.equal(doc.getElementById('form').hidden, false);
  await p.go('▤/w/src/auth/session.rs'); await settle();
  assert.equal(p.S.ui.tabFile.state, 'ok');
  // A link in it names a path from the file's own folder, and opens beside.
  await p.openFileFrom('../README.md', { closest: () => null });
  assert.equal(p.S.ui.file.full, '/w/src/README.md');
});

test('a file beside opens as a tab, and a step that writes it reads the tab again in place', async () => {
  let text = 'one';
  const p = page({ request: async () => ({ nodes: [], workspaces: [], next_from: null }), readFile: async () => new TextEncoder().encode(text) });
  p.S.session = 1; p.S.config = { workspace: '/synthetic' };
  p.upsert({ name: 'Bob', bot_id: 1, provider: 'alpha', model: 'one', workspace: '/w' });
  await p.go('Bob'); await p.openFile('Bob', '/w/notes.txt');
  await p.act({ dataset: { act: 'file-tab' } }); await settle();
  assert.equal(p.S.ui.file, null); assert.equal(p.S.selected, '▤/w/notes.txt');
  const log = p.context.document.getElementById('log');
  assert.match(log.innerHTML, /one/);
  log.scrollTop = 120; text = 'two';
  const t = p.transcript('Bob'); t.items.push({ kind: 'tool', callId: 'c1', turn: 3, name: 'write', path: 'notes.txt', done: false });
  await p.onEvent({ event: 'tool_completed', bot: 'Bob', turn: 3, data: { call_id: 'c1' } }); await settle();
  assert.match(log.innerHTML, /two/); assert.equal(log.scrollTop, 120, 'the reader keeps their place');
});

test('file tabs come back after a restart', () => {
  const storage = new Map(), a = shell({}, storage);
  a.upsert({ name: 'Bob', bot_id: 1, provider: 'alpha', model: 'one' });
  a.S.ui.tabs.push('Bob', '▤/w/a.md'); a.S.selected = '▤/w/a.md'; a.save();
  const b = shell({}, storage); b.upsert({ name: 'Bob', bot_id: 1, provider: 'alpha', model: 'one' }); b.restore();
  assert.deepEqual([...b.S.ui.tabs], ['Bob', '▤/w/a.md']); assert.equal(b.S.selected, '▤/w/a.md');
});

test('a cut listing says so whatever is typed, and a file tab opens no menu', async () => {
  const p = page({ request: async () => ({ nodes: [], workspaces: [], next_from: null }), listFiles: async () => ({ root: '/w', files: ['a.md', 'b.md'], more: true }) });
  p.S.session = 1; p.S.config = { workspace: '/synthetic' };
  p.upsert({ name: 'Bob', bot_id: 1, provider: 'alpha', model: 'one', workspace: '/w' });
  await p.go('Bob');
  const doc = p.context.document, q = doc.getElementById('pickerq');
  p.openPicker('files'); await settle();
  assert.match(doc.getElementById('pickerlist').innerHTML, /searched the first 2 files; the repository has more/);
  q.value = 'zzz'; p.renderPicker();
  assert.match(doc.getElementById('pickerlist').innerHTML, /no file matches.*searched the first 2 files/);
  let prevented = 0;
  const target = { closest: (s) => s === 'a' ? null : { dataset: { tab: '▤/w/a.md' } } };
  doc.listeners.contextmenu({ target, preventDefault() { prevented++; }, clientX: 1, clientY: 1 });
  assert.equal(prevented, 1); assert.equal(p.S.ui.menu, false);
});

test('⌘P pressed beside searches the side agent\'s repository, and Escape in a file tab\'s page focuses its tab', async () => {
  const listed = [];
  const p = page({ request: async () => ({ nodes: [], workspaces: [], next_from: null }), listFiles: async (dir) => { listed.push(dir); return { root: dir, files: [], more: false }; }, readFile: async () => new TextEncoder().encode('<p>x</p>') });
  p.S.session = 1; p.S.config = { workspace: '/synthetic' };
  p.upsert({ name: 'Bob', bot_id: 1, provider: 'alpha', model: 'one', workspace: '/w/main' });
  p.upsert({ name: 'Ann', bot_id: 2, provider: 'alpha', model: 'one', workspace: '/w/side' });
  await p.go('Bob'); p.S.ui.side = 'Ann';
  const doc = p.context.document;
  doc.activeElement = { closest: (s) => s === '.pane.side' ? {} : null };
  p.openPicker('files'); await settle();
  assert.deepEqual(listed, ['/w/side']);
  p.S.ui.picker = false;
  doc.activeElement = { closest: () => null };
  p.openPicker('files'); await settle();
  assert.deepEqual(listed, ['/w/side', '/w/main']);
  // A file beside, with no agent beside, is searched from the side pane too.
  await p.openFile('Bob', '/w/other/notes.md'); p.S.ui.side = null; p.S.ui.picker = false;
  doc.activeElement = { closest: (s) => s === '.pane.side' ? {} : null };
  p.openPicker('files');
  // Down while the list is loading keeps a row to pick once it comes.
  await doc.getElementById('pickerq').listeners.keydown({ key: 'ArrowDown', preventDefault() {} });
  assert.equal(p.S.ui.pickerSel, 0);
  await settle();
  assert.deepEqual(listed, ['/w/side', '/w/main', '/w/other']);
  p.S.ui.picker = false; p.dropFile();
  // A page in a file tab: Escape leaves it for the tab, as the tab has no composer; the sidebar
  // shows Home's list, with New project.
  await p.go('▤/w/main/p.html', 'tab'); await settle();
  let focused = 0; const tabs = doc.getElementById('tabs'); tabs.querySelector = (s) => s === '.wtab.on' ? { focus() { focused++; } } : null;
  const win = {}, frame = { contentWindow: win, closest: () => null };
  doc.querySelectorAll = (s) => s === '.rc iframe' ? [frame] : []; doc.activeElement = frame;
  p.context.window.listeners.message({ source: win, data: { rich: 'escape' } }); await p.tick();
  assert.equal(focused, 1);
  // So does closing the finder opened there.
  p.openPicker('files'); await settle(); doc.activeElement = { closest: () => null };
  await doc.getElementById('pickerq').listeners.keydown({ key: 'Escape', preventDefault() {} }); await p.tick();
  assert.equal(focused, 2);
  assert.equal(doc.getElementById('newproj').hidden, false);
  // ? still shows the keys there.
  await doc.listeners.keydown({ key: '?', target: { id: '', closest: () => null }, preventDefault() {} });
  assert.ok(p.S.ui.help);
});

test('a file found and clicked opens only its tab, from the repository, even under a folder named ~', async () => {
  const p = page({ request: async () => ({ nodes: [], workspaces: [], next_from: null }), listFiles: async () => ({ root: '/w', files: ['~/notes.md'], more: false }), readFile: async () => new TextEncoder().encode('x') });
  p.S.session = 1; p.S.config = { workspace: '/synthetic' };
  p.upsert({ name: 'Bob', bot_id: 1, provider: 'alpha', model: 'one', workspace: '/w' });
  await p.go('Bob');
  const doc = p.context.document;
  p.openPicker('files'); await settle();
  assert.match(doc.getElementById('pickerlist').innerHTML, /data-found="~\/notes\.md"/);
  assert.doesNotMatch(doc.getElementById('pickerlist').innerHTML, /data-file=/, 'a message link\'s attribute would open it beside too');
  const row = { dataset: { found: '~/notes.md' } };
  await doc.getElementById('pickerlist').listeners.click({ target: { closest: (s) => s === '[data-found]' ? row : null } }); await settle();
  assert.equal(p.S.selected, '▤/w/~/notes.md'); assert.ok(!p.S.ui.file);
});

test('⌘P from Home with no folder says what it searches', async () => {
  const p = page({ request: async () => ({ nodes: [], workspaces: [], next_from: null }), listFiles: async () => { throw new Error('unexpected'); } });
  p.openPicker('files'); await settle();
  assert.match(p.context.document.getElementById('pickerlist').innerHTML, /Open an agent first/);
});

test('Escape in a preview the reader is in closes the file it shows', async () => {
  const p = page({ readFile: async () => new TextEncoder().encode('<button>x</button>') }), c = p.context, win = {};
  p.setRender(() => {}); await p.openFile('Bob', '/w/p.html');
  const frame = { contentWindow: win, closest: (s) => s === '.pane.side .fview' ? {} : null };
  c.document.querySelectorAll = (s) => s === '.rc iframe' ? [frame] : [];
  // A page the reader is not in cannot close it.
  c.window.listeners.message({ source: win, data: { rich: 'escape' } });
  assert.ok(p.S.ui.file);
  c.document.activeElement = frame;
  c.window.listeners.message({ source: win, data: { rich: 'escape' } });
  assert.equal(p.S.ui.file, null);
});

test('a file closed and opened again never shares a view key with the one before', async () => {
  const p = page({ readFile: async () => new TextEncoder().encode('{}') });
  p.setRender(() => {});
  await p.openFile('Bob', '/w/c.vl.json'); const first = p.S.ui.file.gen;
  p.dropFile(); const again = p.openFile('Bob', '/w/c.vl.json');
  assert.ok(p.S.ui.file.gen > first, `${p.S.ui.file.gen} after ${first}`);
  await again; assert.ok(p.S.ui.file.gen > first + 1);
});

test('a file an agent rewrote while open waits for a click to run', async () => {
  const enc = (s) => new TextEncoder().encode(s), opened = [];
  const p = page({ readFile: async (full) => { opened.push(full); return enc('<p>hi</p>'); } }), Rich = p.context.Rich;
  assert.match(Rich.file('/w/p.html', enc('<p>hi</p>')).html, /data-kind="html" data-view="view"/);
  assert.match(Rich.file('/w/p.html', enc('<p>hi</p>'), false, false).html, /data-kind="html" data-view="code"/);
  assert.match(Rich.file('/w/d.mmd', enc('graph TD'), false, false).html, /data-lazy data-view="code"/);
  p.S.config = { workspace: '/w' };
  p.S.ui.file = { bot: 'Bob', full: '/w/p.html', asked: true, gen: 2, state: 'ok', bytes: enc('<p>hi</p>'), more: false, url: null };
  await p.onEvent({ event: 'tool_started', bot: 'Bob', turn: 1, data: { call_id: 'w1', name: 'write', arguments: JSON.stringify({ path: 'p.html', content: 'x' }) } });
  await p.onEvent({ event: 'tool_completed', bot: 'Bob', turn: 1, data: { call_id: 'w1' } });
  await settle();
  assert.deepEqual(opened, ['/w/p.html']); assert.equal(p.S.ui.file.asked, false);
  // A write that failed or was refused changed nothing: what is shown stays as it is.
  for (const [id, extra] of [['w2', { failed: true }], ['w3', { denied: true }]]) {
    await p.onEvent({ event: 'tool_started', bot: 'Bob', turn: 1, data: { call_id: id, name: 'edit', arguments: JSON.stringify({ path: 'p.html' }) } });
    await p.onEvent({ event: 'tool_completed', bot: 'Bob', turn: 1, data: { call_id: id, ...extra } });
  }
  await settle();
  assert.deepEqual(opened, ['/w/p.html']);
});

test('an SVG file is drawn after a declaration, comments and a doctype', () => {
  const p = page(), Rich = p.context.Rich, enc = (s) => new TextEncoder().encode(s);
  const svg = '<?xml version="1.0"?>\n<!-- Generator: tool -->\n<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "x.dtd" [ <!ENTITY a "b"> ]>\n<svg xmlns="http://www.w3.org/2000/svg"/>';
  assert.match(Rich.file('/w/a.svg', enc(svg)).html, /data-kind="svg"/);
  assert.doesNotMatch(Rich.file('/w/b.svg', enc('<!-- open <svg/>')).html, /data-kind="svg"/);
  assert.doesNotMatch(Rich.file('/w/c.svg', enc('<svgx/>')).html, /data-kind="svg"/);
  // Many comments are read in one pass.
  const t0 = Date.now(); Rich.file('/w/d.svg', enc('<!--a-->'.repeat(100000) + '<p/>')); assert.ok(Date.now() - t0 < 1000);
});

test('a name starting with a tilde is the folder\'s; only ~/ is home', () => {
  const p = page();
  assert.equal(p.joinPath('/w', '~notes.md'), '/w/~notes.md');
  assert.equal(p.joinPath('/w', '~/notes.md'), '~/notes.md');
});

test('a link in a drawn diagram opens through the guarded opener', () => {
  const p = page(), Rich = p.context.Rich, opened = [];
  p.context.__TAURI__ = { core: { invoke: (cmd, args) => { opened.push([cmd, args.url]); return Promise.resolve(); } } };
  const click = (attrs, dataset) => { let prevented = false; const a = { dataset, getAttribute: (k) => attrs[k] ?? null };
    const handled = Rich.click({ target: { closest: (sel) => sel.includes('.rc a') ? a : null }, preventDefault: () => { prevented = true; } });
    return handled && prevented; };
  assert.equal(click({ 'xlink:href': 'https://example.com/m' }), true);
  assert.equal(click({ href: 'javascript:alert(1)' }), true);
  // A Markdown link is inert, its target data the opener reads.
  assert.equal(click({ href: '#' }, { href: 'https://example.com/d' }), true);
  assert.deepEqual(opened, [['open_link', 'https://example.com/m'], ['open_link', 'https://example.com/d']]);
});

test('charts, file links and opened files draw by kind', () => {
  const p = page(), Rich = p.context.Rich;
  // A chart in a message draws when asked; one in a file someone opened draws at once.
  assert.match(Rich.html('```vega-lite\n{"mark":"bar"}\n```'), /data-kind="chart" data-lang="vega-lite" data-lazy data-view="code"/);
  assert.match(Rich.file('/w/c.vl.json', new TextEncoder().encode('{}')).html, /data-kind="chart" data-lang="vega-lite" data-lazy data-page/);
  assert.match(Rich.html('```vega\n{}\n```'), /data-lang="vega"/);
  const links = Rich.html('[plan](PLAN.md) [code](src/a.rs:12) [line](src/b.rs#L4-L9) [web](https://example.com) [here](#top)');
  assert.match(links, /<a class="file" href="#" data-file="PLAN.md">plan<\/a>/);
  assert.match(links, /data-file="src\/a.rs">code/);
  // A file with no folder takes its line too; a scheme with a number names no file.
  assert.match(Rich.html('[code](a.rs:12) [doc](README.md:20:4)'), /data-file="a.rs">code.*data-file="README.md">doc/);
  assert.doesNotMatch(Rich.html('[call](tel:12345)'), /data-file/);
  assert.match(links, /data-file="src\/b.rs">line/);
  // A section of another file opens that file; a `#` in a name is written `%23`.
  assert.match(Rich.html('[install](README.md#install)'), /data-file="README.md">install/);
  assert.match(Rich.html('[odd](notes/a%23b.md)'), /data-file="notes\/a#b.md">odd/);
  // The line suffix is read before decoding, so `%3A` is a colon in the name.
  assert.match(Rich.html('[log](logs/build%3A2026)'), /data-file="logs\/build:2026">log/);
  assert.match(Rich.html('[log](a%3A2026)'), /data-file="a:2026">log/);
  assert.match(links, /<a href="#" data-href="https:\/\/example.com">web<\/a>/);
  assert.doesNotMatch(links, /data-file="#top"/);
  const enc = (text) => new TextEncoder().encode(text);
  assert.match(Rich.file('/w/PLAN.md', enc('# Plan')).html, /<div class="md"><h1>Plan<\/h1>/);
  assert.match(Rich.file('/w/x.rs', enc('fn a() {}')).html, /data-kind="code".*<span class="lang">rs<\/span>/s);
  assert.match(Rich.file('/w/r/latency.vl.json', enc('{}')).html, /data-kind="chart"/);
  assert.match(Rich.file('/w/flow.mmd', enc('graph TD')).html, /data-kind="mermaid"/);
  assert.match(Rich.file('/w/frag.html', enc('<b>x</b>')).html, /data-kind="html" data-view="view"/);
  assert.match(Rich.file('/w/t.csv', enc('a,b\n1,"2"')).html, /<th>a<\/th><th>b<\/th>.*<td>1<\/td><td>2<\/td>/s);
  // A quoted field keeps its separators, line breaks and doubled quotes.
  assert.match(Rich.file('/w/q.csv', enc('name,note\r\nAlice,"a,b"\nBob,"say ""hi""\nthen go"\n')).html,
    /<td>Alice<\/td><td>a,b<\/td><\/tr><tr><td>Bob<\/td><td>say &quot;hi&quot;\nthen go<\/td><\/tr><\/tbody>/);
  assert.match(Rich.file('/w/t.tsv', enc('a\tb\n1\t2')).html, /<td>1<\/td><td>2<\/td>/);
  // Columns are capped as well as rows, so a line of separators costs no more than a wide table.
  assert.equal(Rich.file('/w/wide.csv', enc(','.repeat(100000))).html.match(/<th>/g).length, 256);
  // Rows stop at the one reaching 10,000 cells, and the view says rows were left out.
  const many = Rich.file('/w/many.csv', enc((','.repeat(255) + '\n').repeat(1001))).html;
  assert.ok(many.match(/<t[hd]>/g).length <= 10000 + 256);
  assert.match(many, /showing the first 39 rows/);
  assert.doesNotMatch(Rich.file('/w/t.csv', enc('a,b\n1,2\n')).html, /showing/);
  assert.match(Rich.file('/w/blob.bin', new Uint8Array([1, 0, 2])).html, /binary file · 3 bytes/);
  assert.match(Rich.file('/w/big.log', enc('x'), true).html, /showing the first/);
  assert.match(Rich.file('/w/<i>.md', enc('<script>alert(1)</script>')).html, /&lt;script&gt;/);
});

test('a message is parsed once and drawn again from what it kept', () => {
  const p = page(), t = p.transcript('Bob'); let parsed = 0;
  const real = p.context.Rich.html; p.context.Rich.html = (text) => { parsed++; return real(text); };
  t.items = Array.from({ length: 50 }, (_, i) => ({ kind: 'text', turn: i, text: `reply **${i}**` }));
  const first = p.itemsHTML(t), again = p.itemsHTML(t);
  assert.equal(parsed, 50); assert.equal(first, again);
  assert.ok(t.bytes > 0);
  t.items[3].text = 'changed'; p.itemsHTML(t); assert.equal(parsed, 51);
});

test('drawing messages past the byte bound folds the oldest instead of keeping them', () => {
  const p = page(), t = p.transcript('Bob'), el = p.context.document.getElementById('log');
  const DECODE_BYTES = 8 * 1024 * 1024, text = 'word '.repeat(40000);
  t.items = Array.from({ length: 20 }, (_, i) => ({ kind: 'text', turn: i, from: i, text, bytes: text.length * 2 }));
  t.bytes = t.items.reduce((n, it) => n + it.bytes, 0);
  assert.ok(t.bytes < DECODE_BYTES);
  el.lastElementChild = p.context.document.createElement('div');
  p.renderTranscript(el, 'Bob');
  assert.ok(t.bytes <= DECODE_BYTES, `${t.bytes} bytes kept`);
  assert.equal(t.items[0].kind, 'history');
  assert.ok(t.items.some((it) => it.kind === 'text' && it.html));
  assert.match(el.innerHTML, /earlier history/);
});

test('what drawn messages put on the page counts toward the window\'s bound across messages', () => {
  const p = page(), t = p.transcript('Bob'), el = p.context.document.getElementById('log');
  const DECODE_BYTES = 8 * 1024 * 1024, list = '- x\n'.repeat(49999);
  // Five replies, each within its own bounds, with a tool call between them.
  t.items = Array.from({ length: 5 }, (_, i) => p.entries({ role: 'assistant', content: [{ type: 'text', text: list }, { type: 'tool_use', id: `c${i}`, name: 'read', input: {} }] })
    .map((e) => ({ ...e, turn: i, from: i, bytes: (e.text ?? '').length * 2 }))).flat();
  t.bytes = t.items.reduce((n, it) => n + it.bytes, 0);
  assert.ok(t.bytes < DECODE_BYTES);
  el.lastElementChild = p.context.document.createElement('div');
  p.renderTranscript(el, 'Bob');
  const items = (el.innerHTML.match(/<li>/g) ?? []).length;
  assert.ok(items > 0 && items <= 100000, `${items} list items drawn`);
  assert.ok(t.bytes <= DECODE_BYTES, `${t.bytes} bytes kept`);
  assert.match(el.innerHTML, /earlier history/);
});

test('an item added to a drawn pane hydrates only what was added', () => {
  const p = page(), c = p.context, t = p.transcript('Bob'), el = c.document.getElementById('log'), R = c.Rich, seen = [];
  c.Rich = { html: R.html, get version() { return R.version; }, get waited() { return R.waited; }, hydrate: (n) => { seen.push(n); } };
  t.items = [{ kind: 'text', turn: 1, text: 'first' }];
  const shown = { name: 'shown' };
  const tail = Object.assign(c.document.createElement('div'), { classList: { contains: (c) => c === 'tail' }, previousElementSibling: shown,
    insertAdjacentHTML(_, html) { const n = { name: 'added', html, nextElementSibling: tail }; shown.nextElementSibling = n; } });
  el.lastElementChild = tail; p.renderTranscript(el, 'Bob');
  seen.length = 0; el.lastElementChild = tail;
  t.items.push({ kind: 'text', turn: 2, text: 'second' });
  p.renderTranscript(el, 'Bob');
  assert.deepEqual(seen.map((n) => n.name), ['added']);
});

test('a step links the whole path it named, not its shortened summary', async () => {
  const p = page(), long = `/w/${'d/'.repeat(200)}a.md`;
  await p.onEvent({ event: 'tool_started', bot: 'Bob', turn: 1, data: { call_id: 'c1', name: 'read', arguments: JSON.stringify({ path: long }) } });
  await p.onEvent({ event: 'tool_started', bot: 'Bob', turn: 1, data: { call_id: 'c2', name: 'shell', arguments: JSON.stringify({ command: 'ls', path: '/w/x' }) } });
  const [read, shell] = p.transcript('Bob').items.filter((it) => it.kind === 'tool');
  assert.equal(read.path, long); assert.ok(read.summary.length < long.length);
  assert.equal(shell.path, undefined);
  p.S.ui.steps = true;
  assert.match(p.runHTML(p.transcript('Bob'), 0).html, new RegExp(`<a class="fpath" href="#" data-file="${long}"`));
});

test('a path a message or step names opens from the folder its turn ran in', async () => {
  const opened = [];
  const p = page({ readFile: async (full) => { opened.push(full); return new TextEncoder().encode('x'); } });
  p.setRender(() => {});
  p.upsert({ name: 'Bob', bot_id: 7, provider: 'alpha', model: 'one', workspace: '/now' });
  p.S.selected = 'Bob';
  await p.onEvent({ event: 'accepted', bot: 'Bob', turn: 1, data: { workspace: '/then' } });
  const t = p.transcript('Bob');
  // A message and a step carry their turn; a click inside one opens from that turn's folder.
  assert.match(p.textHTML({ kind: 'text', text: '[a](a.md)', turn: 1 }, t), /^<div class="md" data-turn="1">/);
  await p.onEvent({ event: 'tool_started', bot: 'Bob', turn: 1, data: { call_id: 'c1', name: 'read', arguments: JSON.stringify({ path: 'b.md' }) } });
  p.S.ui.steps = true; assert.match(p.runHTML(t, t.items.findIndex((it) => it.kind === 'tool')).html, /data-file="b.md" data-turn="1"/);
  const inTurn = (turn) => ({ closest: (s) => s === '[data-turn]' && turn != null ? { dataset: { turn: String(turn) } } : null });
  await p.openFileFrom('a.md', inTurn(1));
  // A turn the daemon no longer names, or none, opens from the agent's folder now.
  await p.openFileFrom('a.md', inTurn(9)); await p.openFileFrom('a.md', inTurn(null));
  assert.deepEqual(opened, ['/then/a.md', '/now/a.md', '/now/a.md']);
});

test('a history page records the folder of each turn it lists', async () => {
  const p = page({ request: async (op) => op === 'history_nodes' ? { nodes: [{ node: 2, turn: 4 }, { node: 1, turn: 3 }], workspaces: [{ folder: '/a', turns: [3] }, { folder: '/b', turns: [4] }], next_from: null, next_newer: null } : { items: [] } });
  const t = p.transcript('Bob'); t.items.push({ kind: 'history', next: null, min: null, loaded: false });
  await p.load('Bob', true);
  assert.deepEqual(new Map(t.folders), new Map([[3, '/a'], [4, '/b']]));
});

test('folding turns out of the window drops their folders', () => {
  const p = page(); const t = p.transcript('Bob');
  t.items = Array.from({ length: 1700 }, (_, i) => ({ kind: 'node', node: i + 1, turn: i + 1 }));
  for (let turn = 1; turn <= 1700; turn++) t.folders.set(turn, '/w');
  p.evict(t);
  assert.equal(t.folders.has(1), false);
  assert.equal(t.folders.get(1700), '/w');
  assert.ok(t.folders.size <= 1200);
});

test('a chat covered by a file beside is not seen until the file closes', () => {
  const p = page();
  p.S.ui.side = 'Bob'; p.S.ui.file = { bot: 'Ann', full: '/w/a.md', url: null }; p.S.unseen.add('Bob');
  p.markSeen(); assert.ok(p.S.unseen.has('Bob'));
  p.S.ui.file = null; p.markSeen(); assert.ok(!p.S.unseen.has('Bob'));
});

test('a file opened from an agent closes when that agent is forgotten', () => {
  const p = page();
  p.S.ui.file = { bot: 'Bob', full: '/w/a.md', url: null };
  p.forgetBot('Carol'); assert.ok(p.S.ui.file);
  p.forgetBot('Bob'); assert.equal(p.S.ui.file, null);
});

test('a long streamed line is searched for its end once, not on every delta', () => {
  const p = page(), Rich = p.context.Rich, st = {};
  const line = 'x'.repeat(200000); let searched = 0;
  for (let i = 1000; i <= line.length; i += 1000) {
    const text = 'para\n\n' + line.slice(0, i);
    const from = Math.max(st.scan ?? 0, st.seen ?? 0); searched += text.length - from;
    assert.equal(Rich.cut(st, text), 6);
  }
  assert.ok(searched < 210000, `searched ${searched} characters`);
  assert.equal(Rich.cut(st, 'para\n\n' + line + '\n\nnext'), 6 + line.length + 2);
});

test('streamed Markdown draws each finished block once and keeps fences whole', () => {
  const p = page(), Rich = p.context.Rich;
  const st = {};
  const text = 'para one\n\n```js\nconst a = 1;\n\nconst b = 2;\n```\nafter\n\n- item';
  const cuts = []; for (let i = 1; i <= text.length; i++) cuts.push(Rich.cut(st, text.slice(0, i)));
  const fence = text.indexOf('```\nafter') + 4, para = text.indexOf('- item');
  assert.deepEqual([...new Set(cuts)], [0, 10, fence, para]);
  assert.equal(text.slice(10, fence), '```js\nconst a = 1;\n\nconst b = 2;\n```\n');

  const t = p.transcript('Bob'); t.streamingTurn = 1; t.streamGen = 1; t.text = '';
  const el = { dataset: {}, children: [], replaceChildren(...nodes) { this.children = nodes; } };
  let parsed = 0; const real = Rich.html; Rich.html = (s) => { parsed++; return real(s); };
  for (const ch of text) { t.text += ch; p.renderTail(el, 'Bob', t); }
  const [done, line] = el.children;
  assert.equal(parsed, 3);
  assert.deepEqual(done.children.map((c) => c.html), [real(text.slice(0, 10)), real(text.slice(10, fence)), real(text.slice(fence, para))]);
  assert.match(done.children[1].html, /data-kind="code"/);
  assert.equal(line.children[0].data, '- item');
});

test('a streamed reply\'s blocks share one message\'s bounds, then stream as text', () => {
  const p = page(), t = p.transcript('Bob'); t.streamingTurn = 1; t.streamGen = 1; t.text = '';
  const el = { dataset: {}, children: [], replaceChildren(...nodes) { this.children = nodes; } };
  const list = '- x\n'.repeat(30000) + '\n';
  for (const piece of [list, list, 'more\n\n', 'tail']) { t.text += piece; p.renderTail(el, 'Bob', t); }
  const [done, line] = el.children, html = done.children.map((c) => c.html);
  assert.equal(html.length, 2);
  assert.match(html[0], /<li>/); assert.doesNotMatch(html[1], /<li>/);
  assert.equal(line.children[0].data, 'more\n\ntail');
});

test('tool-heavy history folds rows and restores their summaries on scroll', async () => {
  const p = page();
  for (let i = 0; i < 4000; i++) {
    await p.onEvent({event:'tool_started', bot:'Bob', turn:i, data:{call_id:`c${i}`, name:'shell', arguments:JSON.stringify({command:`echo ${i}`})}});
    await p.onEvent({event:'tool_completed', bot:'Bob', turn:i, data:{call_id:`c${i}`}});
  }
  const t = p.transcript('Bob'); p.evict(t);
  assert.ok((p.itemsHTML(t).match(/data-call=/g) || []).length <= 1600);
  t.anchor = 'top'; await p.loadBatch('Bob');
  assert.ok(t.items.some(it => it.kind === 'tool' && it.summary === 'echo 2799'));
  assert.ok(t.items.filter(it => it.kind === 'tool').every(it => it.done));
});

test('CSS string escaping removes literal line controls and preserves escaped quotes', () => {
  const { cssEsc, esc } = page();
  assert.equal(esc("id\rpart"), "id&#13;part", "preserve carriage return through HTML attribute parsing");
  assert.equal(cssEsc('a\n\r\f"\\z'), 'a\\a \\d \\c \\"\\\\z');
});

test('incomplete creation events fail explicitly without a resume round trip', async () => {
  let requests = 0; const p = page({request:async () => {requests++; return {name:'bad'};}});
  await assert.rejects(p.onEvent({event:'created',bot:'bad',data:{}}), /invalid_created_event/);
  assert.equal(requests,0); assert.equal(p.S.bots.has('bad'),false);
});

test('same-millisecond submissions use distinct idempotency keys across clients', async () => {
  const ids = [];
  for (let i = 0; i < 2; i++) {
    const p = page({request:async (_,params) => {ids.push(params.request_id);}});
    vm.runInContext('Date.now = () => 123',p.context);
    p.upsert({name:'Bob',bot_id:1}); p.S.selected='Bob'; p.S.config={workspace:'/synthetic'};
    await Promise.all([p.submit('first'),p.submit('second')]);
  }
  assert.equal(new Set(ids).size,4);
});

test('fork history loads by checkpoint in pages even without its source bot', async () => {
  const pages = [];
  const p = page({request:async (op,params) => {
    if (op === 'history_nodes') { pages.push(params.from); return params.from === 4 ? {nodes:[{node:4,turn:2},{node:3,turn:2}],workspaces:[],next_from:2} : {nodes:[{node:2,turn:1},{node:1,turn:1}],workspaces:[],next_from:null}; }
    assert.equal(op,'item'); return {role:params.node % 2 ? 'user':'assistant',content:[{type:'text',text:`inherited ${params.node}`}]};
  }});
  await p.onEvent({event:'forked',bot:'branch',data:{provider:'test',bot_id:2,source:'deleted-source',checkpoint:4}});
  await p.onEvent({event:'text_delta',bot:'branch',turn:3,text:'new work'});
  await p.load('branch');
  let t=p.transcript('branch'); assert.match(p.itemsHTML(t), /inherited 3/); assert.match(p.itemsHTML(t), /inherited 4/);
  assert.equal(t.items.find(it=>it.text==='inherited 4').turn,2); assert.equal(t.text,'new work'); assert.deepEqual(pages,[4]);
  t.anchor='top'; await p.load('branch',true);
  assert.deepEqual(pages,[4,2]);
  const html=p.itemsHTML(t); assert.ok(html.indexOf('inherited 1') < html.indexOf('inherited 4'));
  assert.equal(t.items.some(it=>it.kind==='history'),false);
});


test('persisted tool calls render for both providers without duplicating live events', async () => {
  for (const item of [
    {type:'function_call',name:'shell',call_id:'call',arguments:'{"command":"echo hello"}'},
    {role:'assistant',content:[{type:'tool_use',name:'shell',id:'call',input:{command:'echo hello'}}]},
  ]) {
    const p=page({request:async()=>item}); const t=p.transcript('Bob');
    t.items=[{kind:'node',node:1,turn:1}];t.nodes=1;
    await p.loadBatch('Bob');
    assert.equal(t.items.filter(it=>it.kind==='tool').length,1);
    await p.onEvent({event:'tool_started',bot:'Bob',turn:1,data:{name:'shell',call_id:'call',arguments:'{"command":"echo hello"}'}});
    assert.equal(t.items.filter(it=>it.kind==='tool').length,1);
    assert.equal(t.items[0].done,false);
    await p.onEvent({event:'tool_completed',bot:'Bob',turn:1,data:{call_id:'call'}});
    assert.equal(t.items[0].done,true);
    assert.match(p.itemsHTML(t),/echo hello/);
    const inherited=p.transcript('branch');
    inherited.items=[{kind:'node',node:1,turn:1},{kind:'node',node:2,turn:2}]; inherited.nodes=2;
    await p.loadBatch('branch');
    assert.equal(inherited.items.filter(it=>it.kind==='tool').length,2,'inherited turns may reuse call IDs');
  }
});


test('fork peers use creator identity and stale snapshots cannot resurrect or replace bots', async () => {
  const p = page(); p.S.session = 1; p.S.snapshot = true;
  p.upsert({name:'parent',bot_id:1});
  await p.handle({event:'forked',bot:'branch',data:{provider:'test',bot_id:2,created_by:'parent',created_by_id:1}},1);
  assert.deepEqual(Array.from(p.transcript('parent').peers),['branch']);
  await p.handle({event:'deleted',bot:'branch'},1);
  p.seat({name:'branch',bot_id:2,provider:'test'},1);
  assert.equal(p.S.bots.has('branch'),false);
  const creating=p.handle({event:'created',bot:'branch',data:{bot_id:3,provider:'test'}},1);
  p.seat({name:'branch',bot_id:2,provider:'old'},1);
  await creating;
  assert.equal(p.S.bots.get('branch').id,3);
});

test('truncated tool previews preserve decoded flags and tool failures stay visible', async () => {
  const input = {command:'echo '+ 'x'.repeat(3000),background:true};
  let output = {type:'function_call',call_id:'call',name:'shell',arguments:JSON.stringify(input)};
  const p = page({request:async()=>output}); const t=p.transcript('Bob');
  await p.onEvent({event:'message',bot:'Bob',turn:1,data:{node:1}}); await p.loadBatch('Bob');
  await p.onEvent({event:'tool_started',bot:'Bob',turn:1,data:{call_id:'call',name:'shell',arguments:JSON.stringify(input).slice(0,2048),arguments_truncated:true}});
  assert.equal(t.items.find(it=>it.kind==='tool').background,true);
  // A path longer than the preview keeps the one decoded from the committed call.
  const path='/w/'+'d/'.repeat(1100)+'f.md'; output={type:'function_call',call_id:'read',name:'read',arguments:JSON.stringify({path})};
  await p.onEvent({event:'message',bot:'Bob',turn:1,data:{node:4}}); await p.loadBatch('Bob');
  await p.onEvent({event:'tool_started',bot:'Bob',turn:1,data:{call_id:'read',name:'read',arguments:JSON.stringify({path}).slice(0,2048),arguments_truncated:true}});
  assert.equal(t.items.find(it=>it.callId==='read').path,path);
  output={type:'function_call_output',output:'{"error":"spawn_failed"}'};
  await p.onEvent({event:'tool_completed',bot:'Bob',turn:1,data:{call_id:'call',node:2}});
  await p.loadBatch('Bob'); assert.match(p.itemsHTML(t),/spawn_failed/);
  await p.onEvent({event:'tool_started',bot:'Bob',turn:1,data:{call_id:'wait',name:'wait',arguments:'null'}});
  output={type:'function_call_output',output:'{"error":"invalid_arguments"}'};
  await p.onEvent({event:'tool_completed',bot:'Bob',turn:1,data:{call_id:'wait',node:3}});
  await p.loadBatch('Bob'); assert.match(p.itemsHTML(t),/invalid_arguments/);
});

function historyDaemon(requests = []) {
  return {request:async(op,q)=> {
    if(op==='item') return {role:'assistant',content:[{type:'text',text:`message ${q.node}`}]};
    assert.equal(op,'history_nodes'); requests.push(q);
    const min=q.min_node ?? 1, max=q.from;
    const first=q.oldest_first ? min : Math.max(min,max-q.limit+1);
    const last=q.oldest_first ? Math.min(max,min+q.limit-1) : max;
    return {nodes:Array.from({length:Math.max(0,last-first+1)},(_,i)=>({node:last-i,turn:Math.ceil((last-i)/2)})),workspaces:[],next_from:first>min?first-1:null,next_newer:last<max?last+1:null};
  }};
}

test('100000 nodes retain bounded ranges and scroll in both directions without skipping a page', async () => {
  const requests=[];const p=page(historyDaemon(requests));
  for(let node=1;node<=100000;node++) await p.onEvent({event:'message',bot:'Bob',turn:node,data:{node}});
  const t=p.transcript('Bob'); p.evict(t);
  assert.ok(t.items.length<=1601); assert.ok(t.items.filter(it=>it.kind==='history').length<=2);
  await p.load('Bob'); t.anchor='top';
  // Fill the preceding bare window before reaching the compact prefix.
  for(let i=0;i<5;i++) await p.load('Bob',true);
  assert.ok(requests.length>0); assert.ok(t.items.length<=1602);
  const before=Math.max(...t.items.filter(it=>it.from!=null).map(it=>it.from));
  t.anchor='end'; await p.load('Bob');
  const forward=requests.findLast(q=>q.oldest_first);
  assert.ok(forward); assert.equal(forward.min_node,before+1);
  assert.match(p.itemsHTML(t),new RegExp(`message ${before+1}`));
});

test('large item decoding holds one reply and a bounded decoded byte window', async () => {
  let active=0,peak=0,requests=0;
  const p=page({request:async()=>{active++;peak=Math.max(peak,active);requests++;await Promise.resolve();active--;return {role:'assistant',content:[{type:'text',text:'x'.repeat(512*1024)}]};}});
  const t=p.transcript('Bob'); t.items=Array.from({length:400},(_,i)=>({kind:'node',node:i+1,turn:1}));t.nodes=400;
  await p.loadBatch('Bob');
  assert.equal(peak,1);assert.ok(requests<=8);assert.ok(t.items.reduce((sum,it)=>sum+(it.bytes||0),0)<=8*1024*1024);
});

test('process-heavy history folds cards into recoverable result ranges', async () => {
  const base=historyDaemon();
  const p=page({request:async(op,q)=>op==='history_nodes'?base.request(op,q):q.node%2?{type:'function_call_output',call_id:`c${(q.node-1)/2}`,output:JSON.stringify({handle:`proc:${q.node}`})}:{type:'function_call',name:'shell',call_id:`c${q.node/2}`,arguments:'{"command":"work","background":true}'}});
  for(let i=1;i<=2500;i++) {
    await p.onEvent({event:'message',bot:'Bob',turn:i,data:{node:i*2}});
    await p.onEvent({event:'tool_started',bot:'Bob',turn:i,data:{call_id:`c${i}`,name:'shell',arguments:'{"command":"work","background":true}'}});
    await p.onEvent({event:'tool_completed',bot:'Bob',turn:i,data:{call_id:`c${i}`,node:i*2+1}});
  }
  const t=p.transcript('Bob');p.evict(t);
  assert.ok(t.items.length<=1602);assert.ok(t.items.filter(it=>it.kind==='proc').length<=1200);
  assert.ok(t.items.some(it=>it.kind==='history'));
  const floor=t.items.find(it=>it.kind==='history').next;
  t.anchor='top';await p.load('Bob',true);
  assert.ok(t.items.some(it=>it.kind==='proc' && it.from<=floor), 'scroll restores folded process cards');
});


test('a waiting sidebar row is one row: its glyph says waiting, and no handle line follows it', () => {
  const p=page();p.upsert({name:'Bob',bot_id:1});const b=p.S.bots.get('Bob');
  b.status='waiting';b.waitingOn=['turn:<img src=x onerror=alert(1)>/1'];
  const html=p.botRowHTML({b,depth:1,kids:0});
  assert.ok(!html.includes('img'));assert.doesNotMatch(html,/class="w"/);
  assert.match(html,/role="button" tabindex="0"><span class="glyph waiting">/);
  assert.ok(html.endsWith('</div>') && html.indexOf('<div class="botrow')===0 && html.lastIndexOf('<div')===0,'exactly one element per row');
});

test('event-only notes are bounded with an explicit summary', async () => {
  const p=page();
  for(let i=0;i<10000;i++) {
    await p.onEvent({event:i%2?'queued':'turn_finished',bot:'Bob',turn:i,data:{status:'failed',error:'synthetic failure'}});
  }
  const t=p.transcript('Bob');p.evict(t);
  assert.ok(t.items.length<=1602);assert.match(p.itemsHTML(t),/activity notes/);
});

test('disjoint rows from one node produce no overlapping history ranges', () => {
  const p=page(),t=p.transcript('Bob');
  t.items=[{kind:'text',from:1,text:'one'},{kind:'peer',who:'child'},{kind:'tool',from:1,callId:'call'},...Array.from({length:2000},(_,i)=>({kind:'node',node:i+2}))];t.nodes=2000;
  p.evict(t);
  const ranges=t.items.filter(it=>it.kind==='history').sort((a,b)=>a.min-b.min);
  for(let i=1;i<ranges.length;i++) assert.ok(ranges[i-1].next<ranges[i].min);
});

test('pruned replay can load the retained snapshot lineage without duplicate nodes', async () => {
  const p=page(historyDaemon());p.S.session=1;p.S.snapshot=true;
  p.seat({name:'Bob',bot_id:1,provider:'test',head:10},1);
  await p.onEvent({event:'message',bot:'Bob',turn:5,data:{node:9}});
  await p.onEvent({event:'message',bot:'Bob',turn:5,data:{node:10}});
  await p.load('Bob'); const t=p.transcript('Bob');t.anchor='top';await p.load('Bob',true);
  assert.match(p.itemsHTML(t),/message 1</);
  const rendered=t.items.filter(it=>it.kind==='text').map(it=>it.from);
  assert.equal(new Set(rendered).size,rendered.length);assert.equal(rendered.length,10);
});

test('completed process results retain stdout and stderr in ordinary output', async () => {
  const output=JSON.stringify({results:{'proc:1':{exit_code:0,stdout:'first line\nlast line',stderr:'important diagnostic'}}});
  const p=page({request:async()=>({type:'function_call_output',call_id:'wait',output})});const t=p.transcript('Bob');
  await p.onEvent({event:'tool_started',bot:'Bob',turn:1,data:{name:'wait',call_id:'wait',arguments:'{"handles":["proc:1"]}'}});
  await p.onEvent({event:'tool_completed',bot:'Bob',turn:1,data:{call_id:'wait',node:1}});
  await p.loadBatch('Bob');p.S.ui.steps=true;
  const html=p.itemsHTML(t);assert.match(html,/first line/);assert.match(html,/important diagnostic/);
});

test('snapshot deletion removes creator cards before a name is reused', async () => {
  const p=page({setup:async()=>({}),attach:async()=>({session:2}),pull:()=>new Promise(()=>{}),request:async()=>({bots:[{name:'parent',bot_id:1}],next_after:null})});
  p.upsert({name:'parent',bot_id:1});
  await p.onEvent({event:'created',bot:'child',data:{bot_id:2,provider:'test',created_by:'parent',created_by_id:1}});
  await p.attach();
  await p.onEvent({event:'created',bot:'child',data:{bot_id:3,provider:'test'}});
  assert.equal(p.transcript('parent').peers.includes('child'),false);
  assert.equal(p.transcript('parent').items.some(it=>it.kind==='peer'&&it.who==='child'),false);
});

test('answer deltas follow thinking immediately and failed partial text stays ephemeral', async () => {
  const p=page(),t=p.transcript('Bob');const el={dataset:{},children:[],replaceChildren(...nodes){this.children=nodes;}};
  await p.onEvent({event:'thinking_delta',bot:'Bob',turn:1,text:'thinking'});
  p.renderTail(el,'Bob',t);
  await p.onEvent({event:'text_delta',bot:'Bob',turn:1,text:'answer'});
  p.renderTail(el,'Bob',t);assert.equal(el.children.at(-1).children[0].data,'answer');
  await p.onEvent({event:'turn_finished',bot:'Bob',turn:1,data:{status:'interrupted'}});
  assert.equal(t.text,'');assert.equal(t.items.some(it=>it.kind==='text'&&it.text==='answer'),false);
});


test('history window sends one batch request and leaves byte-limited remainder loadable', async () => {
  let batches=0;
  const p=page({batch:async({nodes})=>{batches++;return {items:nodes.slice(0,2).map(node=>({node,item:{role:'user',content:`node ${node}`}}))};}});
  for(let node=1;node<=4;node++) await p.onEvent({event:'message',bot:'Bob',turn:1,data:{node}});
  await p.loadBatch('Bob');assert.equal(batches,1);assert.equal(p.transcript('Bob').nodes,2);
  await p.loadBatch('Bob');assert.equal(batches,2);assert.equal(p.transcript('Bob').nodes,0);
});


test('creation bursts render the bounded rail once per pulled batch', async () => {
  let pulls=0, writes=0;
  const p=page({pull:async()=> {
    if(pulls===40) { p.S.session=null; return {events:[]}; }
    const start=pulls++*250;
    return {events:Array.from({length:250},(_,i)=>({event:'created',bot:`bot${start+i}`,data:{bot_id:start+i+1,provider:'test'}}))};
  }});
  p.S.session=1; p.S.live=true; p.S.ui.rail=true;
  const rail=p.elements.get('bots');let html='';
  Object.defineProperty(rail,'innerHTML',{get:()=>html,set:value=>{html=value;writes++;}});
  await p.pump(1);
  assert.equal(p.S.bots.size,10000);
  assert.ok(writes<=40, `fleet rail rebuilt ${writes} times for 40 batches`);
  assert.equal((html.match(/data-bot=/g)||[]).length,300);
  assert.match(html,/9700 below/);
});

test('committed thinking and answer nodes match live, replay, and reconnect transcripts', async () => {
  const item={role:'assistant',content:[{type:'thinking',thinking:'durable thought'},{type:'text',text:'durable answer'}]};
  const make=()=>{const p=page({request:async()=>item});p.upsert({name:'Bob',bot_id:1});return p;};
  const live=make(),replay=make();
  await live.onEvent({event:'thinking_delta',bot:'Bob',turn:1,text:'durable thought'});
  await live.onEvent({event:'text_delta',bot:'Bob',turn:1,text:'durable answer'});
  for(const p of [live,replay]) {
    await p.onEvent({event:'message',bot:'Bob',turn:1,data:{node:1}});
    await p.loadBatch('Bob');
    await p.onEvent({event:'turn_finished',bot:'Bob',turn:1,data:{status:'completed'}});
    assert.match(p.itemsHTML(p.transcript('Bob')), /durable answer/);
  }
  // Live keeps how long it watched the thought, which replay cannot know; the rest must match.
  const untimed=(html)=>html.replace(/▸ thought [^<]*/,'▸ thought');
  assert.equal(untimed(live.itemsHTML(live.transcript('Bob'))),replay.itemsHTML(replay.transcript('Bob')));
  await live.onEvent({event:'text_delta',bot:'Bob',turn:2,text:'not committed'});
  live.lost('disconnected');
  assert.equal(live.transcript('Bob').text,'');
});

test('fork snapshot and replay converge on one inherited range in either order', async () => {
  for(const snapshotFirst of [true,false]) {
    const p=page(historyDaemon());p.S.session=1;
    const record={name:'branch',bot_id:2,provider:'test',head:10};
    const event={event:'forked',bot:'branch',data:{bot_id:2,provider:'test',source:'source',checkpoint:10}};
    if(snapshotFirst)p.seat(record,1);
    await p.onEvent(event);
    if(!snapshotFirst)p.seat(record,1);
    assert.equal(p.transcript('branch').items.filter(it=>it.kind==='history').length,1);
    await p.load('branch');
    const ids=p.transcript('branch').items.filter(it=>it.from!=null).map(it=>it.from);
    assert.equal(new Set(ids).size,10);
    assert.equal(ids.length,10);
  }
});

test('a pending process result cannot mutate a reused bot name after reconnect', async () => {
  const reply=deferred();const p=page({request:async()=>reply.promise});p.S.session=1;p.S.live=true;
  p.upsert({name:'Bob',bot_id:1});
  await p.onEvent({event:'tool_started',bot:'Bob',turn:1,data:{call_id:'c',name:'shell',arguments:'{"background":true}'}});
  const completing=p.handle({event:'tool_completed',bot:'Bob',turn:1,data:{node:1,call_id:'c'}},1);
  await settle();p.lost('disconnected');p.S.session=2;p.upsert({name:'Bob',bot_id:2});
  reply.resolve({output:'{"handle":"proc:1"}'});await completing;
  assert.equal(p.transcript('Bob').items.length,0);
  assert.notEqual(p.S.bots.get('Bob').touched,1);
});


test('reconnect after pruning reloads the full lineage in either snapshot/replay order', async () => {
  for(const snapshotFirst of [true,false]) {
    const requests=[],p=page(historyDaemon(requests));p.S.session=1;
    p.upsert({name:'Bob',bot_id:1,head:2});await p.load('Bob');
    p.lost('offline');p.S.session=2;
    const seat=()=>p.seat({name:'Bob',bot_id:1,head:10},2);
    if(snapshotFirst)seat();
    await p.onEvent({event:'pruned',bot:'Bob'});
    for(const node of [9,10])await p.onEvent({event:'message',bot:'Bob',turn:5,data:{node}});
    if(!snapshotFirst)seat();
    await p.load('Bob');p.transcript('Bob').anchor='top';await p.load('Bob',true);
    const ids=p.transcript('Bob').items.filter(it=>it.from!=null).map(it=>it.from);
    assert.deepEqual(Array.from(ids).sort((a,b)=>a-b),Array.from({length:10},(_,i)=>i+1));
    assert.equal(new Set(ids).size,ids.length);
    const loads=requests.length;p.seat({name:'Bob',bot_id:1,head:10},2);await p.load('Bob');
    assert.equal(requests.length,loads,'same-session snapshots do not reload covered history');
  }
});

test('rail scrolling moves a bounded window both ways independently of what is open', () => {
  const p=page();p.S.ui.rail=true;
  for(let i=0;i<1000;i++)p.upsert({name:`bot${i}`,id:i+1});
  p.S.selected='';p.renderRail();const el=p.elements.get('bots');
  el.scrollHeight=1000;el.clientHeight=100;
  for(let i=0;i<6;i++){el.scrollTop=900;el.listeners.scroll();assert.ok((el.innerHTML.match(/data-bot=/g)||[]).length<=300);}
  assert.match(el.innerHTML,/data-bot="bot999"/);
  assert.equal(p.S.selected,'');
  for(let i=0;i<6;i++){el.scrollTop=0;el.listeners.scroll();}
  assert.match(el.innerHTML,/data-bot="bot0"/);
  // A level drawn anew centers on the bot beside.
  p.S.ui.side='bot900';p.S.shapeGen++;p.renderRail();assert.match(el.innerHTML,/data-bot="bot900"/);
});


test('a reconnect snapshot drops the folders of the nodes it rebuilds', async () => {
  const p=page({request:async()=>({nodes:[{node:2,turn:1},{node:1,turn:1}],workspaces:[{folder:'/a',turns:[1]}],next_from:null,next_newer:null})});
  p.S.session=1;p.upsert({name:'Bob',bot_id:1,head:2});await p.load('Bob');
  const t=p.transcript('Bob');assert.equal(t.folders.get(1),'/a');
  p.lost('offline');p.S.session=2;p.seat({name:'Bob',bot_id:1,head:10},2);
  assert.equal(t.folders.has(1),false);
});

test('a delayed reconnect snapshot preserves newer folded replay ranges', async () => {
  const p=page(historyDaemon());p.S.session=1;p.upsert({name:'Bob',bot_id:1,head:2});await p.load('Bob');
  p.lost('offline');p.S.session=2;
  for(let node=9;node<=6000;node++)await p.onEvent({event:'message',bot:'Bob',turn:node,data:{node}});
  p.seat({name:'Bob',bot_id:1,head:10},2);
  const t=p.transcript('Bob');
  const covered=node=>t.items.some(it=>it.node===node||it.from===node||it.kind==='history'&&(it.min??0)<=node&&node<=(it.next-(it.exclusive?1:0)));
  for(let node=1;node<=6000;node++)assert.ok(covered(node),`lost node ${node}`);
  p.evict(t);
  assert.ok(t.items.length<=1605,'recovery retains bounded metadata');
});


test('snapshot-only lineage restores bounded peer cards regardless of page order', async () => {
  const p=page({setup:async()=>({}),attach:async()=>({session:1}),pull:()=>new Promise(()=>{}),request:async(op,q)=> {
    assert.equal(op,'bots');return q.after ? {bots:[{name:'parent',bot_id:1,provider:'test'}]} : {bots:Array.from({length:1000},(_,i)=>({name:`child${i}`,id:i+2,provider:'test',created_by:'parent',created_by_id:1})),next_after:'children'};
  }});
  await p.attach();const t=p.transcript('parent');
  assert.ok(t.peers.includes('child999'));assert.ok(t.peers.length<=600);
  assert.equal(t.items.filter(it=>it.kind==='peer').length,t.peers.length);
});

test('Anthropic block order is identical in live and replayed tool transcripts', async () => {
  const item={role:'assistant',content:[{type:'text',text:'before'},{type:'tool_use',name:'read',id:'c',input:{path:'x'}},{type:'text',text:'after'}]};
  for(const live of [false,true]) {
    const p=page({request:async()=>item});
    await p.onEvent({event:'message',bot:'Bob',turn:1,data:{node:1}});
    if(live)await p.onEvent({event:'tool_started',bot:'Bob',turn:1,data:{call_id:'c',name:'read',arguments:'{"path":"x"}'}});
    await p.loadBatch('Bob');const rows=p.transcript('Bob').items;
    assert.deepEqual(Array.from(rows,it=>it.kind),['text','tool','text']);
    assert.equal(rows[0].text,'before');assert.equal(rows[2].text,'after');
  }
});

test('ready work is interruptible while later queued work preserves the active turn', async () => {
  const calls=[];const p=page({request:async(op,q)=>{calls.push([op,q]);}});p.upsert({name:'Bob',bot_id:1});p.S.selected='Bob';
  await p.onEvent({event:'queued',bot:'Bob',turn:7,data:{status:'ready'}});
  await p.onEvent({event:'queued',bot:'Bob',turn:8,data:{status:'queued'}});
  await p.interrupt();assert.equal(calls.length,1);assert.equal(calls[0][1].turn,7);
});

test('snapshot-first replay preserves every durable node across eviction boundaries', async () => {
  const p=page(historyDaemon());p.S.session=1;p.upsert({name:'Bob',bot_id:1,head:6000});
  const t=p.transcript('Bob');
  for(let node=1;node<=6000;node++)await p.onEvent({event:'message',bot:'Bob',turn:node,data:{node}});
  const covered=new Set();
  for(const it of t.items) {
    if(it.kind==='history')for(let node=Math.max(1,it.min??0);node<=it.next-(it.exclusive?1:0);node++)covered.add(node);
    else if(it.node!=null||it.from!=null)covered.add(it.node??it.from);
  }
  assert.deepEqual([...covered].sort((a,b)=>a-b),Array.from({length:6000},(_,i)=>i+1));
});

test('low-byte replay evicts as soon as the count allowance is exceeded', async () => {
  const p=page();
  for(let node=1;node<=5000;node++) {
    await p.onEvent({event:'message',bot:'Bob',turn:node,data:{node}});
    assert.ok(p.transcript('Bob').items.length<=1600,`count bound at node ${node}`);
  }
});

test('snapshot reconciliation cannot splice over an in-flight history page', async () => {
  const snapshot=deferred(), history=deferred();let firstPull=true,firstHistory=true;
  const base=historyDaemon();
  const p=page({setup:async()=>({}),attach:async()=>({session:2}),
    pull:()=>{if(firstPull){firstPull=false;return Promise.resolve({events:[{event:'follow_live'}]});}return new Promise(()=>{});},
    request:async(op,q)=>{
      if(op==='bots')return snapshot.promise;
      if(op==='history_nodes'&&firstHistory){firstHistory=false;return history.promise;}
      return base.request(op,q);
    }});
  p.setRender(() => {}); // This probe isolates async state ordering, not DOM layout.
  p.S.session=1;p.upsert({name:'Bob',bot_id:1,head:2});p.S.selected='Bob';p.lost('offline');
  const attaching=p.attach();await settle();assert.equal(firstHistory,false);
  snapshot.resolve({bots:[{name:'Bob',bot_id:1,head:10}],next_after:null});await settle();
  history.resolve({nodes:[{node:2,turn:1},{node:1,turn:1}],workspaces:[],next_from:null});await attaching;
  const ids=p.transcript('Bob').items.filter(it=>it.from!=null).map(it=>it.from);
  assert.deepEqual(Array.from(ids).sort((a,b)=>a-b),Array.from({length:10},(_,i)=>i+1));
});

test('a task that ends live before the snapshot names its creator still reaches the coordinator', async () => {
  const snapshot=deferred();let firstPull=true;
  const base=historyDaemon();
  const p=page({setup:async()=>({}),attach:async()=>({session:2}),
    pull:()=>{if(firstPull){firstPull=false;return Promise.resolve({events:[{event:'follow_live'},{event:'turn_finished',bot:'demo.build',turn:4,data:{status:'completed'}}]});}return new Promise(()=>{});},
    request:async(op,q)=>op==='bots'?snapshot.promise:base.request(op,q)});
  p.setRender(() => {});
  p.S.session=1;p.lost('offline');
  const attaching=p.attach();await settle();
  assert.equal(p.S.live,true);assert.equal(p.S.wakes.size,0,'no creator known yet');
  snapshot.resolve({bots:[{name:'demo.lead',bot_id:1,provider:'openai',model:'m'},{name:'demo.build',bot_id:2,provider:'openai',model:'m',created_by:'demo.lead',created_by_id:1}],next_after:null});
  await attaching;
  assert.deepEqual({...p.S.wakes.get('demo.lead').tasks.get('demo.build').theirs},{first:4,turn:4,status:'completed',by:'the person',count:1});
  assert.equal(p.S.heldNews.length,0);
  // News held for a bot deleted meanwhile is not a later same-named bot's.
  p.S.heldNews.push(['demo.build',5,'completed',undefined]);p.forgetBot('demo.build');
  assert.equal(p.S.heldNews.length,0);
});

test('submissions wait for a known bot identity instead of sending an unpinned name', async () => {
  const sent=[];const p=page({request:async(op,q)=>{sent.push([op,q]);}});
  p.S.session=1;p.S.config={workspace:'/synthetic'};
  await p.onEvent({event:'text_delta',bot:'Bob',turn:1,text:'working'});p.S.selected='Bob';
  await assert.rejects(p.submit('next'),/identity/);assert.equal(sent.length,0);
  p.seat({name:'Bob',bot_id:7},1);await p.submit('next');assert.equal(sent[0][1].bot_id,7);
});

test('an oversized item shows one error while neighboring history still decodes', async () => {
  const p=page({batch:async({nodes})=>({items:nodes.map(node=>node===2?{node,error:'item_too_large'}:{node,item:{role:'user',content:`message ${node}`}})})});
  for(const node of [1,2,3])await p.onEvent({event:'message',bot:'Bob',turn:1,data:{node}});
  await p.load('Bob');const items=p.transcript('Bob').items;
  assert.deepEqual(Array.from(items.filter(it=>it.kind==='user').map(it=>it.text)),['message 1','message 3']);
  assert.equal(items.filter(it=>it.kind==='note'&&it.text.includes('item_too_large')).length,1);
});

test('activity summaries do not block paging the evicted durable prefix', async () => {
  const requests=[],p=page(historyDaemon(requests));p.S.session=1;
  const t=p.transcript('Bob');
  t.items=Array.from({length:1800},(_,i)=>i%3===0?{kind:'note',text:'activity'}:{kind:'user',text:`message ${i}`,from:i});
  p.evict(t);assert.equal(t.items[0].kind,'note_gap');
  t.anchor='top';await p.load('Bob',true);
  assert.ok(requests.length>0);
  assert.ok(t.items.some(it=>it.from!=null&&it.from<600));
});

test('app creation carries shared compaction policy and seats its response', async () => {
  let created;
  const p=page({policy:async()=>({instructions:'agent policy',compaction_instructions:'summary policy',note:'test'}),
    request:async(op,q)=>{assert.equal(op,'create');created=q;return{name:q.bot,bot_id:7,head:null};}});
  p.setRender(()=>{});p.S.session=1;p.S.config={workspace:'/synthetic',tools:[]};
  await p.submit('/new Bob test/model');
  assert.equal(created.compaction_instructions,'summary policy');assert.equal(p.S.bots.get('Bob').id,7);
});

test('app creation fails when the workspace policy cannot be composed', async () => {
  const sent=[];
  const p=page({policy:async()=>{throw 'instructions_unreadable: cannot read /synthetic/AGENTS.md: invalid utf-8';},
    request:async(op,q)=>{sent.push([op,q]);return{name:q.bot,bot_id:7,head:null};}});
  p.setRender(()=>{});p.S.session=1;p.S.config={workspace:'/synthetic',tools:[]};
  await assert.rejects(p.submit('/new Bob test/model'),e=>e.startsWith('instructions_unreadable: '));
  assert.equal(sent.length,0);assert.equal(p.S.bots.has('Bob'),false);
});

test('completed Responses and Anthropic thoughts retain observed thinking duration', async () => {
  for(const item of [{type:'reasoning',summary:[{type:'summary_text',text:'reason'}]},
    {role:'assistant',content:[{type:'thinking',thinking:'reason'},{type:'text',text:'answer'}]}]) {
    const p=page({request:async()=>item});p.S.session=1;p.S.live=true;
    let now=1000;vm.runInContext('Date.now = () => clock()',p.context);p.context.clock=()=>now;
    await p.onEvent({event:'thinking_delta',bot:'Bob',turn:1,text:'reason'});
    now=8000;await p.onEvent({event:'text_delta',bot:'Bob',turn:1,text:'answer'});
    now=15000;await p.onEvent({event:'message',bot:'Bob',turn:1,data:{node:1}});
    await p.load('Bob');assert.equal(p.transcript('Bob').items.find(it=>it.kind==='thought').secs,7);
  }
});

// ---------- the app shell: projects, panes, composers, menus, runs ----------
const shell = (daemon = {}, storage = null) => { const p = page(daemon, storage); p.setRender(() => {}); p.S.session = 1; p.S.store = 'store-1'; p.S.config = { workspace: '/synthetic', model: 'alpha/one', tools: [] }; return p; };
// A window whose renders move drafts, as the real render does.
const drafting = (daemon = {}) => { const p = shell(daemon); p.setRender(() => p.followDrafts()); return p; };
const names = (rows) => Array.from(rows, (r) => r.label ?? r.b.name);

test('the sidebar lists one level below what is open, and the crumbs go back up', () => {
  const p = shell();
  p.upsert({ name: 'app.lead', bot_id: 1, provider: 'alpha', model: 'one' });
  p.upsert({ name: 'app.build', bot_id: 2, provider: 'alpha', model: 'one', created_by: 'app.lead', created_by_id: 1 });
  p.upsert({ name: 'app.review', bot_id: 3, provider: 'alpha', model: 'one', created_by: 'app.build', created_by_id: 2 });
  p.upsert({ name: 'app.manual', bot_id: 4, provider: 'alpha', model: 'one' });
  p.upsert({ name: 'loose', bot_id: 5, provider: 'alpha', model: 'one' });
  // A coordinator started by a task heads its own project instead of nesting under the task.
  p.upsert({ name: 'zeta.lead', bot_id: 6, provider: 'alpha', model: 'one', created_by: 'app.build', created_by_id: 2 });
  const rows = p.tree();
  assert.deepEqual(names(rows), ['app.lead', 'app.build', 'app.review', 'app.manual', 'zeta.lead', 'bots', 'loose']);
  assert.equal(rows[0].head, 'app');
  const bot = (n) => p.S.bots.get(n);
  assert.equal(p.shortName(bot('app.lead')), 'app'); assert.equal(p.shortName(bot('app.review')), 'review'); assert.equal(p.shortName(bot('loose')), 'loose');
  const level = (open) => { p.S.selected = open; return Array.from(p.railRows(), (r) => [r.label ?? r.b.name, r.kids ?? 0]); };
  assert.deepEqual(level(''), [['app.lead', 2], ['zeta.lead', 0], ['bots', 0], ['loose', 0]], 'Home: projects with their thread counts, then the rest');
  assert.deepEqual(level('app.lead'), [['app.build', 1], ['app.manual', 0]], 'a project: its threads, a prefixed root among them');
  assert.deepEqual(level('app.build'), [['app.review', 0]]);
  assert.deepEqual(level('app.review'), []);
  assert.match(p.botRowHTML(p.railRows()[0] ?? { b: bot('app.build'), kids: 1 }), /data-act="more" data-who="app.build"/);
  assert.equal(p.upOf('app.review'), 'app.build'); assert.equal(p.upOf('app.manual'), 'app.lead');
  assert.equal(p.upOf('zeta.lead'), '', 'a coordinator sits under Home, whoever started it');
  const crumbs = p.crumbsHTML('app.review');
  assert.match(crumbs, /data-act="home">Home<\/button>.*data-who="app.lead">app<\/button>.*data-who="app.build">build<\/button>.*<b>review<\/b>/);
  assert.equal(p.context.document.title, 'Agent › app › build › review');
});

test('a card looks in beside, full screen takes the tab, and Home and the finder open tabs', async () => {
  const p = shell({ request: async () => ({ nodes: [], workspaces:[],next_from: null }) });
  for (const [name, id] of [['app.lead', 1], ['app.build', 2], ['app.test', 3]]) p.upsert({ name, bot_id: id, provider: 'alpha', model: 'one', created_by: id > 1 ? 'app.lead' : null, created_by_id: id > 1 ? 1 : null });
  p.tree(); await p.go('app.lead');
  assert.deepEqual([...p.S.ui.tabs], ['app.lead'], 'from Home it opens a tab');
  await p.go('app.build', 'beside'); assert.equal(p.S.ui.side, 'app.build');
  await p.go('app.build', 'beside'); assert.equal(p.S.ui.side, null, 'the same card closes it again');
  await p.go('app.build', 'beside');
  await p.go('app.lead', 'beside'); assert.equal(p.S.ui.side, 'app.build', 'the thread in view never opens beside itself');
  await p.go(p.S.ui.side);
  assert.equal(p.S.selected, 'app.build'); assert.equal(p.S.ui.side, null);
  assert.deepEqual([...p.S.ui.tabs], ['app.build'], 'inside a tab it goes down a level in that tab');
  await p.go(''); assert.equal(p.S.selected, ''); assert.deepEqual([...p.S.ui.tabs], ['app.build'], 'Home keeps the tabs');
  await p.go('app.test'); assert.deepEqual([...p.S.ui.tabs], ['app.build', 'app.test']);
  await p.go('app.build'); assert.deepEqual([...p.S.ui.tabs], ['app.build', 'app.test'], 'an agent already in a tab is that tab');
  await p.go('app.lead', 'tab'); assert.deepEqual([...p.S.ui.tabs], ['app.build', 'app.test', 'app.lead']); assert.equal(p.S.selected, 'app.lead');
  await p.go('app.lead', 'close'); assert.equal(p.S.selected, 'app.test', 'a closed tab hands over to the one before');
  await p.go('app.build', 'close'); assert.equal(p.S.selected, 'app.test'); assert.deepEqual([...p.S.ui.tabs], ['app.test']);
  await p.go('app.build', 'tab'); await p.go('app.test');
  await p.go('app.test', 'close'); assert.equal(p.S.selected, '', 'before the first tab is Home'); assert.deepEqual([...p.S.ui.tabs], ['app.build']);
  await p.go('app.test'); await p.go('app.build', 'close'); assert.deepEqual([...p.S.ui.tabs], ['app.test']); assert.equal(p.S.selected, 'app.test');
  // A deleted agent's tab goes up a level; one whose way up is Home closes.
  await p.onEvent({ event: 'deleted', bot: 'app.test' });
  assert.deepEqual([...p.S.ui.tabs], ['app.lead']); assert.equal(p.S.selected, 'app.lead');
  await p.onEvent({ event: 'deleted', bot: 'app.lead' });
  assert.deepEqual([...p.S.ui.tabs], []); assert.equal(p.S.selected, '');
});

test('from Home the arrows open the first or last agent; Enter or Space on a tab chooses it', async () => {
  const p = shell({ request: async () => ({ nodes: [], workspaces:[],next_from: null }) });
  for (const [name, id] of [['app.lead', 1], ['app.build', 2], ['loose', 3]]) p.upsert({ name, bot_id: id, provider: 'alpha', model: 'one', created_by: id === 2 ? 'app.lead' : null, created_by_id: id === 2 ? 1 : null });
  const doc = p.context.document, key = (k, target = { id: 'input' }) => doc.listeners.keydown({ key: k, target, preventDefault() {} });
  await key('ArrowDown'); assert.equal(p.S.selected, 'app.lead', 'down from Home is the first row');
  await key('ArrowDown'); assert.equal(p.S.selected, 'app.build');
  await p.go(''); await key('ArrowUp'); assert.equal(p.S.selected, 'loose', 'up from Home is the last row');
  await key('ArrowUp'); assert.equal(p.S.selected, 'app.build');
  // A focused tab is a button: Enter or Space opens it, and its close button keeps its own keys.
  p.S.ui.tabs = ['app.lead', 'app.build']; p.S.selected = 'app.build';
  const tab = (who, act = false) => ({ id: '', closest: (sel) => (sel === '[data-tab]' ? { dataset: { tab: who } } : sel === '[data-act]' && act ? {} : null) });
  await key('Enter', tab('app.lead')); assert.equal(p.S.selected, 'app.lead');
  await key(' ', tab('app.build')); assert.equal(p.S.selected, 'app.build');
  await key('Enter', tab('app.lead', true)); assert.equal(p.S.selected, 'app.build', 'Enter on × is the close button\'s');
});

test('a saved tab comes back only for the same bot identity', () => {
  const storage = new Map();
  const p = shell({}, storage);
  for (const [name, id] of [['lead', 1], ['task', 2], ['peek', 3]]) p.upsert({ name, bot_id: id, provider: 'alpha', model: 'one' });
  p.S.ui.tabs = ['lead', 'task']; p.S.selected = 'task'; p.S.ui.side = 'peek'; p.save();
  const q = shell({}, storage);
  q.upsert({ name: 'lead', bot_id: 1, provider: 'alpha', model: 'one' });
  q.upsert({ name: 'task', bot_id: 9, provider: 'alpha', model: 'one' }); // deleted and made again
  q.upsert({ name: 'peek', bot_id: 3, provider: 'alpha', model: 'one' });
  q.restore();
  assert.deepEqual([...q.S.ui.tabs], ['lead'], 'a new bot under an old name does not take its tab');
  assert.equal(q.S.selected, '', 'the dropped tab is not selected');
  assert.equal(q.S.ui.side, 'peek');
  const r = shell({}, storage);
  r.upsert({ name: 'lead', bot_id: 1, provider: 'alpha', model: 'one' }); r.upsert({ name: 'task', bot_id: 2, provider: 'alpha', model: 'one' }); r.upsert({ name: 'peek', bot_id: 8, provider: 'alpha', model: 'one' });
  r.restore();
  assert.deepEqual([...r.S.ui.tabs], ['lead', 'task']); assert.equal(r.S.selected, 'task');
  assert.equal(r.S.ui.side, null, 'nor the agent beside');
});

test('a saved tab whose agent is gone goes up to what made it', async () => {
  const storage = new Map();
  const p = shell({}, storage);
  await p.onEvent({ event: 'created', bot: 'lead', data: { bot_id: 1, provider: 'alpha', model: 'one' } });
  await p.onEvent({ event: 'created', bot: 'helper', data: { bot_id: 2, provider: 'alpha', model: 'one', created_by: 'lead', created_by_id: 1 } });
  p.S.ui.tabs = ['helper']; p.S.selected = 'helper'; p.save();
  const q = shell({}, storage);
  q.upsert({ name: 'lead', bot_id: 1, provider: 'alpha', model: 'one' }); // helper was deleted while the app was closed
  q.restore();
  assert.deepEqual([...q.S.ui.tabs], ['lead']); assert.equal(q.S.selected, 'lead', 'the tab moved up stays selected');
  const r = shell({}, storage);
  r.upsert({ name: 'lead', bot_id: 7, provider: 'alpha', model: 'one' }); r.restore();
  assert.deepEqual([...r.S.ui.tabs], [], 'nor to a new bot under its maker\'s name'); assert.equal(r.S.selected, '');
  // A live deletion's move up is saved at once, so a window lost before the next save keeps it.
  await p.onEvent({ event: 'deleted', bot: 'helper', data: {} });
  const s = shell({}, storage); s.upsert({ name: 'lead', bot_id: 1, provider: 'alpha', model: 'one' }); s.restore();
  assert.equal(s.S.selected, 'lead');
});

test('each composer sends to its own pane, and a working bot gets the sticky queue or steer pick', async () => {
  const sent = [], storage = new Map();
  const p = shell({ request: async (op, q) => { sent.push([op, q]); } }, storage);
  p.upsert({ name: 'lead', bot_id: 1, provider: 'alpha', model: 'one' });
  p.upsert({ name: 'task', bot_id: 2, provider: 'alpha', model: 'one', status: 'running', running_turn: 3 });
  p.S.selected = 'lead'; p.S.ui.side = 'task';
  await p.submit('hello');
  await p.submit('more', 'side');
  p.setSend('steer'); await p.submit('now', 'side');
  assert.deepEqual(sent.map(([, q]) => [q.bot, q.delivery]), [['lead', 'reject'], ['task', 'queue'], ['task', 'steer']]);
  assert.equal(storage.get('agent:send'), 'steer');
  assert.equal(shell({}, storage).S.send, 'steer', 'the last pick sticks for the next window');
  const items = p.sendMenuItems('side');
  assert.deepEqual(Array.from(items.filter((i) => i.act), (i) => [i.act, i.v, !!i.on]), [['set-send', 'queue', false], ['set-send', 'steer', true], ['set-send', 'side', false]]);
});

test('the model chip switches within the provider and offers other providers as new agents', async () => {
  const sent = [];
  const p = shell({ request: async (op, q) => { sent.push(q); } });
  p.upsert({ name: 'lead', bot_id: 1, provider: 'alpha', model: 'one' }); p.S.selected = 'lead';
  const b = p.S.bots.get('lead');
  const choices = p.modelChoices(b, [{ id: 'beta/x' }, { id: 'alpha/two' }, { id: 'alpha/one' }]);
  assert.deepEqual(Array.from(choices, (c) => [c.id, c.ok, c.on]), [['alpha/two', true, false], ['alpha/one', true, true], ['beta/x', false, false]]);
  assert.deepEqual(Array.from(p.modelChoices(b, []), (c) => c.id), ['alpha/one'], 'the bot\'s own model is offered even when unlisted');
  const other = p.modelMenuItems(b, [{ id: 'beta/x' }]).find((i) => i.v === 'beta/x');
  assert.equal(other.disabled, true); assert.equal(other.hint, 'new agent');
  assert.equal(p.setModel('lead', 'beta/x'), false);
  assert.equal(p.setModel('lead', 'alpha/two'), true);
  await p.submit('next turn'); assert.equal(sent.at(-1).model, 'alpha/two');
  p.setModel('lead', 'alpha/one');
  await p.submit('back'); assert.equal('model' in sent.at(-1), false);
});

test('the model chip offers every provider of the bot\'s family and keeps unknown families apart', async () => {
  const sent = [];
  const p = shell({ request: async (op, q) => { sent.push(q); } });
  p.upsert({ name: 'lead', bot_id: 1, provider: 'alpha', family: 'anthropic', model: 'one' });
  p.upsert({ name: 'peer', bot_id: 2, provider: 'gamma', family: 'anthropic', model: 'two' });
  p.upsert({ name: 'other', bot_id: 3, provider: 'beta', family: 'responses', model: 'x' });
  p.S.selected = 'lead';
  const b = p.S.bots.get('lead');
  const choices = p.modelChoices(b, [{ id: 'beta/x' }, { id: 'gamma/two' }, { id: 'delta/y' }, { id: 'alpha/one' }]);
  assert.deepEqual(Array.from(choices, (c) => [c.id, c.ok]), [['gamma/two', true], ['alpha/one', true], ['beta/x', false], ['delta/y', false]]);
  assert.equal(p.setModel('lead', 'beta/x'), false);
  assert.equal(p.setModel('lead', 'gamma/two'), true);
  await p.submit('next turn'); assert.equal(sent.at(-1).model, 'gamma/two');
  // A creation event carries no family; the bot takes its provider's.
  await p.onEvent({ event: 'created', bot: 'late', data: { bot_id: 4, provider: 'alpha', model: 'one' } });
  assert.equal(p.S.bots.get('late').family, 'anthropic');
});

test('New project offers max to a hand-set Claude provider before Settings has been opened', async () => {
  const p = shell({ models: async () => [{ id: 'mine/claude' }], settings: async () => ({ providers: ['mine=anthropic,https://synthetic.invalid/v1'], keys: [] }) }, new Map([['agent:model', 'mine/claude']]));
  await p.openProjectSheet();
  assert.equal(p.S.setup?.settings ?? null, null, 'Settings was never opened');
  assert.match(p.elements.get('sheet').innerHTML, /<option value="max">max effort<\/option>/);
});

test('the New project sheet asks only for a folder, the lead\'s and threads\' models, and where threads work', async () => {
  const calls = [];
  const p = shell({
    models: async () => [{ id: 'anthropic/claude-x' }, { id: 'openai/gpt-6-luna' }],
    settings: async () => ({ providers: ['anthropic', 'openai'], keys: [] }),
    project: async (dir) => ({ dir, name: 'weather', coordinator: 'weather.lead', model: null, file: false }),
    policy: async () => ({ instructions: 'rules', compaction_instructions: 'summary', note: 'test' }),
    writeProject: async (q) => { calls.push(['write', { ...q, threads: { ...q.threads } }]); },
    chooseFolder: async (start) => { calls.push(['choose', start]); return '/synthetic/weather'; },
    request: async (op, q) => { calls.push([op, q]); return op === 'create' ? { name: q.bot, bot_id: 7, provider: 'anthropic', model: 'claude-x', workspace: q.workspace } : { nodes: [], next_from: null }; },
  });
  const el = (id) => p.context.document.getElementById(id);
  await p.act({ dataset: { act: 'new-project' } });
  const html = el('sheet').innerHTML;
  // The threads take the lead's model until one is picked; their effort waits for it.
  assert.match(html, /<option value="" selected>Same as the lead<\/option>/);
  assert.match(html, /<select disabled id="np-teffort"/);
  assert.doesNotMatch(html, /clone|git init|setup command/i, 'basics only');
  assert.match(el('np-in').innerHTML, /class="opt on" data-act="np-in" data-v="worktree"/);
  // Choose… is the system's picker, from the folder typed or the window's own.
  await p.act({ dataset: { act: 'np-choose' } });
  assert.deepEqual(calls.shift(), ['choose', '/synthetic']);
  assert.equal(el('np-dir').value, '/synthetic/weather');
  // A folder is enough to try: one already a project needs no pick, and a new one without a model is
  // refused when it is made.
  assert.equal(el('np-create').disabled, false);
  el('np-dir').value = ''; el('sheet').listeners.input({ target: el('np-dir') });
  assert.equal(el('np-create').disabled, true, 'no folder');
  el('np-dir').value = '/synthetic/weather';
  el('np-model').value = 'anthropic/claude-x'; el('np-effort').value = 'high';
  el('np-tmodel').value = 'openai/gpt-6-luna'; el('np-teffort').value = 'low';
  el('sheet').listeners.input({ target: el('np-model') });
  assert.equal(el('np-create').disabled, false);
  await p.act({ dataset: { act: 'np-in', v: 'project' } });
  assert.match(el('np-in').innerHTML, /class="opt on" data-act="np-in" data-v="project"/);
  await el('sheet').listeners.submit({ preventDefault() {} });
  const create = calls.find(([op]) => op === 'create')[1];
  assert.deepEqual([create.bot, create.workspace, create.model, create.effort], ['weather.lead', '/synthetic/weather', 'anthropic/claude-x', 'high']);
  // The coordinator is told the threads' picks after its role, whichever role that is.
  assert.equal(create.instructions, 'rules\n\nThis project\'s tasks, as the person set them up: start every new task, in a role (--profile) or not, with --model \'openai/gpt-6-luna\' --effort \'low\'. Every task works in this folder, with no worktree of its own.');
  assert.deepEqual(calls.find(([op]) => op === 'write')[1], { dir: '/synthetic/weather', name: 'weather', model: 'anthropic/claude-x', reasoning: 'high', threads: { model: 'openai/gpt-6-luna', reasoning: 'low', inProject: true } });
  assert.equal(p.S.ui.sheet, false, 'the sheet closes once the project is made');
  assert.equal(p.S.selected, 'weather.lead');
});

test('the model chip changes the effort of an agent\'s next turns and never of a steer', async () => {
  const sent = []; const storage = new Map();
  const p = shell({ request: async (op, q) => { sent.push(q); } }, storage);
  p.upsert({ name: 'lead', bot_id: 1, provider: 'openai', family: 'responses', model: 'gpt-6-luna', effort: 'high', status: 'idle', running_turn: null });
  p.upsert({ name: 'plain', bot_id: 2, provider: 'openai', family: 'responses', model: 'gpt-6-luna', status: 'idle', running_turn: null });
  p.S.selected = 'lead';
  const lead = p.S.bots.get('lead');
  const levels = (b) => Array.from(p.modelMenuItems(b, []).filter((i) => i.act === 'set-effort'), (i) => [i.label, !!i.on]);
  // An agent made with a level always sends one; Claude's max is not offered to OpenAI's.
  assert.deepEqual(levels(lead), [['low', false], ['medium', false], ['high', true], ['xhigh', false]]);
  assert.equal(p.setEffort('lead', 'max'), false);
  assert.equal(p.setEffort('lead', 'xhigh'), true);
  assert.deepEqual(levels(lead).find(([, on]) => on), ['xhigh', true]);
  await p.submit('think harder'); assert.equal(sent.at(-1).effort, 'xhigh');
  // A steer joins the running turn at that turn's level, so it names none.
  p.upsert({ name: 'lead', bot_id: 1, provider: 'openai', model: 'gpt-6-luna', status: 'running', running_turn: 5 });
  p.setSend('queue'); await p.submit('next'); assert.equal(sent.at(-1).effort, 'xhigh', 'queued work runs at the pick');
  p.setSend('steer'); await p.submit('also'); assert.equal(sent.at(-1).delivery, 'steer'); assert.equal('effort' in sent.at(-1), false);
  p.upsert({ name: 'lead', bot_id: 1, provider: 'openai', model: 'gpt-6-luna', status: 'idle', running_turn: null });
  // Its own level again sends none: the agent's own applies.
  p.setEffort('lead', 'high');
  await p.submit('back'); assert.equal('effort' in sent.at(-1), false);
  // An agent made without a level can go back to the model's own.
  const plain = p.S.bots.get('plain');
  assert.deepEqual(levels(plain)[0], ['default', true]);
  p.setEffort('plain', 'low'); p.S.selected = 'plain';
  await p.submit('quick'); assert.equal(sent.at(-1).effort, 'low');
  // The pick comes back for the same identity only.
  p.setEffort('lead', 'medium'); p.save();
  const q = shell({}, storage);
  q.upsert({ name: 'lead', bot_id: 1, provider: 'openai', family: 'responses', model: 'gpt-6-luna', effort: 'high' });
  q.upsert({ name: 'plain', bot_id: 9, provider: 'openai', family: 'responses', model: 'gpt-6-luna' });
  q.restore();
  assert.equal(q.S.effort.get('lead'), 'medium');
  assert.equal(q.S.effort.has('plain'), false);
  p.setEffort('plain', ''); assert.equal(p.S.effort.has('plain'), false);
});

test('a saved model pick comes back only for the same bot identity', () => {
  const storage = new Map();
  const p = shell({}, storage);
  p.upsert({ name: 'lead', bot_id: 1, provider: 'alpha', model: 'one' });
  p.upsert({ name: 'task', bot_id: 2, provider: 'alpha', model: 'one' });
  p.setModel('lead', 'alpha/two'); p.setModel('task', 'alpha/two'); p.save();
  const q = shell({}, storage);
  q.upsert({ name: 'lead', bot_id: 1, provider: 'alpha', model: 'one' });
  q.upsert({ name: 'task', bot_id: 9, provider: 'alpha', model: 'one' }); // deleted and made again
  q.restore();
  assert.equal(q.S.override.get('lead'), 'alpha/two');
  assert.equal(q.S.override.has('task'), false, 'a new bot under an old name starts on its own model');
  storage.set([...storage.keys()].find((k) => k !== 'agent:send'), JSON.stringify({ override: [['lead', 'alpha/two']] }));
  const r = shell({}, storage); r.upsert({ name: 'lead', bot_id: 1, provider: 'alpha', model: 'one' }); r.restore();
  assert.equal(r.S.override.has('lead'), false, 'a pick with no identity is not restored');
});

test('a steer joins the running turn: it names no model and no workspace', async () => {
  const sent = [];
  const p = shell({ request: async (op, q) => { sent.push(q); } });
  p.upsert({ name: 'task', bot_id: 2, provider: 'alpha', model: 'one', workspace: '/synthetic/task', status: 'running', running_turn: 3 });
  p.S.selected = 'task'; p.setModel('task', 'alpha/two');
  p.setSend('queue'); await p.submit('later');
  // A bot keeps its folder, so a message names none; a turn run elsewhere moves the head's folder.
  assert.equal('workspace' in sent.at(-1), false); assert.equal(sent.at(-1).model, 'alpha/two');
  p.setSend('steer'); await p.submit('now');
  assert.equal(sent.at(-1).delivery, 'steer'); assert.equal('workspace' in sent.at(-1), false); assert.equal('model' in sent.at(-1), false);
  assert.equal(sent.at(-1).expected_turn, 3, 'a steer is for the turn on screen');
  // A turn still waiting for a slot has not started, so it takes a queue, not a steer.
  p.upsert({ name: 'task', bot_id: 2, provider: 'alpha', model: 'one', workspace: '/synthetic/task', status: 'ready', running_turn: null });
  await p.onEvent({ event: 'queued', bot: 'task', turn: 4, data: { status: 'ready' } });
  await p.submit('soon');
  assert.equal(sent.at(-1).delivery, 'queue'); assert.equal('expected_turn' in sent.at(-1), false);
});

test('a bot keeps its folder: only a bot without one is sent the app\'s', async () => {
  const sent = [];
  const p = shell({ request: async (op, q) => { sent.push(q); } });
  p.upsert({ name: 'loose', bot_id: 3, provider: 'alpha', model: 'one' });
  p.S.selected = 'loose'; await p.submit('here');
  assert.equal(sent.at(-1).workspace, '/synthetic');
  // A turn's folder is not the bot's: a steer run elsewhere leaves the bot where it was.
  p.upsert({ name: 'task', bot_id: 2, provider: 'alpha', model: 'one', workspace: '/synthetic/task' });
  await p.onEvent({ event: 'accepted', bot: 'task', turn: 5, data: { workspace: '/synthetic/steer' } });
  assert.equal(p.S.bots.get('task').workspace, '/synthetic/task');
});

test('a steer whose turn ended meanwhile is refused as stale, with a short message, and never queued', async () => {
  const sent = [];
  const p = shell({ request: async (op, q) => { sent.push(q); if (q.delivery === 'steer' && q.expected_turn !== 4) throw new Error('stale_turn: turn 3 is not running'); } });
  p.upsert({ name: 'task', bot_id: 2, provider: 'alpha', model: 'one', status: 'running', running_turn: 3 });
  p.S.selected = 'task'; p.setSend('steer');
  await assert.rejects(p.submit('now'), /^Error: that turn ended; not steered$/);
  assert.equal(sent.length, 1);
  p.setSend('queue'); await assert.doesNotReject(p.submit('later'));
});

test('one menu per agent: side chat any time, stop while running, fork and delete at rest', () => {
  const p = shell();
  p.upsert({ name: 'busy', bot_id: 1, provider: 'alpha', model: 'one', status: 'running', running_turn: 4 });
  p.upsert({ name: 'rest', bot_id: 2, provider: 'alpha', model: 'one' });
  const state = (name) => Object.fromEntries(p.botMenuItems(name).filter((i) => i.act).map((i) => [i.act, !i.disabled]));
  assert.deepEqual(state('busy'), { 'open-tab': true, 'side-chat': true, stop: true, fork: false, delete: false, steps: true });
  assert.deepEqual(state('rest'), { 'open-tab': true, 'side-chat': true, stop: false, fork: true, delete: true, steps: true });
  p.S.ui.tabs.push('rest'); assert.equal(state('rest')['open-tab'], false, 'an agent in a tab already has one');
});

test('a side chat forks a running bot under it, beside, with its tools and folder, and takes the first message', async () => {
  const sent = [], storage = new Map(); let sideAtSubmit;
  const p = shell({ request: async (op, q) => { sent.push([op, q]); if (op === 'submit') sideAtSubmit = p.S.ui.side; return op === 'fork' ? { name: q.bot, bot_id: 10 + sent.length, provider: 'alpha', model: 'one', workspace: q.workspace, created_by: q.created_by, created_by_id: q.created_by_id, allowed: q.allow } : { nodes: [], workspaces:[],next_from: null }; } }, storage);
  p.upsert({ name: 'app.lead', bot_id: 1, provider: 'alpha', model: 'one', workspace: '/synthetic', status: 'running', running_turn: 3, tools: ['shell', 'read', 'write', 'history'] });
  p.S.selected = 'app.lead';
  await p.sideChat('app.lead');
  const forks = () => sent.filter(([op]) => op === 'fork').map(([, q]) => q);
  // No allowed list and no folder: the copy keeps its source's tools and works where it does.
  assert.deepEqual(forks().map((q) => [q.source, q.bot, q.created_by, q.created_by_id, 'allow' in q, 'workspace' in q, 'checkpoint' in q]), [['app.lead', 'app.lead-side', 'app.lead', 1, false, false, false]]);
  assert.equal(p.S.ui.side, 'app.lead-side'); assert.equal(p.S.bots.get('app.lead-side').parent, 'app.lead');
  // Send's side pick asks a new side chat with this message; the running source gets nothing.
  p.setSend('side');
  await p.submit('what are you waiting on?', 'main');
  assert.equal('allow' in forks().at(-1), false); assert.equal(forks().at(-1).bot, 'app.lead-side-2');
  const submits = sent.filter(([op]) => op === 'submit').map(([, q]) => [q.bot, q.bot_id, q.prompt, q.delivery]);
  assert.deepEqual(submits, [['app.lead-side-2', 12, 'what are you waiting on?', 'reject']]);
  assert.equal(sideAtSubmit, 'app.lead-side', 'the first message goes before the new side chat opens and loads its history');
  assert.equal(p.S.ui.side, 'app.lead-side-2');
  // A bot at rest sends normally; the side pick is only for a working one.
  p.S.bots.get('app.lead').status = 'idle'; p.S.bots.get('app.lead').runningTurn = null;
  await p.submit('plain', 'main');
  assert.deepEqual(sent.at(-1)[1].bot, 'app.lead');
});

test('an open agent menu is rebuilt when its bot changes status and closed when it is deleted', async () => {
  const p = shell();
  p.upsert({ name: 'task', bot_id: 2, provider: 'alpha', model: 'one', status: 'running', running_turn: 4 });
  p.upsert({ name: 'other', bot_id: 3, provider: 'alpha', model: 'one' });
  p.showMenu(p.botMenuItems('task'), { x: 10, y: 10 }, 'task');
  const menu = p.elements.get('menu');
  assert.match(menu.innerHTML, /when idle/); assert.match(menu.innerHTML, /<button type="button" role="menuitem" data-act="stop"/);
  await p.onEvent({ event: 'turn_finished', bot: 'task', turn: 4, data: { status: 'completed' } }); p.refreshMenu();
  assert.equal(p.S.ui.menu, true);
  assert.doesNotMatch(menu.innerHTML, /when idle/); assert.match(menu.innerHTML, /disabled data-act="stop"/);
  const before = menu.innerHTML; p.S.bots.get('other').status = 'running'; p.refreshMenu();
  assert.equal(menu.innerHTML, before, 'another bot changing leaves the menu alone');
  await p.onEvent({ event: 'deleted', bot: 'task' }); p.refreshMenu();
  assert.equal(p.S.ui.menu, false);
});

test('fork copies a bot at rest next to it and opens the copy beside', async () => {
  const sent = [];
  const p = shell({ request: async (op, q) => { sent.push([op, q]); return op === 'fork' ? { name: q.bot, bot_id: 10 + sent.length, provider: 'alpha', model: 'one', created_by: q.created_by, created_by_id: q.created_by_id } : { nodes: [], workspaces:[],next_from: null }; } });
  p.upsert({ name: 'app.lead', bot_id: 1, provider: 'alpha', model: 'one' });
  p.upsert({ name: 'app.task', bot_id: 2, provider: 'alpha', model: 'one', created_by: 'app.lead', created_by_id: 1 });
  p.upsert({ name: 'app.busy', bot_id: 3, provider: 'alpha', model: 'one', status: 'running', running_turn: 1 });
  p.S.selected = 'app.lead';
  await p.fork('app.task'); await p.fork('app.task');
  const forks = sent.filter(([op]) => op === 'fork').map(([, q]) => q);
  assert.deepEqual(forks.map((q) => [q.source, q.bot, q.created_by, q.created_by_id]), [['app.task', 'app.task-fork', 'app.lead', 1], ['app.task', 'app.task-fork-2', 'app.lead', 1]]);
  assert.equal(p.S.ui.side, 'app.task-fork-2');
  await assert.rejects(p.fork('app.busy'), /bot_busy/);
  assert.equal(sent.filter(([op]) => op === 'fork').length, 2);
  // A task known only by its prefix forks under the coordinator, beside itself.
  p.upsert({ name: 'app.solo', bot_id: 4, provider: 'alpha', model: 'one' }); p.tree();
  assert.equal(p.S.bots.get('app.solo').project, 'app');
  await p.fork('app.solo');
  const solo = sent.filter(([op]) => op === 'fork').at(-1)[1];
  assert.deepEqual([solo.created_by, solo.created_by_id], ['app.lead', 1]);
  // A root bot outside any project forks to a root beside it, not under itself.
  p.upsert({ name: 'loose', bot_id: 5, provider: 'alpha', model: 'one' }); p.tree();
  await p.fork('loose');
  const loose = sent.filter(([op]) => op === 'fork').at(-1)[1];
  assert.equal('created_by' in loose, false);
});

test('a failed send comes back only to the bot it was for', async () => {
  let fail = null;
  const p = shell({ request: async (op) => { if (op === 'submit') { await new Promise((r) => { fail = r; }); throw new Error('daemon_unavailable'); } return { nodes: [], workspaces:[],next_from: null }; } });
  for (const [name, id] of [['app.lead', 1], ['app.build', 2], ['app.test', 3]]) p.upsert({ name, bot_id: id, provider: 'alpha', model: 'one', created_by: id > 1 ? 'app.lead' : null, created_by_id: id > 1 ? 1 : null });
  p.tree(); p.S.selected = 'app.lead';
  const doc = p.context.document, side = doc.getElementById('sideinput');
  await p.go('app.build', 'beside');
  side.value = 'for build';
  const sending = doc.getElementById('sideform').listeners.submit({ preventDefault() {} });
  await new Promise((r) => setImmediate(r));
  await p.go('app.test', 'beside');
  fail(); await sending;
  assert.equal(p.S.ui.side, 'app.test'); assert.equal(side.value, '', 'not restored under another bot');
});

test('fork names fit the daemon\'s 128-byte limit and forks work in the source\'s folder', async () => {
  const sent = [];
  const p = shell({ request: async (op, q) => { sent.push([op, q]); return op === 'fork' ? { name: q.bot, bot_id: 10 + sent.length, provider: 'alpha', model: 'one' } : { nodes: [], workspaces:[],next_from: null }; } });
  const long = 'a'.repeat(128);
  p.upsert({ name: long, bot_id: 1, provider: 'alpha', model: 'one', workspace: '/synthetic/elsewhere' });
  p.S.selected = long;
  await p.fork(long); await p.fork(long);
  const forks = sent.filter(([op]) => op === 'fork').map(([, q]) => q);
  // The daemon starts a fork where its source is, so the app names no folder.
  assert.deepEqual(forks.map((q) => [q.bot.length, q.bot.slice(-7), 'workspace' in q]), [[128, 'aa-fork', false], [128, '-fork-2', false]]);
});

test("Home's first message starts its agent in your home folder in the home role, and then goes to it", async () => {
  const calls = [];
  const p = shell({
    models: async () => [{ id: 'alpha/one' }],
    settings: async () => ({ providers: ['alpha'], keys: [] }),
    homeDir: async () => '/synthetic/you',
    policy: async (dir, profile) => { calls.push(['policy', dir, profile]); return { instructions: 'home rules', compaction_instructions: 'summary', note: 'test' }; },
    request: async (op, q) => { calls.push([op, q]); return op === 'create' ? { name: q.bot, bot_id: 9, provider: 'alpha', model: 'one', workspace: q.workspace } : { nodes: [], workspaces: [], next_from: null }; },
  });
  const el = (id) => p.context.document.getElementById(id);
  assert.equal(p.mainBot(), '');
  await p.submit('what is running?');
  assert.equal(calls.length, 0, 'nothing is made before a model is picked');
  assert.match(el('sheet').innerHTML, /<h4>Start Home<\/h4>/);
  assert.match(el('sheet').innerHTML, /id="hm-model"/);
  // A second message before Home is picked does not replace the first.
  await assert.rejects(p.submit('and another'), /home_starting/);
  el('hm-model').value = 'alpha/one'; el('hm-effort').value = 'high';
  await el('sheet').listeners.submit({ preventDefault() {} }); await settle();
  assert.deepEqual(calls.find(([op]) => op === 'policy').slice(1), ['/synthetic/you', 'home']);
  const create = calls.find(([op]) => op === 'create')[1];
  assert.deepEqual([create.bot, create.workspace, create.model, create.effort, create.instructions], ['home', '/synthetic/you', 'alpha/one', 'high', 'home rules']);
  const sent = () => calls.filter(([op]) => op === 'submit').map(([, q]) => [q.bot, q.bot_id, q.prompt]);
  assert.deepEqual(sent(), [['home', 9, 'what is running?']]);
  // At Home its chat is the main pane, so the next message goes straight to it.
  assert.equal(p.S.selected, ''); assert.equal(p.mainBot(), 'home');
  await p.submit('and what waits on me?');
  assert.deepEqual(sent().at(-1), ['home', 9, 'and what waits on me?']);
  assert.equal(calls.filter(([op]) => op === 'create').length, 1);
  // A window on another host has no Home to start: it says so and asks for nothing.
  const far = shell({ models: async () => { calls.push(['models']); return []; } });
  far.S.config.host = 'box'; calls.length = 0;
  await assert.rejects(far.submit('what is running?'), /home_local_only: .*box/);
  assert.deepEqual(calls, []);
  assert.doesNotMatch(far.context.document.getElementById('sheet').innerHTML, /Start Home/);
});

test("Home's agent is Home: no row, no tab, no crumb, and a closed Start Home gives the message back", async () => {
  const p = shell({ models: async () => [], settings: async () => ({ providers: [], keys: [] }) });
  p.upsert({ name: 'home', bot_id: 1, provider: 'alpha', model: 'one' });
  p.upsert({ name: 'app.lead', bot_id: 2, provider: 'alpha', model: 'one' });
  p.upsert({ name: 'home-side', bot_id: 3, provider: 'alpha', model: 'one', created_by: 'home', created_by_id: 1 });
  p.S.shapeGen += 1;
  assert.deepEqual(names(p.railRows()), ['app.lead']);
  await p.go('home');
  assert.equal(p.S.selected, ''); assert.deepEqual(Array.from(p.S.ui.tabs), []);
  assert.equal(p.upOf('home-side'), '');
  const head = p.context.document.getElementById('title');
  p.renderHead(head, p.S.bots.get('home'), 'main');
  assert.match(head.innerHTML, /^<div class="crumbs"><b>Home<\/b><span class="glyph/);
  // Away from Home, its button says what Home's agent is doing, and that it finished.
  const homebtn = () => p.context.document.getElementById('tabs').innerHTML.match(/<button[^>]*class="homebtn[^"]*"[^>]*>.*?<\/button>/)[0];
  p.S.bots.get('home').status = 'running'; p.renderTabs();
  assert.doesNotMatch(homebtn(), /glyph/, 'at Home its chat is on screen');
  p.S.selected = 'app.lead'; p.renderTabs();
  assert.match(homebtn(), /<span class="glyph running">/);
  p.S.bots.get('home').status = 'idle'; p.S.unseen.add('home'); p.renderTabs();
  assert.match(homebtn(), /<span class="glyph done">✔<\/span> Home/);
  p.S.unseen.delete('home'); p.renderTabs();
  assert.match(homebtn(), />⌂ Home</);
  p.S.selected = '';
  // Before Home's agent exists, Cancel puts the message back in the composer.
  const q = shell({ models: async () => [], settings: async () => ({ providers: [], keys: [] }) });
  await q.submit('hello');
  assert.match(q.elements.get('sheet').innerHTML, /No models listed/);
  q.closeSheet();
  assert.equal(q.context.document.getElementById('input').value, 'hello');
  // Cancelled while Home is being made: Home exists, and nothing is sent.
  const made = deferred(), sent = [];
  const r = shell({ models: async () => [{ id: 'alpha/one' }], settings: async () => ({ providers: ['alpha'], keys: [] }), homeDir: async () => '/synthetic/you', policy: async () => ({ instructions: 'home rules', compaction_instructions: 'summary', note: 'test' }),
    request: async (op, x) => { sent.push(op); if (op === 'create') { await made.promise; return { name: x.bot, bot_id: 9, provider: 'alpha', model: 'one', workspace: x.workspace }; } return { nodes: [], workspaces: [], next_from: null }; } });
  r.setRender(() => r.followDrafts());
  await r.submit('status?');
  r.context.document.getElementById('hm-model').value = 'alpha/one';
  const starting = r.context.document.getElementById('sheet').listeners.submit({ preventDefault() {} });
  await settle(); r.closeSheet(); made.resolve(); await starting; await settle();
  assert.equal(r.S.bots.has('home'), true);
  assert.deepEqual(sent, ['create']);
  assert.equal(r.context.document.getElementById('input').value, 'status?');
});

test('a new project creates its coordinator in the folder, in its role, writes its file once, and is not made twice', async () => {
  const calls = []; let written = false;
  const p = shell({
    project: async (dir) => ({ dir, name: 'weather', coordinator: 'weather.lead', model: null, file: written }),
    policy: async (dir, profile) => { calls.push(['policy', dir, profile]); return { instructions: 'rules', compaction_instructions: 'summary', model: 'alpha/role', tools: ['shell', 'wait'], note: 'test' }; },
    writeProject: async (q) => { calls.push(['write', q]); written = true; },
    request: async (op, q) => { calls.push([op, q]); return op === 'create' ? { name: q.bot, bot_id: 7, provider: 'alpha', model: 'role', workspace: q.workspace } : { nodes: [], workspaces:[],next_from: null }; },
  });
  await p.createProject('/synthetic/weather');
  const create = calls.find(([op]) => op === 'create')[1];
  // The coordinator profile composes the text, then the project's task settings follow; its model and
  // tools apply when the project names none.
  assert.deepEqual(calls.find(([op]) => op === 'policy').slice(1), ['/synthetic/weather', 'coordinator']);
  assert.deepEqual([create.bot, create.workspace, create.model, create.instructions.split('\n\n')[0], Array.from(create.tools)], ['weather.lead', '/synthetic/weather', 'alpha/role', 'rules', ['shell', 'wait']]);
  assert.deepEqual({ ...calls.find(([op]) => op === 'write')[1] }, { dir: '/synthetic/weather', name: 'weather', model: 'alpha/role', reasoning: null, threads: null });
  assert.equal('effort' in create, false, 'no effort picked sends none: the model uses its own');
  assert.equal(p.S.selected, 'weather.lead');
  const before = calls.length;
  await p.createProject('/synthetic/weather');
  assert.equal(calls.filter(([op]) => op === 'create').length, 1); assert.equal(calls.length, before);
});

test('an agent\'s effort is picked beside its model, kept in the project file, and shown with its model', async () => {
  const sent = []; const storage = new Map(); let file = null;
  const p = shell({
    project: async (dir) => ({ dir, name: 'weather', coordinator: 'weather.lead', model: file?.model ?? null, reasoning: file?.reasoning ?? null, threads_model: file?.threads_model ?? null, threads_reasoning: null, threads_in: file?.threads_in ?? 'worktree', file: !!file }),
    policy: async () => ({ instructions: 'rules', compaction_instructions: 'summary', note: 'test' }),
    writeProject: async (q) => { sent.push(['write', { ...q }]); },
    request: async (op, q) => { sent.push([op, { ...q }]); return op === 'create' ? { name: q.bot, bot_id: sent.length, provider: q.model.split('/')[0], model: q.model.split('/')[1], effort: q.effort ?? null, workspace: q.workspace ?? '/synthetic' } : { nodes: [], workspaces:[],next_from: null }; },
  }, storage);
  await p.createProject('/synthetic/weather', 'anthropic/claude-x', 'max');
  const creates = () => sent.filter(([op]) => op === 'create').map(([, q]) => q);
  assert.equal(creates()[0].effort, 'max');
  // With no threads' model, tasks are started on the lead's own, named so a role's model cannot replace it.
  assert.match(creates()[0].instructions, /with --model "\$AGENT_MODEL" \$\{AGENT_EFFORT:\+--effort "\$AGENT_EFFORT"\}\. When this folder is a git repository, a task that changes files works in its own worktree/);
  assert.deepEqual(sent.find(([op]) => op === 'write')[1], { dir: '/synthetic/weather', name: 'weather', model: 'anthropic/claude-x', reasoning: 'max', threads: null });
  assert.equal(p.S.bots.get('weather.lead').reasoning, 'max');
  assert.equal(storage.get('agent:effort'), 'max', 'the last pick is offered next time, as the model is');
  // A folder whose file names a model keeps that model's effort, whatever was picked.
  p.S.bots.clear(); file = { model: 'alpha/one', reasoning: 'low', threads_model: 'beta/two', threads_in: 'project' };
  await p.createProject('/synthetic/weather', 'anthropic/claude-x', 'high', { model: 'gamma/three', reasoning: null, inProject: false });
  assert.deepEqual([creates()[1].model, creates()[1].effort], ['alpha/one', 'low']);
  assert.match(creates()[1].instructions, /with --model 'beta\/two'\. Every task works in this folder/, 'and its threads\' picks');
  // /new takes an effort after the model.
  await p.submit('/new Bob anthropic/claude-x max');
  await p.submit('/new Ann openai/gpt-6-luna xhigh');
  await p.submit('/new Cy openai/gpt-6-luna');
  assert.deepEqual(creates().slice(2).map((q) => [q.bot, q.effort]), [['Bob', 'max'], ['Ann', 'xhigh'], ['Cy', undefined]]);
  // A creation event from another client carries the level, as the list does.
  await p.onEvent({ event: 'created', bot: 'Eve', cursor: 900, data: { bot_id: 90, provider: 'openai', model: 'gpt-6-luna', effort: 'high', status: 'idle', running_turn: null } });
  assert.equal(p.S.bots.get('Eve').reasoning, 'high');
  // An event that does not name the level leaves the one known.
  p.upsert({ name: 'Eve', bot_id: 90, provider: 'openai', model: 'gpt-6-luna', status: 'running' });
  assert.equal(p.S.bots.get('Eve').reasoning, 'high');
});

test('the coordinator the app ships gives editing tasks worktrees and cleans up failed starts', () => {
  const text = fs.readFileSync(require.resolve('../agents/coordinator.md'), 'utf8');
  assert.match(text, /^---\nname: coordinator\n/);
  assert.match(text, /git worktree add -b agent\/NAME/); assert.match(text, /starts with your own name before \.lead/); assert.match(text, /--workspace "\$HOME\/\.agent\/worktrees\/NAME\/\$\(git rev-parse --show-prefix\)"/);
  assert.match(text, /A task keeps its folder, so later messages to it need no --workspace/);
  assert.match(text, /run --detach --new --agents --bot NAME/); assert.match(text, /pass --profile ROLE in place of --agents/);
  assert.match(text, /If \.agents\/setup exists here/);
  assert.match(text, /the start fails and "\$AGENT_BIN" ls does not list NAME, remove the worktree/);
  assert.match(text, /git worktree remove --force, git branch -D/);
  assert.match(text, /when this folder is not a git repository, works in this folder/);
  // The threads' picks are told to the coordinator by the app, not by this role, which a folder may replace.
  assert.doesNotMatch(text, /threads_|project\.toml/);
  assert.match(text, /Unless this project's tasks all work in this folder/);
});

test('a project name taken by another folder\'s coordinator is refused, and a refused model is never written', async () => {
  const calls = []; let fail = true, failWrite = false, threads;
  const p = shell({
    project: async (dir) => ({ dir, name: dir.endsWith('taken') ? 'demo' : 'weather', coordinator: dir.endsWith('taken') ? 'demo.lead' : 'weather.lead', model: null, file: false }),
    policy: async () => ({ instructions: 'rules', compaction_instructions: 'summary', note: 'test' }),
    writeProject: async (q) => { calls.push(['write', q.dir, q.model]); threads = q.threads; if (failWrite) throw new Error('project_unwritable'); },
    request: async (op, q) => { calls.push([op, q.bot]); if (op === 'create' && fail) throw new Error('create_failed'); return op === 'create' ? { name: q.bot, bot_id: 7, provider: 'alpha', model: 'one', workspace: q.workspace } : { nodes: [], workspaces:[],next_from: null }; },
  });
  p.upsert({ name: 'demo.lead', bot_id: 1, provider: 'alpha', model: 'one', workspace: '/synthetic/first' });
  p.S.selected = '';
  await assert.rejects(p.createProject('/synthetic/taken'), /demo\.lead already belongs to \/synthetic\/first/);
  assert.equal(p.S.selected, ''); assert.equal(calls.length, 0);
  await assert.rejects(p.createProject('/synthetic/weather'), /model_required/);
  assert.equal(calls.length, 0, 'there is no default model to fall back on');
  await assert.rejects(p.createProject('/synthetic/weather', 'alpha/one'), /create_failed/);
  assert.deepEqual(calls.map(([op]) => op), ['create'], 'a model the daemon refuses is not saved to the folder');
  fail = false; failWrite = true;
  await assert.rejects(p.createProject('/synthetic/weather', 'alpha/one'), /project_unwritable/);
  assert.deepEqual(calls.map(([op]) => op), ['create', 'create', 'write'], 'the file follows an accepted coordinator');
  // The coordinator made before the failed write keeps what it was told: a retry opens it, writes no
  // file that would claim other picks, and says the picks were not applied.
  failWrite = false; const n = calls.length; threads = 'unwritten';
  await p.createProject('/synthetic/weather', 'alpha/one', null, { model: 'beta/two', reasoning: 'low', inProject: true });
  assert.equal(calls.length, n, 'no create and no write'); assert.equal(threads, 'unwritten');
  assert.match(p.S.ui.toast ?? '', /weather\.lead already exists and keeps the settings it was made with/);
  assert.equal(p.S.selected, 'weather.lead');
  // An effort picked on its own is a pick too.
  p.S.ui.toast = null;
  await p.createProject('/synthetic/weather', null, 'high');
  assert.match(p.S.ui.toast ?? '', /keeps the settings it was made with/);
  // So is the sheet's worktree choice left at its default.
  p.S.ui.toast = null;
  await p.createProject('/synthetic/weather', null, null, { model: null, reasoning: null, inProject: false });
  assert.match(p.S.ui.toast ?? '', /keeps the settings it was made with/);
});

test('runs fold thinking and tool calls to one line each, keep failures visible, and expand on demand', () => {
  const p = shell(); const t = p.transcript('Bob');
  const tool = (name, callId, summary, turn) => ({ kind: 'tool', name, callId, summary, done: true, started: 0, took: 0, turn });
  t.items = [
    { kind: 'user', text: 'go', turn: 1 },
    { kind: 'thought', text: 'private plan', secs: 2, turn: 1 },
    tool('shell', 'a', 'ls', 1), { kind: 'out', callId: 'a', text: 'one\ntwo\nthree\nfour', turn: 1 },
    { kind: 'backing', turn: 1 },
    tool('read', 'b', 'notes.md', 1), { kind: 'out', callId: 'b', text: 'denied', err: 'denied', turn: 1 },
    { kind: 'text', text: 'done', turn: 1 },
    { kind: 'thought', text: 'again', secs: 1, turn: 2 },
    tool('shell', 'c', 'make', 2),
  ];
  let html = p.itemsHTML(t);
  assert.equal((html.match(/class="steps"/g) || []).length, 2);
  assert.match(html, /3 steps <span class="now">shell · read<\/span> <span class="err">✘ denied<\/span>/);
  assert.match(html, /2 steps/); assert.doesNotMatch(html, /private plan/);
  assert.equal(p.runStart(t, 6), 1); assert.equal(p.runStart(t, 9), 8);
  t.items[3].runOpen = true; html = p.itemsHTML(t);
  assert.match(html, /private plan/); assert.match(html, /\+2 lines/); assert.doesNotMatch(html, /again/);
  p.S.ui.steps = true; html = p.itemsHTML(t);
  assert.match(html, /four/); assert.match(html, /again/);
  const thoughtOnly = p.transcript('Ann'); thoughtOnly.items = [{ kind: 'thought', text: 'hm', secs: 7, turn: 1 }]; p.S.ui.steps = false;
  assert.match(p.itemsHTML(thoughtOnly), /▸ thought 7s/);
  // A page that starts at an output whose call is not loaded shows the output, not a thought.
  const outputOnly = p.transcript('Cy'); outputOnly.items = [{ kind: 'out', callId: 'z', text: 'result', turn: 1 }, { kind: 'text', text: 'ok', turn: 1 }];
  const out = p.itemsHTML(outputOnly);
  assert.doesNotMatch(out, /thought|class="sum"/); assert.match(out, /class="line out">result</);
});

test('the demo daemon delivers a steer at the next round boundary and refuses a stale one', async () => {
  const context = vm.createContext({ window: {}, setTimeout, clearTimeout, Math, JSON, Promise, Error, String, Set, Map, Infinity });
  vm.runInContext(fs.readFileSync(require.resolve('../ui/daemon.js'), 'utf8'), context);
  const d = context.window.Daemon;
  await d.request('create', { bot: 'solo', model: 'alpha/one' });
  const { turn } = await d.request('submit', { bot: 'solo', prompt: 'read the dispatch', delivery: 'reject' });
  await new Promise((r) => setTimeout(r, 300));
  await assert.rejects(d.request('submit', { bot: 'solo', prompt: 'x', delivery: 'steer', expected_turn: turn + 1 }), /stale_turn/);
  await d.request('submit', { bot: 'solo', prompt: 'mention the wait op too', delivery: 'steer', expected_turn: turn });
  const events = [];
  while (!events.some(ended)) events.push(...(await d.pull()).events);
  const nodes = events.filter((e) => e.event === 'message' || e.event === 'steered').map((e) => e.data.node);
  const { items: read } = await d.request('history_items', { bot: 'solo', nodes });
  const items = read.map((entry) => entry.item);
  const user = items.findIndex((i) => i.role === 'user');
  assert.equal(items[user].content[0].text, 'mention the wait op too');
  assert.match(items[user + 1].content[0].text, /^Noted: mention the wait op too\./);
  assert.equal(events.filter(ended).length, 1, 'the steer joined the running turn');
  const steer = events.find((e) => e.event === 'queued');
  assert.equal(steer.data.delivery, 'steer');
  assert.deepEqual(events.filter((e) => e.event === 'turn_finished' && e.turn === steer.turn).map((e) => [e.data.status, e.data.into]), [['steered', turn]]);
  d.close();
});

test('Escape in the finder never stops a turn, and a deleted bot takes its draft with it', async () => {
  const sent = [];
  const p = drafting({ request: async (op, q) => { sent.push(op); return { nodes: [], workspaces:[],next_from: null }; } });
  p.upsert({ name: 'app.lead', bot_id: 1, provider: 'alpha', model: 'one', status: 'running', running_turn: 1 });
  p.upsert({ name: 'app.task', bot_id: 2, provider: 'alpha', model: 'one', created_by: 'app.lead', created_by_id: 1 });
  p.tree(); p.S.selected = 'app.lead'; p.followDrafts();
  const doc = p.context.document;
  doc.getElementById('pickerq').value = '';
  await doc.listeners.keydown({ key: 'Escape', target: { id: 'pickerq' }, preventDefault() {} });
  assert.deepEqual(sent.filter((op) => op === 'interrupt'), []);
  doc.getElementById('input').value = 'for lead';
  await p.go('app.task'); doc.getElementById('input').value = 'for task';
  await p.onEvent({ event: 'deleted', bot: 'app.task' }); p.followDrafts();
  assert.equal(p.S.selected, 'app.lead'); assert.equal(doc.getElementById('input').value, 'for lead');
  assert.equal(p.S.drafts.has('app.task'), false, 'a deleted bot takes its draft with it');
});

test('the demo daemon ends a stopped turn quietly when its bot is deleted before the script wakes', async () => {
  const context = vm.createContext({ window: {}, setTimeout, clearTimeout, Math, JSON, Promise, Error, String, Set, Map, Infinity });
  vm.runInContext(fs.readFileSync(require.resolve('../ui/daemon.js'), 'utf8'), context);
  const d = context.window.Daemon, failures = [], onFail = (e) => failures.push(e);
  process.on('unhandledRejection', onFail);
  try {
    await d.request('create', { bot: 'solo', model: 'alpha/one' });
    await d.request('submit', { bot: 'solo', prompt: 'read the dispatch', delivery: 'reject' });
    await new Promise((r) => setTimeout(r, 300));
    await d.request('interrupt', { bot: 'solo' });
    await d.request('delete', { bot: 'solo' });
    await new Promise((r) => setTimeout(r, 1200));
    assert.deepEqual(failures, []);
  } finally { process.off('unhandledRejection', onFail); d.close(); }
});

test('a folded run names a timeout or a failed call; the finder reaches any task; a side draft stays with its bot', async () => {
  const p = drafting({ request: async () => ({ nodes: [], workspaces:[],next_from: null }) });
  for (const [out, want] of [[{ stdout: '', exit_code: null, success: false, timed_out: true }, 'timed out'], [{ stdout: '', exit_code: null, success: false }, 'failed'], [{ stdout: 'ok', exit_code: 0, success: true }, null]])
    assert.equal(p.entries({ type: 'function_call_output', call_id: 'c', output: JSON.stringify(out) })[0].err, want);
  for (const [name, id] of [['app.lead', 1], ['app.build', 2], ['app.test', 3]]) p.upsert({ name, bot_id: id, provider: 'alpha', model: 'one', created_by: id > 1 ? 'app.lead' : null, created_by_id: id > 1 ? 1 : null });
  p.tree(); p.S.selected = 'app.test';
  p.context.document.getElementById('pickerq').value = 'build';
  assert.deepEqual(Array.from(p.pickerRows(), (r) => r.b.name), ['app.build'], 'a task outside the open level is still found');
  p.S.selected = 'app.lead';
  const draft = p.context.document.getElementById('sideinput');
  await p.go('app.build', 'beside'); draft.value = 'for build only';
  await p.go('app.test', 'beside'); assert.equal(draft.value, '', 'another bot beside has its own draft');
  draft.value = 'for test only'; await p.go(p.S.ui.side, 'beside'); assert.equal(draft.value, '');
  await p.go('app.build', 'beside'); assert.equal(draft.value, 'for build only', 'a closed pane keeps its bot\'s draft');
  await p.go('app.test', 'beside'); assert.equal(draft.value, 'for test only');
});

test('full screen carries each draft with its bot; a long wait list stays short in the head', async () => {
  const p = drafting({ request: async () => ({ nodes: [], workspaces:[],next_from: null }) });
  for (const [name, id] of [['app.lead', 1], ['app.build', 2]]) p.upsert({ name, bot_id: id, provider: 'alpha', model: 'one', created_by: id > 1 ? 'app.lead' : null, created_by_id: id > 1 ? 1 : null });
  p.tree(); p.S.selected = 'app.lead'; p.followDrafts();
  const main = p.context.document.getElementById('input'), side = p.context.document.getElementById('sideinput');
  await p.go('app.build', 'beside'); main.value = 'to lead'; side.value = 'to build';
  await p.go(p.S.ui.side);
  assert.equal(p.S.selected, 'app.build'); assert.equal(main.value, 'to build');
  await p.go('app.lead', 'beside'); assert.equal(side.value, 'to lead', 'the draft left behind waits with its bot');
  p.upsert({ name: 'app.test', bot_id: 3, provider: 'alpha', model: 'one', created_by: 'app.lead', created_by_id: 1 });
  p.transcript('app.build').peers = ['app.lead', 'app.test'];
  p.transcript('app.build').peers = ['app.lead'];
  side.value = 'still to lead'; await p.nextBeside();
  assert.equal(p.S.ui.side, 'app.lead'); assert.equal(side.value, 'still to lead', 'Ctrl-P onto the same bot keeps its draft');
  p.transcript('app.build').peers = ['app.lead', 'app.test'];
  await p.nextBeside(); assert.equal(p.S.ui.side, 'app.test'); assert.equal(side.value, '', 'Ctrl-P shows the next bot\'s own draft');
  const b = p.S.bots.get('app.lead'); b.status = 'waiting';
  b.waitingOn = Array.from({ length: 5000 }, (_, i) => `turn:app.t${i}/1`);
  assert.equal(p.waitSummary(b), 'app.t0/1, app.t1/1, app.t2/1 +4997');
});

test('the demo daemon answers as a side chat only for a fork nested under its source, whatever the name', async () => {
  const context = vm.createContext({ window: {}, setTimeout, clearTimeout, Math, JSON, Promise, Error, String, Set, Map, Infinity });
  vm.runInContext(fs.readFileSync(require.resolve('../ui/daemon.js'), 'utf8'), context);
  const d = context.window.Daemon;
  const texts = async (bot) => {
    await d.request('submit', { bot, prompt: 'run the tests', delivery: 'reject' });
    const events = [];
    while (!events.some((e) => ended(e) && e.bot === bot)) events.push(...(await d.pull()).events);
    return events.filter((e) => e.bot === bot && e.event === 'tool_started').map((e) => e.data.name);
  };
  try {
    await d.request('create', { bot: 'client-side', model: 'alpha/one' });
    assert.deepEqual(await texts('client-side'), ['shell'], 'a bot merely named -side runs its prompt');
    await d.request('fork', { source: 'client-side', bot: 'copy' });
    assert.deepEqual(await texts('copy'), ['shell'], 'a plain fork runs its prompt');
    await d.request('fork', { source: 'client-side', bot: 'peek', created_by: 'client-side' });
    assert.deepEqual(await texts('peek'), ['read'], 'a fork nested under its source answers from history');
  } finally { d.close(); }
});

test('a task card leaves the keyboard beside; a row looks in, and a double-click opens a tab', async () => {
  const p = shell({ request: async () => ({ nodes: [], workspaces:[],next_from: null }) });
  for (const [name, id] of [['app.lead', 1], ['app.build', 2]]) p.upsert({ name, bot_id: id, provider: 'alpha', model: 'one', created_by: id > 1 ? 'app.lead' : null, created_by_id: id > 1 ? 1 : null });
  p.tree(); p.S.selected = 'app.lead';
  const doc = p.context.document, focused = [];
  for (const id of ['input', 'sideinput']) doc.getElementById(id).focus = () => focused.push(id);
  const card = { dataset: { task: 'app.build' } };
  await doc.listeners.click({ target: { closest: (sel) => (sel === '[data-task]' ? card : sel === '.pane.main' ? {} : null) } });
  await p.tick();
  assert.equal(p.S.ui.side, 'app.build'); assert.equal(focused.at(-1), 'sideinput');
  p.S.ui.side = null; p.S.selected = ''; p.S.ui.tabs = [];
  const row = { dataset: { bot: 'app.lead' } }, at = (sel) => (sel === '[data-bot]' ? row : null);
  await doc.listeners.click({ detail: 1, target: { closest: at } }); await p.tick();
  assert.equal(p.S.ui.side, 'app.lead'); assert.equal(p.S.selected, '', 'a click on a row looks in beside');
  // The second click opens the tab by its count, even when the look already covers the list and the
  // click lands on it.
  await doc.listeners.click({ detail: 2, target: { closest: (sel) => (sel === '.pane.side' ? {} : null) } }); await p.tick();
  assert.equal(p.S.selected, 'app.lead'); assert.deepEqual([...p.S.ui.tabs], ['app.lead']); assert.equal(p.S.ui.side, null);
  assert.equal(doc.listeners.dblclick, undefined, 'no dblclick handler to miss a redrawn row');
  // A fast double-click takes the tab before the look fires.
  p.S.selected = ''; p.S.ui.tabs = [];
  const build = { dataset: { bot: 'app.build' } }, atBuild = (sel) => (sel === '[data-bot]' ? build : null);
  await doc.listeners.click({ detail: 1, target: { closest: atBuild } });
  await doc.listeners.click({ detail: 2, target: { closest: atBuild } }); await p.tick();
  assert.equal(p.S.selected, 'app.build'); assert.equal(p.S.ui.side, null, 'the pending look was cancelled');
  // A move after the look is newer: a click the system counts as a second no longer opens the old row.
  p.S.selected = ''; p.S.ui.tabs = [];
  await doc.listeners.click({ detail: 1, target: { closest: at } }); await p.tick();
  assert.equal(p.S.ui.side, 'app.lead');
  await p.go('app.build');
  await doc.listeners.click({ detail: 2, target: { closest: () => null } }); await p.tick();
  assert.equal(p.S.selected, 'app.build'); assert.deepEqual([...p.S.ui.tabs], ['app.build'], 'the old row did not open');
});

test('a click in a file beside keeps the keyboard in the visible composer', async () => {
  const p = shell({ request: async () => ({ nodes: [], workspaces:[],next_from: null }) });
  p.upsert({ name: 'Bob', bot_id: 1, provider: 'alpha', model: 'one' }); p.tree(); p.S.selected = 'Bob';
  const doc = p.context.document, focused = [];
  for (const id of ['input', 'sideinput']) doc.getElementById(id).focus = () => focused.push(id);
  p.S.ui.file = { bot: 'Bob', full: '/w/a.md', gen: 0, state: 'loading' };
  await doc.listeners.click({ target: { closest: (sel) => (sel === '.pane.side' ? {} : null) } });
  await p.tick();
  assert.equal(focused.at(-1), 'input');
});

test('a failed first message waits in the side chat\'s composer', async () => {
  const sent = [];
  const p = shell({ request: async (op, q) => {
    sent.push([op, q]);
    if (op === 'fork') return { name: q.bot, bot_id: 20, provider: 'alpha', model: 'one', created_by: q.created_by, created_by_id: q.created_by_id, allowed: q.allow };
    if (op === 'submit') throw new Error('daemon_gone');
    return { nodes: [], workspaces:[],next_from: null };
  } });
  p.upsert({ name: 'lead', bot_id: 1, provider: 'alpha', model: 'one', status: 'running', running_turn: 3 });
  p.upsert({ name: 'task', bot_id: 2, provider: 'alpha', model: 'one', status: 'running', running_turn: 4, created_by: 'lead', created_by_id: 1 });
  p.S.selected = 'lead'; await p.go('task', 'beside');
  // Asked from the side pane: the copy replaces its source there, and keeps the unsent message.
  const side = p.context.document.getElementById('sideinput'), main = p.context.document.getElementById('input');
  p.setSend('side');
  side.value = 'what now?'; await p.context.document.getElementById('sideform').listeners.submit({ preventDefault() {} });
  assert.equal(p.S.ui.side, 'task-side'); assert.equal(side.value, 'what now?'); assert.equal(main.value, '');
  assert.deepEqual(sent.filter(([op]) => op === 'resume'), [], 'a side chat needs nothing from its source first');
});

test('a bot in a linked worktree shows its branch in its head, read once per folder', async () => {
  const asked = [];
  const p = shell({ request: async () => ({ nodes: [], workspaces:[],next_from: null }), branch: async (dir) => { asked.push(dir); return dir.endsWith('/worktrees/app.build') ? 'agent/app.build' : null; } });
  p.upsert({ name: 'app.lead', bot_id: 1, provider: 'alpha', model: 'one', workspace: '/synthetic' });
  p.upsert({ name: 'app.build', bot_id: 2, provider: 'alpha', model: 'one', workspace: '/home/u/.agent/worktrees/app.build', created_by: 'app.lead', created_by_id: 1 });
  p.tree();
  const head = p.context.document.getElementById('title'), b = p.S.bots.get('app.build');
  p.renderHead(head, b, 'main'); await new Promise((r) => setImmediate(r));
  p.renderHead(head, b, 'main');
  assert.match(head.innerHTML, /⎇ agent\/app\.build/);
  p.renderHead(head, p.S.bots.get('app.lead'), 'main'); await new Promise((r) => setImmediate(r));
  assert.doesNotMatch(head.innerHTML, /⎇/, 'a main checkout shows no branch');
  p.renderHead(head, b, 'side');
  assert.deepEqual(asked, ['/home/u/.agent/worktrees/app.build', '/synthetic'], 'each folder is read once');
  p.upsert({ name: 'app.build', bot_id: 2, provider: 'alpha', model: 'one', workspace: '/synthetic', created_by: 'app.lead', created_by_id: 1 });
  p.renderHead(head, b, 'main'); await new Promise((r) => setImmediate(r));
  assert.doesNotMatch(head.innerHTML, /⎇/, 'a new folder is read again');
});

const rowsOf = (rows) => Array.from(rows, (r) => r.label ?? r.key ?? r.b.name);
const PLAIN = [{ identity: '', model: 'alpha/one', share: 100 }];
const swarmRecord = (members = [], extra = {}) => ({ swarm: 'app.latency', dir: "/home/u/.agent/swarms/app.latency", project: 'app', goal: 'Halve p99.', workspace: '/w/app.latency', budget_tokens: 3000000, mix: PLAIN, members, rows: Object.fromEntries(members.map((m) => [m, 0])), stopped: false, ...extra });

test('a swarm is one row in its project\'s list; its agents and what they made sit under it in the fleet', () => {
  const p = shell();
  p.upsert({ name: 'app.lead', bot_id: 1, provider: 'alpha', model: 'one' });
  p.upsert({ name: 'app.build', bot_id: 2, provider: 'alpha', model: 'one', created_by: 'app.lead', created_by_id: 1 });
  for (const [n, id] of [['app.latency-1', 3], ['app.latency-2', 4]]) p.upsert({ name: n, bot_id: id, provider: 'alpha', model: 'one' });
  p.upsert({ name: 'app.latency-1-side', bot_id: 5, provider: 'alpha', model: 'one', created_by: 'app.latency-1', created_by_id: 3 });
  p.upsert({ name: 'app.latency-1-deep', bot_id: 7, provider: 'alpha', model: 'one', created_by: 'app.latency-1-side', created_by_id: 5 });
  // A bot that took a member's name after the member was deleted is not the swarm's: it stays in the tree.
  p.upsert({ name: 'app.latency-3', bot_id: 9, provider: 'alpha', model: 'one' });
  p.learnSwarm(swarmRecord(['app.latency-1', 'app.latency-2', 'app.latency-3'], { ids: { 'app.latency-1': 3, 'app.latency-2': 4, 'app.latency-3': 6 } }));
  // A swarm whose project is gone still has a row, among the bots in no project.
  p.learnSwarm({ ...swarmRecord([]), swarm: 'gone.x', project: 'gone' });
  const rows = p.tree();
  assert.deepEqual(rowsOf(rows), ['app.lead', '⁂app.latency', 'app.latency-1', 'app.latency-1-side', 'app.latency-1-deep', 'app.latency-2', 'app.build', 'app.latency-3', 'bots', '⁂gone.x']);
  p.S.selected = 'app.lead';
  assert.deepEqual(Array.from(p.railRows(), (r) => r.key ?? r.b.name), ['⁂app.latency', 'app.build', 'app.latency-3'], 'the project\'s list holds the swarm as one row');
  assert.equal(rows[1].prefix, '├ ');
  p.S.bots.get('app.latency-3').status = 'running';
  assert.match(p.botRowHTML(rows[1]), /glyph idle/, 'the stranger does not count as working');
  assert.equal(p.S.bots.get('app.latency-1-side').project, 'app');
  p.S.bots.get('app.latency-2').status = 'running';
  const html = p.botRowHTML(rows[1]);
  assert.match(html, /data-bot="⁂app.latency"/); assert.match(html, /glyph running/); assert.match(html, /⁂ latency/);
  assert.match(html, /data-act="more" data-who="⁂app.latency"/);
  // A swarm open in the window lists its agents; its agent's way up is the swarm.
  p.S.selected = '⁂app.latency';
  assert.deepEqual(Array.from(p.railRows(), (r) => [r.b.name, r.kids]), [['app.latency-1', 1], ['app.latency-2', 0]], 'each agent counts what it made');
  assert.equal(p.upOf('app.latency-1'), '⁂app.latency'); assert.equal(p.upOf('⁂app.latency'), 'app.lead');
  // An agent of the swarm open in a tab lists its helpers, and they theirs.
  const level = (open) => { p.S.selected = open; return Array.from(p.railRows(), (r) => [r.b.name, r.kids]); };
  assert.deepEqual(level('app.latency-1'), [['app.latency-1-side', 1]]);
  assert.deepEqual(level('app.latency-1-side'), [['app.latency-1-deep', 0]]);
  assert.deepEqual(level('app.latency-2'), []);
  p.S.selected = '⁂app.latency';
  // With no tasks after it, the last swarm closes the branch.
  p.S.bots.delete('app.build'); p.S.bots.delete('app.latency-3'); p.S.shapeGen++;
  assert.equal(p.tree()[1].prefix, '└ ');
  assert.deepEqual(Array.from(p.botMenuItems('⁂app.latency'), (i) => i.act).filter(Boolean), ['open-tab', 'swarm-stop', 'swarm-add']);
  // A stopped swarm takes no new agent until a post resumes it.
  p.S.swarms.get('app.latency').stopped = true;
  assert.equal(p.botMenuItems('⁂app.latency').find((i) => i.act === 'swarm-add').disabled, true);
  assert.equal(p.botMenuItems('app.lead')[2].act, 'new-swarm');
  assert.ok(!(p.botMenuItems('app.build-x') ?? []).some((i) => i.act === 'new-swarm'));
});

test('a new swarm is one call: its agents are dealt from the mix, and the page seats them and opens it', async () => {
  const calls = [];
  const p = shell({
    swarmStart: async (q) => { calls.push(q); const members = Array.from({ length: q.agents }, (_, i) => `${q.project}.latency-2-${i + 1}`); return { swarm: swarmRecord(members, { swarm: `${q.project}.latency-2`, mix: q.mix }), bots: members.map((m, i) => ({ name: m, bot_id: 10 + i, provider: 'alpha', model: 'one' })), failed: [{ agent: members[2], error: 'provider_unknown' }] }; },
    swarmBoard: async () => ({ lines: [], offset: 0, more: false }),
    request: async () => ({ bots: [], next_after: null, nodes: [], workspaces:[],next_from: null }),
  });
  p.upsert({ name: 'app.lead', bot_id: 1, provider: 'alpha', model: 'one', workspace: '/synthetic/app' });
  p.upsert({ name: 'app.latency-9', bot_id: 2, provider: 'alpha', model: 'one' });
  const mix = [{ identity: '', model: 'alpha/one', share: 75 }, { identity: 'reviewer', model: 'beta/two', share: 25 }];
  await p.createSwarm('app', { goal: '  Cut the p99 latency of agent run in half.  ', n: 4, mix, shared: true, budget: 3000000, council: 3 });
  // The app's side names it and deals the agents; the page seats what it made.
  assert.deepEqual(JSON.parse(JSON.stringify(calls)), [{ project: 'app', folder: '/synthetic/app', goal: 'Cut the p99 latency of agent run in half.', shared: true, mix, agents: 4, budgetTokens: 3000000, council: 3 }]);
  assert.equal(p.S.bots.get('app.latency-2-2').id, 11);
  assert.equal(p.S.selected, '⁂app.latency-2');
});

test('a mix is dealt to whole agents by share, one at a time, and an added agent keeps the shares', () => {
  const p = shell();
  const counts = (mix, n) => { const c = mix.map(() => 0); for (const r of p.mixRows(mix, n)) c[r]++; return c; };
  const half = [{ share: 50 }, { share: 50 }], odd = [{ share: 60 }, { share: 30 }, { share: 10 }];
  assert.deepEqual(Array.from(p.mixRows(half, 4)), [0, 1, 0, 1]);
  assert.deepEqual(counts(odd, 10), [6, 3, 1]);
  assert.deepEqual(counts(odd, 4), [3, 1, 0]);
  assert.deepEqual(counts(odd, 16), [10, 5, 1]);
  assert.deepEqual(counts([{ share: 100 }], 3), [3]);
  // Two reviewers of five against a 50% share: the next one is a reviewer.
  assert.equal(p.nextRow(half, [3, 2]), 1);
  assert.equal(p.nextRow(half, [2, 2]), 0);
});

test('the sheet offers the folder\'s profiles as identities and shows each row\'s agents', async () => {
  const p = shell({
    models: async () => [{ id: 'alpha/one' }, { id: 'beta/two' }],
    settings: async () => ({ providers: ['alpha', 'beta'] }),
    profiles: async (dir) => { assert.equal(dir, '/synthetic/app'); return [{ name: 'reviewer', summary: 'Reviews', model: 'beta/two' }]; },
  });
  p.upsert({ name: 'app.lead', bot_id: 1, provider: 'alpha', model: 'one', effort: 'high', workspace: '/synthetic/app' });
  const el = (id) => p.context.document.getElementById(id);
  el('sw-n').value = '4'; el('sw-n').id = 'sw-n'; el('sw-budget').value = '3'; el('sw-budget').id = 'sw-budget';
  await p.openSwarmSheet('app');
  // Its agents start on the lead's model and effort.
  assert.match(el('sw-mix').innerHTML, /data-mix="0" data-f="reasoning"[^>]*>[\s\S]*?<option value="high" selected>high<\/option>/);
  assert.match(el('sw-mix').innerHTML, /<option value="" selected>Plain agent<\/option><option value="reviewer">reviewer<\/option>/);
  assert.match(el('sw-mix').innerHTML, /4 agents/);
  assert.match(el('sw-each').textContent, /^10M tokens per agent/);
  assert.equal(el('sw-budget').value, '40');
  // Any whole number of agents can be typed; anything else says what it takes.
  el('sw-n').value = '23'; el('sheet').listeners.input({ target: el('sw-n') });
  assert.match(el('sw-mix').innerHTML, /23 agents/);
  assert.equal(el('sw-budget').value, '230');
  assert.match(el('sw-each').textContent, /^10M tokens per agent/);
  el('sw-n').value = '2.5'; el('sheet').listeners.input({ target: el('sw-n') });
  assert.match(el('sw-mix').innerHTML, /Agents is a whole number from 1 to 64/);
  el('sw-n').value = '4'; el('sheet').listeners.input({ target: el('sw-n') });
  // The budget is typed in millions, and each agent's share follows it.
  el('sw-budget').value = '12'; el('sheet').listeners.input({ target: el('sw-budget') });
  assert.match(el('sw-each').textContent, /^3M tokens per agent/);
  el('sw-n').value = '6'; el('sheet').listeners.input({ target: el('sw-n') });
  assert.equal(el('sw-budget').value, '12', 'an explicitly entered total is preserved');
  el('sw-n').value = '4'; el('sheet').listeners.input({ target: el('sw-n') });
  el('sw-budget').value = '0'; el('sheet').listeners.input({ target: el('sw-budget') });
  assert.match(el('sw-mix').innerHTML, /Budget is 0.1 to 1000 million tokens/);
  el('sw-budget').value = '3'; el('sheet').listeners.input({ target: el('sw-budget') });
  await p.act({ dataset: { act: 'mix-add' } });
  assert.equal((el('sw-mix').innerHTML.match(/2 agents/g) ?? []).length, 2);
  // Picking an identity picks the model its profile names.
  el('sheet').listeners.change({ target: { dataset: { mix: '1', f: 'identity' }, value: 'reviewer' } });
  assert.match(el('sw-mix').innerHTML, /data-mix="1" data-f="model"[^>]*>[\s\S]*?<option value="beta\/two" selected>/);
  // Shares that do not make 100% say so, and a removed row's share goes to the first.
  el('sheet').listeners.input({ target: { dataset: { mix: '1', f: 'share' }, type: 'number', value: '40' } });
  await p.act({ dataset: { act: 'mix-remove', v: '1' } });
  assert.match(el('sw-mix').innerHTML, /value="90"[\s\S]*The shares add up to 90%, not 100%/);
});

test('the board shows roles, proposals, votes and decisions, and a stream tag filters it', async () => {
  const decided = [];
  const state = { roles: { 'latency-1': 'profiler' }, streams: { 'latency-4': 'conn-pool', 'latency-2': 'conn-pool' }, proposals: [
    { id: 'P1', stream: 'conn-pool', why: 'Handshake is 61%.', by: 'latency-4', at: 1, votes: { 'latency-1': { yes: true, reason: 'measured' }, 'latency-2': { yes: true, reason: 'small' } }, status: 'approved', decided_by: 'council' },
    { id: 'P2', stream: 'batch-commits', why: 'Commits are 22%.', by: 'latency-3', at: 2, votes: { 'latency-1': { yes: false, reason: 'pool first' } }, status: 'open', decided_by: null }] };
  const lines = [
    { from: 'latency-1', bot: 'app.latency-1', kind: 'role', role: 'profiler' },
    { from: 'latency-4', bot: 'app.latency-4', kind: 'propose', id: 'P1', stream: 'conn-pool', text: 'Handshake is 61%.' },
    { from: 'latency-1', bot: 'app.latency-1', kind: 'vote', id: 'P1', yes: true, text: 'measured' },
    { from: 'council', kind: 'decision', id: 'P1', stream: 'conn-pool', approved: true, lead: 'latency-4' },
    { from: 'latency-2', bot: 'app.latency-2', kind: 'join', stream: 'conn-pool' },
    { from: 'latency-2', bot: 'app.latency-2', text: 'Pooled: p99 142 → 71 ms.', stream: 'conn-pool' },
    { from: 'latency-3', bot: 'app.latency-3', text: 'Store tests pass.' }];
  const p = shell({ swarmBoard: async () => ({ lines, offset: 99, more: false, state }), swarmDecide: async (swarm, id, approve) => { decided.push([swarm, id, approve]); return { decided: approve ? 'approved' : 'denied' }; }, request: async () => ({ bots: [], next_after: null }) });
  const sw = p.learnSwarm(swarmRecord(['app.latency-1', 'app.latency-2', 'app.latency-3', 'app.latency-4'], { council: 3, seats: ['app.latency-1', 'app.latency-2', 'app.latency-3'] }));
  await p.readBoard(sw);
  assert.equal(sw.state.proposals.length, 2);
  const html = sw.lines.map((l) => p.postHTML(sw, l)).join('\n');
  assert.match(html, /latency-1<\/button><span class="pt">is now <i>profiler<\/i>/);
  assert.match(html, /proposes <b>P1<\/b> <button type="button" class="tag" data-act="swarm-filter" data-v="conn-pool">#conn-pool<\/button>: Handshake is 61%\. <span class="tally">approved<\/span>/);
  assert.match(html, /votes <b>yes<\/b> on P1: measured/);
  assert.match(html, /<span class="who council">council<\/span><span class="pt">P1 <button[^>]*>#conn-pool<\/button> approved · latency-4 leads it/);
  assert.match(html, /joined <button[^>]*>#conn-pool/);
  p.S.selected = '⁂app.latency';
  const log = { dataset: {}, innerHTML: '', scrollHeight: 0, scrollTop: 0, clientHeight: 0, querySelectorAll: () => [] }, title = { dataset: {}, innerHTML: '' };
  const act = async (v) => { await p.act({ dataset: v }); p.renderSwarmHead(title, sw); p.renderSwarm(log, sw); };
  await act({ act: 'swarm-filter', v: 'conn-pool' });
  assert.match(log.innerHTML, /Pooled: p99/); assert.doesNotMatch(log.innerHTML, /Store tests pass/);
  assert.match(title.innerHTML, /#conn-pool ×/); assert.match(title.innerHTML, /Council <span class="count">1<\/span>/); assert.match(title.innerHTML, /Streams 1/);
  await act({ act: 'swarm-filter', v: '' });
  assert.match(log.innerHTML, /Store tests pass/);
  await act({ act: 'swarm-tab', v: 'council' });
  assert.match(log.innerHTML, /Seats: latency-1, latency-2, latency-3\. 2 of 3 decide/);
  assert.match(log.innerHTML, /<b>P2<\/b>[\s\S]*0 yes · 1 no of 3[\s\S]*pool first[\s\S]*not yet[\s\S]*data-v="P2:yes">Approve/);
  assert.doesNotMatch(log.innerHTML, /data-v="P1:yes"/, 'a decided proposal takes no decision');
  await act({ act: 'swarm-decide', v: 'P2:no' });
  assert.deepEqual(decided, [['app.latency', 'P2', false]]);
  await act({ act: 'swarm-tab', v: 'streams' });
  assert.match(log.innerHTML, /#conn-pool<\/button> <span class="dim">2 agents<\/span>/);
  assert.match(log.innerHTML, /data-task="app.latency-4">latency-4 <span class="dim">lead<\/span>/);
  // A seat that leaves while the Council tab is open shows gone at once, with no board change.
  await act({ act: 'swarm-tab', v: 'council' });
  p.learnSwarm(swarmRecord(['app.latency-2', 'app.latency-3', 'app.latency-4'], { council: 3, seats: ['app.latency-2', 'app.latency-3', 'app.latency-4'] }));
  p.renderSwarm(log, sw);
  assert.match(log.innerHTML, /Seats: latency-2, latency-3, latency-4\./);
});

test('a swarm row opens a beat later, so a double-click opens it as a new tab', async () => {
  const p = shell({ swarmBoard: async () => ({ lines: [], offset: 0, more: false }), request: async () => ({ bots: [], next_after: null, nodes: [], workspaces:[],next_from: null }) });
  p.upsert({ name: 'app.lead', bot_id: 1, provider: 'alpha', model: 'one' });
  p.upsert({ name: 'app.latency-1', bot_id: 3, provider: 'alpha', model: 'one' });
  p.learnSwarm(swarmRecord(['app.latency-1'], { ids: { 'app.latency-1': 3 } }));
  await p.go('app.lead');
  const doc = p.context.document, row = { dataset: { bot: '⁂app.latency' } }, at = (sel) => (sel === '[data-bot]' ? row : null);
  await doc.listeners.click({ detail: 1, target: { closest: at } });
  assert.equal(p.S.selected, 'app.lead', 'the first click waits');
  await doc.listeners.click({ detail: 2, target: { closest: at } }); await p.tick();
  assert.deepEqual([...p.S.ui.tabs], ['app.lead', '⁂app.latency'], 'a double-click opens a new tab'); assert.equal(p.S.selected, '⁂app.latency');
  await p.go('app.lead');
  await doc.listeners.click({ detail: 1, target: { closest: at } }); await p.tick();
  assert.equal(p.S.selected, '⁂app.latency'); assert.deepEqual([...p.S.ui.tabs], ['app.lead', '⁂app.latency'], 'a click goes to its tab');
  // A slow double-click: the swarm already took the tab in view, and the second click, landing on what
  // replaced the row, gives that tab back and opens the swarm beside it.
  p.S.ui.tabs = ['app.lead']; await p.go('app.lead');
  await doc.listeners.click({ detail: 1, target: { closest: at } }); await p.tick();
  assert.deepEqual([...p.S.ui.tabs], ['⁂app.latency']);
  await doc.listeners.click({ detail: 2, target: { closest: () => null } }); await p.tick();
  assert.deepEqual([...p.S.ui.tabs], ['app.lead', '⁂app.latency']); assert.equal(p.S.selected, '⁂app.latency');
});

test('a tab is renamed when what it belongs to changes, with no change of state', async () => {
  const p = shell({ request: async () => ({ nodes: [], workspaces:[],next_from: null }) });
  p.upsert({ name: 'app.lead', bot_id: 1, provider: 'alpha', model: 'one' });
  p.upsert({ name: 'app.build', bot_id: 2, provider: 'alpha', model: 'one', created_by: 'app.lead', created_by_id: 1 });
  p.S.ui.tabs = ['app.build']; p.S.selected = 'app.build'; p.railRows(); p.renderTabs();
  const tabs = p.context.document.getElementById('tabs');
  assert.match(tabs.innerHTML, /<span class="tl">build<\/span>/);
  await p.handle({ event: 'deleted', bot: 'app.lead' }, p.S.session);
  p.railRows(); p.renderTabs();
  assert.match(tabs.innerHTML, /<span class="tl">app\.build<\/span>/, 'a task whose coordinator is gone is named in full');
});

test('every control is one move: a pending look yields to Home or a tab, a double-click opens a tab, the finder reaches a swarm helper', async () => {
  const p = shell({ swarmBoard: async () => ({ lines: [], offset: 0, more: false }), request: async () => ({ bots: [], next_after: null, nodes: [], workspaces:[],next_from: null }) });
  p.upsert({ name: 'app.lead', bot_id: 1, provider: 'alpha', model: 'one' });
  p.upsert({ name: 'app.build', bot_id: 2, provider: 'alpha', model: 'one', created_by: 'app.lead', created_by_id: 1 });
  p.upsert({ name: 'app.latency-1', bot_id: 3, provider: 'alpha', model: 'one' });
  p.upsert({ name: 'app.latency-1-probe', bot_id: 4, provider: 'alpha', model: 'one', created_by: 'app.latency-1', created_by_id: 3 });
  p.learnSwarm(swarmRecord(['app.latency-1'], { ids: { 'app.latency-1': 3 } }));
  const doc = p.context.document, on = (sel, el) => ({ closest: (q) => (q === sel ? el : null) });
  const row = (who) => on('[data-bot]', { dataset: { bot: who } }), click = (target, detail = 1) => doc.listeners.click({ detail, target });
  await p.go('app.lead');
  // A click, then Home within the beat: Home wins, and nothing opens beside after it.
  await click(row('app.build')); await click(on('[data-act]', { dataset: { act: 'home' } })); await p.tick();
  assert.equal(p.S.selected, ''); assert.equal(p.S.ui.side, null);
  // A click, then a tab: the tab wins.
  await click(row('app.build')); await click(on('[data-tab]', { dataset: { tab: 'app.lead' } })); await p.tick();
  assert.equal(p.S.selected, 'app.lead'); assert.equal(p.S.ui.side, null);
  // A fast double-click opens a tab and no look.
  await click(row('app.build')); await click(row('app.build'), 2); await p.tick();
  assert.deepEqual([...p.S.ui.tabs], ['app.lead', 'app.build']); assert.equal(p.S.selected, 'app.build'); assert.equal(p.S.ui.side, null);
  // A slow one, on a swarm: the first click already opened it in the tab in view, and the second,
  // landing on what replaced the row, puts that tab back and opens the swarm in a new one.
  await p.go('app.lead');
  await click(row('⁂app.latency')); await p.tick();
  assert.deepEqual([...p.S.ui.tabs], ['⁂app.latency', 'app.build']);
  await click(on('.pane.main', {}), 2); await p.tick();
  assert.deepEqual([...p.S.ui.tabs], ['app.lead', 'app.build', '⁂app.latency']); assert.equal(p.S.selected, '⁂app.latency');
  // The finder reads the whole fleet, swarm agents and what they made included.
  doc.getElementById('pickerq').value = 'probe';
  assert.deepEqual(Array.from(p.pickerRows(), (r) => r.b.name), ['app.latency-1-probe']);
  await doc.getElementById('pickerlist').listeners.click({ target: on('[data-pick]', { dataset: { pick: 'app.latency-1-probe' } }) }); await p.tick();
  assert.equal(p.S.selected, 'app.latency-1-probe'); assert.equal(p.S.ui.tabs.at(-1), 'app.latency-1-probe');
  assert.equal(p.upOf('app.latency-1-probe'), 'app.latency-1');
  await p.go('app.latency-1');
  assert.deepEqual(Array.from(p.railRows(), (r) => r.b.name), ['app.latency-1-probe'], 'the helper is the level below its agent');
});

test('looking at an agent clears the done glyph on its tab', async () => {
  const p = shell({ request: async () => ({ nodes: [], workspaces:[],next_from: null }) });
  p.context.document.hasFocus = () => true;
  for (const [name, id] of [['lead', 1], ['task', 2]]) p.upsert({ name, bot_id: id, provider: 'alpha', model: 'one' });
  p.S.ui.tabs = ['lead', 'task']; p.S.selected = 'lead'; p.S.unseen.add('task');
  p.renderTabs();
  const tabs = p.elements.get('tabs');
  assert.match(tabs.innerHTML, /data-tab="task"/); assert.match(tabs.innerHTML, /glyph done/);
  p.S.selected = 'task'; p.markSeen();
  assert.equal(p.S.unseen.has('task'), false);
  assert.doesNotMatch(tabs.innerHTML, /glyph done/, 'the tab redraws with the agent seen');
});

test('a flat swarm has no Council or Streams tab', async () => {
  const p = shell({ swarmBoard: async () => ({ lines: [], offset: 0, more: false }), request: async () => ({ bots: [], next_after: null }) });
  const sw = p.learnSwarm(swarmRecord(['app.latency-1']));
  const title = { dataset: {}, innerHTML: '' }; p.renderSwarmHead(title, sw);
  assert.doesNotMatch(title.innerHTML, /Council|Streams/);
});

test('the board is read on, a tail read afresh replaces what was read, and the composer posts to it', async () => {
  const reads = [], posts = [];
  let board = { lines: [{ from: 'user', text: 'Halve p99.' }], offset: 20, more: false, reset: true };
  const p = shell({ swarmBoard: async (swarm, offset) => { reads.push(offset); return board; }, swarmPost: async (swarm, text) => { posts.push([swarm, text]); return { posted: true, steered: ['latency-1'], woke: [], missed: [] }; }, request: async () => ({ bots: [], next_after: null }) });
  const sw = p.learnSwarm(swarmRecord(['app.latency-1']));
  await p.readBoard(sw);
  board = { lines: [{ from: 'latency-1', bot: 'app.latency-1', text: 'Taking the profile. @latency-2 yours?' }], offset: 90, more: false };
  await p.readBoard(sw);
  assert.deepEqual(reads, [null, 20]); assert.equal(sw.lines.length, 2);
  board = { lines: [{ from: 'user', text: 'again' }], offset: 900000, more: false, reset: true };
  await p.readBoard(sw);
  assert.deepEqual(Array.from(sw.lines, (l) => l.text), ['again']);
  // A board ending in half a line says there is more but gets no further: one read, not a spin.
  board = { lines: [], offset: 900000, more: true };
  reads.length = 0; await p.readBoard(sw);
  assert.deepEqual(reads, [900000]);
  const html = p.postHTML(sw, { from: 'latency-1', bot: 'app.latency-1', text: '<b> ask @latency-2.' });
  assert.match(html, /data-task="app.latency-1"/); assert.match(html, /&lt;b&gt;/); assert.match(html, /<span class="at">@latency-2<\/span>\./);
  p.S.selected = '⁂app.latency';
  await p.submit('@latency-1 check fsync');
  assert.deepEqual(posts, [['app.latency', '@latency-1 check fsync']]);
});

test('a member\'s durable event reads the board only while the swarm is on screen', async () => {
  let reads = 0;
  const p = shell({ swarmBoard: async () => { reads++; return { lines: [], offset: 0, more: false }; }, request: async () => ({ bots: [], next_after: null }) });
  p.upsert({ name: 'app.latency-1', bot_id: 3, provider: 'alpha', model: 'one' });
  p.learnSwarm(swarmRecord(['app.latency-1'], { ids: { 'app.latency-1': 3 } }));
  p.S.selected = 'app.latency-1';
  await p.handle({ event: 'tool_completed', bot: 'app.latency-1', turn: 1, data: { call_id: 'c', node: 1 } }, 1, false);
  await p.tick(); assert.equal(reads, 0);
  p.S.selected = '⁂app.latency';
  await p.handle({ event: 'text_delta', bot: 'app.latency-1', turn: 1, text: 'x', durable: false }, 1, false);
  await p.tick(); assert.equal(reads, 0, 'deltas never read the board');
  await p.handle({ event: 'tool_completed', bot: 'app.latency-1', turn: 1, data: { call_id: 'd', node: 2 } }, 1, false);
  await p.handle({ event: 'tool_completed', bot: 'app.latency-1', turn: 1, data: { call_id: 'e', node: 3 } }, 1, false);
  await p.tick(); assert.equal(reads, 1, 'a burst reads once');
});

test('Stop and Add are one call each; an added agent comes from the row furthest below its share', async () => {
  const calls = [];
  const mix = [{ identity: '', model: 'alpha/one', share: 50 }, { identity: 'reviewer', model: 'beta/two', share: 50 }];
  const ids = { 'app.latency-1': 3, 'app.latency-2': 4, 'app.latency-3': 5, 'app.latency-4': 9 };
  const members = ['app.latency-1', 'app.latency-2', 'app.latency-3'], rows = { 'app.latency-1': 0, 'app.latency-2': 1, 'app.latency-3': 0 };
  const p = shell({
    swarmStop: async (swarm) => { calls.push(['stop', swarm]); return { swarm: swarmRecord(members.slice(0, 2), { stopped: true, mix, rows, ids }), failed: [] }; },
    swarmAdd: async (swarm, row) => { calls.push(['add', swarm, row]); return { swarm: swarmRecord([...members.slice(0, 2), 'app.latency-4'], { mix, rows: { ...rows, 'app.latency-4': row }, ids }), bots: [{ name: 'app.latency-4', bot_id: 9, provider: 'beta', model: 'two' }], failed: [] }; },
    request: async () => ({ bots: [], next_after: null }),
  });
  const sw = p.learnSwarm(swarmRecord(members, { mix, rows, ids }));
  await p.stopSwarm(sw);
  assert.equal(sw.stopped, true);
  assert.deepEqual(sw.members, ['app.latency-1', 'app.latency-2']);
  await p.addAgent(sw);
  assert.deepEqual(calls, [['stop', 'app.latency'], ['add', 'app.latency', 0]]);
  assert.equal(p.S.bots.get('app.latency-4').id, 9);
  // Cards and posts say what an agent is, where the swarm has more than one kind.
  p.upsert({ name: 'app.latency-2', bot_id: 4, provider: 'beta', model: 'two' });
  assert.match(p.postHTML(sw, { from: 'latency-2', bot: 'app.latency-2', text: 'LGTM' }), /class="who wide" data-task="app.latency-2">latency-2 <span class="kind">reviewer · two<\/span><\/button>/);
  assert.doesNotMatch(p.postHTML(p.learnSwarm(swarmRecord(['app.latency-1'], { swarm: 'app.flat' })), { from: 'latency-1', bot: 'app.latency-1', text: 'x' }), /kind/);
  sw.tab = 'agents';
  const log = { dataset: {}, innerHTML: '', scrollHeight: 0, scrollTop: 0, clientHeight: 0, querySelectorAll: () => [] };
  p.renderSwarm(log, sw);
  assert.match(log.innerHTML, /latency-2 · reviewer · two/);
});

test('a swarm a coordinator started from its shell shows once its first agent takes its brief', async () => {
  let reads = 0, listed = [];
  const p = shell({ swarms: async () => { reads += 1; return { swarms: listed, broken: [] }; }, request: async () => ({ bots: [], next_after: null, nodes: [], workspaces:[],next_from: null }) });
  p.S.live = true;
  p.upsert({ name: 'app.lead', bot_id: 1, provider: 'alpha', model: 'one' });
  // A task named like an agent is looked for once, however many turns it takes.
  p.upsert({ name: 'app.fix-2', bot_id: 2, provider: 'alpha', model: 'one' });
  for (const turn of [1, 2]) await p.handle({ event: 'accepted', bot: 'app.fix-2', turn, durable: true }, 1);
  await p.tick();
  assert.equal(reads, 1);
  assert.equal(p.S.swarms.size, 0);
  // Once that task is deleted, a bot that takes its name is looked for again.
  await p.handle({ event: 'deleted', bot: 'app.fix-2' }, 1);
  p.upsert({ name: 'app.fix-2', bot_id: 5, provider: 'alpha', model: 'one' });
  await p.handle({ event: 'accepted', bot: 'app.fix-2', turn: 1, durable: true }, 1);
  await p.tick();
  assert.equal(reads, 2);
  listed = [swarmRecord(['app.latency-1', 'app.latency-2'], { ids: { 'app.latency-1': 3, 'app.latency-2': 4 } })];
  for (const [name, id] of [['app.latency-1', 3], ['app.latency-2', 4]]) p.upsert({ name, bot_id: id, provider: 'alpha', model: 'one', created_by: 'app.lead', created_by_id: 1 });
  await p.handle({ event: 'accepted', bot: 'app.latency-1', turn: 1, durable: true }, 1);
  await p.handle({ event: 'queued', bot: 'app.latency-2', turn: 1, durable: true }, 1);
  await p.tick();
  assert.equal(reads, 3, 'one read for the burst');
  assert.deepEqual([...p.S.swarms.keys()], ['app.latency']);
  // A read that failed is tried again at the agent's next turn.
  let failing = true;
  const q = shell({ swarms: async () => { reads += 1; if (failing) throw new Error('unavailable'); return { swarms: listed, broken: [] }; }, request: async () => ({ bots: [], next_after: null, nodes: [], workspaces:[],next_from: null }) });
  q.S.live = true;
  q.upsert({ name: 'app.latency-1', bot_id: 3, provider: 'alpha', model: 'one' });
  await q.handle({ event: 'accepted', bot: 'app.latency-1', turn: 1, durable: true }, 1);
  await q.tick();
  assert.equal(q.S.swarms.size, 0);
  failing = false;
  await q.handle({ event: 'accepted', bot: 'app.latency-1', turn: 2, durable: true }, 1);
  await q.tick();
  assert.deepEqual([...q.S.swarms.keys()], ['app.latency']);
  assert.equal(p.S.memberOf.get('app.latency-2'), 'app.latency');
  // Two reads at once: an older answer arriving last never removes a swarm the newer one found.
  const answers = [];
  const r = shell({ swarms: () => new Promise((resolve) => answers.push(resolve)), request: async () => ({ bots: [], next_after: null, nodes: [], workspaces:[],next_from: null }) });
  r.S.live = true;
  for (const [name, id] of [['app.fix-1', 2], ['app.latency-1', 3]]) r.upsert({ name, bot_id: id, provider: 'alpha', model: 'one' });
  await r.handle({ event: 'accepted', bot: 'app.fix-1', turn: 1, durable: true }, 1);
  await r.tick();
  await r.handle({ event: 'accepted', bot: 'app.latency-1', turn: 1, durable: true }, 1);
  await r.tick();
  assert.equal(answers.length, 2);
  answers[1]({ swarms: listed, broken: [] }); await r.tick();
  answers[0]({ swarms: [], broken: [] }); await r.tick();
  assert.deepEqual([...r.S.swarms.keys()], ['app.latency']);
});

test('a helper finishing a turn has its swarm check its budget, and only current seats are tallied', async () => {
  const checks = [];
  const p = shell({ swarmCheck: async (swarm) => { checks.push(swarm); return {}; }, request: async () => ({ bots: [], next_after: null, nodes: [], workspaces:[],next_from: null }) });
  for (const [name, id] of [['app.latency-1', 3], ['app.latency-2', 4]]) p.upsert({ name, bot_id: id, provider: 'alpha', model: 'one' });
  // A helper of latency-1, and a helper of that helper.
  p.upsert({ name: 'app.latency-1.fix', bot_id: 7, provider: 'alpha', model: 'one', created_by: 'app.latency-1', created_by_id: 3 });
  p.upsert({ name: 'app.latency-1.fix.deep', bot_id: 8, provider: 'alpha', model: 'one', created_by: 'app.latency-1.fix', created_by_id: 7 });
  p.upsert({ name: 'app.other', bot_id: 9, provider: 'alpha', model: 'one' });
  const sw = p.learnSwarm(swarmRecord(['app.latency-1', 'app.latency-2'], { ids: { 'app.latency-1': 3, 'app.latency-2': 4 }, council: 3, seats: ['app.latency-1', 'app.latency-2'] }));
  await p.handle({ event: 'turn_finished', bot: 'app.latency-1.fix.deep', turn: 1, durable: true }, 1);
  await p.handle({ event: 'turn_finished', bot: 'app.other', turn: 1, durable: true }, 1);
  await p.tick();
  assert.deepEqual(checks, ['app.latency']);
  // A helper whose maker left and is gone still has its swarm check, by the maker's id; a check that
  // passed a share reads the board, which no agent's event may do.
  let reads = 0; p.context.Daemon.swarmBoard = async () => { reads += 1; return { lines: [], offset: 0, more: false, reset: true, state: sw.state }; };
  let passed = null;
  p.context.Daemon.swarmCheck = async (swarm) => { checks.push(swarm); return { budget: passed, board_changed: !!passed }; };
  sw.left = [11]; p.S.selected = '⁂app.latency';
  p.upsert({ name: 'app.latency-5.fix', bot_id: 12, provider: 'alpha', model: 'one', created_by: 'app.latency-5', created_by_id: 11 });
  await p.handle({ event: 'turn_finished', bot: 'app.latency-5.fix', turn: 1, durable: true }, 1);
  await p.tick(); await p.tick();
  assert.deepEqual(checks, ['app.latency', 'app.latency']);
  const quiet = reads;
  passed = 'the swarm has used 50% of its budget';
  await p.handle({ event: 'turn_finished', bot: 'app.latency-5.fix', turn: 2, durable: true }, 1);
  await p.tick(); await p.tick();
  assert.equal(reads, 2 * quiet + 1, 'the same reads again, and one more for the budget line');
  // A seat that left keeps no say: latency-9's yes is not counted, and the majority is still of the council's three.
  assert.equal(p.tally(sw, { votes: { 'latency-1': { yes: true }, 'latency-9': { yes: true } } }), '1 yes of 3');
});

test('a stall notice refreshes the open board, while an unchanged check adds no read', async () => {
  const persisted = [], checks = [];
  let reads = 0;
  const p = shell({
    swarmBoard: async (_, offset) => { reads++; return { lines: persisted.slice(offset ?? 0), offset: persisted.length }; },
    swarmCheck: async () => { const pending = deferred(); checks.push(pending); return pending.promise; },
    request: async () => ({ bots: [], nodes: [], next_after: null }),
  });
  p.upsert({ name: 'app.latency-1', bot_id: 3, status: 'running' });
  const sw = p.learnSwarm(swarmRecord(['app.latency-1'], { ids: { 'app.latency-1': 3 } }));
  p.S.selected = '⁂app.latency';
  for (const changed of [true, false]) {
    const before = reads;
    await p.handle({ event: 'turn_finished', bot: 'app.latency-1', turn: changed ? 1 : 2, data: { status: 'completed' } }, 1);
    await p.tick();
    assert.equal(reads, before + 1, 'the event reads the board before the check completes');
    if (changed) persisted.push({ at: 1, from: 'swarm', kind: 'quiet', text: 'nothing is running and there is no final result (partial)' });
    checks.at(-1).resolve({ board_changed: changed });
    await settle(); await p.tick();
    assert.equal(reads, before + (changed ? 2 : 1), 'only an appended entry causes a follow-up read');
    assert.equal(sw.lines.length, 1);
    assert.equal(sw.lines[0].kind, 'quiet');
  }
});

test('a deleted agent leaves its swarm', async () => {
  const calls = [];
  const p = shell({
    swarmLeave: async (swarm, member) => { calls.push(['leave', swarm, member]); return swarmRecord(['app.latency-2']); },
    request: async () => ({ bots: [], next_after: null }),
  });
  for (const [n, id] of [['app.latency-1', 3], ['app.latency-2', 4]]) p.upsert({ name: n, bot_id: id, provider: 'alpha', model: 'one' });
  const sw = p.learnSwarm(swarmRecord(['app.latency-1', 'app.latency-2']));
  await p.onEvent({ event: 'deleted', bot: 'app.latency-1', durable: true });
  await p.tick();
  assert.deepEqual(calls.at(-1), ['leave', 'app.latency', 'app.latency-1']);
  assert.deepEqual(sw.members, ['app.latency-2']);
  assert.equal(p.S.memberOf.has('app.latency-1'), false);
});

test('the demo daemon\'s swarm: agents post, working ones hear it, and an idle one wakes only when named', async () => {
  const context = vm.createContext({ window: {}, setTimeout, clearTimeout, Math, JSON, Promise, Error, String, Set, Map, Infinity, Date });
  vm.runInContext(fs.readFileSync(require.resolve('../ui/daemon.js'), 'utf8'), context);
  const d = context.window.Daemon;
  const { swarm: sw } = await d.swarmStart({ project: 'demo', folder: '/workspace', goal: 'Halve p99 latency.', shared: true, mix: [{ identity: '', model: 'alpha/one', share: 100 }], agents: 2, budgetTokens: 1000 });
  assert.equal(sw.workspace, '~/.agent/worktrees/demo.latency');
  // Their briefs start them; the swarm is quiet again once both are done.
  const done = new Set();
  while (done.size < 2) for (const e of (await d.pull()).events) if (ended(e)) done.add(e.bot);
  const before = (await d.swarmBoard('demo.latency', null)).lines.length;
  // Your post naming nobody wakes both; one naming latency-2 wakes only it.
  await d.swarmPost('demo.latency', '@latency-2 look at fsync');
  const events = [];
  while (!events.some(ended)) events.push(...(await d.pull()).events);
  assert.deepEqual([...new Set(events.filter((e) => e.event === 'accepted').map((e) => e.bot))], ['demo.latency-2']);
  const board = (await d.swarmBoard('demo.latency', null)).lines.slice(before);
  assert.deepEqual(Array.from(board, (l) => l.from), ['user', 'latency-2']);
  assert.equal(board[1].text, 'On it: look at fsync.');
  d.close();
});

test('the demo daemon\'s council opens a stream by the seats\' majority and leaves a proposal for you', async () => {
  const context = vm.createContext({ window: {}, setTimeout, clearTimeout, Math, JSON, Promise, Error, String, Set, Map, Infinity, Date, structuredClone });
  vm.runInContext(fs.readFileSync(require.resolve('../ui/daemon.js'), 'utf8'), context);
  const d = context.window.Daemon;
  const names = [1, 2, 3, 4].map((i) => `demo.latency-${i}`);
  const { swarm: sw } = await d.swarmStart({ project: 'demo', folder: '/workspace', goal: 'Halve p99 latency.', shared: true, mix: [{ identity: '', model: 'alpha/one', share: 100 }], agents: 4, budgetTokens: 1000, council: 3 });
  assert.deepEqual([sw.council, Array.from(sw.seats)], [3, names.slice(0, 3)]);
  const done = new Set();
  while (done.size < 4) for (const e of (await d.pull()).events) if (ended(e)) done.add(e.bot);
  const { state, lines } = await d.swarmBoard('demo.latency', null);
  assert.deepEqual(state.proposals.map((p) => [p.id, p.stream, p.status]), [['P1', 'conn-pool', 'approved'], ['P2', 'batch-commits', 'open']]);
  assert.deepEqual({ ...state.streams }, { 'latency-1': 'conn-pool', 'latency-2': 'conn-pool', 'latency-4': 'conn-pool' });
  assert.ok(lines.some((l) => l.kind === 'decision' && l.from === 'council'));
  assert.ok(lines.some((l) => l.stream === 'conn-pool' && !l.kind), 'a stream member posts to its stream');
  await d.swarmDecide('demo.latency', 'P2', true);
  assert.equal((await d.swarmBoard('demo.latency', null)).state.proposals[1].status, 'approved');
  d.close();
});

test('a draft stays with the bot it was typed for, and Enter sends it there even mid-switch', async () => {
  const sent = [];
  let release; const slow = new Promise((r) => { release = r; });
  const p = drafting({ request: async (op, q) => { if (op === 'history_nodes') await slow; if (op === 'submit') sent.push([q.bot, q.bot_id, q.prompt]); return { nodes: [], workspaces:[],next_from: null }; } });
  for (const [name, id] of [['app.lead', 1], ['app.lead-side', 2]]) p.upsert({ name, bot_id: id, provider: 'alpha', model: 'one', created_by: id > 1 ? 'app.lead' : null, created_by_id: id > 1 ? 1 : null });
  p.S.transcripts.get('app.lead-side') ?? p.transcript('app.lead-side').nodes;
  p.tree(); await p.go('app.lead');
  const doc = p.context.document, main = doc.getElementById('input');
  main.value = 'Ship it.';
  // Sol's audit, step 7: the coordinator's text followed the selection to its side chat.
  release(); await p.go('app.lead-side');
  assert.equal(main.value, '', 'the side chat has its own, empty composer');
  main.value = 'Only for the side chat.';
  await p.go('app.lead');
  assert.equal(main.value, 'Ship it.');
  // Enter while another bot is being opened: the text goes to the bot it was typed for.
  const opening = p.go('app.lead-side');
  await doc.getElementById('form').listeners.submit({ preventDefault() {} });
  await opening;
  assert.deepEqual(sent, [['app.lead', 1, 'Ship it.']]);
  assert.equal(main.value, 'Only for the side chat.');
});

// A window over settings kept as the app keeps them: saved values, a daemon restarted to apply them,
// and providers that answer with their models.
function settingsShell({ env = {}, lists = {} } = {}) {
  const calls = [];
  const specs = () => (env.AGENT_PROVIDER ?? '').split(/\s+/).filter(Boolean);
  const answer = () => Object.fromEntries(specs().map((spec) => { const n = spec.split('=')[0]; return [n, lists[n] ?? { error: 'provider_http_401', detail: 'no' }]; }));
  const p = shell({
    settings: async () => ({ providers: specs(), region: env.AWS_REGION ?? null, profile: null, keys: Object.keys(env).filter((k) => k.endsWith('_KEY') || k.startsWith('AWS_BEARER')) }),
    saveSettings: async (changes) => { calls.push(['save', { ...changes }]); for (const [k, v] of Object.entries(changes)) { if (v) env[k] = v; else delete env[k]; } },
    restartDaemon: async () => { calls.push(['restart']); },
    discoverModels: async () => { calls.push(['discover']); const a = answer(); return { providers: Object.fromEntries(Object.entries(a).map(([n, l]) => [n, l.models ? { models: l.models.length } : l])), written: true, error: null }; },
    models: async () => Object.entries(answer()).flatMap(([n, l]) => (l.models ?? []).map((m) => ({ id: `${n}/${m.id}` }))),
    attach: async () => { calls.push(['attach']); if (!specs().length) throw new Error('no_provider: connect a provider in Settings'); return { session: 2 }; },
    pull: () => new Promise(() => {}),
    project: async (dir) => ({ dir, name: 'weather', coordinator: 'weather.lead', model: null, file: false }),
    policy: async () => ({ instructions: 'rules', compaction_instructions: 'summary', note: 'test' }),
    writeProject: async (q) => { calls.push(['write', q.model]); },
    request: async (op, q) => { if (op === 'provider_models') return { providers: answer() }; if (op === 'bots') return { bots: [], next_after: null }; if (op === 'create') { calls.push(['create', q.bot, q.model]); return { name: q.bot, bot_id: 9, provider: q.model.split('/')[0], model: q.model.split('/')[1], workspace: q.workspace }; } return { nodes: [], workspaces:[],next_from: null }; },
  });
  p.S.config.model = null; p.S.attached = true;
  return { p, calls, env };
}

test('Bedrock is one provider running both its APIs, named alone whether it signs with the AWS login or a key', () => {
  const p = shell();
  assert.deepEqual([...p.providerSpecs('bedrock')], ['bedrock', 'bedrock-openai']);
  assert.deepEqual([...p.providerSpecs('anthropic')], ['anthropic']);
});

test('connecting a provider keeps the others, saves only what was typed, restarts the daemon and lists its models', async () => {
  const { p, calls, env } = settingsShell({ env: { AGENT_PROVIDER: 'openai', OPENAI_API_KEY: 'k' }, lists: { openai: { models: [{ id: 'gpt' }] }, bedrock: { models: [{ id: 'claude' }, { id: 'haiku' }] }, 'bedrock-openai': { models: [{ id: 'grok' }] } } });
  await p.openSetup();
  await assert.rejects(p.connectProvider('bedrock', { AWS_REGION: 'us east-1', AWS_PROFILE: '', AWS_BEARER_TOKEN_BEDROCK: '' }), /Region must look like us-east-1/);
  assert.equal(calls.length, 0);
  await p.connectProvider('bedrock', { AWS_REGION: 'us-east-1', AWS_PROFILE: '', AWS_BEARER_TOKEN_BEDROCK: '' });
  // Only providers are saved; the empty key field keeps whatever key was saved.
  assert.deepEqual(calls.map(([c]) => c), ['save', 'restart', 'attach', 'discover']);
  // An empty profile is saved empty, so a start clears one the shell exports too.
  assert.deepEqual({ ...calls[0][1] }, { AGENT_PROVIDER: 'openai bedrock bedrock-openai', AWS_REGION: 'us-east-1', AWS_PROFILE: '' });
  assert.deepEqual(JSON.parse(JSON.stringify(p.S.setup.status)), { openai: { models: 1 }, bedrock: { models: 2 }, 'bedrock-openai': { models: 1 } });
  assert.deepEqual(p.S.setup.list.map((m) => m.id), ['openai/gpt', 'bedrock/claude', 'bedrock/haiku', 'bedrock-openai/grok']);
  // One row and one model group for Bedrock, whichever of its APIs serves a model.
  const html = p.setupHTML();
  // No model is chosen for the user: the first project's picker waits for one, by provider.
  assert.match(html, /<option value="" selected disabled>Choose a model/);
  assert.equal(html.match(/data-act="setup-remove"/g).length, 2);
  assert.match(html, /Amazon Bedrock<\/span><span class="st ok">✓ 3 models/);
  assert.equal(html.match(/<optgroup label="Amazon Bedrock">/g).length, 1);
  assert.match(html, /<option value="bedrock-openai\/grok">grok<\/option>/);
  // A Bedrock key, once saved, stays in use when the region changes and its field is left empty;
  // a key works only in its own region, so it needs one named.
  await p.connectProvider('bedrock', { AWS_REGION: 'us-east-1', AWS_PROFILE: '', AWS_BEARER_TOKEN_BEDROCK: 'k' });
  await assert.rejects(p.connectProvider('bedrock', { AWS_REGION: '', AWS_PROFILE: '', AWS_BEARER_TOKEN_BEDROCK: '' }), /Region is required with a Bedrock API key/);
  await p.connectProvider('bedrock', { AWS_REGION: 'us-west-2', AWS_PROFILE: '', AWS_BEARER_TOKEN_BEDROCK: '' });
  assert.equal(env.AGENT_PROVIDER, 'openai bedrock bedrock-openai');
  assert.equal(env.AWS_BEARER_TOKEN_BEDROCK, 'k');
  assert.equal(env.AWS_REGION, 'us-west-2');
  // Removing Bedrock removes both of its APIs, its key, and a default model on either.
  calls.length = 0;
  await p.removeProvider('bedrock');
  assert.deepEqual({ ...calls[0][1] }, { AGENT_PROVIDER: 'openai', AWS_BEARER_TOKEN_BEDROCK: null });
});

test('a provider that fails says why beside the ones that answered', async () => {
  const { p } = settingsShell({ env: { AGENT_PROVIDER: 'openai openrouter' }, lists: { openai: { models: [{ id: 'gpt' }] } } });
  await p.openSetup();
  assert.deepEqual({ ...p.S.setup.status.openrouter }, { error: 'provider_http_401', detail: 'no' });
  const html = p.setupHTML();
  assert.match(html, /✘ not ready/); assert.match(html, /provider_http_401: no</); assert.match(html, /✓ 1 model</); assert.match(html, /data-act="setup-retry"/);
  // Half of Bedrock failing keeps its row usable and names the half that failed.
  const half = settingsShell({ env: { AGENT_PROVIDER: 'bedrock bedrock-openai' }, lists: { bedrock: { models: [{ id: 'claude' }] } } });
  await half.p.openSetup();
  const row = half.p.setupHTML();
  assert.match(row, /✓ 1 model</); assert.match(row, /bedrock-openai: provider_http_401: no</); assert.match(row, /data-act="setup-retry"/);
});

test('Settings edits flat and council profiles independently', async () => {
  const edited = [], own = new Set();
  const p = shell({
    settings: async () => ({ providers: ['openai'], region: null, profile: null, keys: [] }),
    request: async () => ({ providers: { openai: { models: [{ id: 'gpt' }] } } }),
    models: async () => [{ id: 'openai/gpt' }],
    roles: async () => ['coordinator', 'swarm-flat', 'swarm-council'].map((name) => ({ name, file: own.has(name) ? `/home/u/.agents/agents/${name}.md` : null })),
    editRole: async (name) => { edited.push(name); own.add(name); return `/home/u/.agents/agents/${name}.md`; },
  });
  // Onboarding has no roles to show; Settings, once Home or a project exists, does.
  await p.openSetup();
  assert.doesNotMatch(p.setupHTML(), /Roles/);
  p.upsert({ name: 'home', bot_id: 2, provider: 'openai', model: 'gpt' });
  assert.match(p.setupHTML(), /<h3>Roles<\/h3>/);
  p.upsert({ name: 'app.lead', bot_id: 1, provider: 'openai', model: 'gpt' });
  let html = p.setupHTML();
  assert.match(html, /<h3>Roles<\/h3>/);
  assert.match(html, /Coordinator<\/span><span class="st dim">the app's own/);
  await p.act({ dataset: { act: 'edit-role', v: 'coordinator' } });
  assert.deepEqual(edited, ['coordinator']);
  html = p.setupHTML();
  assert.match(html, /Coordinator<\/span><span class="st">~\/.agents\/agents\/coordinator.md/);
  assert.match(html, /Flat swarm<\/span><span class="st dim">the app's own/);
  assert.match(html, /Council swarm<\/span><span class="st dim">the app's own/);
  await p.act({ dataset: { act: 'edit-role', v: 'swarm-flat' } });
  html = p.setupHTML();
  assert.match(html, /Flat swarm<\/span><span class="st">~\/.agents\/agents\/swarm-flat.md/);
  assert.match(html, /Council swarm<\/span><span class="st dim">the app's own/);
  await p.act({ dataset: { act: 'edit-role', v: 'swarm-council' } });
  assert.deepEqual(edited, ['coordinator', 'swarm-flat', 'swarm-council']);
  assert.match(p.setupHTML(), /Council swarm<\/span><span class="st">~\/.agents\/agents\/swarm-council.md/);
});

test('removing a provider drops its key unless another provider uses it', async () => {
  const { p, calls } = settingsShell({ env: { AGENT_PROVIDER: 'openai anthropic', OPENAI_API_KEY: 'k', ANTHROPIC_API_KEY: 'k' }, lists: { openai: { models: [{ id: 'gpt' }] }, anthropic: { models: [{ id: 'claude' }] } } });
  await p.openSetup();
  await p.removeProvider('anthropic');
  assert.deepEqual({ ...calls[0][1] }, { AGENT_PROVIDER: 'openai', ANTHROPIC_API_KEY: null });
});

test('removing the last provider hides a key the shell exports, so the next start detects nothing', async () => {
  const { p, calls } = settingsShell({ env: { AGENT_PROVIDER: 'bedrock bedrock-openai', AWS_REGION: 'us-west-2', OPENAI_API_KEY: 'shell' }, lists: { bedrock: { models: [{ id: 'claude' }] }, 'bedrock-openai': { models: [{ id: 'grok' }] } } });
  await p.openSetup();
  await p.removeProvider('bedrock');
  assert.deepEqual({ ...calls[0][1] }, { AGENT_PROVIDER: '', AWS_BEARER_TOKEN_BEDROCK: null, OPENAI_API_KEY: '' });
  // The daemon then has nothing to run, which is where removing the last provider should end.
  assert.equal(p.S.setup.error, null);
  assert.equal(calls.filter(([c]) => c === 'discover').length, 0);
  assert.match(p.setupHTML(), /Connect a provider first/);
});

test('a key a custom provider names stays when the catalog provider that shares it goes', async () => {
  const { p, calls } = settingsShell({ env: { AGENT_PROVIDER: 'openai corp=responses,https://corp.example/v1,OPENAI_API_KEY', OPENAI_API_KEY: 'k' }, lists: { openai: { models: [{ id: 'gpt' }] }, corp: { models: [{ id: 'm' }] } } });
  await p.openSetup();
  await p.removeProvider('openai');
  assert.deepEqual({ ...calls[0][1] }, { AGENT_PROVIDER: 'corp=responses,https://corp.example/v1,OPENAI_API_KEY' });
});

test('setup refresh keeps effort options tied to the current model selection', () => {
  for (const [remembered, model, effort, max] of [
    ['openai/gpt', 'anthropic/claude', 'max', true],
    ['anthropic/claude', 'openai/gpt', 'high', false],
  ]) {
    const p = page({}, new Map([['agent:model', remembered]]));
    p.S.config = { workspace: '/synthetic' };
    Object.assign(p.setupState(), { open: true, settings: { providers: ['openai', 'anthropic'] },
      list: [{ id: 'openai/gpt' }, { id: 'anthropic/claude' }] });
    const box = p.context.document.getElementById('setup');
    box.querySelectorAll = () => [{ id: 'setupmodel', value: model }, { id: 'setupeffort', value: effort }];
    p.renderSetup();
    const choices = /<select id="setupeffort"[^>]*>(.*?)<\/select>/.exec(box.innerHTML)[1];
    assert.equal(choices.includes('value="max"'), max);
    assert.ok(choices.includes(`value="${effort}" selected`), 'the current effort survives a refresh');
    assert.ok(box.innerHTML.includes(`value="${model}" selected`), 'the current model survives a refresh');
  }
});

test('a first launch with a provider but no agents opens setup on the first project', async () => {
  const { p } = settingsShell({ env: { AGENT_PROVIDER: 'openai', OPENAI_API_KEY: 'k' }, lists: { openai: { models: [{ id: 'gpt' }] } } });
  p.S.attached = false; p.S.setupSeen = false;
  await p.attach();
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(p.S.setup.open, true);
  assert.match(p.setupHTML(), /First project/);
});

test('the list offers only models of connected providers', async () => {
  const { p } = settingsShell({ env: { AGENT_PROVIDER: 'openai', OPENAI_API_KEY: 'k' }, lists: { openai: { models: [{ id: 'gpt' }] } } });
  p.context.Daemon.models = async () => [{ id: 'openai/gpt' }, { id: 'anthropic/claude' }];
  await p.openSetup();
  assert.deepEqual(p.S.setup.list.map((m) => m.id), ['openai/gpt']);
});

test('a window that cannot restart its daemon saves no provider change', async () => {
  const { p, calls } = settingsShell({ env: { AGENT_PROVIDER: 'openai', OPENAI_API_KEY: 'k' }, lists: { openai: { models: [{ id: 'gpt' }] } } });
  const settings = p.context.Daemon.settings;
  p.context.Daemon.settings = async () => ({ ...(await settings()), restartable: false });
  await p.openSetup();
  await assert.rejects(p.connectProvider('anthropic', { ANTHROPIC_API_KEY: 'k' }), /restart_unavailable/);
  await assert.rejects(p.removeProvider('openai'), /restart_unavailable/);
  assert.equal(calls.filter(([c]) => c === 'save').length, 0);
  assert.match(p.setupHTML(), /cannot apply provider changes/);
});

test('a daemon with no provider is not started again until settings change', async () => {
  let attaches = 0;
  const p = shell({ attach: async () => { attaches += 1; throw new Error('no_provider: connect a provider in Settings'); }, settings: async () => ({ providers: [], keys: [] }), models: async () => [] });
  p.S.attached = false;
  await p.attach(); await settle();
  await p.tick(); await p.tick();
  assert.equal(attaches, 1);
  assert.match(p.elements.get('detached').innerHTML, /waiting for a provider/);
});

test('Bedrock with a saved key switches to the AWS login when asked, and keeps the key otherwise', async () => {
  const { p, calls } = settingsShell({ env: { AGENT_PROVIDER: 'bedrock bedrock-openai', AWS_REGION: 'us-east-1', AWS_BEARER_TOKEN_BEDROCK: 'k' }, lists: { bedrock: { models: [{ id: 'claude' }] }, 'bedrock-openai': { models: [{ id: 'grok' }] } } });
  await p.openSetup();
  p.S.setup.adding = 'bedrock';
  assert.match(p.setupHTML(), /<option value="key" selected>Bedrock API key/);
  await p.connectProvider('bedrock', { AWS_REGION: 'us-east-1', AUTH: 'key', AWS_PROFILE: '', AWS_BEARER_TOKEN_BEDROCK: '' });
  assert.deepEqual({ ...calls.filter(([c]) => c === 'save')[0][1] }, { AGENT_PROVIDER: 'bedrock bedrock-openai', AWS_REGION: 'us-east-1', AWS_PROFILE: '' });
  await p.connectProvider('bedrock', { AWS_REGION: 'us-east-1', AUTH: 'aws', AWS_PROFILE: 'work', AWS_BEARER_TOKEN_BEDROCK: '' });
  assert.deepEqual({ ...calls.filter(([c]) => c === 'save')[1][1] }, { AGENT_PROVIDER: 'bedrock bedrock-openai', AWS_REGION: 'us-east-1', AWS_PROFILE: 'work', AWS_BEARER_TOKEN_BEDROCK: '' });
});

test('Bedrock signs in with any key the daemon would be given, the shell\'s too, until the AWS login empties it', async () => {
  const { p, env } = settingsShell({ env: { AGENT_PROVIDER: 'bedrock bedrock-openai', AWS_BEARER_TOKEN_BEDROCK: 'shell' }, lists: { bedrock: { models: [{ id: 'claude' }] } } });
  await p.openSetup();
  p.S.setup.adding = 'bedrock';
  assert.match(p.setupHTML(), /<option value="key" selected>Bedrock API key/);
  await p.connectProvider('bedrock', { AWS_REGION: '', AUTH: 'aws', AWS_PROFILE: '', AWS_BEARER_TOKEN_BEDROCK: '' });
  assert.equal(env.AWS_BEARER_TOKEN_BEDROCK, undefined);
  p.S.setup.adding = 'bedrock';
  assert.match(p.setupHTML(), /<option value="aws" selected>AWS login/);
});

test('a change another window saved is kept when this one connects a provider', async () => {
  const { p, calls, env } = settingsShell({ env: { AGENT_PROVIDER: 'openai', OPENAI_API_KEY: 'k' }, lists: { openai: { models: [{ id: 'gpt' }] } } });
  await p.openSetup();
  env.AGENT_PROVIDER = 'openai chatgpt';
  await p.connectProvider('anthropic', { ANTHROPIC_API_KEY: 'k' });
  assert.equal(calls.find(([c]) => c === 'save')[1].AGENT_PROVIDER, 'openai chatgpt anthropic');
});

test('a folder with a project file keeps its model when its coordinator is made again', async () => {
  const { p, calls } = settingsShell({ env: { AGENT_PROVIDER: 'openai', OPENAI_API_KEY: 'k' }, lists: { openai: { models: [{ id: 'gpt' }, { id: 'mini' }] } } });
  p.context.Daemon.project = async (dir) => ({ dir, name: 'weather', coordinator: 'weather.lead', model: 'openai/gpt', file: true });
  await p.createProject('/synthetic/weather', 'openai/mini');
  assert.deepEqual(calls.filter(([c]) => c === 'create' || c === 'write'), [['create', 'weather.lead', 'openai/gpt']]);
});

test('a connected provider can be edited in place', async () => {
  const { p } = settingsShell({ env: { AGENT_PROVIDER: 'bedrock bedrock-openai', AWS_REGION: 'us-west-2' }, lists: { bedrock: { models: [{ id: 'claude' }] }, 'bedrock-openai': { models: [{ id: 'grok' }] } } });
  await p.openSetup();
  assert.match(p.setupHTML(), /data-act="setup-pick" data-v="bedrock"[^>]*>Edit</);
  p.S.setup.adding = 'bedrock';
  assert.match(p.setupHTML(), /name="AWS_REGION"[^>]*value="us-west-2"/);
});

test('a gateway set up by hand under a known name is not offered the catalog form, which would overwrite it', async () => {
  const gateway = 'openai=responses,https://proxy.example/v1,PROXY_KEY';
  const { p, calls } = settingsShell({ env: { AGENT_PROVIDER: `${gateway} bedrock bedrock-openai`, AWS_BEARER_TOKEN_BEDROCK: 'k' }, lists: { openai: { models: [{ id: 'gpt' }] } } });
  await p.openSetup();
  const html = p.setupHTML();
  assert.doesNotMatch(html, /data-act="setup-pick" data-v="openai"/);
  assert.match(html, /data-act="setup-pick" data-v="bedrock"[^>]*>Edit</, 'a keyed Bedrock is what the form writes');
  await assert.rejects(p.connectProvider('openai', { OPENAI_API_KEY: 'k' }), /set up by hand/);
  assert.equal(calls.filter(([c]) => c === 'save').length, 0);
});

test('removing a provider asks nobody for models and rewrites no list', async () => {
  const { p, calls } = settingsShell({ env: { AGENT_PROVIDER: 'openai anthropic', OPENAI_API_KEY: 'k', ANTHROPIC_API_KEY: 'k' }, lists: { openai: { models: [{ id: 'gpt' }] }, anthropic: { models: [{ id: 'claude' }] } } });
  p.context.Daemon.models = async () => [{ id: 'openai/gpt' }, { id: 'openai/older-by-hand' }, { id: 'anthropic/claude' }];
  await p.openSetup();
  await p.removeProvider('anthropic');
  assert.equal(calls.filter(([c]) => c === 'discover').length, 0);
  assert.deepEqual(p.S.setup.list.map((m) => m.id), ['openai/gpt', 'openai/older-by-hand']);
});

test('the model chip offers only models of connected providers', async () => {
  const { p } = settingsShell({ env: { AGENT_PROVIDER: 'openai', OPENAI_API_KEY: 'k' }, lists: { openai: { models: [{ id: 'gpt' }] } } });
  p.context.Daemon.models = async () => [{ id: 'openai/gpt' }, { id: 'openrouter/gpt' }];
  p.upsert({ name: 'lead', bot_id: 1, provider: 'openai', family: 'openai', model: 'gpt' });
  p.S.selected = 'lead';
  await p.modelMenu('main', { x: 1, y: 1 });
  const menu = p.elements.get('menu').innerHTML;
  assert.match(menu, /OpenAI/); assert.doesNotMatch(menu, /OpenRouter|openrouter/);
});

test('a project starts on the model picked for it, and the pick is offered first next time', async () => {
  const storage = new Map();
  const { p, calls } = settingsShell({ env: { AGENT_PROVIDER: 'openai bedrock bedrock-openai', OPENAI_API_KEY: 'k' }, lists: { openai: { models: [{ id: 'gpt' }] }, bedrock: { models: [{ id: 'claude' }] }, 'bedrock-openai': { models: [{ id: 'grok' }] } } });
  p.context.localStorage = { getItem: (k) => storage.get(k) ?? null, setItem: (k, v) => storage.set(k, String(v)) };
  await assert.rejects(p.createProject('/synthetic/weather'), /model_required/);
  assert.equal(calls.filter(([c]) => c === 'create').length, 0);
  await p.createProject('/synthetic/weather', 'bedrock-openai/grok');
  assert.deepEqual(calls.filter(([c]) => c === 'create' || c === 'write'), [['create', 'weather.lead', 'bedrock-openai/grok'], ['write', 'bedrock-openai/grok']]);
  await p.openSetup();
  p.S.bots.clear();
  const html = p.setupHTML();
  assert.match(html, /<option value="bedrock-openai\/grok" selected>grok/); assert.doesNotMatch(html, /Choose a model/);
});

test('the model chip names each provider above its models', () => {
  const p = shell();
  p.upsert({ name: 'lead', bot_id: 1, provider: 'bedrock', family: 'anthropic', model: 'claude' });
  const items = p.modelMenuItems(p.S.bots.get('lead'), [{ id: 'bedrock/claude' }, { id: 'openai/gpt' }]);
  assert.deepEqual(Array.from(items, (i) => i.head ?? (i.sep ? '—' : i.label)), ['Effort', 'default', 'low', 'medium', 'high', 'xhigh', 'max', '—', 'Amazon Bedrock', 'claude', '—', 'OpenAI', 'gpt']);
});

test('a daemon with no provider to run opens setup instead of an error', async () => {
  const p = shell({ attach: async () => { throw new Error('no_provider: connect a provider in Settings'); }, settings: async () => ({ providers: [], model: null, keys: [] }), models: async () => [] });
  p.S.attached = false;
  await p.attach();
  assert.equal(p.S.setup.open, true);
  assert.equal(p.S.setup.adding, '');
  assert.match(p.setupHTML(), /Set up Agent/); assert.match(p.setupHTML(), /data-v="bedrock">Amazon Bedrock</); assert.doesNotMatch(p.setupHTML(), /bedrock-openai/);
});

test('a refused model list says why and can be asked again; a key already set answers for an empty field', async () => {
  const { p, calls, env } = settingsShell({ env: { AGENT_PROVIDER: 'openai' }, lists: { openai: { models: [] } } });
  p.context.Daemon.discoverModels = async () => ({ providers: { openai: { models: 0 } }, written: false, error: 'models_none_listed: openai: no usable models' });
  await p.openSetup();
  await p.refreshModels();
  const html = p.setupHTML();
  assert.match(html, /models_none_listed: openai: no usable models/);
  assert.match(html, /data-act="setup-refresh"/); assert.match(html, /No models listed yet/);
  // OPENAI_API_KEY from the shell: re-adding OpenAI with the field left empty keeps using it.
  env.OPENAI_API_KEY = 'shell';
  await p.connectProvider('openai', { OPENAI_API_KEY: '' });
  assert.deepEqual({ ...calls.find(([c]) => c === 'save')[1] }, { AGENT_PROVIDER: 'openai' });
  await assert.rejects(p.connectProvider('anthropic', { ANTHROPIC_API_KEY: '' }), /API key is required/);
});

test('removing a provider while agents work asks once more before the restart stops them', async () => {
  const { p, calls } = settingsShell({ env: { AGENT_PROVIDER: 'openai anthropic', ANTHROPIC_API_KEY: 'k' }, lists: { openai: { models: [{ id: 'gpt' }] }, anthropic: { models: [{ id: 'claude' }] } } });
  await p.openSetup();
  p.upsert({ name: 'busy', bot_id: 1, provider: 'openai', model: 'gpt', status: 'running', running_turn: 3 }); p.S.botsGen += 1;
  const press = () => p.act({ dataset: { act: 'setup-remove', v: 'anthropic' } });
  await press();
  assert.equal(calls.filter(([c]) => c === 'save').length, 0);
  assert.match(p.setupHTML(), /Remove anyway/); assert.match(p.setupHTML(), /Removing restarts the daemon/);
  await press();
  assert.equal(calls.filter(([c]) => c === 'save').length, 1);
});

test('a swarm counts its helpers\' tokens, and the board says when it passes a share of its budget', async () => {
  const bots = [
    { name: 'app.latency-1', bot_id: 3, tokens_used: 1000 },
    { name: 'app.latency-1.fix', bot_id: 40, created_by_id: 3, tokens_used: 200 },
    { name: 'app.latency-1.fix.deep', bot_id: 41, created_by_id: 40, tokens_used: 30 },
    { name: 'app.latency-1.stray', bot_id: 42, created_by_id: 77, tokens_used: 5000 },
    { name: 'app.latency-2', bot_id: 99, tokens_used: 7000 },
    { name: 'app.other', bot_id: 50, tokens_used: 9000 },
  ];
  const p = shell({ request: async (op) => (op === 'bots' ? { bots, next_after: null } : { bots: [], next_after: null }) });
  const sw = p.learnSwarm(swarmRecord(['app.latency-1', 'app.latency-2'], { ids: { 'app.latency-1': 3, 'app.latency-2': 4 } }));
  await p.readUsage(sw);
  assert.equal(sw.used, 1230);
  // A member that left still makes its helpers count; a helper deleted since the board saw it
  // counts with what the board recorded, and helpers gone before that with the board's total.
  bots.push({ name: 'app.latency-3.fix', bot_id: 43, created_by_id: 8, tokens_used: 400 });
  bots.sort((a, b) => (a.name < b.name ? -1 : 1));
  sw.left = [8];
  sw.state = { ...sw.state, helpers: { 40: 150, 44: 60 }, gone: 500 };
  await p.readUsage(sw);
  assert.equal(sw.used, 1230 + 400 + 60 + 500);
  assert.match(p.postHTML(sw, { from: 'budget', text: 'the swarm has used 50% of its budget (1.5M of 3M tokens)', spent: 50 }), /<span class="who council">budget<\/span><span class="pt">the swarm has used 50%/);
  // fix is deleted: deep, made by it, still counts through what the board knows of fix.
  bots.splice(bots.findIndex((b) => b.bot_id === 40), 1);
  sw.state = { ...sw.state, helpers: { 41: 30 }, gone: 0, roots: { 40: 3, 41: 3 } };
  await p.readUsage(sw);
  assert.equal(sw.used, 1000 + 30 + 400);
});

test('a swarm\'s departures apply in order, and a stream that changed hands shows its new lead', async () => {
  const answers = [deferred(), deferred()], calls = [];
  const p = shell({
    swarmLeave: (swarm, member) => { calls.push(member); return answers[calls.length - 1].promise; },
    swarmBoard: async () => ({ lines: [], offset: 0, more: false, reset: true }),
    request: async () => ({ bots: [], next_after: null }),
  });
  for (const [n, id] of [['app.latency-1', 3], ['app.latency-2', 4], ['app.latency-3', 5]]) p.upsert({ name: n, bot_id: id, provider: 'alpha', model: 'one' });
  const sw = p.learnSwarm(swarmRecord(['app.latency-1', 'app.latency-2', 'app.latency-3']));
  await p.onEvent({ event: 'deleted', bot: 'app.latency-1', durable: true });
  await p.onEvent({ event: 'deleted', bot: 'app.latency-2', durable: true });
  await p.tick();
  // The second waits for the first's answer, so the first's never lands after it.
  assert.deepEqual(calls, ['app.latency-1']);
  answers[0].resolve(swarmRecord(['app.latency-2', 'app.latency-3']));
  await p.tick();
  assert.deepEqual(calls, ['app.latency-1', 'app.latency-2']);
  answers[1].resolve(swarmRecord(['app.latency-3']));
  await p.tick();
  assert.deepEqual(sw.members, ['app.latency-3']);
  const lead = p.postHTML(sw, { from: 'council', kind: 'lead', stream: 'cache', lead: 'latency-3', was: 'latency-1' });
  assert.match(lead, /latency-1 left · latency-3 leads it/);
  assert.match(p.postHTML(sw, { from: 'council', kind: 'seat', seat: 'latency-3', was: 'latency-1' }), /latency-1 left · latency-3 holds a council seat/);
});

test('an older daemon on the socket is replaced from the detached screen; a newer one is left to an app update', async () => {
  let attaches = 0, replaced = 0; const stopping = deferred();
  const p = page({ setup: async () => ({}), attach: async () => { attaches++; throw new Error('daemon_older: the daemon speaks protocol 3, this app 4'); }, replaceDaemon: () => { replaced++; return stopping.promise; }, pull: () => new Promise(() => {}), request: async () => ({ bots: [] }) });
  p.lost('daemon_older: the daemon speaks protocol 3, this app 4');
  const screen = p.context.document.getElementById('detached');
  assert.match(screen.innerHTML, /A daemon from before this update is still running/);
  assert.match(screen.innerHTML, /data-act="replace-daemon"/);
  const button = { dataset: { act: 'replace-daemon' }, disabled: false, textContent: '' };
  const pressed = p.act(button);
  // No reattach runs while the old daemon closes.
  await p.tick(); assert.equal(attaches, 0); assert.equal(button.textContent, 'Restarting…');
  stopping.resolve(); await pressed; await settle();
  assert.equal(replaced, 1); assert.equal(attaches, 1);
  // A newer daemon is not attached to again and again: only a newer app helps.
  await p.tick();
  const before = attaches;
  p.lost('daemon_newer: the daemon speaks protocol 5, this app 4; update the app');
  assert.doesNotMatch(screen.innerHTML, /replace-daemon/);
  assert.match(screen.innerHTML, /newer than this app: update the app/);
  assert.match(screen.innerHTML, /· stopped/);
  await p.tick(); await p.tick();
  assert.equal(attaches, before);
  // A window given a socket did not start that daemon, so it offers no restart.
  p.S.config = { managed: false, socket: '/synthetic/agent.sock' };
  p.lost('daemon_older: the daemon speaks protocol 3, this app 4');
  assert.doesNotMatch(screen.innerHTML, /replace-daemon/);
  assert.match(screen.innerHTML, /this window did not start it: stop it with its own agent \(agent shutdown\), then start one from this update's agent/);
  // A mismatch the app could not age names no older or newer daemon, so nothing is restarted and retrying goes on.
  p.S.config = { managed: true };
  p.lost('daemon_protocol_mismatch: the daemon speaks protocol "4", this client 4');
  assert.doesNotMatch(screen.innerHTML, /replace-daemon|update the app/);
  assert.match(screen.innerHTML, /· retrying/);
});

test('a message another agent sent names its sender, live, steered in, and read back from history', async () => {
  const items = { 1: { role: 'user', content: [{ type: 'input_text', text: 'fix the login bug' }] }, 2: { role: 'user', content: [{ type: 'input_text', text: 'also check the refresh path' }] },
    3: { role: 'user', content: [{ type: 'input_text', text: 'what changed?' }] }, 4: { role: 'user', content: [{ type: 'input_text', text: 'then the docs' }] } };
  // The daemon names who sent each prompt with its item: the bot, its turn, and the identity its name held.
  const lead = (turn) => ({ from: { bot: 'demo.lead', turn, bot_id: 1 } });
  const sent = { 1: lead(4), 2: lead(5), 4: lead(6) };
  const batch = async ({ nodes }) => ({ items: nodes.map((node) => ({ node, item: items[node], ...sent[node] })) });
  const history = async (op) => {
    if (op === 'history_nodes') return { nodes: [{ node: 4, turn: 10 }, { node: 3, turn: 9 }, { node: 2, turn: 7 }, { node: 1, turn: 7 }], workspaces:[],next_from: null, next_newer: null };
    throw new Error(op);
  };
  const p = page({ request: history, batch });
  p.upsert({ name: 'demo.lead', bot_id: 1 }); p.upsert({ name: 'demo.build', bot_id: 2, created_by: 'demo.lead', created_by_id: 1 }); p.tree();
  // The coordinator starts a task, then steers a second message into its running turn; you ask
  // something; a third message from it waits in line and starts later. Each event that puts a
  // prompt on the lineage names its sender, in the order the store writes them.
  await p.onEvent({ event: 'accepted', bot: 'demo.build', turn: 7, data: { node: 1, ...lead(4) } });
  await p.onEvent({ event: 'queued', bot: 'demo.build', turn: 8, data: { delivery: 'steer', ...lead(5) } });
  // The store finishes the steer's own turn first, then its message joins the running turn.
  await p.onEvent({ event: 'turn_finished', bot: 'demo.build', turn: 8, data: { status: 'steered', into: 7, node: 2 } });
  await p.onEvent({ event: 'steered', bot: 'demo.build', turn: 7, data: { steer: 8, node: 2, ...lead(5) } });
  await p.onEvent({ event: 'accepted', bot: 'demo.build', turn: 9, data: { node: 3 } });
  await p.onEvent({ event: 'queued', bot: 'demo.build', turn: 10, data: { delivery: 'queue', ...lead(6) } });
  await p.onEvent({ event: 'turn_finished', bot: 'demo.build', turn: 9, data: { status: 'completed' } });
  await p.onEvent({ event: 'accepted', bot: 'demo.build', turn: 10, data: { node: 4, ...lead(6) } });
  const t = p.transcript('demo.build');
  // Live, before any history read: each prompt already knows its sender.
  assert.equal(JSON.stringify([1, 2, 4].map((node) => t.items.find((it) => it.node === node)?.by)), JSON.stringify([4, 5, 6].map((turn) => ({ bot: 'demo.lead', turn, id: 1 }))));
  await p.loadBatch('demo.build');
  const html = p.itemsHTML(t);
  assert.equal((html.match(/class="line user agent"><button type="button" class="by" data-task="demo.lead"[^>]*>coordinator<\/button> /g) || []).length, 3, html);
  assert.match(html, /coordinator<\/button> also check the refresh path/);
  assert.match(html, /coordinator<\/button> then the docs/);
  assert.match(html, /<div class="line user">› what changed\?<\/div>/, 'yours keep their mark');
  // Read back from the daemon's history, as after a restart.
  const q = page({ request: history, batch });
  q.upsert({ name: 'demo.lead', bot_id: 1 }); q.tree();
  const r = q.transcript('demo.build'); r.items = [{ kind: 'history', next: 4, seed: true }]; r.history = r.items[0]; r.seed = r.items[0];
  await q.load('demo.build');
  const read = q.itemsHTML(r);
  assert.equal((read.match(/class="by" data-task="demo.lead"/g) || []).length, 3, read);
  assert.match(read, /› what changed\?/);
  // A new agent under the sender's name is not who sent them: the tag stays, with no link to it.
  const u = page({ request: history, batch });
  u.upsert({ name: 'demo.lead', bot_id: 5 }); u.tree();
  const v = u.transcript('demo.build'); v.items = [{ kind: 'history', next: 4, seed: true }]; v.history = v.items[0]; v.seed = v.items[0];
  await u.load('demo.build');
  const reused = u.itemsHTML(v);
  assert.doesNotMatch(reused, /data-task="demo.lead"/);
  assert.equal((reused.match(/<span class="by" title="Sent by demo.lead, turn \d, since deleted">coordinator<\/span>/g) || []).length, 3, reused);
});

test('the app\'s own task updates and triggered messages are tagged by the origin it sent them with', async () => {
  const items = { 1: { role: 'user', content: [{ type: 'input_text', text: 'Task updates: build ended' }] }, 2: { role: 'user', content: [{ type: 'input_text', text: 'check the nightly run' }] },
    3: { role: 'user', content: [{ type: 'input_text', text: 'thanks' }] } };
  const origins = { 1: { origin: 'tasks' }, 2: { origin: 'trigger' } };
  const p = page({ request: async (op) => {
    if (op === 'history_nodes') return { nodes: [{ node: 3, turn: 3 }, { node: 2, turn: 2 }, { node: 1, turn: 1 }], workspaces:[],next_from: null, next_newer: null };
    throw new Error(op);
  }, batch: async ({ nodes }) => ({ items: nodes.map((node) => ({ node, item: items[node], ...origins[node] })) }) });
  const t = p.transcript('demo.lead'); t.items = [{ kind: 'history', next: 3, seed: true }]; t.history = t.items[0]; t.seed = t.items[0];
  await p.load('demo.lead');
  let html = p.itemsHTML(t);
  assert.match(html, /<span class="by">tasks<\/span> Task updates: build ended/);
  assert.match(html, /<span class="by">trigger<\/span> check the nightly run/);
  assert.match(html, /<div class="line user">› thanks<\/div>/);
  // Live, the origin comes with the turn's start.
  const q = page({ request: async (op, r) => items[r.node] });
  await q.onEvent({ event: 'accepted', bot: 'demo.lead', turn: 2, data: { node: 2, origin: 'trigger' } });
  await q.loadBatch('demo.lead');
  assert.match(q.itemsHTML(q.transcript('demo.lead')), /<span class="by">trigger<\/span> check the nightly run/);
});

test('a coordinator hears once, when it rests, of turns its tasks ended that it did not ask for', async () => {
  const sent = [];
  const p = page({ request: async (op, params) => { if (op === 'submit') sent.push(params); return { turn: 9 }; }, log() {} });
  p.S.live = true; p.S.attached = true;
  p.upsert({ name: 'demo.lead', bot_id: 1, status: 'idle' });
  for (const [name, id] of [['demo.build', 2], ['demo.test', 3], ['demo.lead-side', 4]]) p.upsert({ name, bot_id: id, status: 'idle', created_by: 'demo.lead', created_by_id: 1 });
  p.upsert({ name: 'demo.build.helper', bot_id: 5, status: 'idle', created_by: 'demo.build', created_by_id: 2 });
  const turn = async (bot, n, status = 'completed', from = null) => {
    await p.onEvent({ event: 'accepted', bot, turn: n, data: { node: 1, ...(from ? { from: { bot: from, turn: 1 } } : {}) } });
    await p.onEvent({ event: 'turn_finished', bot, turn: n, data: { status } });
  };
  // The coordinator's own ask that it is waiting on: its wait reads the reply.
  p.S.bots.get('demo.lead').waitingOn = ['turn:demo.build/1'];
  await turn('demo.build', 1, 'completed', 'demo.lead');
  p.S.bots.get('demo.lead').waitingOn = [];
  await turn('demo.lead-side', 1); // a side chat of the coordinator
  await turn('demo.build.helper', 1); // not the coordinator's task
  await p.tick();
  assert.equal(sent.length, 0);
  // You, working in two tasks; one turn was a steer that joined another.
  await turn('demo.build', 2);
  await turn('demo.build', 3, 'steered');
  await turn('demo.test', 1, 'failed');
  await turn('demo.build', 4, 'completed', 'demo.test');
  assert.equal(p.S.turnFrom.size, 0, 'authors are forgotten as turns end');
  await p.tick();
  assert.equal(sent.length, 1, 'one message for the whole batch');
  assert.equal(sent[0].bot, 'demo.lead'); assert.equal(sent[0].bot_id, 1); assert.equal(sent[0].delivery, 'queue'); assert.equal(sent[0].origin, 'tasks');
  assert.match(sent[0].prompt, /^Task updates: /);
  // What another bot asked of a task is the coordinator's; what you asked is listed last, as yours,
  // each by its own handle.
  assert.match(sent[0].prompt, /\n- demo\.build: turn:demo\.build\/4 completed, asked by demo\.test\nThe person asked for these turns in the task themselves, so they are theirs:\n- demo\.build: turn:demo\.build\/2 completed, asked by the person\n- demo\.test: turn:demo\.test\/1 failed, asked by the person$/);
  // Within the window, while it works: nothing until its turn ends and the window allows. An ask of its
  // own that it did not wait for is news too.
  p.S.bots.get('demo.lead').status = 'running';
  await turn('demo.test', 2, 'completed', 'demo.lead');
  await p.tick();
  assert.equal(sent.length, 1);
  await p.onEvent({ event: 'turn_finished', bot: 'demo.lead', turn: 7, data: { status: 'completed' } });
  await p.tick();
  assert.equal(sent.length, 2);
  assert.match(sent[1].prompt, /\n- demo\.test: turn:demo\.test\/2 completed, asked by you$/);
  // Nothing new: nothing sent.
  await p.tick();
  assert.equal(sent.length, 2);
  // An approval only the person can give is news, naming no asker.
  await p.onEvent({ event: 'turn_waiting', bot: 'demo.build', turn: 5, data: { call_id: 'c1', approval: true } });
  await p.tick();
  assert.equal(sent.length, 3);
  assert.match(sent[2].prompt, /\n- demo\.build: turn:demo\.build\/5 waiting for approval$/);
  // A coordinator's ask between two of yours keeps its own handle, in the list to act on.
  p.S.bots.get('demo.lead').status = 'running';
  await turn('demo.test', 3);
  await turn('demo.test', 4, 'completed', 'demo.lead');
  await turn('demo.test', 5);
  await p.onEvent({ event: 'turn_finished', bot: 'demo.lead', turn: 8, data: { status: 'completed' } });
  await p.tick();
  assert.equal(sent.length, 4);
  assert.match(sent[3].prompt, /\n- demo\.test: turn:demo\.test\/4 completed, asked by you\nThe person asked for these turns in the task themselves, so they are theirs:\n- demo\.test: turn:demo\.test\/5 completed, asked by the person, and 1 earlier since turn:demo\.test\/3$/);
  // A deleted coordinator hears nothing more.
  await p.onEvent({ event: 'deleted', bot: 'demo.lead' });
  await turn('demo.test', 6);
  await p.tick();
  assert.equal(sent.length, 4);
});

test('a coordinator wake that fails is kept for the next one, and replayed turns are not news', async () => {
  let fail = true; const sent = [];
  const p = page({ request: async (op, params) => { if (op === 'submit') { if (fail) throw new Error('daemon_draining'); sent.push(params); } return {}; }, log() {} });
  p.S.attached = true;
  p.upsert({ name: 'demo.lead', bot_id: 1, status: 'idle' });
  p.upsert({ name: 'demo.build', bot_id: 2, status: 'idle', created_by: 'demo.lead', created_by_id: 1 });
  await p.onEvent({ event: 'turn_finished', bot: 'demo.build', turn: 1, data: { status: 'completed' } });
  await p.tick();
  assert.equal(p.S.wakes.size, 0, 'the replay is history, not news');
  p.S.live = true;
  await p.onEvent({ event: 'turn_finished', bot: 'demo.build', turn: 2, data: { status: 'completed' } });
  await p.tick();
  assert.equal(sent.length, 0);
  fail = false;
  await p.onEvent({ event: 'turn_finished', bot: 'demo.build', turn: 3, data: { status: 'completed' } });
  await p.tick();
  assert.equal(sent.length, 1);
  assert.match(sent[0].prompt, /turn:demo\.build\/3 completed, asked by the person, and 1 earlier since turn:demo\.build\/2$/);
});

test('two windows telling a coordinator the same news make one turn, and a bot gone while detached leaves nothing behind', async () => {
  // One window attached earlier and also saw turn 2; both have turn 3 as the newest.
  const told = new Map(), asked = [];
  const submit = async (op, params) => {
    if (op !== 'submit') return {};
    asked.push(params.request_id);
    if (told.has(params.request_id) && told.get(params.request_id) !== params.prompt) throw new Error('idempotency_conflict: ');
    told.set(params.request_id, params.prompt); return {};
  };
  const pages = [0, 1].map(() => page({ request: submit, log() {} }));
  for (const [i, p] of pages.entries()) {
    p.S.live = true; p.S.attached = true;
    p.upsert({ name: 'demo.lead', bot_id: 1, status: 'running' });
    p.upsert({ name: 'demo.build', bot_id: 2, status: 'idle', created_by: 'demo.lead', created_by_id: 1 });
    for (const turn of i === 0 ? [2, 3] : [3]) await p.onEvent({ event: 'turn_finished', bot: 'demo.build', turn, data: { status: 'completed' } });
  }
  for (const p of pages) { await p.onEvent({ event: 'turn_finished', bot: 'demo.lead', turn: 5, data: { status: 'completed' } }); await p.tick(); }
  assert.equal(asked.length, 2);
  assert.equal(asked[0], asked[1]);
  assert.match(asked[0], /^app-wake-1-[0-9a-f]{16}$/);
  assert.equal(told.size, 1, 'one turn');
  assert.equal(pages[1].S.wakes.get('demo.lead').tasks.size, 0, 'the refused window does not try again');
  const [p] = pages;
  // A task deleted before its news goes out is no news.
  p.upsert({ name: 'demo.test', bot_id: 3, status: 'idle', created_by: 'demo.lead', created_by_id: 1 });
  p.S.bots.get('demo.lead').status = 'running';
  await p.onEvent({ event: 'turn_finished', bot: 'demo.test', turn: 1, data: { status: 'completed' } });
  assert.equal(p.S.wakes.get('demo.lead').tasks.size, 1);
  p.forgetBot('demo.test');
  assert.equal(p.S.wakes.get('demo.lead').tasks.size, 0);
  await p.onEvent({ event: 'queued', bot: 'demo.lead', turn: 9, data: { from: { bot: 'demo.build', turn: 3 } } });
  await p.onEvent({ event: 'turn_finished', bot: 'demo.build', turn: 4, data: { status: 'completed' } });
  assert.equal(p.S.turnFrom.size, 1); assert.ok(p.S.wakes.has('demo.lead'));
  p.forgetBot('demo.lead');
  assert.equal(p.S.turnFrom.size, 0); assert.equal(p.S.wakes.size, 0);
});

test('a window that saw news to act on never defers to one that saw only yours', async () => {
  const told = new Map();
  const submit = async (op, params) => {
    if (op !== 'submit') return {};
    if (told.has(params.request_id) && told.get(params.request_id) !== params.prompt) throw new Error('idempotency_conflict: ');
    told.set(params.request_id, params.prompt); return {};
  };
  const pages = [0, 1].map(() => page({ request: submit, log() {} }));
  for (const [i, p] of pages.entries()) {
    p.S.live = true; p.S.attached = true;
    p.upsert({ name: 'demo.lead', bot_id: 1, status: 'running' });
    p.upsert({ name: 'demo.build', bot_id: 2, status: 'idle', created_by: 'demo.lead', created_by_id: 1 });
    // Only the first window saw the coordinator's ask; both see yours after it.
    if (i === 0) {
      await p.onEvent({ event: 'accepted', bot: 'demo.build', turn: 2, data: { node: 1, from: { bot: 'demo.lead', turn: 1 } } });
      await p.onEvent({ event: 'turn_finished', bot: 'demo.build', turn: 2, data: { status: 'completed' } });
    }
    await p.onEvent({ event: 'turn_finished', bot: 'demo.build', turn: 3, data: { status: 'completed' } });
    await p.onEvent({ event: 'turn_finished', bot: 'demo.lead', turn: 5, data: { status: 'completed' } });
    await p.tick();
  }
  assert.equal(told.size, 2, 'different news, different turns');
  assert.ok([...told.values()].some((prompt) => /\n- demo\.build: turn:demo\.build\/2 completed, asked by you\n/.test(prompt)));
});

test('an approval answered before the coordinator hears of it is not raised, and a trigger\'s turn is the coordinator\'s', async () => {
  const sent = [];
  const p = page({ request: async (op, params) => { if (op === 'submit') sent.push(params); return {}; }, log() {} });
  p.S.live = true; p.S.attached = true;
  p.upsert({ name: 'demo.lead', bot_id: 1, status: 'running' });
  for (const [name, id] of [['demo.build', 2], ['demo.nightly', 3]]) p.upsert({ name, bot_id: id, status: 'idle', created_by: 'demo.lead', created_by_id: 1 });
  // Your turn waits for your approval, which you give before the coordinator rests.
  await p.onEvent({ event: 'accepted', bot: 'demo.build', turn: 1, data: { node: 1 } });
  await p.onEvent({ event: 'turn_waiting', bot: 'demo.build', turn: 1, data: { call_id: 'c1', approval: true } });
  await p.onEvent({ event: 'turn_finished', bot: 'demo.build', turn: 1, data: { status: 'completed' } });
  // A trigger asks a task for a check.
  await p.onEvent({ event: 'accepted', bot: 'demo.nightly', turn: 4, data: { node: 2, origin: 'trigger' } });
  await p.onEvent({ event: 'turn_finished', bot: 'demo.nightly', turn: 4, data: { status: 'completed' } });
  await p.onEvent({ event: 'turn_finished', bot: 'demo.lead', turn: 2, data: { status: 'completed' } });
  await p.tick();
  assert.equal(sent.length, 1);
  assert.doesNotMatch(sent[0].prompt, /waiting for approval/);
  assert.doesNotMatch(sent[0].prompt, /theirs/);
  assert.match(sent[0].prompt, /\n- demo\.build: turn:demo\.build\/1 completed, asked by the person, and 1 earlier since turn:demo\.build\/1\n- demo\.nightly: turn:demo\.nightly\/4 completed, asked by trigger$/);
  assert.equal(p.S.turnOrigin.size, 0, 'origins are forgotten as turns end');
});

test('approval calls and completion in one turn each reach the coordinator once across windows', async () => {
  const delivered = new Map(), attempts = [];
  const pages = [0, 1].map(() => page({ request: async (op, params) => {
    if (op !== 'submit') return {};
    attempts.push(params);
    if (delivered.has(params.request_id) && delivered.get(params.request_id) !== params.prompt) throw new Error('idempotency_conflict: ');
    delivered.set(params.request_id, params.prompt); return {};
  }, log() {} }));
  for (const p of pages) {
    p.setRender(() => {}); p.S.live = true; p.S.attached = true;
    p.upsert({ name: 'demo.lead', bot_id: 1, status: 'idle' });
    p.upsert({ name: 'demo.build', bot_id: 2, status: 'running', created_by: 'demo.lead', created_by_id: 1 });
    await p.onEvent({ event: 'accepted', bot: 'demo.build', turn: 1, data: { node: 1 } });
  }
  for (const [i, call] of ['c1', 'c2', null].entries()) {
    for (const [j, p] of pages.entries()) {
      // One window holds the first approval until its snapshot completes.
      p.S.snapshot = i === 0 && j === 1;
      await p.onEvent({ event: call ? 'turn_waiting' : 'turn_finished', bot: 'demo.build', turn: 1,
        data: call ? { approval: true, call_id: call } : { status: 'completed' } });
      if (p.S.snapshot) {
        p.S.snapshot = false;
        vm.runInContext('for (const news of app.S.heldNews.splice(0)) app.tellLead(...news)', p.context);
      }
      await p.tick();
    }
    assert.equal(attempts.at(-1).request_id, attempts.at(-2).request_id, 'windows deduplicate the same update');
    assert.equal(delivered.size, i + 1, 'each approval call and the completion is distinct');
  }
  assert.match([...delivered.values()].at(-1), /completed, asked by the person$/);
});

test('Home lists the triggers under its projects, each with its kind, its time and who it wakes', async () => {
  const p = page({ triggers: async () => ({ triggers: [
    { name: 'loose', bot: 'loose', bot_id: 3, when: 'in 2h', once: true, ended: true, message: 'x', last: { outcome: 'failed', fired_ms: 0, detail: 'daemon_unavailable' } },
    { name: 'late', bot: 'late', bot_id: 4, when: 'at 2026-09-01 09:00', once: true, ended: false, missed: true, message: 'y', last: null },
    { name: 'odd', ended: false, problem: 'unreadable: not a trigger\'s plist' },
    { name: 'review', bot: 'demo.review', bot_id: null, start: { model: 'a/m', effort: null }, reply_to: 'demo.lead', if: 'git diff --quiet', runs: 3, sent: 1, when: 'commit /Users/you/demo', once: false, ended: false, message: 'z', last: null },
    { name: 'pr', bot: 'demo.build', bot_id: 2, when: 'every 30m', message: 'w', last: null },
    { name: 'after', bot: 'demo.lead', bot_id: 1, when: 'every 3 turns of demo.build', message: 'v', last: null },
  ], next_after: 'review' }) });
  p.S.attached = true;
  await p.readTriggers();
  const html = p.elements.get('trigs').innerHTML;
  assert.equal(p.elements.get('trigs').hidden, false);
  assert.match(html, /<span class="tk">⏱<\/span><span class="n">loose<\/span><span class="w">ended<\/span>/);
  assert.match(html, /trow bad" data-act="trigger" data-v="late">.*missed/);
  assert.match(html, /data-v="odd">.*unreadable/);
  assert.match(html, /<span class="tk">⎇<\/span><span class="n">review<\/span><span class="w">commit demo<\/span><\/span><span class="r2">→ demo\.review · answer to demo\.lead/);
  assert.match(html, /<span class="tk">↻<\/span><span class="n">after<\/span><span class="w">every 3 turns<\/span>/);
  assert.match(html, /data-act="triggers-next"/, 'a page with more after it says so');
  // An agent's project opened in the list hides them, and a window on a host has none.
  p.S.selected = 'demo.lead'; p.S.bots.set('demo.lead', { name: 'demo.lead', id: 1 });
  p.renderTriggers();
  assert.equal(p.elements.get('trigs').hidden, true);
});

test('a trigger\'s sheet shows when, what and where its answer goes, and Run now looks again until the fire has written', async () => {
  let fired = null, reads = 0, fires = 0;
  const row = () => ({ name: 'r', bot: 'demo.build', bot_id: 2, when: 'every 30m', if: 'test -s queue', reply_to: 'demo.lead', runs: 5, sent: fired ? 2 : 1, message: 'check the queue', last: fired ? { outcome: 'sent', fired_ms: fired, turn: 7 } : { outcome: 'declined', fired_ms: 0, detail: '--if: exit status: 1', reply: null } });
  const p = page({ fireTrigger: async () => { fires++; return { fired: true }; }, trigger: async () => { reads++; return row(); }, triggers: async () => ({ triggers: [row()] }) });
  p.S.attached = true;
  p.S.bots.set('demo.build', { name: 'demo.build', id: 2 });
  await p.openTriggerSheet('r');
  let html = p.elements.get('sheet').innerHTML;
  assert.match(html, /<span class="lab">When<\/span>Every 30m<\/div>.*<span class="lab">Do<\/span>Message <b>demo\.build<\/b>.*<span class="lab">Reply to<\/span><b>demo\.lead<\/b>/s);
  assert.match(html, /checks first<\/dt><dd>test -s queue/);
  assert.match(html, /not sent, its check said no \(--if: exit status: 1\)/);
  assert.match(html, /<dt>sent<\/dt><dd>1 of 5, then it ends/);
  assert.match(html, /~\/\.agent\/trigger fire r</);
  assert.match(html, /data-act="trigger-open" title="Open its chat">Open demo\.build/);
  const run = p.act({ dataset: { act: 'trigger-fire' } });
  await settle();
  assert.equal(fires, 1);
  assert.equal(reads, 1, 'read once as the sheet opened');
  await p.tick();
  assert.equal(reads, 2, 'and again while the fire has not written');
  fired = 5;
  await p.tick();
  assert.equal(reads, 3);
  await p.tick();
  await run;
  assert.equal(reads, 3, 'its result ends the looking');
  html = p.elements.get('sheet').innerHTML;
  assert.match(html, /: sent, turn 7/);
});

test('a trigger that starts its agent, one gone, and one past the first page each show in a sheet', async () => {
  const rows = { s: { name: 's', bot: 'p.review', bot_id: null, start: { model: 'a/m', effort: 'high' }, when: 'fire', message: 'm', last: null },
    gone: { name: 'gone', bot: 'p.old', bot_id: 9, when: 'file /tmp/x.csv', message: 'm', last: null } };
  const p = page({ trigger: async (name) => rows[name] ?? null });
  await p.openTriggerSheet('s');
  let html = p.elements.get('sheet').innerHTML;
  assert.match(html, /Only when run.*Start <b>p\.review<\/b> on a\/m at high.*Stays in its chat/s);
  assert.match(html, /data-act="trigger-open" disabled title="Its first fire starts it"/);
  await p.openTriggerSheet('gone');
  html = p.elements.get('sheet').innerHTML;
  assert.match(html, /When \/tmp\/x\.csv is written/);
  assert.match(html, /disabled title="Its agent is gone"/);
  await p.openTriggerSheet('nope');
  assert.match(p.elements.get('sheet').innerHTML, /No trigger by this name now/);
});

test('trigger pages replace the previous rows, and a window on a host reads none', async () => {
  const calls = [];
  const p = page({ triggers: async (after) => {
    calls.push(after);
    return { triggers: [{ name: after ? 'second' : 'first', bot: 'x', when: 'fire', message: '' }], next_after: after ? null : 'first' };
  } });
  await p.readTriggers();
  assert.equal(p.S.trig.list[0].name, 'first');
  await p.act({ dataset: { act: 'triggers-next' } });
  assert.deepEqual(p.S.trig.list.map((x) => x.name), ['second']);
  assert.equal(p.S.trig.next, null);
  assert.equal(p.S.trig.after, 'first');
  p.S.config = { host: 'remote' };
  await p.readTriggers();
  assert.deepEqual(calls, [null, 'first']);
  assert.equal(p.S.trig.list, null);
});

test('reads asked for while one runs come to one more read, and a trigger\'s message or script call asks for one', async () => {
  let reads = 0; const gate = deferred();
  const p = page({ triggers: async () => { reads++; await gate.promise; return { triggers: [] }; }, request: async () => ({}) });
  p.S.live = true;
  const first = p.readTriggers();
  p.readTriggers(); p.readTriggers(); p.readTriggers();
  gate.resolve(); await first; await settle();
  assert.equal(reads, 2);
  // A fire's message: read a little later, once, however many arrive.
  await p.onEvent({ event: 'accepted', bot: 'a', turn: 1, data: { node: 1, origin: 'trigger' } });
  await p.onEvent({ event: 'accepted', bot: 'b', turn: 1, data: { node: 2, origin: 'trigger' } });
  await p.tick();
  assert.equal(reads, 3);
  // An agent's shell call of the trigger script.
  await p.onEvent({ event: 'tool_started', bot: 'a', turn: 1, data: { call_id: 'c', name: 'shell', arguments: JSON.stringify({ command: '"$HOME/.agent/trigger" add --every 30m -- check' }) } });
  await p.onEvent({ event: 'tool_completed', bot: 'a', turn: 1, data: { call_id: 'c' } });
  await p.tick();
  assert.equal(reads, 4);
  await p.onEvent({ event: 'tool_started', bot: 'a', turn: 1, data: { call_id: 'd', name: 'shell', arguments: JSON.stringify({ command: 'ls ~/.agent/triggers' }) } });
  await p.onEvent({ event: 'tool_completed', bot: 'a', turn: 1, data: { call_id: 'd' } });
  await p.tick();
  assert.equal(reads, 4, 'its folder is not the script');
});

test('a triggered message names its trigger and why it fired, and the name opens it', () => {
  const p = page({});
  const html = p.itemsHTML(Object.assign(p.transcript('x'), { items: [{ kind: 'user', by: { app: 'trigger' }, text: '[trigger pr-check · 2026-10-10 09:30 · commit /r at 1a2b3c]\nCheck the PR.' }] }));
  assert.match(html, /<button type="button" class="by" data-act="trigger" data-v="pr-check" title="Open this trigger">⎇ pr-check<\/button> <span class="why">commit \/r at 1a2b3c · 2026-10-10 09:30<\/span>\nCheck the PR\./);
  p.S.config = { host: 'box' };
  assert.match(p.itemsHTML(p.transcript('x')), /<span class="by">⎇ pr-check<\/span>/, 'a window on a host cannot open it');
});

test('a task turn whose answer a trigger passes to its coordinator is not news for it again', async () => {
  const p = page({ request: async () => ({}), log() {} });
  p.S.live = true; p.S.attached = true;
  p.upsert({ name: 'demo.lead', bot_id: 1, status: 'idle' });
  p.upsert({ name: 'demo.review', bot_id: 2, status: 'idle', created_by: 'demo.lead', created_by_id: 1 });
  await p.onEvent({ event: 'accepted', bot: 'demo.review', turn: 7, data: { request_id: 'trigger_9-1_2.1790000000.41.to.1', origin: 'trigger' } });
  await p.onEvent({ event: 'turn_finished', bot: 'demo.review', turn: 7, data: { status: 'completed' } });
  await p.onEvent({ event: 'accepted', bot: 'demo.lead', turn: 3, data: { from: { bot: 'demo.review', turn: 7, bot_id: 2 } } });
  await p.tick();
  assert.equal(p.S.wakes.get('demo.lead')?.tasks.size ?? 0, 0, 'its answer reaches the coordinator already');
  // An answer the trigger failed to pass on leaves the turn news after all.
  await p.onEvent({ event: 'accepted', bot: 'demo.review', turn: 9, data: { request_id: 'trigger_9-1_2.1790000000.43.to.1', origin: 'trigger' } });
  await p.onEvent({ event: 'turn_finished', bot: 'demo.review', turn: 9, data: { status: 'completed' } });
  assert.equal(p.S.wakes.get('demo.lead')?.tasks.size ?? 0, 0, 'not before the answer had its time');
  await p.tick();
  assert.deepEqual({ ...p.S.wakes.get('demo.lead').tasks.get('demo.review').act }, { first: 9, turn: 9, status: 'completed', by: 'trigger', count: 1 });
  p.S.wakes.get('demo.lead').tasks.clear();
  // Held while a snapshot loads, the turn keeps where its answer goes.
  p.S.snapshot = true;
  await p.onEvent({ event: 'accepted', bot: 'demo.review', turn: 10, data: { request_id: 'trigger_9-1_2.1790000000.44.to.1', origin: 'trigger' } });
  await p.onEvent({ event: 'turn_finished', bot: 'demo.review', turn: 10, data: { status: 'completed' } });
  assert.equal(p.S.heldNews.at(-1)[6], 1);
  // Its answer, replayed before the held news goes out, keeps it from being news.
  await p.onEvent({ event: 'accepted', bot: 'demo.lead', turn: 4, data: { from: { bot: 'demo.review', turn: 10, bot_id: 2 } } });
  p.S.snapshot = false;
  for (const news of p.S.heldNews.splice(0)) p.turnNews(...news);
  await p.tick();
  assert.equal(p.S.wakes.get('demo.lead')?.tasks.size ?? 0, 0, 'the answer it was held with reached the coordinator');
  // One whose answer goes elsewhere, or a plain trigger's, still is.
  await p.onEvent({ event: 'accepted', bot: 'demo.review', turn: 8, data: { request_id: 'trigger_9-1_2.1790000000.42.to.9', origin: 'trigger' } });
  await p.onEvent({ event: 'turn_finished', bot: 'demo.review', turn: 8, data: { status: 'completed' } });
  assert.equal(p.S.wakes.get('demo.lead').tasks.get('demo.review').act.turn, 8);
});

test('a coordinator\'s backlog stays small however much its tasks do, and what one message leaves out comes next', async () => {
  const sent = [];
  const p = page({ request: async (op, params) => { if (op === 'submit') sent.push(params); return {}; }, log() {} });
  p.S.live = true; p.S.attached = true;
  p.upsert({ name: 'demo.lead', bot_id: 1, status: 'running' });
  for (let i = 0; i < 40; i++) p.upsert({ name: `demo.t${i}`, bot_id: 10 + i, status: 'idle', created_by: 'demo.lead', created_by_id: 1 });
  // A long coordinator turn: every task ends many turns meanwhile.
  for (let n = 1; n <= 50; n++) for (let i = 0; i < 40; i++) await p.onEvent({ event: 'turn_finished', bot: `demo.t${i}`, turn: n, data: { status: 'completed' } });
  const w = p.S.wakes.get('demo.lead');
  assert.equal(w.tasks.size, 40);
  assert.equal(w.timer, null, 'nothing is armed while the coordinator works');
  assert.deepEqual({ ...w.tasks.get('demo.t0').theirs }, { first: 1, turn: 50, status: 'completed', by: 'the person', count: 50 });
  await p.onEvent({ event: 'turn_finished', bot: 'demo.lead', turn: 2, data: { status: 'completed' } });
  await p.tick();
  assert.equal(sent.length, 1);
  assert.equal(sent[0].prompt.split('\n').length, 1 + 1 + 32 + 1);
  assert.match(sent[0].prompt, /\n- demo\.t0: turn:demo\.t0\/50 completed, asked by the person, and 49 earlier since turn:demo\.t0\/1\n/);
  assert.match(sent[0].prompt, /\n- 8 more tasks in the next update$/);
  assert.equal(w.tasks.size, 8);
  // Its next rest brings the rest.
  await p.onEvent({ event: 'turn_finished', bot: 'demo.lead', turn: 3, data: { status: 'completed' } });
  await p.tick();
  assert.equal(sent.length, 2);
  assert.match(sent[1].prompt, /\n- demo\.t32: turn:demo\.t32\/50 completed/);
  assert.doesNotMatch(sent[1].prompt, /more tasks/);
  assert.equal(w.tasks.size, 0);
});

test('what a window remembers belongs to its store, so two hosts, or a host and this machine, never share it', () => {
  const storage = new Map();
  const p = shell({}, storage);
  p.upsert({ name: 'lead', bot_id: 1, provider: 'alpha', model: 'one' });
  p.S.ui.tabs = ['gone', 'lead', 'lead']; p.S.selected = 'lead'; p.S.ui.rail = false; p.save();
  const other = shell({}, storage); other.S.store = 'store-2';
  other.upsert({ name: 'lead', bot_id: 1, provider: 'alpha', model: 'one' });
  other.restore();
  assert.equal(other.S.ui.rail, true, 'another store with the same folder starts fresh');
  assert.deepEqual([...other.S.ui.tabs], []); assert.equal(other.S.selected, '');
  const same = shell({}, storage);
  same.upsert({ name: 'lead', bot_id: 1, provider: 'alpha', model: 'one' });
  same.restore();
  assert.equal(same.S.ui.rail, false);
  assert.deepEqual([...same.S.ui.tabs], ['lead'], 'tabs come back once each, less agents gone since'); assert.equal(same.S.selected, 'lead');
  // Before a daemon has said which store it is, nothing is saved.
  const unknown = shell({}, storage); unknown.S.store = null; unknown.S.ui.rail = false;
  const before = storage.size; unknown.save();
  assert.equal(storage.size, before);
});

test('a tab deleted while detached goes up to what made it on reconnect, as a live delete moves it', async () => {
  const storage = new Map();
  const p = page({
    attach: async () => ({ session: 2, store: 'store-1' }),
    pull: () => new Promise(() => {}),
    request: async (op) => (op === 'bots' ? { bots: [{ name: 'app.lead', bot_id: 1, provider: 'alpha', model: 'one', status: 'idle' }] } : { nodes: [], workspaces:[],next_from: null }),
  }, storage);
  p.setRender(() => {}); p.S.store = 'store-1'; p.S.config = { workspace: '/synthetic', tools: [] };
  p.upsert({ name: 'app.lead', bot_id: 1, provider: 'alpha', model: 'one' });
  p.upsert({ name: 'app.build', bot_id: 2, provider: 'alpha', model: 'one', created_by: 'app.lead', created_by_id: 1 });
  p.tree(); p.S.ui.tabs = ['app.build']; p.S.selected = 'app.build'; p.save();
  await p.attach(); await settle();
  assert.equal(p.S.attached, true);
  assert.equal(p.S.bots.has('app.build'), false);
  assert.deepEqual([...p.S.ui.tabs], ['app.lead'], 'the tab shows what made it'); assert.equal(p.S.selected, 'app.lead');
});

test('a window on a host takes the home the host names and leaves out what reads this machine', async () => {
  const calls = [];
  const p = page({
    setup: async () => ({ socket: null, host: 'box', workspace: null, managed: true, tools: [] }),
    attach: async () => ({ session: 1, store: 'store-box', workspace: '/home/someone' }),
    pull: () => new Promise(() => {}),
    request: async (op) => (op === 'bots' ? { bots: [{ name: 'app.lead', bot_id: 1, provider: 'alpha', model: 'one', workspace: '/home/someone/app', status: 'idle' }] } : {}),
    triggers: async () => { calls.push('triggers'); return []; },
    swarms: async () => { calls.push('swarms'); return { swarms: [], broken: [] }; },
    branch: async () => { calls.push('branch'); return null; },
    settings: async () => ({ providers: [], keys: [], restartable: false, host: 'box' }),
    models: async () => { throw new Error('remote_unsupported: The model list (~/.agent/models) reads files, and this window\'s agents run on box; reading them there is not built yet'); },
  });
  p.setRender(() => {});
  await p.attach(); await settle();
  assert.equal(p.S.attached, true);
  assert.equal(p.S.store, 'store-box');
  assert.equal(p.S.config.workspace, '/home/someone');
  p.renderHead(p.context.document.getElementById('title'), p.S.bots.get('app.lead'), 'main'); await settle();
  assert.deepEqual(calls, [], 'no swarm or branch is read from this machine for a host');
  const swarm = p.botMenuItems('app.lead').find((i) => i.act === 'new-swarm');
  assert.equal(swarm.disabled, true);
  await p.openSetup(); await settle();
  const html = p.setupHTML();
  assert.match(html, /runs on box, with the providers its login shell there exports/);
  assert.match(html, /remote_unsupported: The model list/);
  assert.doesNotMatch(html, /Roles/);
  assert.doesNotMatch(html, /<h3>Triggers<\/h3>/);
  assert.ok(!calls.includes('triggers'), 'remote Settings never reads local triggers');
  assert.doesNotMatch(html, /Add a provider/);
  p.lost('agent_missing: box has no agent on its login shell\'s PATH');
  assert.match(p.elements.get('detached').innerHTML, /daemon on <span class="k">box<\/span>/);
});

test('Settings lists the hosts in the ssh config and opens a window on one', async () => {
  const opened = [];
  const p = shell({
    settings: async () => ({ providers: [], keys: [], restartable: true }),
    models: async () => [],
    hosts: async () => [{ alias: 'box', to: { user: 'someone', hostname: 'box.example', port: '2200' } }, { alias: 'build', to: null }],
    openHost: async (host) => { opened.push(host); },
  });
  await p.openSetup(); await settle();
  const html = p.setupHTML();
  assert.match(html, /<span class="pn">box<\/span><span class="st dim">someone@box\.example:2200<\/span>/);
  assert.match(html, /data-act="open-host" data-v="build"/);
  await p.act({ dataset: { act: 'open-host', v: 'box' } });
  assert.deepEqual(opened, ['box']);
});

test('another store answering on reattach is followed from its start, with nothing kept from the last', async () => {
  const stores = ['store-1', 'store-2', 'store-2'], afters = [];
  let at = 0;
  const p = page({
    setup: async () => ({ socket: '/synthetic.sock', workspace: '/synthetic', tools: [] }),
    attach: async (after) => { afters.push(after); return { session: afters.length, store: stores[Math.min(at++, stores.length - 1)] }; },
    pull: () => new Promise(() => {}),
    request: async (op) => (op === 'bots' ? { bots: [] } : {}),
  });
  p.setRender(() => {});
  await p.attach(); await settle();
  p.upsert({ name: 'Bob', bot_id: 7, provider: 'alpha', model: 'one' });
  p.transcript('Bob').items.push({ kind: 'note', text: 'from the first store' });
  p.S.cursor = 50; p.S.drafts.set('Bob', 'unsent');
  p.S.turnFrom.set(42, 'Bob');
  p.S.wakes.set('Bob', { tasks: new Map([['old-task', { turn: 42 }]]), timer: null, last: 0 });
  p.lost('closed'); await p.tick(); await settle();
  assert.deepEqual(afters, [0, 50, 0], 'the new store is followed from cursor zero');
  assert.equal(p.S.store, 'store-2');
  assert.equal(p.S.bots.has('Bob'), false);
  assert.equal(p.S.transcripts.has('Bob'), false);
  assert.equal(p.S.drafts.size, 0);
  assert.equal(p.S.wakes.size, 0, 'wake backlogs belong to the old store');
  assert.equal(p.S.turnFrom.size, 0, 'turn authors belong to the old store');
  assert.equal(p.S.config.workspace, '/synthetic', 'a folder the window was given is kept');
});

test('a home the last host named is replaced by the one the new store names', async () => {
  const answers = [['store-1', '/home/a'], ['store-2', '/home/b'], ['store-2', '/home/b']];
  let at = 0;
  const p = page({
    setup: async () => ({ socket: '/synthetic.sock', workspace: null, tools: [] }),
    attach: async () => { const [store, workspace] = answers[Math.min(at++, answers.length - 1)]; return { session: at, store, workspace }; },
    pull: () => new Promise(() => {}),
    request: async (op) => (op === 'bots' ? { bots: [] } : {}),
  });
  p.setRender(() => {});
  await p.attach(); await settle();
  assert.equal(p.S.config.workspace, '/home/a');
  p.lost('closed'); await p.tick(); await settle();
  assert.equal(p.S.store, 'store-2');
  assert.equal(p.S.config.workspace, '/home/b');
});

test('usage checks member and descendant budgets before any turn ends, once enough tokens could move a share', async () => {
  const checks = [], pending = deferred();
  const p = shell({ swarmCheck: async name => { checks.push(name); return pending.promise; }, request: async () => ({ bots: [], nodes: [], next_after: null }) });
  p.upsert({name:'app.latency-1',bot_id:3,provider:'alpha',model:'one'});
  p.upsert({name:'app.latency-1.helper',bot_id:4,provider:'alpha',model:'one',created_by:'app.latency-1',created_by_id:3});
  const sw = p.learnSwarm(swarmRecord(['app.latency-1'], {ids:{'app.latency-1':3}}));
  // One member of 3M: a check is due every 150k tokens, input (cached included) plus output.
  const usage = (name, input) => p.handle({event:'usage',bot:name,turn:1,data:{input_tokens:input,output_tokens:20}},1);
  await usage('app.latency-1', 5000); await p.tick(); assert.deepEqual(checks,[],'a small call reads nothing');
  await usage('app.latency-1.helper', 145000); await p.tick(); assert.deepEqual(checks,['app.latency'],'a descendant\'s tokens count');
  await usage('app.latency-1', 150000); await p.tick(); assert.equal(checks.length,1,'no overlapping scan');
  pending.resolve({}); await settle();
  await p.tick(); assert.equal(checks.length,2,'usage arriving during a scan causes a follow-up, without waiting for another round');
  sw.stopped = true; await usage('app.latency-1', 300000); await p.tick(); assert.equal(checks.length,2,'a stopped swarm stays quiet');
});

test('work view retains partial findings and independent verdicts without calling a turn complete', () => {
  const p = shell();
  const sw = p.learnSwarm(swarmRecord(['app.latency-1','app.latency-2']));
  sw.tab = 'streams'; sw.state.tasks = { waits: {owner:'latency-1',reviewer:'latency-2',brief:'Check wait cleanup',status:'reviewing',result:'Source evidence; measurement missing'} };
  const el = p.context.document.getElementById('main');
  p.renderSwarm(el,sw);
  assert.match(el.innerHTML,/reviewing/); assert.match(el.innerHTML,/measurement missing/); assert.doesNotMatch(el.innerHTML,/Final result/);
  sw.state.tasks.waits.status = 'reviewed'; sw.state.tasks.waits.verdict = 'conditional'; sw.state.tasks.waits.evidence = 'Only active for helper waits'; sw.state.result = {outcome:'partial',summary:'Conditional bottleneck'}; sw.stateGen = 2;
  p.renderSwarm(el,sw); assert.match(el.innerHTML,/conditional/); assert.match(el.innerHTML,/Only active for helper waits/); assert.match(el.innerHTML,/Final result · partial/);
  sw.state.tasks = {}; sw.state.result = {outcome:'failed',summary:'Could not access the inputs'}; sw.stateGen++;
  p.renderSwarm(el,sw); assert.match(el.innerHTML,/Final result · failed/); assert.match(el.innerHTML,/Could not access the inputs/);

  // A council's plain streams stay beside its assigned tasks; an empty flat view names its own tool.
  sw.council = 3; sw.state.tasks = { waits: {owner:'latency-1',reviewer:'latency-2',brief:'Check wait cleanup',status:'working'} };
  sw.state.proposals = [{id:'P1',stream:'waits',why:'w',by:'latency-1',status:'approved',votes:{}},{id:'P2',stream:'profile',why:'Profile the store',by:'latency-2',status:'approved',votes:{}}]; sw.stateGen++;
  p.renderSwarm(el,sw); assert.match(el.innerHTML,/#waits/); assert.match(el.innerHTML,/Profile the store/);
  sw.council = 0; sw.state.tasks = {}; sw.state.proposals = []; sw.state.result = null; sw.stateGen++;
  p.renderSwarm(el,sw); assert.match(el.innerHTML,/no tasks yet: an agent registers one with assign/);
  // The swarm's own notices are system lines, not a member to open.
  sw.tab = 'board'; sw.lines = [{at:1,from:'swarm',kind:'quiet',text:'nothing is running and there is no final result (partial)'}];
  p.renderSwarm(el,sw); assert.match(el.innerHTML,/<span class="who council">swarm<\/span>/); assert.doesNotMatch(el.innerHTML,/data-task="app\.swarm"/);
});

test('a turn the person asked for that ends off screen shows done until its bot is looked at', async () => {
  const p = page();
  for (const [name, id] of [['Ann', 1], ['Bob', 2], ['Cy', 3]]) p.upsert({ name, bot_id: id });
  p.S.selected = 'Ann'; p.S.live = true;
  const glyph = (name) => /class="glyph (\w+)"/.exec(p.botRowHTML({ b: p.S.bots.get(name), prefix: '' }, false))[1];
  const run = async (name, turn, status, from) => {
    await p.onEvent({ event: 'accepted', bot: name, turn, data: from ? { from: { bot: from } } : {} });
    await p.onEvent({ event: 'turn_finished', bot: name, turn, data: { status } });
  };
  await run('Bob', 1, 'completed');
  await run('Cy', 1, 'completed', 'Ann');
  await run('Ann', 1, 'completed');
  // Another bot's ask is that bot's news; the bot on screen was seen as it finished.
  assert.deepEqual(['Bob', 'Cy', 'Ann'].map(glyph), ['done', 'idle', 'idle']);
  // Still done after a turn that failed and one more that completed.
  await run('Bob', 2, 'failed');
  assert.equal(glyph('Bob'), 'failed');
  await run('Bob', 3, 'completed');
  assert.equal(glyph('Bob'), 'done');
  // Help over the thread hides it.
  p.S.ui.side = 'Bob'; p.S.ui.help = {}; p.markSeen();
  assert.equal(glyph('Bob'), 'done');
  p.S.ui.help = false; p.markSeen();
  assert.equal(glyph('Bob'), 'idle');
  p.S.selected = 'Ann'; p.S.ui.side = null; p.S.setup = { open: true };
  await run('Ann', 2, 'completed');
  assert.equal(glyph('Ann'), 'done');
  p.S.setup.open = false; p.markSeen();
  assert.equal(glyph('Ann'), 'idle');
  // A turn another bot asked for is the person's to see once they steer into it.
  await p.onEvent({ event: 'accepted', bot: 'Cy', turn: 3, data: { from: { bot: 'Ann' } } });
  await p.onEvent({ event: 'steered', bot: 'Cy', turn: 3, data: {} });
  await p.onEvent({ event: 'turn_finished', bot: 'Cy', turn: 3, data: { status: 'completed' } });
  assert.equal(glyph('Cy'), 'done');
  p.S.selected = 'Cy'; p.markSeen(); p.S.selected = 'Ann';
  await run('Cy', 2, 'completed');
  await p.onEvent({ event: 'deleted', bot: 'Cy', data: {} });
  assert.equal(p.S.unseen.has('Cy'), false);
  // A replayed snapshot is history, not news.
  p.S.ui.side = null; p.S.live = false;
  await run('Bob', 4, 'completed');
  assert.equal(glyph('Bob'), 'idle');
});

test('a swarm row shows done while one of its agents has an unseen result, until the swarm is opened', async () => {
  const p = shell();
  p.upsert({ name: 'app.lead', bot_id: 1, provider: 'alpha', model: 'one' });
  for (const [n, id] of [['app.latency-1', 3], ['app.latency-2', 4]]) p.upsert({ name: n, bot_id: id, provider: 'alpha', model: 'one' });
  p.learnSwarm(swarmRecord(['app.latency-1', 'app.latency-2'], { ids: { 'app.latency-1': 3, 'app.latency-2': 4 } }));
  p.S.selected = 'app.lead'; p.S.live = true;
  const row = () => /class="glyph (\w+)"/.exec(p.botRowHTML(p.tree().find((n) => n.swarm), false))[1];
  await p.onEvent({ event: 'accepted', bot: 'app.latency-1', turn: 1, data: {} });
  await p.onEvent({ event: 'turn_finished', bot: 'app.latency-1', turn: 1, data: { status: 'completed' } });
  assert.equal(row(), 'done');
  await p.onEvent({ event: 'accepted', bot: 'app.latency-2', turn: 1, data: {} });
  assert.equal(row(), 'running');
  p.S.bots.get('app.latency-2').status = 'idle';
  // A later failure does not hide the unseen result.
  await p.onEvent({ event: 'accepted', bot: 'app.latency-1', turn: 2, data: {} });
  await p.onEvent({ event: 'turn_finished', bot: 'app.latency-1', turn: 2, data: { status: 'failed' } });
  assert.equal(row(), 'done');
  p.S.selected = '⁂app.latency'; p.markSeen();
  assert.equal(row(), 'idle');
});

test('a plan is its marked lines: what it has done, the step it is on, and the rest', () => {
  const p = page();
  const plan = p.parsePlan('[x] Read it\n[>] Write it\nnot a step\n[ ] \n[ ] Ship it\n');
  assert.deepEqual(JSON.parse(JSON.stringify(plan.steps)), [{ s: 'done', t: 'Read it' }, { s: 'now', t: 'Write it' }, { s: 'todo', t: 'Ship it' }]);
  assert.equal(plan.done, 1); assert.equal(plan.at.t, 'Write it');
  // With no step under way, the next one to do is where it is; with all done, nowhere.
  assert.equal(p.parsePlan('[x] Read it\n[ ] Ship it').at.t, 'Ship it');
  assert.equal(p.parsePlan('[x] Read it').at, null);
  assert.equal(p.parsePlan('no plan yet\n'), null);
});

test("plans are read at once, then an agent's again when it runs the plan script, and shown on its row, card and chat", async () => {
  const files = new Map([[2, '[x] Read it\n[>] Write <b>it</b>\n[ ] Ship it\n']]), asked = [];
  const p = page({ plans: async (ids) => { asked.push(ids); return Object.fromEntries(ids.filter((id) => files.has(id)).map((id) => [id, files.get(id)])); } });
  p.S.config = { workspace: '/synthetic' }; p.S.session = 1;
  p.upsert({ name: 'app.lead', bot_id: 1 });
  p.upsert({ name: 'app.build', bot_id: 2, created_by: 'app.lead', created_by_id: 1 });
  // An agent deleted from another window left a plan behind; only the seated agents' are read.
  files.set(9, '[>] Gone\n');
  await p.loadPlans();
  assert.deepEqual(JSON.parse(JSON.stringify(asked)), [[1, 2]]);
  assert.equal(p.S.plans.has(9), false);
  const row = () => p.botRowHTML({ b: p.S.bots.get('app.build'), depth: 1, kids: 0 });
  assert.match(row(), /<span class="step"> · Write &lt;b&gt;it&lt;\/b&gt;<\/span><\/span><span class="meta" title="steps done">1\/3<\/span>/);
  assert.equal(p.taskCard('app.build').last, '✱ Write <b>it</b>');
  assert.equal(p.taskCard('app.build').elapsed, '1/3');
  assert.doesNotMatch(p.botRowHTML({ b: p.S.bots.get('app.lead'), depth: 0, kids: 1 }), /steps done/);
  const el = p.context.document.getElementById('plan'), beside = p.context.document.getElementById('sideplan');
  el.id = 'plan'; beside.id = 'sideplan';
  p.renderPlan(el, p.S.bots.get('app.build'));
  assert.equal(el.hidden, false);
  assert.match(el.innerHTML, /data-act="plan-fold"[^>]*>1\/3<\/button><div class="pstep done"><span class="pm">✓<\/span>Read it<\/div><div class="pstep now"><span class="pm">✱<\/span>Write &lt;b&gt;it&lt;\/b&gt;<\/div><div class="pstep todo"><span class="pm">○<\/span>Ship it<\/div>$/);
  // Its count folds that pane's plan alone.
  await p.act({ dataset: { act: 'plan-fold', v: 'plan' } });
  p.renderPlan(el, p.S.bots.get('app.build')); p.renderPlan(beside, p.S.bots.get('app.build'));
  assert.equal((el.innerHTML.match(/pstep/g) ?? []).length, 1);
  assert.equal((beside.innerHTML.match(/pstep/g) ?? []).length, 3);
  // Another shell call reads nothing; the script's call reads that agent's plan alone, once it ends.
  p.S.live = true;
  const shell = async (id, command) => {
    await p.onEvent({ event: 'tool_started', bot: 'app.build', turn: 1, data: { call_id: id, name: 'shell', arguments: JSON.stringify({ command }) } });
    await p.onEvent({ event: 'tool_completed', bot: 'app.build', turn: 1, data: { call_id: id } });
    await settle();
  };
  await shell('c1', 'cargo test');
  assert.equal(asked.length, 1);
  files.set(2, '[x] Read it\n[x] Write it\n[>] Ship it\n');
  await shell('c2', `sh "$HOME/.agents/skills/plan/plan" '[x] Read it' '[x] Write it' '[>] Ship it'`);
  assert.deepEqual(JSON.parse(JSON.stringify(asked.at(-1))), [2]);
  assert.equal(p.taskCard('app.build').last, '✱ Ship it');
  assert.equal(p.taskCard('app.build').elapsed, '2/3');
  // A provider may escape the slashes in its JSON; the decoded command still names the script.
  files.set(2, '[x] Read it\n[x] Write it\n[>] Ship it\n[ ] Tell them\n');
  await p.onEvent({ event: 'tool_started', bot: 'app.build', turn: 1, data: { call_id: 'c5', name: 'shell', arguments: JSON.stringify({ command: `sh "$HOME/.agents/skills/plan/plan" '[ ] Tell them'` }).replaceAll('/', '\\/') } });
  await p.onEvent({ event: 'tool_completed', bot: 'app.build', turn: 1, data: { call_id: 'c5' } });
  await settle();
  assert.equal(p.taskCard('app.build').elapsed, '2/4');
  // A long plan's call arrives cut short, as the daemon previews it, and still reads the plan.
  files.set(2, '[x] Read it\n[x] Write it\n[x] Ship it\n');
  const cut = JSON.stringify({ command: `sh "$HOME/.agents/skills/plan/plan" '[x] Read it' '[x] ${'Write it '.repeat(300)}` }).slice(0, 2048);
  await p.onEvent({ event: 'tool_started', bot: 'app.build', turn: 1, data: { call_id: 'c4', name: 'shell', arguments: cut, arguments_truncated: true } });
  await p.onEvent({ event: 'tool_completed', bot: 'app.build', turn: 1, data: { call_id: 'c4' } });
  await settle();
  assert.equal(p.taskCard('app.build').elapsed, '3/3');
  // A plan the agent no longer has is no longer shown.
  files.delete(2);
  await shell('c3', `sh "$HOME/.agents/skills/plan/plan" --clear`);
  assert.equal(p.taskCard('app.build').elapsed, '');
  p.renderPlan(el, p.S.bots.get('app.build'));
  assert.equal(el.hidden, true);
});

test("a deleted agent's plan goes with it, and a window that cannot read plans stops asking", async () => {
  const forgot = [];
  let calls = 0;
  const p = page({ plans: async () => { calls += 1; return { 4: '[>] Watch it\n' }; }, forgetPlan: async (id) => { forgot.push(id); } });
  p.S.config = { workspace: '/synthetic' }; p.S.session = 1; p.S.live = true;
  p.upsert({ name: 'Cy', bot_id: 4 });
  await p.loadPlans();
  assert.equal(p.taskCard('Cy').elapsed, '0/1');
  await p.onEvent({ event: 'deleted', bot: 'Cy', data: {} });
  assert.deepEqual(forgot, [4]);
  assert.equal(p.S.plans.has(4), false);
  const off = page({ plans: async () => { calls += 1; throw new Error('plans_unsupported: opened on a socket'); } });
  off.S.config = { workspace: '/synthetic' }; off.S.session = 1; off.upsert({ name: 'Cy', bot_id: 4 });
  await off.loadPlans(); await off.loadPlans([1]);
  assert.equal(calls, 2);
  // A window on a host asks nothing.
  const remote = page({ plans: async () => { calls += 1; return {}; } });
  remote.S.config = { host: 'box' }; await remote.loadPlans();
  assert.equal(calls, 2);
});
