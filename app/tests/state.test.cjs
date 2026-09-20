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
    document: { getElementById(id) { if (!elements.has(id)) elements.set(id, element()); return elements.get(id); }, addEventListener() {},
      createElement: element, createTextNode: () => ({ data: '', appended: 0, appendData(s) { this.data += s; this.appended += s.length; } }) },
    window: { addEventListener() {} }, localStorage: { getItem() { return null; } },
    setTimeout(fn) { const id = ++timer; timers.set(id, fn); return id; }, clearTimeout(id) { timers.delete(id); }, setInterval() {},
  });
  let source = fs.readFileSync(require.resolve('../ui/app.js'), 'utf8');
  source = source.slice(0, source.indexOf('// ---------- boot ----------')) +
    'globalThis.app = { setRender: fn => { render = fn; }, S, rail, renderRail, transcript, upsert, onEvent, handle, pump, loadBatch, evict, itemsHTML, attach, lost, enqueue, load, cssEsc, esc, submit, interrupt, seat, botRowHTML, renderTail: typeof renderTail === "function" ? renderTail : null };\n})();';
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


test('waiting rail handles are escaped as text', () => {
  const p=page();p.upsert({name:'Bob',id:1});const b=p.S.bots.get('Bob');
  b.waitingOn=['turn:<img src=x onerror=alert(1)>/1'];
  const html=p.botRowHTML({b,depth:0,prefix:''},false);
  assert.ok(!html.includes('<img'));assert.match(html,/&lt;img/);
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
  await p.loadBatch('Bob');p.S.ui.output=true;
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
  assert.equal(live.itemsHTML(live.transcript('Bob')),replay.itemsHTML(replay.transcript('Bob')));
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
