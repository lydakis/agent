const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

// Run the actual page state machine without booting a provider or a webview.
function page(daemon = {}, storage = null) {
  const elements = new Map(), timers = new Map();
  let timer = 0;
  const element = () => ({
    children: [], replaceChildren(...nodes) { this.children = nodes; }, dataset: {}, style: {}, innerHTML: '', value: '', scrollHeight: 0, scrollTop: 0, clientHeight: 0,
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
    Daemon: transport, console, queueMicrotask, crypto: require('node:crypto').webcrypto,
    document: { getElementById(id) { if (!elements.has(id)) elements.set(id, element()); return elements.get(id); }, listeners: {}, addEventListener(type, fn) { this.listeners[type] = fn; },
      createElement: element, createTextNode: () => ({ data: '', appended: 0, appendData(s) { this.data += s; this.appended += s.length; } }) },
    window: { addEventListener() {} }, localStorage: storage ? { getItem: k => storage.get(k) ?? null, setItem: (k, v) => storage.set(k, String(v)) } : { getItem() { return null; } },
    setTimeout(fn) { const id = ++timer; timers.set(id, fn); return id; }, clearTimeout(id) { timers.delete(id); }, setInterval() {},
  });
  let source = fs.readFileSync(require.resolve('../ui/app.js'), 'utf8');
  source = source.slice(0, source.indexOf('// ---------- boot ----------')) +
    'globalThis.app = { setRender: fn => { render = fn; }, S, rail, renderRail, transcript, upsert, onEvent, handle, pump, loadBatch, evict, itemsHTML, attach, lost, enqueue, load, cssEsc, esc, submit, interrupt, seat, botRowHTML, renderTail, tree, shortName, runStart, runHTML, botMenuItems, modelChoices, modelMenuItems, sendMenuItems, setSend, setModel, fork, remove, createProject, openOnly, openBeside, swap, save, restore, showMenu, refreshMenu, entries, pickerRows, closeSide, waitSummary, nextBeside, sideChat, renderHead, followDrafts, openSetup, connectProvider, removeProvider, providerSpecs, act, setupHTML, refreshModels };\n})();';
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
  const p = page(); p.upsert({ name: 'parent', id: 1 });
  for (let i = 0; i < 10000; i++) await p.onEvent({ event: 'created', bot: `child${i}`, data: { id: i + 2, provider: 'test', created_by: 'parent', created_by_id: 1 } });
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

test('a slow login-shell model lookup never delays attaching, and a later answer fills the default', async () => {
  const lookup = deferred(); let lookups = 0, setups = 0;
  const p = page({ setup: async () => (++setups, {}), defaultModel: () => (++lookups, lookup.promise), attach: async () => ({ session: 1 }), pull: () => new Promise(() => {}), request: async () => ({ bots: [] }) });
  await p.attach(); await settle();
  assert.equal(p.S.attached, true);
  assert.equal(lookups, 1);
  lookup.resolve('anthropic/model-x'); await settle();
  assert.equal(p.S.config.model, 'anthropic/model-x');
  assert.equal(setups, 1);
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
    if (op === 'history_nodes') { pages.push(params.from); return params.from === 4 ? {nodes:[{node:4,turn:2},{node:3,turn:2}],next_from:2} : {nodes:[{node:2,turn:1},{node:1,turn:1}],next_from:null}; }
    assert.equal(op,'item'); return {role:params.node % 2 ? 'user':'assistant',content:[{type:'text',text:`inherited ${params.node}`}]};
  }});
  await p.onEvent({event:'forked',bot:'branch',data:{provider:'test',id:2,source:'deleted-source',checkpoint:4}});
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
  p.upsert({name:'parent',id:1});
  await p.handle({event:'forked',bot:'branch',data:{provider:'test',id:2,created_by:'parent',created_by_id:1}},1);
  assert.deepEqual(Array.from(p.transcript('parent').peers),['branch']);
  await p.handle({event:'deleted',bot:'branch'},1);
  p.seat({name:'branch',id:2,provider:'test'},1);
  assert.equal(p.S.bots.has('branch'),false);
  const creating=p.handle({event:'created',bot:'branch',data:{id:3,provider:'test'}},1);
  p.seat({name:'branch',id:2,provider:'old'},1);
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
    return {nodes:Array.from({length:Math.max(0,last-first+1)},(_,i)=>({node:last-i,turn:Math.ceil((last-i)/2)})),next_from:first>min?first-1:null,next_newer:last<max?last+1:null};
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
  const p=page();p.upsert({name:'Bob',id:1});const b=p.S.bots.get('Bob');
  b.status='waiting';b.waitingOn=['turn:<img src=x onerror=alert(1)>/1'];
  const html=p.botRowHTML({b,depth:1,prefix:'│ └'},false);
  assert.ok(!html.includes('img'));assert.doesNotMatch(html,/class="w"/);
  assert.match(html,/<span class="tree">│ └<\/span><span class="glyph waiting">/);
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
  p.seat({name:'Bob',id:1,provider:'test',head:10},1);
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
  const p=page({setup:async()=>({}),attach:async()=>({session:2}),pull:()=>new Promise(()=>{}),request:async()=>({bots:[{name:'parent',id:1}],next_after:null})});
  p.upsert({name:'parent',id:1});
  await p.onEvent({event:'created',bot:'child',data:{id:2,provider:'test',created_by:'parent',created_by_id:1}});
  await p.attach();
  await p.onEvent({event:'created',bot:'child',data:{id:3,provider:'test'}});
  assert.equal(p.transcript('parent').peers.includes('child'),false);
  assert.equal(p.transcript('parent').items.some(it=>it.kind==='peer'&&it.who==='child'),false);
});

