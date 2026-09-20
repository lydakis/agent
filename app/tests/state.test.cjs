const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

// Run the actual page state machine without booting a provider or a webview.
function page(daemon = {}) {
  const elements = new Map(), timers = new Map();
  let timer = 0;
  const element = () => ({
    children: [], replaceChildren(...nodes) { this.children = nodes; }, dataset: {}, innerHTML: '', value: '', scrollHeight: 0, scrollTop: 0, clientHeight: 0,
    classList: { add() {}, remove() {}, toggle() {}, contains() { return false; } },
    addEventListener() {}, querySelector() { return null; }, querySelectorAll() { return []; }, focus() {},
  });
  const context = vm.createContext({
    Daemon: daemon, console, queueMicrotask, crypto: require('node:crypto').webcrypto,
    document: { getElementById(id) { if (!elements.has(id)) elements.set(id, element()); return elements.get(id); }, addEventListener() {},
      createElement: element, createTextNode: () => ({ data: '', appended: 0, appendData(s) { this.data += s; this.appended += s.length; } }) },
    window: { addEventListener() {} }, localStorage: { getItem() { return null; } },
    setTimeout(fn) { const id = ++timer; timers.set(id, fn); return id; }, clearTimeout(id) { timers.delete(id); }, setInterval() {},
  });
  let source = fs.readFileSync(require.resolve('../ui/app.js'), 'utf8');
  source = source.slice(0, source.indexOf('// ---------- boot ----------')) +
    'globalThis.app = { S, transcript, upsert, onEvent, handle, loadBatch, evict, itemsHTML, attach, lost, enqueue, load, cssEsc, esc, submit, renderTail: typeof renderTail === "function" ? renderTail : null };\n})();';
  vm.runInContext(source, context);
  return { ...context.app, context, elements, async tick() { const jobs = [...timers.values()]; timers.clear(); jobs.forEach(fn => fn()); await settle(); } };
}
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
  const outputs = { 1: { output: '{"handle":"proc:1"}' }, 2: { output: '{"results":{"proc:1":{"exit_code":0,"stdout":"finished"}}}' } };
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
  const same = t.items.filter(it => it.from === 1200 || it.node === 1200);
  assert.equal(same.length, 1);
  assert.equal(same[0].kind, 'node');
});

test('large fan-out keeps rendered peer cards and retained peer entries bounded', async () => {
  const p = page(); p.upsert({ name: 'parent', id: 1 });
  for (let i = 0; i < 10000; i++) await p.onEvent({ event: 'created', bot: `child${i}`, data: { id: i + 2, provider: 'test', created_by: 'parent', created_by_id: 1 } });
  const t = p.transcript('parent');
  assert.ok(t.items.filter(it => it.kind === 'peer').length <= 600);
  assert.ok(t.peers.length <= 600);
  assert.ok((p.itemsHTML(t).match(/data-peek=/g) || []).length <= 600);
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
    const line = el.children[0], text = line.children[0];
    assert.equal(text.data, t[field]);
    assert.equal(text.appended, t[field].length);
    t[field] = 'new'; t.streamGen += 1;
    p.renderTail(el, 'Bob', t);
    assert.equal(el.children[0].children[0].data, 'new');
  }
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
    p.upsert({name:'Bob',id:1}); p.S.selected='Bob'; p.S.config={workspace:'/synthetic'};
    await Promise.all([p.submit('first'),p.submit('second')]);
  }
  assert.equal(new Set(ids).size,4);
});

test('fork history loads by checkpoint in pages even without its source bot', async () => {
  const pages = [];
  const p = page({request:async (op,params) => {
    if (op === 'history_nodes') { pages.push(params.from); return params.from === 4 ? {nodes:[{node:4,turn_seq:2},{node:3,turn_seq:2}],next_from:2} : {nodes:[{node:2,turn_seq:1},{node:1,turn_seq:1}],next_from:null}; }
    assert.equal(op,'item'); return {role:params.node % 2 ? 'user':'assistant',content:[{type:'text',text:`inherited ${params.node}`}]};
  }});
  await p.onEvent({event:'forked',bot:'branch',data:{provider:'test',id:2,source:'deleted-source',checkpoint:4}});
  await p.onEvent({event:'text_delta',bot:'branch',turn:3,text:'new work'});
  await p.load('branch');
  let t=p.transcript('branch'); assert.match(p.itemsHTML(t), /inherited 3/); assert.match(p.itemsHTML(t), /inherited 4/);
  assert.equal(t.text,'new work'); assert.deepEqual(pages,[4]);
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
    inherited.items=[{kind:'node',node:1,turn:null},{kind:'node',node:2,turn:null}]; inherited.nodes=2;
    await p.loadBatch('branch');
    assert.equal(inherited.items.filter(it=>it.kind==='tool').length,2,'inherited turns may reuse call IDs');
  }
});