test('answer deltas follow thinking immediately and failed partial text stays ephemeral', async () => {
  const p=page(),t=p.transcript('Bob');const el={dataset:{},children:[],replaceChildren(...nodes){this.children=nodes;}};
  await p.onEvent({event:'thinking_delta',bot:'Bob',turn:1,text:'thinking'});
  p.renderTail(el,'Bob',t);
  await p.onEvent({event:'text_delta',bot:'Bob',turn:1,text:'answer'});
  p.renderTail(el,'Bob',t);assert.equal(el.children[0].children[0].data,'answer');
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
    return {events:Array.from({length:250},(_,i)=>({event:'created',bot:`bot${start+i}`,data:{id:start+i+1,provider:'test'}}))};
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
  const make=()=>{const p=page({request:async()=>item});p.upsert({name:'Bob',id:1});return p;};
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
    const record={name:'branch',id:2,provider:'test',head:10};
    const event={event:'forked',bot:'branch',data:{id:2,provider:'test',source:'source',checkpoint:10}};
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
  p.upsert({name:'Bob',id:1});
  await p.onEvent({event:'tool_started',bot:'Bob',turn:1,data:{call_id:'c',name:'shell',arguments:'{"background":true}'}});
  const completing=p.handle({event:'tool_completed',bot:'Bob',turn:1,data:{node:1,call_id:'c'}},1);
  await settle();p.lost('disconnected');p.S.session=2;p.upsert({name:'Bob',id:2});
  reply.resolve({output:'{"handle":"proc:1"}'});await completing;
  assert.equal(p.transcript('Bob').items.length,0);
  assert.notEqual(p.S.bots.get('Bob').touched,1);
});


test('reconnect after pruning reloads the full lineage in either snapshot/replay order', async () => {
  for(const snapshotFirst of [true,false]) {
    const requests=[],p=page(historyDaemon(requests));p.S.session=1;
    p.upsert({name:'Bob',id:1,head:2});await p.load('Bob');
    p.lost('offline');p.S.session=2;
    const seat=()=>p.seat({name:'Bob',id:1,head:10},2);
    if(snapshotFirst)seat();
    await p.onEvent({event:'pruned',bot:'Bob'});
    for(const node of [9,10])await p.onEvent({event:'message',bot:'Bob',turn:5,data:{node}});
    if(!snapshotFirst)seat();
    await p.load('Bob');p.transcript('Bob').anchor='top';await p.load('Bob',true);
    const ids=p.transcript('Bob').items.filter(it=>it.from!=null).map(it=>it.from);
    assert.deepEqual(Array.from(ids).sort((a,b)=>a-b),Array.from({length:10},(_,i)=>i+1));
    assert.equal(new Set(ids).size,ids.length);
    const loads=requests.length;p.seat({name:'Bob',id:1,head:10},2);await p.load('Bob');
    assert.equal(requests.length,loads,'same-session snapshots do not reload covered history');
  }
});

test('rail scrolling moves a bounded window both ways independently of selection', () => {
  const p=page();p.S.ui.rail=true;
  for(let i=0;i<1000;i++)p.upsert({name:`bot${i}`,id:i+1});
  p.S.selected='bot0';p.renderRail();const el=p.elements.get('bots');
  el.scrollHeight=1000;el.clientHeight=100;
  for(let i=0;i<6;i++){el.scrollTop=900;el.listeners.scroll();assert.ok((el.innerHTML.match(/data-bot=/g)||[]).length<=300);}
  assert.match(el.innerHTML,/data-bot="bot999"/);
  assert.equal(p.S.selected,'bot0');
  for(let i=0;i<6;i++){el.scrollTop=0;el.listeners.scroll();}
  assert.match(el.innerHTML,/data-bot="bot0"/);
  p.S.selected='bot900';p.renderRail();assert.match(el.innerHTML,/data-bot="bot900"/);
});


test('a delayed reconnect snapshot preserves newer folded replay ranges', async () => {
  const p=page(historyDaemon());p.S.session=1;p.upsert({name:'Bob',id:1,head:2});await p.load('Bob');
  p.lost('offline');p.S.session=2;
  for(let node=9;node<=6000;node++)await p.onEvent({event:'message',bot:'Bob',turn:node,data:{node}});
  p.seat({name:'Bob',id:1,head:10},2);
  const t=p.transcript('Bob');
  const covered=node=>t.items.some(it=>it.node===node||it.from===node||it.kind==='history'&&(it.min??0)<=node&&node<=(it.next-(it.exclusive?1:0)));
  for(let node=1;node<=6000;node++)assert.ok(covered(node),`lost node ${node}`);
  p.evict(t);
  assert.ok(t.items.length<=1605,'recovery retains bounded metadata');
});


test('snapshot-only lineage restores bounded peer cards regardless of page order', async () => {
  const p=page({setup:async()=>({}),attach:async()=>({session:1}),pull:()=>new Promise(()=>{}),request:async(op,q)=> {
    assert.equal(op,'bots');return q.after ? {bots:[{name:'parent',id:1,provider:'test'}]} : {bots:Array.from({length:1000},(_,i)=>({name:`child${i}`,id:i+2,provider:'test',created_by:'parent',created_by_id:1})),next_after:'children'};
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
  const calls=[];const p=page({request:async(op,q)=>{calls.push([op,q]);}});p.upsert({name:'Bob',id:1});p.S.selected='Bob';
  await p.onEvent({event:'queued',bot:'Bob',turn:7,data:{status:'ready'}});
  await p.onEvent({event:'queued',bot:'Bob',turn:8,data:{status:'queued'}});
  await p.interrupt();assert.equal(calls.length,1);assert.equal(calls[0][1].turn,7);
});

test('snapshot-first replay preserves every durable node across eviction boundaries', async () => {
  const p=page(historyDaemon());p.S.session=1;p.upsert({name:'Bob',id:1,head:6000});
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
  p.S.session=1;p.upsert({name:'Bob',id:1,head:2});p.S.selected='Bob';p.lost('offline');
  const attaching=p.attach();await settle();assert.equal(firstHistory,false);
  snapshot.resolve({bots:[{name:'Bob',id:1,head:10}],next_after:null});await settle();
  history.resolve({nodes:[{node:2,turn:1},{node:1,turn:1}],next_from:null});await attaching;
  const ids=p.transcript('Bob').items.filter(it=>it.from!=null).map(it=>it.from);
  assert.deepEqual(Array.from(ids).sort((a,b)=>a-b),Array.from({length:10},(_,i)=>i+1));
});

test('submissions wait for a known bot identity instead of sending an unpinned name', async () => {
  const sent=[];const p=page({request:async(op,q)=>{sent.push([op,q]);}});
  p.S.session=1;p.S.config={workspace:'/synthetic'};
  await p.onEvent({event:'text_delta',bot:'Bob',turn:1,text:'working'});p.S.selected='Bob';
  await assert.rejects(p.submit('next'),/identity/);assert.equal(sent.length,0);
  p.seat({name:'Bob',id:7},1);await p.submit('next');assert.equal(sent[0][1].bot_id,7);
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
    request:async(op,q)=>{assert.equal(op,'create');created=q;return{name:q.bot,id:7,head:null};}});
  p.setRender(()=>{});p.S.session=1;p.S.config={workspace:'/synthetic',tools:[]};
  await p.submit('/new Bob test/model');
  assert.equal(created.compaction_instructions,'summary policy');assert.equal(p.S.bots.get('Bob').id,7);
});

test('app creation fails when the workspace policy cannot be composed', async () => {
  const sent=[];
  const p=page({policy:async()=>{throw 'instructions_unreadable: cannot read /synthetic/AGENTS.md: invalid utf-8';},
    request:async(op,q)=>{sent.push([op,q]);return{name:q.bot,id:7,head:null};}});
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
const shell = (daemon = {}, storage = null) => { const p = page(daemon, storage); p.setRender(() => {}); p.S.session = 1; p.S.config = { workspace: '/synthetic', model: 'alpha/one', tools: [] }; return p; };
// A window whose renders move drafts, as the real render does.
const drafting = (daemon = {}) => { const p = shell(daemon); p.setRender(() => p.followDrafts()); return p; };
const names = (rows) => Array.from(rows, (r) => r.label ?? r.b.name);

test('projects list coordinators with their lineage and prefixed tasks, then other bots', () => {
  const p = shell();
  p.upsert({ name: 'app.lead', id: 1, provider: 'alpha', model: 'one' });
  p.upsert({ name: 'app.build', id: 2, provider: 'alpha', model: 'one', created_by: 'app.lead', created_by_id: 1 });
  p.upsert({ name: 'app.review', id: 3, provider: 'alpha', model: 'one', created_by: 'app.build', created_by_id: 2 });
  p.upsert({ name: 'app.manual', id: 4, provider: 'alpha', model: 'one' });
  p.upsert({ name: 'loose', id: 5, provider: 'alpha', model: 'one' });
  // A coordinator started by a task heads its own project instead of nesting under the task.
  p.upsert({ name: 'zeta.lead', id: 6, provider: 'alpha', model: 'one', created_by: 'app.build', created_by_id: 2 });
  const rows = p.tree();
  assert.deepEqual(names(rows), ['app.lead', 'app.build', 'app.review', 'app.manual', 'zeta.lead', 'bots', 'loose']);
  assert.equal(rows[0].head, 'app'); assert.equal(rows[0].tasks, 3); assert.equal(rows[4].tasks, 0);
  const bot = (n) => p.S.bots.get(n);
  assert.equal(p.shortName(bot('app.lead')), 'app'); assert.equal(p.shortName(bot('app.review')), 'review'); assert.equal(p.shortName(bot('loose')), 'loose');
  assert.match(p.botRowHTML(rows[0], true), /data-act="fold"/); assert.doesNotMatch(p.botRowHTML(rows[4], false), /data-act="fold"/);
  assert.match(p.botRowHTML(rows[1], false), /data-act="more" data-who="app.build"/);
  p.S.ui.folded.add('app');
  assert.deepEqual(names(p.tree()), ['app.lead', 'zeta.lead', 'bots', 'loose']);
  assert.equal(bot('app.review').project, 'app', 'a folded project still owns its tasks');
});

test('a sidebar row opens a thread alone, a card opens it beside, and swap trades them', async () => {
  const p = shell({ request: async () => ({ nodes: [], next_from: null }) });
  for (const [name, id] of [['app.lead', 1], ['app.build', 2], ['app.test', 3]]) p.upsert({ name, id, provider: 'alpha', model: 'one', created_by: id > 1 ? 'app.lead' : null, created_by_id: id > 1 ? 1 : null });
  p.tree(); p.S.selected = 'app.lead';
  await p.openBeside('app.build'); assert.equal(p.S.ui.side, 'app.build');
  await p.openBeside('app.build'); assert.equal(p.S.ui.side, null, 'the same card closes it again');
  await p.openBeside('app.lead'); assert.equal(p.S.ui.side, null, 'the thread in view never opens beside itself');
  await p.openBeside('app.build'); p.swap();
  assert.equal(p.S.selected, 'app.build'); assert.equal(p.S.ui.side, 'app.lead');
  p.S.ui.folded.add('app');
  await p.openOnly('app.test');
  assert.equal(p.S.selected, 'app.test'); assert.equal(p.S.ui.side, null); assert.equal(p.S.ui.folded.has('app'), false);
});

test('each composer sends to its own pane, and a working bot gets the sticky queue or steer pick', async () => {
  const sent = [], storage = new Map();
  const p = shell({ request: async (op, q) => { sent.push([op, q]); } }, storage);
  p.upsert({ name: 'lead', id: 1, provider: 'alpha', model: 'one' });
  p.upsert({ name: 'task', id: 2, provider: 'alpha', model: 'one', status: 'running', running_turn: 3 });
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
  p.upsert({ name: 'lead', id: 1, provider: 'alpha', model: 'one' }); p.S.selected = 'lead';
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
  p.upsert({ name: 'lead', id: 1, provider: 'alpha', family: 'anthropic', model: 'one' });
  p.upsert({ name: 'peer', id: 2, provider: 'gamma', family: 'anthropic', model: 'two' });
  p.upsert({ name: 'other', id: 3, provider: 'beta', family: 'responses', model: 'x' });
  p.S.selected = 'lead';
  const b = p.S.bots.get('lead');
  const choices = p.modelChoices(b, [{ id: 'beta/x' }, { id: 'gamma/two' }, { id: 'delta/y' }, { id: 'alpha/one' }]);
  assert.deepEqual(Array.from(choices, (c) => [c.id, c.ok]), [['gamma/two', true], ['alpha/one', true], ['beta/x', false], ['delta/y', false]]);
  assert.equal(p.setModel('lead', 'beta/x'), false);
  assert.equal(p.setModel('lead', 'gamma/two'), true);
  await p.submit('next turn'); assert.equal(sent.at(-1).model, 'gamma/two');
  // A creation event carries no family; the bot takes its provider's.
  await p.onEvent({ event: 'created', bot: 'late', data: { id: 4, provider: 'alpha', model: 'one' } });
  assert.equal(p.S.bots.get('late').family, 'anthropic');
});

test('a saved model pick comes back only for the same bot identity', () => {
  const storage = new Map();
  const p = shell({}, storage);
  p.upsert({ name: 'lead', id: 1, provider: 'alpha', model: 'one' });
  p.upsert({ name: 'task', id: 2, provider: 'alpha', model: 'one' });
  p.setModel('lead', 'alpha/two'); p.setModel('task', 'alpha/two'); p.save();
  const q = shell({}, storage);
  q.upsert({ name: 'lead', id: 1, provider: 'alpha', model: 'one' });
  q.upsert({ name: 'task', id: 9, provider: 'alpha', model: 'one' }); // deleted and made again
  q.restore();
  assert.equal(q.S.override.get('lead'), 'alpha/two');
  assert.equal(q.S.override.has('task'), false, 'a new bot under an old name starts on its own model');
  storage.set([...storage.keys()].find((k) => k !== 'agent:send'), JSON.stringify({ override: [['lead', 'alpha/two']] }));
  const r = shell({}, storage); r.upsert({ name: 'lead', id: 1, provider: 'alpha', model: 'one' }); r.restore();
  assert.equal(r.S.override.has('lead'), false, 'a pick with no identity is not restored');
});

test('a steer joins the running turn: it names no model and no workspace', async () => {
  const sent = [];
  const p = shell({ request: async (op, q) => { sent.push(q); } });
  p.upsert({ name: 'task', id: 2, provider: 'alpha', model: 'one', workspace: '/synthetic/task', status: 'running', running_turn: 3 });
  p.S.selected = 'task'; p.setModel('task', 'alpha/two');
  p.setSend('queue'); await p.submit('later');
  // A bot keeps its folder, so a message names none; a turn run elsewhere moves the head's folder.
  assert.equal('workspace' in sent.at(-1), false); assert.equal(sent.at(-1).model, 'alpha/two');
  p.setSend('steer'); await p.submit('now');
  assert.equal(sent.at(-1).delivery, 'steer'); assert.equal('workspace' in sent.at(-1), false); assert.equal('model' in sent.at(-1), false);
  assert.equal(sent.at(-1).expected_turn, 3, 'a steer is for the turn on screen');
  // A turn still waiting for a slot has not started, so it takes a queue, not a steer.
  p.upsert({ name: 'task', id: 2, provider: 'alpha', model: 'one', workspace: '/synthetic/task', status: 'ready', running_turn: null });
  await p.onEvent({ event: 'queued', bot: 'task', turn: 4, data: { status: 'ready' } });
  await p.submit('soon');
  assert.equal(sent.at(-1).delivery, 'queue'); assert.equal('expected_turn' in sent.at(-1), false);
});

test('a bot keeps its folder: only a bot without one is sent the app\'s', async () => {
  const sent = [];
  const p = shell({ request: async (op, q) => { sent.push(q); } });
  p.upsert({ name: 'loose', id: 3, provider: 'alpha', model: 'one' });
  p.S.selected = 'loose'; await p.submit('here');
  assert.equal(sent.at(-1).workspace, '/synthetic');
  // A turn's folder is not the bot's: a steer run elsewhere leaves the bot where it was.
  p.upsert({ name: 'task', id: 2, provider: 'alpha', model: 'one', workspace: '/synthetic/task' });
  await p.onEvent({ event: 'accepted', bot: 'task', turn: 5, data: { workspace: '/synthetic/steer' } });
  assert.equal(p.S.bots.get('task').workspace, '/synthetic/task');
});

test('a steer whose turn ended meanwhile is refused as stale, with a short message, and never queued', async () => {
  const sent = [];
  const p = shell({ request: async (op, q) => { sent.push(q); if (q.delivery === 'steer' && q.expected_turn !== 4) throw new Error('stale_turn: turn 3 is not running'); } });
  p.upsert({ name: 'task', id: 2, provider: 'alpha', model: 'one', status: 'running', running_turn: 3 });
  p.S.selected = 'task'; p.setSend('steer');
  await assert.rejects(p.submit('now'), /^Error: that turn ended; not steered$/);
  assert.equal(sent.length, 1);
  p.setSend('queue'); await assert.doesNotReject(p.submit('later'));
});

test('one menu per agent: side chat any time, stop while running, fork and delete at rest', () => {
  const p = shell();
  p.upsert({ name: 'busy', id: 1, provider: 'alpha', model: 'one', status: 'running', running_turn: 4 });
  p.upsert({ name: 'rest', id: 2, provider: 'alpha', model: 'one' });
  const state = (name) => Object.fromEntries(p.botMenuItems(name).filter((i) => i.act).map((i) => [i.act, !i.disabled]));
  assert.deepEqual(state('busy'), { 'side-chat': true, stop: true, fork: false, delete: false, steps: true });
  assert.deepEqual(state('rest'), { 'side-chat': true, stop: false, fork: true, delete: true, steps: true });
});

test('a side chat forks a running bot under it, beside, with its tools and folder, and takes the first message', async () => {
  const sent = [], storage = new Map(); let sideAtSubmit;
  const p = shell({ request: async (op, q) => { sent.push([op, q]); if (op === 'submit') sideAtSubmit = p.S.ui.side; return op === 'fork' ? { name: q.bot, id: 10 + sent.length, provider: 'alpha', model: 'one', workspace: q.workspace, created_by: q.created_by, created_by_id: q.created_by_id, allowed: q.allow } : { nodes: [], next_from: null }; } }, storage);
  p.upsert({ name: 'app.lead', id: 1, provider: 'alpha', model: 'one', workspace: '/synthetic', status: 'running', running_turn: 3, tools: ['shell', 'read', 'write', 'history'] });
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
  p.upsert({ name: 'task', id: 2, provider: 'alpha', model: 'one', status: 'running', running_turn: 4 });
  p.upsert({ name: 'other', id: 3, provider: 'alpha', model: 'one' });
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
  const p = shell({ request: async (op, q) => { sent.push([op, q]); return op === 'fork' ? { name: q.bot, id: 10 + sent.length, provider: 'alpha', model: 'one', created_by: q.created_by, created_by_id: q.created_by_id } : { nodes: [], next_from: null }; } });
  p.upsert({ name: 'app.lead', id: 1, provider: 'alpha', model: 'one' });
  p.upsert({ name: 'app.task', id: 2, provider: 'alpha', model: 'one', created_by: 'app.lead', created_by_id: 1 });
  p.upsert({ name: 'app.busy', id: 3, provider: 'alpha', model: 'one', status: 'running', running_turn: 1 });
  p.S.selected = 'app.lead';
  await p.fork('app.task'); await p.fork('app.task');
  const forks = sent.filter(([op]) => op === 'fork').map(([, q]) => q);
  assert.deepEqual(forks.map((q) => [q.source, q.bot, q.created_by, q.created_by_id]), [['app.task', 'app.task-fork', 'app.lead', 1], ['app.task', 'app.task-fork-2', 'app.lead', 1]]);
  assert.equal(p.S.ui.side, 'app.task-fork-2');
  await assert.rejects(p.fork('app.busy'), /bot_busy/);
  assert.equal(sent.filter(([op]) => op === 'fork').length, 2);
  // A task known only by its prefix forks under the coordinator, beside itself.
  p.upsert({ name: 'app.solo', id: 4, provider: 'alpha', model: 'one' }); p.tree();
  assert.equal(p.S.bots.get('app.solo').project, 'app');
  await p.fork('app.solo');
  const solo = sent.filter(([op]) => op === 'fork').at(-1)[1];
  assert.deepEqual([solo.created_by, solo.created_by_id], ['app.lead', 1]);
  // A root bot outside any project forks to a root beside it, not under itself.
  p.upsert({ name: 'loose', id: 5, provider: 'alpha', model: 'one' }); p.tree();
  await p.fork('loose');
  const loose = sent.filter(([op]) => op === 'fork').at(-1)[1];
  assert.equal('created_by' in loose, false);
});

test('a failed send comes back only to the bot it was for', async () => {
  let fail = null;
  const p = shell({ request: async (op) => { if (op === 'submit') { await new Promise((r) => { fail = r; }); throw new Error('daemon_unavailable'); } return { nodes: [], next_from: null }; } });
  for (const [name, id] of [['app.lead', 1], ['app.build', 2], ['app.test', 3]]) p.upsert({ name, id, provider: 'alpha', model: 'one', created_by: id > 1 ? 'app.lead' : null, created_by_id: id > 1 ? 1 : null });
  p.tree(); p.S.selected = 'app.lead';
  const doc = p.context.document, side = doc.getElementById('sideinput');
  await p.openBeside('app.build');
  side.value = 'for build';
  const sending = doc.getElementById('sideform').listeners.submit({ preventDefault() {} });
  await new Promise((r) => setImmediate(r));
  await p.openBeside('app.test');
  fail(); await sending;
  assert.equal(p.S.ui.side, 'app.test'); assert.equal(side.value, '', 'not restored under another bot');
});

test('fork names fit the daemon\'s 128-byte limit and forks work in the source\'s folder', async () => {
  const sent = [];
  const p = shell({ request: async (op, q) => { sent.push([op, q]); return op === 'fork' ? { name: q.bot, id: 10 + sent.length, provider: 'alpha', model: 'one' } : { nodes: [], next_from: null }; } });
  const long = 'a'.repeat(128);
  p.upsert({ name: long, id: 1, provider: 'alpha', model: 'one', workspace: '/synthetic/elsewhere' });
  p.S.selected = long;
  await p.fork(long); await p.fork(long);
  const forks = sent.filter(([op]) => op === 'fork').map(([, q]) => q);
  // The daemon starts a fork where its source is, so the app names no folder.
  assert.deepEqual(forks.map((q) => [q.bot.length, q.bot.slice(-7), 'workspace' in q]), [[128, 'aa-fork', false], [128, '-fork-2', false]]);
});

test('a new project creates its coordinator in the folder, in its role, writes its file once, and is not made twice', async () => {
  const calls = []; let written = false;
  const p = shell({
    project: async (dir) => ({ dir, name: 'weather', coordinator: 'weather.lead', model: null, file: written }),
    policy: async (dir, profile) => { calls.push(['policy', dir, profile]); return { instructions: 'rules', compaction_instructions: 'summary', model: 'alpha/role', tools: ['shell', 'wait'], note: 'test' }; },
    writeProject: async (q) => { calls.push(['write', q]); written = true; },
    request: async (op, q) => { calls.push([op, q]); return op === 'create' ? { name: q.bot, id: 7, provider: 'alpha', model: 'role', workspace: q.workspace } : { nodes: [], next_from: null }; },
  });
  await p.createProject('/synthetic/weather');
  const create = calls.find(([op]) => op === 'create')[1];
  // The coordinator profile composes the whole text; its model and tools apply when the project names none.
  assert.deepEqual(calls.find(([op]) => op === 'policy').slice(1), ['/synthetic/weather', 'coordinator']);
  assert.deepEqual([create.bot, create.workspace, create.model, create.instructions, Array.from(create.tools)], ['weather.lead', '/synthetic/weather', 'alpha/role', 'rules', ['shell', 'wait']]);
  assert.deepEqual({ ...calls.find(([op]) => op === 'write')[1] }, { dir: '/synthetic/weather', name: 'weather', model: 'alpha/role' });
  assert.equal(p.S.selected, 'weather.lead');
  const before = calls.length;
  await p.createProject('/synthetic/weather');
  assert.equal(calls.filter(([op]) => op === 'create').length, 1); assert.equal(calls.length, before);
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
});

test('a project name taken by another folder\'s coordinator is refused, and a refused model is never written', async () => {
  const calls = []; let fail = true, failWrite = false;
  const p = shell({
    project: async (dir) => ({ dir, name: dir.endsWith('taken') ? 'demo' : 'weather', coordinator: dir.endsWith('taken') ? 'demo.lead' : 'weather.lead', model: null, file: false }),
    policy: async () => ({ instructions: 'rules', compaction_instructions: 'summary', note: 'test' }),
    writeProject: async (q) => { calls.push(['write', q.dir, q.model]); if (failWrite) throw new Error('project_unwritable'); },
    request: async (op, q) => { calls.push([op, q.bot]); if (op === 'create' && fail) throw new Error('create_failed'); return op === 'create' ? { name: q.bot, id: 7, provider: 'alpha', model: 'one', workspace: q.workspace } : { nodes: [], next_from: null }; },
  });
  p.upsert({ name: 'demo.lead', id: 1, provider: 'alpha', model: 'one', workspace: '/synthetic/first' });
  p.S.selected = '';
  await assert.rejects(p.createProject('/synthetic/taken'), /demo\.lead already belongs to \/synthetic\/first/);
  assert.equal(p.S.selected, ''); assert.equal(calls.length, 0);
  await assert.rejects(p.createProject('/synthetic/weather'), /create_failed/);
  assert.deepEqual(calls.map(([op]) => op), ['create'], 'a model the daemon refuses is not saved to the folder');
  fail = false; failWrite = true;
  await assert.rejects(p.createProject('/synthetic/weather'), /project_unwritable/);
  assert.deepEqual(calls.map(([op]) => op), ['create', 'create', 'write'], 'the file follows an accepted coordinator');
  failWrite = false; await p.createProject('/synthetic/weather');
  assert.deepEqual(calls.at(-1), ['write', '/synthetic/weather', 'alpha/one'], 'a retry writes the missing file with the coordinator\'s model');
  assert.equal(calls.filter(([op]) => op === 'create').length, 2);
  assert.equal(p.S.selected, 'weather.lead');
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
  while (!events.some((e) => e.event === 'turn_finished')) events.push(...(await d.pull()).events);
  const nodes = events.filter((e) => e.event === 'message').map((e) => e.data.node);
  const { items: read } = await d.request('history_items', { bot: 'solo', nodes });
  const items = read.map((entry) => entry.item);
  const user = items.findIndex((i) => i.role === 'user');
  assert.equal(items[user].content[0].text, 'mention the wait op too');
  assert.match(items[user + 1].content[0].text, /^Noted: mention the wait op too\./);
  assert.equal(events.filter((e) => e.event === 'turn_finished').length, 1, 'the steer joined the running turn');
  d.close();
});

test('Escape in the finder never stops a turn, and a deleted bot takes its draft with it', async () => {
  const sent = [];
  const p = drafting({ request: async (op, q) => { sent.push(op); return { nodes: [], next_from: null }; } });
  p.upsert({ name: 'app.lead', id: 1, provider: 'alpha', model: 'one', status: 'running', running_turn: 1 });
  p.upsert({ name: 'app.task', id: 2, provider: 'alpha', model: 'one', created_by: 'app.lead', created_by_id: 1 });
  p.tree(); p.S.selected = 'app.lead'; p.followDrafts();
  const doc = p.context.document;
  doc.getElementById('pickerq').value = '';
  await doc.listeners.keydown({ key: 'Escape', target: { id: 'pickerq' }, preventDefault() {} });
  assert.deepEqual(sent.filter((op) => op === 'interrupt'), []);
  doc.getElementById('input').value = 'for lead';
  await p.openOnly('app.task'); doc.getElementById('input').value = 'for task';
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

test('a folded run names a timeout or a failed call; the finder reaches folded tasks; a side draft stays with its bot', async () => {
  const p = drafting({ request: async () => ({ nodes: [], next_from: null }) });
  for (const [out, want] of [[{ stdout: '', exit_code: null, success: false, timed_out: true }, 'timed out'], [{ stdout: '', exit_code: null, success: false }, 'failed'], [{ stdout: 'ok', exit_code: 0, success: true }, null]])
    assert.equal(p.entries({ type: 'function_call_output', call_id: 'c', output: JSON.stringify(out) })[0].err, want);
  for (const [name, id] of [['app.lead', 1], ['app.build', 2], ['app.test', 3]]) p.upsert({ name, id, provider: 'alpha', model: 'one', created_by: id > 1 ? 'app.lead' : null, created_by_id: id > 1 ? 1 : null });
  p.tree(); p.S.selected = 'app.lead'; p.S.ui.folded.add('app');
  p.context.document.getElementById('pickerq').value = 'build';
  assert.deepEqual(Array.from(p.pickerRows(), (r) => r.b.name), ['app.build'], 'a folded task is still found');
  const draft = p.context.document.getElementById('sideinput');
  await p.openBeside('app.build'); draft.value = 'for build only';
  await p.openBeside('app.test'); assert.equal(draft.value, '', 'another bot beside has its own draft');
  draft.value = 'for test only'; p.closeSide(); assert.equal(draft.value, '');
  await p.openBeside('app.build'); assert.equal(draft.value, 'for build only', 'a closed pane keeps its bot\'s draft');
  await p.openBeside('app.test'); assert.equal(draft.value, 'for test only');
});

test('swap carries each draft with its bot; a long wait list stays short in the head', async () => {
  const p = drafting({ request: async () => ({ nodes: [], next_from: null }) });
  for (const [name, id] of [['app.lead', 1], ['app.build', 2]]) p.upsert({ name, id, provider: 'alpha', model: 'one', created_by: id > 1 ? 'app.lead' : null, created_by_id: id > 1 ? 1 : null });
  p.tree(); p.S.selected = 'app.lead'; p.followDrafts();
  const main = p.context.document.getElementById('input'), side = p.context.document.getElementById('sideinput');
  await p.openBeside('app.build'); main.value = 'to lead'; side.value = 'to build';
  p.swap();
  assert.equal(p.S.selected, 'app.build'); assert.equal(main.value, 'to build');
  assert.equal(p.S.ui.side, 'app.lead'); assert.equal(side.value, 'to lead');
  p.upsert({ name: 'app.test', id: 3, provider: 'alpha', model: 'one', created_by: 'app.lead', created_by_id: 1 });
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
    while (!events.some((e) => e.event === 'turn_finished' && e.bot === bot)) events.push(...(await d.pull()).events);
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

test('a task card leaves the keyboard beside; swapping a folded task in unfolds its project', async () => {
  const p = shell({ request: async () => ({ nodes: [], next_from: null }) });
  for (const [name, id] of [['app.lead', 1], ['app.build', 2]]) p.upsert({ name, id, provider: 'alpha', model: 'one', created_by: id > 1 ? 'app.lead' : null, created_by_id: id > 1 ? 1 : null });
  p.tree(); p.S.selected = 'app.lead';
  const doc = p.context.document, focused = [];
  for (const id of ['input', 'sideinput']) doc.getElementById(id).focus = () => focused.push(id);
  const card = { dataset: { task: 'app.build' } };
  await doc.listeners.click({ target: { closest: (sel) => (sel === '[data-task]' ? card : sel === '.pane.main' ? {} : null) } });
  await p.tick();
  assert.equal(p.S.ui.side, 'app.build'); assert.equal(focused.at(-1), 'sideinput');
  p.S.ui.folded.add('app'); p.swap();
  assert.equal(p.S.selected, 'app.build'); assert.equal(p.S.ui.folded.has('app'), false);
  assert.ok(p.tree().some((n) => n.b?.name === 'app.build'), 'the selected task has a sidebar row');
});

test('a failed first message waits in the side chat\'s composer', async () => {
  const sent = [];
  const p = shell({ request: async (op, q) => {
    sent.push([op, q]);
    if (op === 'fork') return { name: q.bot, id: 20, provider: 'alpha', model: 'one', created_by: q.created_by, created_by_id: q.created_by_id, allowed: q.allow };
    if (op === 'submit') throw new Error('daemon_gone');
    return { nodes: [], next_from: null };
  } });
  p.upsert({ name: 'lead', id: 1, provider: 'alpha', model: 'one', status: 'running', running_turn: 3 });
  p.upsert({ name: 'task', id: 2, provider: 'alpha', model: 'one', status: 'running', running_turn: 4, created_by: 'lead', created_by_id: 1 });
  p.S.selected = 'lead'; await p.openBeside('task');
  // Asked from the side pane: the copy replaces its source there, and keeps the unsent message.
  const side = p.context.document.getElementById('sideinput'), main = p.context.document.getElementById('input');
  p.setSend('side');
  side.value = 'what now?'; await p.context.document.getElementById('sideform').listeners.submit({ preventDefault() {} });
  assert.equal(p.S.ui.side, 'task-side'); assert.equal(side.value, 'what now?'); assert.equal(main.value, '');
  assert.deepEqual(sent.filter(([op]) => op === 'resume'), [], 'a side chat needs nothing from its source first');
});

test('a bot in a linked worktree shows its branch in its head, read once per folder', async () => {
  const asked = [];
  const p = shell({ request: async () => ({ nodes: [], next_from: null }), branch: async (dir) => { asked.push(dir); return dir.endsWith('/worktrees/app.build') ? 'agent/app.build' : null; } });
  p.upsert({ name: 'app.lead', id: 1, provider: 'alpha', model: 'one', workspace: '/synthetic' });
  p.upsert({ name: 'app.build', id: 2, provider: 'alpha', model: 'one', workspace: '/home/u/.agent/worktrees/app.build', created_by: 'app.lead', created_by_id: 1 });
  p.tree();
  const head = p.context.document.getElementById('title'), b = p.S.bots.get('app.build');
  p.renderHead(head, b, 'main'); await new Promise((r) => setImmediate(r));
  p.renderHead(head, b, 'main');
  assert.match(head.innerHTML, /⎇ agent\/app\.build/);
  p.renderHead(head, p.S.bots.get('app.lead'), 'main'); await new Promise((r) => setImmediate(r));
  assert.doesNotMatch(head.innerHTML, /⎇/, 'a main checkout shows no branch');
  p.renderHead(head, b, 'side');
  assert.deepEqual(asked, ['/home/u/.agent/worktrees/app.build', '/synthetic'], 'each folder is read once');
  p.upsert({ name: 'app.build', id: 2, provider: 'alpha', model: 'one', workspace: '/synthetic', created_by: 'app.lead', created_by_id: 1 });
  p.renderHead(head, b, 'main'); await new Promise((r) => setImmediate(r));
  assert.doesNotMatch(head.innerHTML, /⎇/, 'a new folder is read again');
});

test('a draft stays with the bot it was typed for, and Enter sends it there even mid-switch', async () => {
  const sent = [];
  let release; const slow = new Promise((r) => { release = r; });
  const p = drafting({ request: async (op, q) => { if (op === 'history_nodes') await slow; if (op === 'submit') sent.push([q.bot, q.bot_id, q.prompt]); return { nodes: [], next_from: null }; } });
  for (const [name, id] of [['app.lead', 1], ['app.lead-side', 2]]) p.upsert({ name, id, provider: 'alpha', model: 'one', created_by: id > 1 ? 'app.lead' : null, created_by_id: id > 1 ? 1 : null });
  p.S.transcripts.get('app.lead-side') ?? p.transcript('app.lead-side').nodes;
  p.tree(); await p.openOnly('app.lead');
  const doc = p.context.document, main = doc.getElementById('input');
  main.value = 'Ship it.';
  // Sol's audit, step 7: the coordinator's text followed the selection to its side chat.
  release(); await p.openOnly('app.lead-side');
  assert.equal(main.value, '', 'the side chat has its own, empty composer');
  main.value = 'Only for the side chat.';
  await p.openOnly('app.lead');
  assert.equal(main.value, 'Ship it.');
  // Enter while another bot is being opened: the text goes to the bot it was typed for.
  const opening = p.openOnly('app.lead-side');
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
    attach: async () => { calls.push(['attach']); return { session: 2 }; },
    pull: () => new Promise(() => {}),
    project: async (dir) => ({ dir, name: 'weather', coordinator: 'weather.lead', model: null, file: false }),
    policy: async () => ({ instructions: 'rules', compaction_instructions: 'summary', note: 'test' }),
    writeProject: async (q) => { calls.push(['write', q.model]); },
    request: async (op, q) => { if (op === 'provider_models') return { providers: answer() }; if (op === 'bots') return { bots: [], next_after: null }; if (op === 'create') { calls.push(['create', q.bot, q.model]); return { name: q.bot, id: 9, provider: q.model.split('/')[0], model: q.model.split('/')[1], workspace: q.workspace }; } return { nodes: [], next_from: null }; },
  });
  p.S.config.model = null; p.S.attached = true;
  return { p, calls, env };
}

test('Bedrock is one provider running both its APIs, signed with the AWS login unless an API key is given', () => {
  const p = shell();
  assert.deepEqual([...p.providerSpecs('bedrock', { AWS_REGION: 'us-west-2' })], ['bedrock', 'bedrock-openai']);
  assert.deepEqual([...p.providerSpecs('bedrock', { AWS_REGION: 'eu-west-1', AWS_BEARER_TOKEN_BEDROCK: 'k' })], [
    'bedrock=anthropic,https://bedrock-mantle.eu-west-1.api.aws/anthropic/v1,AWS_BEARER_TOKEN_BEDROCK',
    'bedrock-openai=responses,https://bedrock-mantle.eu-west-1.api.aws/openai/v1,AWS_BEARER_TOKEN_BEDROCK']);
  assert.deepEqual([...p.providerSpecs('anthropic', { ANTHROPIC_API_KEY: 'k' })], ['anthropic']);
});

test('connecting a provider keeps the others, saves only what was typed, restarts the daemon and lists its models', async () => {
  const { p, calls, env } = settingsShell({ env: { AGENT_PROVIDER: 'openai', OPENAI_API_KEY: 'k' }, lists: { openai: { models: [{ id: 'gpt' }] }, bedrock: { models: [{ id: 'claude' }, { id: 'haiku' }] }, 'bedrock-openai': { models: [{ id: 'grok' }] } } });
  await p.openSetup();
  await assert.rejects(p.connectProvider('bedrock', { AWS_REGION: '', AWS_PROFILE: '', AWS_BEARER_TOKEN_BEDROCK: '' }), /Region is required/);
  assert.equal(calls.length, 0);
  await p.connectProvider('bedrock', { AWS_REGION: 'us-east-1', AWS_PROFILE: '', AWS_BEARER_TOKEN_BEDROCK: '' });
  // Only providers are saved; the empty key field keeps whatever key was saved.
  assert.deepEqual(calls.map(([c]) => c), ['save', 'restart', 'attach', 'discover']);
  assert.deepEqual({ ...calls[0][1] }, { AGENT_PROVIDER: 'openai bedrock bedrock-openai', AWS_REGION: 'us-east-1', AWS_PROFILE: null });
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
  // A Bedrock key, once saved, stays in use when the region changes and its field is left empty.
  await p.connectProvider('bedrock', { AWS_REGION: 'us-east-1', AWS_PROFILE: '', AWS_BEARER_TOKEN_BEDROCK: 'k' });
  await p.connectProvider('bedrock', { AWS_REGION: 'us-west-2', AWS_PROFILE: '', AWS_BEARER_TOKEN_BEDROCK: '' });
  assert.equal(env.AGENT_PROVIDER, 'openai bedrock=anthropic,https://bedrock-mantle.us-west-2.api.aws/anthropic/v1,AWS_BEARER_TOKEN_BEDROCK bedrock-openai=responses,https://bedrock-mantle.us-west-2.api.aws/openai/v1,AWS_BEARER_TOKEN_BEDROCK');
  assert.equal(env.AWS_BEARER_TOKEN_BEDROCK, 'k');
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
  assert.deepEqual({ ...calls[0][1] }, { AGENT_PROVIDER: null, AWS_BEARER_TOKEN_BEDROCK: null, OPENAI_API_KEY: '' });
});

test('a first launch with a provider but no agents opens setup on the first project', async () => {
  const { p } = settingsShell({ env: { AGENT_PROVIDER: 'openai', OPENAI_API_KEY: 'k' }, lists: { openai: { models: [{ id: 'gpt' }] } } });
  p.S.attached = false; p.S.setupSeen = false;
  await p.attach();
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(p.S.setup.open, true);
  assert.match(p.setupHTML(), /First project/);
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
  p.upsert({ name: 'lead', id: 1, provider: 'bedrock', family: 'anthropic', model: 'claude' });
  const items = p.modelMenuItems(p.S.bots.get('lead'), [{ id: 'bedrock/claude' }, { id: 'openai/gpt' }]);
  assert.deepEqual(Array.from(items, (i) => i.head ?? (i.sep ? '—' : i.label)), ['Amazon Bedrock', 'claude', '—', 'OpenAI', 'gpt']);
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
  const { p, calls } = settingsShell({ env: { AGENT_PROVIDER: 'openai' }, lists: { openai: { models: [] } } });
  p.context.Daemon.discoverModels = async () => ({ providers: { openai: { models: 0 } }, written: false, error: 'models_none_listed: openai: no usable models' });
  await p.openSetup();
  await p.refreshModels();
  const html = p.setupHTML();
  assert.match(html, /models_none_listed: openai: no usable models/);
  assert.match(html, /data-act="setup-refresh"/); assert.match(html, /No models listed yet/);
  // OPENAI_API_KEY from the shell: re-adding OpenAI with the field left empty keeps using it.
  p.S.setup.settings.keys = ['OPENAI_API_KEY'];
  await p.connectProvider('openai', { OPENAI_API_KEY: '' });
  assert.deepEqual({ ...calls.find(([c]) => c === 'save')[1] }, { AGENT_PROVIDER: 'openai' });
  await assert.rejects(p.connectProvider('anthropic', { ANTHROPIC_API_KEY: '' }), /API key is required/);
});

test('removing a provider while agents work asks once more before the restart stops them', async () => {
  const { p, calls } = settingsShell({ env: { AGENT_PROVIDER: 'openai anthropic', ANTHROPIC_API_KEY: 'k' }, lists: { openai: { models: [{ id: 'gpt' }] }, anthropic: { models: [{ id: 'claude' }] } } });
  await p.openSetup();
  p.upsert({ name: 'busy', id: 1, provider: 'openai', model: 'gpt', status: 'running', running_turn: 3 }); p.S.botsGen += 1;
  const press = () => p.act({ dataset: { act: 'setup-remove', v: 'anthropic' } });
  await press();
  assert.equal(calls.filter(([c]) => c === 'save').length, 0);
  assert.match(p.setupHTML(), /Remove anyway/); assert.match(p.setupHTML(), /Removing restarts the daemon/);
  await press();
  assert.equal(calls.filter(([c]) => c === 'save').length, 1);
});
