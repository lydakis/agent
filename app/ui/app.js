// The Thread client. State is built from the daemon's event stream exactly
// as the terminal client builds it; rendering is the prototype's, verbatim.
(() => {
'use strict';
const $ = (id) => document.getElementById(id);
const GLYPH = { running: '●', waiting: '◐', paced: '◔', failed: '✘', idle: '○', queued: '◌', ready: '◌', interrupted: '✘' };
const LABEL = { running: 'working', waiting: 'waiting', paced: 'rate limited', failed: 'failed', idle: 'idle', queued: 'queued', ready: 'queued', interrupted: 'interrupted' };
const LAZY_ITEMS = 400;
// Decoded items kept around the reader's end of a transcript; bodies beyond it fold back into their
// nodes and a scroll toward them loads them again.
const WINDOW = 3 * LAZY_ITEMS;
const PEER_WINDOW = 300;
const DECODE_BYTES = 8 * 1024 * 1024;

const S = {
  bots: new Map(), transcripts: new Map(), selected: '', cursor: 0, live: false, attached: false, autoSelect: true,
  // Bumped whenever a bot is added, removed or changes status (botsGen), and when one is added or
  // removed (shapeGen), so the activity check and the rail's tree rebuild once per change instead of
  // scanning the fleet on every event.
  botsGen: 0, shapeGen: 0, deleted: new Set(),
  config: null, ui: { rail: false, peek: null, picker: false, pickerSel: 0, help: false, thoughts: false, output: false, toast: null },
};
const sessionKey = () => `agent:${S.config?.socket}|${S.config?.workspace}`;
const bot = (name) => S.bots.get(name);
const transcript = (name) => { if (!S.transcripts.has(name)) S.transcripts.set(name, { items: [], nodes: 0, thoughts: 0, longOut: 0, peers: [], anchor: 'end', gen: 0, text: '', thinking: '', thinkingSince: 0, streamingTurn: null, streamGen: 0 }); return S.transcripts.get(name); };
// Counters kept in step with the items, so the key bar never scans the history.
function count(t, it, d) {
  t.bytes = Math.max(0, (t.bytes || 0) + d * (it.bytes || 0));
  if (it.kind === 'node' || it.kind === 'tool_stub') t.nodes = Math.max(0, t.nodes + d);
  else if (it.kind === 'thought') t.thoughts = Math.max(0, t.thoughts + d);
  else if (it.kind === 'out' && it.text.split('\n', 3).length > 2) t.longOut = Math.max(0, t.longOut + d);
  else if (it.kind === 'peer' && d > 0 && !t.peers.includes(it.who)) t.peers.push(it.who);
}
const addItem = (t, it) => {
  if (it.kind === 'peer' && t.peers.includes(it.who)) return;
  if (it.kind === 'note') it.bytes = it.text.length * 2;
  count(t, it, 1); t.items.push(it);
  if (t.items.length % LAZY_ITEMS === 0 || t.bytes > DECODE_BYTES) evict(t);
  if (it.kind === 'peer' && t.peers.length > PEER_WINDOW * 2) {
    const keep = new Set(t.peers.slice(-PEER_WINDOW));
    let removed = 0;
    t.items = t.items.filter((entry) => { if (entry.kind !== 'peer' || keep.has(entry.who)) return true; removed++; return false; });
    t.peers = [...keep];
    const gap = t.items.find((entry) => entry.kind === 'peer_gap');
    if (gap) gap.total += removed; else t.items.unshift({ kind: 'peer_gap', total: removed });
    t.gen += 1;
  }
};
// Replace the bare node at `at` with what it decoded to; each entry remembers its node.
function decodeAt(t, at, entries) {
  const bare = t.items[at]; if (bare.kind !== 'node') return;
  if (!entries.length) entries = [{kind:'backing',turn:bare.turn}];
  count(t, bare, -1);
  for (const e of entries) { e.from = bare.node; e.fromCall = bare.callId; count(t, e, 1); }
  t.items.splice(at, 1, ...entries);
  t.gen += 1;
}
// Fold decoded bodies outside the window back into their nodes. Whole nodes only: a run split by the
// boundary folds entirely, so a later decode cannot sit next to its own remainder.
function evict(t) {
  const len = t.items.length;
  let totalBytes = 0; for (const it of t.items) totalBytes += it.bytes || 0;
  if (len <= WINDOW + LAZY_ITEMS && totalBytes <= DECODE_BYTES) return;
  let lo = 0, hi = len, bytes = 0;
  if (t.anchor === 'end') {
    lo = Math.max(0, len - WINDOW);
    for (let i = len - 1; i >= lo; i--) { bytes += t.items[i].bytes || 0; if (bytes > DECODE_BYTES) { lo = i + 1; break; } }
  } else {
    lo = Math.max(0, t.items.findIndex((it) => !['node','tool_stub','history','peer_gap'].includes(it.kind)));
    hi = Math.min(len, lo + WINDOW);
    for (let i = lo; i < hi; i++) { bytes += t.items[i].bytes || 0; if (bytes > DECODE_BYTES) { hi = i; break; } }
  }
  const source = t.items; const result = [];
  let earlierNotes = 0, laterNotes = 0;
  const nodeOf = (it) => it.kind === 'node' ? it.node : it.from;
  const folded = new Set();
  for (let i = 0; i < source.length; i++) if (i < lo || i >= hi) { const node = nodeOf(source[i]); if (node != null) folded.add(node); }
  const append = (it) => {
    const prev = result[result.length - 1];
    if (it.kind === 'history' && prev?.kind === 'history' && !!it.forward === !!prev.forward) {
      prev.next = Math.max(prev.next, it.next); prev.min = Math.min(prev.min ?? 0, it.min ?? 0);
    } else result.push(it);
  };
  for (let i = 0; i < source.length;) {
    const it = source[i], node = nodeOf(it); let end = i + 1;
    if (node != null) while (end < source.length && nodeOf(source[end]) === node) end++;
    if (it.kind === 'note_gap' || (it.kind === 'note' && node == null && (i < lo || i >= hi))) {
      count(t, it, -1);
      if (i >= hi) laterNotes += it.total ?? 1; else earlierNotes += it.total ?? 1;
    } else if (node != null && (folded.has(node) || end > hi)) {
      for (let j = i; j < end; j++) count(t, source[j], -1);
      append({kind:'history', next:node, min:node, loaded:true, forward:i >= hi});
    } else if (it.kind === 'history') {
      append({...it, forward: i >= hi ? true : i < lo ? false : it.forward});
    } else if (it.kind === 'tool' && (i < lo || i >= hi)) {
      const {args, ...stub} = it; stub.kind = 'tool_stub'; count(t, stub, 1); append(stub);
    } else for (let j = i; j < end; j++) append(source[j]);
    i = end;
  }
  if (earlierNotes) result.unshift({kind:'note_gap',total:earlierNotes});
  if (laterNotes) result.push({kind:'note_gap',total:laterNotes,later:true});
  t.items = result; normalizeRanges(t); t.gen += 1;
}
// Ranges may straddle peer cards when one provider node holds several calls.
// Union overlapping intervals globally, retaining only the first placeholder.
function normalizeRanges(t) {
  const ranges = t.items.filter(it => it.kind === 'history').sort((a,b) => (a.min ?? 0) - (b.min ?? 0));
  const removed = new Set(); let previous = null;
  const high = r => r.next - (r.exclusive ? 1 : 0);
  for (const r of ranges) {
    if (previous && (r.min ?? 0) <= high(previous)) {
      if (high(r) > high(previous)) { previous.next = r.next; previous.exclusive = r.exclusive; }
      previous.seed ||= r.seed;
      removed.add(r);
    } else previous = r;
  }
  if (removed.size) t.items = t.items.filter(it => !removed.has(it));
  t.history = t.items.find(it => it.kind === 'history') ?? null;
  t.seed = t.items.find(it => it.kind === 'history' && it.seed) ?? null;
}
function pushNode(t, it) {
  // Snapshot history and replay can arrive in either order. Replay owns its
  // visible suffix; the snapshot marker points strictly before its first node.
  if (t.seed && it.node <= t.seed.next) { t.seed.next = it.node; t.seed.exclusive = true; }
  if (t.items.some(x => (x.kind === 'node' ? x.node : x.from) === it.node)) return;
  addItem(t, it);
}
function seedHistory(record) {
  if (typeof record.head !== 'number') return;
  const t = transcript(record.name); if (t.seeded) return; t.seeded = true;
  const ids = t.items.map(it => it.kind === 'node' ? it.node : it.from).filter(id => id != null);
  const first = ids.length ? Math.min(...ids) : null;
  t.seed = {kind:'history',next:first ?? record.head,exclusive:first != null,seed:true};
  t.items.unshift(t.seed); normalizeRanges(t); t.gen += 1;
}
const glyphOf = (status) => GLYPH[status] || '✘';
const labelOf = (status) => LABEL[status] || 'failed';
const fmt = (ms) => { const s = Math.max(0, Math.round(ms / 1000)); return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m${String(s % 60).padStart(2, '0')}s`; };
// Safe in text and inside a quoted attribute alike: names and call ids come from providers and land in both.
const esc = (s) => String(s).replace(/[&<>"'\r]/g, (c) => ({ '\r': '&#13;', '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
function toast(text, ms = 2200) { S.ui.toast = text; render(); setTimeout(() => { if (S.ui.toast === text) { S.ui.toast = null; render(); } }, ms); }

// ---------- bots ----------
function upsert(record) {
  if (!record?.name) return;
  // Same name, different identity: everything known about the old bot belongs to the old bot.
  const known = bot(record.name);
  if (known && known.id != null && record.id != null && known.id !== record.id) { forgetBot(record.name); }
  const b = bot(record.name) || { name: record.name, id: null, parent: null, waitingOn: [], turnStarted: 0, elapsed: 0 };
  if (record.id != null) b.id = record.id;
  if (!known || known !== b) S.shapeGen += 1;
  S.botsGen += 1;
  b.status = record.status === 'completed' ? 'idle' : (record.status || 'idle');
  b.runningTurn = record.running_turn ?? null;
  b.model = `${record.provider ?? '?'}/${record.model ?? '?'}`;
  b.workspace = record.workspace ?? null;
  if (record.created_by) { b.parent = record.created_by; b.parentId = record.created_by_id ?? null; }
  S.bots.set(b.name, b);
  seedHistory(record);
}
// The creator, when the bot holding that name now is the identity that did the creating. A later
// bot reusing the name is a stranger, and a creator the store could not resolve links to nothing.
function creatorOf(b) { const p = b.parent && b.parentId != null ? S.bots.get(b.parent) : null; return p && p.id === b.parentId ? p : null; }
function forgetBot(name) {
  const parent = bot(name) && creatorOf(bot(name));
  const t = parent && S.transcripts.get(parent.name);
  if (t) { t.items = t.items.filter(it => it.kind !== 'peer' || it.who !== name); t.peers = t.peers.filter(who => who !== name); t.gen += 1; }
  S.bots.delete(name); S.transcripts.delete(name); if (S.ui.peek === name) S.ui.peek = null;
}
async function refreshBot(name) { try { upsert(await Daemon.request('resume', { bot: name })); } catch (_) {} }
const ACTIVE = new Set(['running', 'waiting', 'paced', 'queued', 'ready']);
const isActive = (status) => ACTIVE.has(status);
// Only the agent CLI's detached run yields the handle JSON a peer card already shows.
// Each shell segment on its own: the executable must be the agent CLI, its first argument `run`,
// and `--detach` among the rest before `--`. A command that merely prints those words does not count.
const spawnsPeer = (command) => command.split(/[;|&\n]/).some((segment) => {
  const tokens = segment.trim().split(/\s+/).map((t) => t.replace(/^["']|["']$/g, ''));
  const exe = tokens[0] ?? '';
  const isAgent = exe === '$AGENT_BIN' || exe === '${AGENT_BIN}' || exe === 'agent' || exe.endsWith('/agent');
  if (!isAgent || tokens[1] !== 'run') return false;
  const end = tokens.indexOf('--', 2);
  return tokens.slice(2, end < 0 ? undefined : end).includes('--detach');
});
function tree() {
  // One pass builds the children index; an explicit stack walks it, so a deep delegation chain
  // costs one prefix string per row and no recursion.
  const children = new Map();
  for (const b of S.bots.values()) { const key = creatorOf(b)?.name ?? null; if (!children.has(key)) children.set(key, []); children.get(key).push(b); }
  const out = []; const seen = new Set(); const stack = [];
  const pushKids = (parent, depth, cont) => {
    const kids = (children.get(parent) ?? []).filter((b) => !seen.has(b.name));
    for (let i = kids.length - 1; i >= 0; i--) stack.push([kids[i], depth, i === kids.length - 1, cont]);
  };
  const walk = () => {
    while (stack.length) {
      const [b, depth, last, cont] = stack.pop();
      if (seen.has(b.name)) continue; seen.add(b.name);
      const prefix = depth === 0 ? '' : cont + (last ? '└ ' : '├ ');
      out.push({ b, depth, prefix });
      // The continuation stops growing past a few levels: a chain of thousands must not cost thousands per row.
      pushKids(b.name, depth + 1, depth === 0 ? '' : depth > 6 ? cont : cont + (last ? '  ' : '│ '));
    }
  };
  pushKids(null, 0, ''); walk();
  // Anything the roots do not reach is rooted where it stands: one pass, nothing hidden.
  for (const b of S.bots.values()) if (!seen.has(b.name)) { stack.push([b, 0, true, '']); walk(); }
  return out;
}
function callSummary(name, args) {
  let a = {}; try { a = JSON.parse(args) ?? {}; } catch (_) {}
  let s = name === 'shell' ? a.command ?? '' : ['read', 'write', 'edit'].includes(name) ? a.path ?? '' : name === 'wait' ? (Array.isArray(a.handles) ? a.handles : []).filter((h) => typeof h === 'string').map((h) => h.replace(/^turn:/, '')).join(', ') : args;
  return String(s).split('\n')[0].slice(0, 300);
}

// ---------- events ----------
const FLEET_EVENTS = new Set(['created', 'forked', 'accepted', 'queued', 'turn_waiting', 'turn_paced', 'turn_resumed', 'turn_finished', 'deleted']);
const SHAPE_EVENTS = new Set(['created', 'forked', 'deleted']);
const NAMELESS = new Set(['created', 'forked', 'deleted', 'pruned', 'follow_live', 'follow_lagged']);
async function onEvent(ev) {
  const kind = ev.event, name = ev.bot ?? '', turn = ev.turn ?? null, data = ev.data ?? {};
  if ((kind === 'created' || kind === 'forked') && (typeof data.provider !== 'string' || !data.provider)) throw new Error('invalid_created_event: provider required');
  if (typeof ev.cursor === 'number') S.cursor = Math.max(S.cursor, ev.cursor);
  // The replay runs while the snapshot pages: a bot an event names before its record arrives gets a
  // seat now, so the event's state is kept; the record fills in what events do not carry.
  if (name && name !== '*' && !NAMELESS.has(kind) && !S.bots.has(name)) { S.bots.set(name, { name, id: null, parent: null, parentId: null, waitingOn: [], turnStarted: 0, elapsed: 0, status: 'idle', runningTurn: null, model: '?', workspace: null }); S.shapeGen += 1; }
  if (name && bot(name)) bot(name).touched = S.session;
  switch (kind) {
    case 'follow_live': {
      S.live = true;
      if (S.autoSelect) { const first = tree()[0]; if (first) S.selected = first.b.name; }
      S.autoSelect = false;
      break;
    }
    // Attaching queues its own load on the chain this handler runs in; done here it would wait on itself.
    case 'follow_lagged': lost('event stream lagged; attaching again'); retryAttach(0); return true;
    case 'text_delta': { const t = transcript(name); t.streamingTurn = turn; t.text += ev.text ?? ''; break; }
    case 'thinking_delta': { const t = transcript(name); t.streamingTurn = turn; if (!t.thinkingSince) t.thinkingSince = Date.now(); t.thinking += ev.text ?? ''; break; }
    case 'created': case 'forked': {
      // The event carries the record's list fields, so a burst of creations costs no request each. A
      // bot the snapshot already holds keeps its record; the event says the same thing.
      S.deleted.delete(name);
      const known = bot(name);
      if (!known || known.id == null || known.id !== data.id) upsert({ name, ...data });
      bot(name).touched = S.session;
      const parent = bot(name) && creatorOf(bot(name));
      if (parent) addItem(transcript(parent.name), { kind: 'peer', who: name, turn: parent.runningTurn ?? null });
      if (kind === 'forked') {
        const t = transcript(name);
        if (typeof data.checkpoint === 'number') { t.history = { kind: 'history', next: data.checkpoint }; addItem(t, t.history); }
        addItem(t, { kind: 'note', text: `forked from ${data.source ?? '?'}`, turn: null });
      }
      break;
    }
    case 'accepted': {
      const b = bot(name); if (b) { b.status = 'running'; b.runningTurn = turn; b.waitingOn = []; b.turnStarted = S.live ? Date.now() : 0; b.elapsed = 0; }
      if (typeof data.node === 'number') pushNode(transcript(name), { kind: 'node', node: data.node, turn });
      break;
    }
    case 'queued': {
      // `ready` waits for a daemon-wide slot with nothing else running on the bot; `queued` sits behind its own turn.
      const b = bot(name); const behindOwn = !!b && (b.runningTurn !== null || isActive(b.status));
      if (b && !behindOwn) b.status = data.status ?? 'queued';
      addItem(transcript(name), { kind: 'note', text: behindOwn ? 'queued behind the running turn' : 'queued for a slot', turn });
      break;
    }
    case 'message': {
      const t = transcript(name);
      t.callNode = data.node;
      if (t.streamingTurn === turn) {
        if (t.thinking) { addItem(t, { kind: 'thought', from: data.node, text: t.thinking, secs: t.thinkingSince ? Math.round((Date.now() - t.thinkingSince) / 1000) : 0, turn }); t.thinking = ''; t.thinkingSince = 0; }
        t.text = ''; t.streamGen += 1;
      }
      if (typeof data.node === 'number') pushNode(t, { kind: 'node', node: data.node, turn });
      break;
    }
    case 'tool_started': {
      const args = data.arguments ?? '';
      let parsed = {}; try { parsed = JSON.parse(args) ?? {}; } catch (_) {}
      const tname = data.name ?? 'tool';
      const t = transcript(name);
      const row = { kind: 'tool', from: t.callNode, callId: data.call_id, name: tname, summary: callSummary(tname, args), args, background: tname === 'shell' && parsed.background === true, spawns: tname === 'shell' && spawnsPeer(String(parsed.command ?? '')), done: false, started: S.live ? Date.now() : 0, took: 0, turn };
      let existing = null;
      for (let i = t.items.length - 1; i >= 0; i--) {
        const it = t.items[i]; if (it.turn !== turn) break;
        if (it.kind === 'tool' && it.callId === data.call_id) { existing = it; break; }
      }
      if (existing) {
        row.from = existing.from ?? row.from;
        if (data.arguments_truncated) { row.summary = existing.summary; row.background = existing.background; row.spawns = existing.spawns; }
        Object.assign(existing, row); t.gen += 1; } else addItem(t, row);
      break;
    }
    case 'tool_completed': {
      const t = transcript(name);
      let call = null;
      for (let i = t.items.length - 1; i >= 0; i--) { const it = t.items[i]; if ((it.kind === 'tool' || it.kind === 'tool_stub') && it.turn === turn && it.callId === data.call_id) { call = it; break; } }
      if (call) { call.done = true; if (call.started) call.took = Date.now() - call.started; call.started = 0; patchItem(name, `[data-call="${cssEsc(call.callId)}"]`, call); }
      if (typeof data.node === 'number') {
        pushNode(t, { kind: 'node', node: data.node, callId: data.call_id, turn });
        if (call && (call.background || call.name === 'wait') && await loadWaitOrProc(name, data.node, call)) {
          // The node is spent: the cards show its result, and a later lazy load must not decode it again.
          // A failed fetch keeps it for the next session's lazy load instead.
          const pos = t.items.findLastIndex((it) => it.kind === 'node' && it.node === data.node);
          if (pos >= 0) decodeAt(t, pos, []);
        }
      }
      break;
    }
    case 'turn_waiting': { const b = bot(name); if (b) { b.status = 'waiting'; b.waitingOn = data.handles ?? []; } break; }
    case 'turn_paced': { const b = bot(name); if (b) b.status = 'paced'; break; }
    case 'turn_resumed': { const b = bot(name); if (b) { b.status = 'running'; b.waitingOn = []; } break; }
    case 'steered': addItem(transcript(name), { kind: 'note', text: 'steered into the running turn', turn }); break;
    case 'turn_finished': {
      const status = data.status ?? '?';
      const b = bot(name);
      // A steer absorbed into a running turn finishes as its own turn while that turn goes on.
      if (b && (b.runningTurn === null || b.runningTurn === turn)) { b.runningTurn = null; b.waitingOn = []; if (b.turnStarted) b.elapsed = Date.now() - b.turnStarted; b.turnStarted = 0; b.status = status === 'completed' || status === 'steered' ? 'idle' : status; }
      const t = transcript(name);
      if (t.streamingTurn === turn) { t.text = ''; t.thinking = ''; t.thinkingSince = 0; t.streamingTurn = null; t.streamGen += 1; }
      if (status !== 'completed' && status !== 'steered') addItem(t, { kind: 'note', text: data.error ? `${status}: ${data.error}${data.detail ? ': ' + data.detail : ''}` : status, turn });
      // A background command may outlive its turn; only a wait result says how it ended.
      break;
    }
    case 'deleted': {
      if (S.snapshot) S.deleted.add(name);
      forgetBot(name); if (S.selected === name) S.selected = S.bots.keys().next().value ?? ''; break;
    }
    case 'pruned': {
      // A `follow *` replay reports a retention gap with bot "*": a notice about the store, not a transcript.
      if (name === '*') toast(`activity events before cursor ${ev.before ?? 0} were pruned; durable history is still available`, 5000);
      else addItem(transcript(name), { kind: 'note', text: 'earlier activity events pruned; durable history remains available', turn: null });
      break;
    }
    default: break;
  }
}
async function loadWaitOrProc(name, node, call) {
  let item; try { item = await Daemon.request('item', { bot: name, node }); } catch (_) { return false; }
  return applyWaitOrProc(name, item, call, node);
}
// Decode a background start (a proc handle) or a wait result into the cards; also reached by a retried load.
function addProc(t, item) {
  const at = t.items.findIndex((it) => (it.kind === 'node' ? it.node : it.from) === item.from);
  if (at < 0) addItem(t, item);
  else { count(t, item, 1); t.items.splice(at, 0, item); t.gen += 1; }
}
function applyWaitOrProc(name, item, call, node) {
  const output = item.output ?? item.content?.[0]?.content ?? '';
  let value; try { value = JSON.parse(output); } catch (_) { return false; }
  if (!value || typeof value !== 'object') return false;
  let consumed = false;
  const t = transcript(name);
  if (call.background || (typeof value.handle === 'string' && value.handle.startsWith('proc:'))) {
    // A decode seen twice adds no second card.
    if (typeof value.handle === 'string' && value.handle.startsWith('proc:')) {
      consumed = true;
      const existing = t.items.find((it) => it.kind === 'proc' && it.handle === value.handle);
      if (existing) { if (call.summary) existing.cmd = call.summary; t.gen += 1; }
      else addProc(t, { kind: 'proc', from: node, callId: call.callId, handle: value.handle, cmd: call.summary ?? value.handle, done: null, turn: call.turn ?? null });
    }
  } else if (value.results) {
    for (const [handle, result] of Object.entries(value.results)) {
      if (!handle.startsWith('proc:') || !result || typeof result !== 'object' || result.pending) continue;
      // History is fetched newest first, sometimes in separate batches. Keep the terminal card
      // even before its start is decoded; that start fills in its command without clearing done.
      let it = t.items.find((entry) => entry.kind === 'proc' && entry.handle === handle);
      if (!it) { it = { kind: 'proc', from: node, callId: call.callId, handle, cmd: handle, done: null, turn: call.turn ?? null }; addProc(t, it); }
      consumed = true;
      if (it.resultNode != null && node != null && it.resultNode > node) continue;
      it.resultNode = node;
      const out = String(result.stdout ?? result.output ?? '');
      queueMicrotask(() => patchItem(name, `[data-proc="${cssEsc(handle)}"]`, it));
      // A process can end without an exit status: a spawn failure, a timeout, an output limit. Say which.
      if (result.error) it.done = result.detail ? `${result.error}: ${result.detail}` : String(result.error);
      else if (typeof result.exit_code === 'number' && result.exit_code !== 0) it.done = `exit ${result.exit_code}`;
      else if (result.success === false) it.done = 'failed';
      else it.done = tailOf(out);
    }
    // Keep the ordinary result row: it preserves stdout, stderr and errors.
    consumed = false;
  }
  return consumed;
}
function storedTool(name, callId, args) {
  let parsed = {}; try { parsed = JSON.parse(args) ?? {}; } catch (_) {}
  return { kind: 'tool', name, callId, summary: callSummary(name, args), background: name === 'shell' && parsed.background === true, spawns: name === 'shell' && spawnsPeer(String(parsed.command ?? '')), done: true, started: 0, took: 0 };
}
function entries(item) {
  const out = [];
  const text = (content, keys) => Array.isArray(content) ? content.filter((p) => keys.includes(p.type)).map((p) => p.text ?? '').join('') : typeof content === 'string' ? content : '';
  const shell = (o) => { let v; try { v = JSON.parse(o); } catch (_) { return o; } if (!v || typeof v !== 'object' || !('stdout' in v)) return o; let s = (v.stdout ?? '').trimEnd(); if (v.stderr?.trim()) s += (s ? '\n' : '') + 'stderr: ' + v.stderr.trimEnd(); if (v.exit_code) s += (s ? '\n' : '') + `exit ${v.exit_code}`; return s || '(no output)'; };
  if (item.type === 'function_call_output') return [{ kind: 'out', callId: item.call_id, raw: item.output ?? '', text: shell(item.output ?? '') }];
  if (item.type === 'function_call') return [storedTool(item.name, item.call_id, item.arguments)];
  if (item.type === 'reasoning') { const s = text(item.summary, ['summary_text']); if (s) out.push({ kind: 'thought', text: s, secs: 0 }); return out; }
  if (item.role === 'user') {
    if (Array.isArray(item.content)) { let t = ''; for (const p of item.content) { if (p.type === 'tool_result') out.push({ kind: 'out', callId: p.tool_use_id, raw: text(p.content, ['text']), text: shell(text(p.content, ['text'])) }); else if (p.type === 'text' || p.type === 'input_text') t += p.text ?? ''; } if (t) out.unshift({ kind: 'user', text: t }); }
    else out.push({ kind: 'user', text: text(item.content, ['text']) });
  } else if (item.role === 'assistant' && Array.isArray(item.content)) {
    let t = ''; for (const p of item.content) { if (p.type === 'tool_use') out.push(storedTool(p.name, p.id, JSON.stringify(p.input))); else if (p.type === 'thinking' && p.thinking) out.push({ kind: 'thought', text: p.thinking, secs: 0 }); else if (p.type === 'text' || p.type === 'output_text') t += p.text ?? ''; } if (t) out.push({ kind: 'text', text: t });
  }
  return out;
}
async function loadInherited(name, older) {
  const t = S.transcripts.get(name); if (!t) return;
  const ranges = t.items.filter((it) => it.kind === 'history');
  if (!ranges.length) return;
  const first = t.items.findIndex((it) => !['node','tool_stub','history','peer_gap'].includes(it.kind));
  const marker = older ? ranges.find((r) => t.items.indexOf(r) <= Math.max(first, 0)) : ranges.findLast((r) => !r.loaded || (r.forward && t.anchor === 'end'));
  if (!marker || (!older && !marker.forward && t.items.length > WINDOW)) return;
  const at = t.items.indexOf(marker), session = S.session;
  let page;
  try { page = await Daemon.request('history_nodes', { bot: name, from: marker.next, min_node: marker.min ?? null, oldest_first: !!marker.forward, limit: LAZY_ITEMS }); }
  catch (e) { if (S.transcripts.get(name) === t) toast(`history: ${e?.message ?? e}`); return; }
  if (S.transcripts.get(name) !== t || S.session !== session) return;
  const present = new Set(t.items.map(it => it.kind === 'node' ? it.node : it.from).filter(id => id != null));
  const nodes = page.nodes.slice().reverse().filter(n => !(marker.exclusive && n.node === marker.next) && !present.has(n.node)).map((n) => ({ kind: 'node', node: n.node, turn: n.turn ?? null }));
  for (const it of nodes) count(t, it, 1);
  let replacement;
  if (marker.forward) replacement = [...nodes, ...(page.next_newer == null ? [] : [{...marker, min: page.next_newer, loaded: true}])];
  else replacement = [...(page.next_from == null ? [] : [{...marker, next: page.next_from, exclusive:false, loaded: true}]), ...nodes];
  t.items.splice(at, 1, ...replacement); normalizeRanges(t); t.gen += 1;
}
async function load(name, older = false) {
  await loadInherited(name, older);
  // One batch of the newest bare nodes: what the pane can show. Scrolling up asks for the next
  // batch, so a long history is materialized only as far as someone reads.
  await loadBatch(name);
}
async function loadBatch(name) {
  const t = S.transcripts.get(name); if (!t || !t.nodes) return false;
  // At the end: the newest bare nodes inside the window. At the top: the newest bare nodes above the
  // oldest decoded item, so the reader's next screen fills and nothing folded at the far end is fetched.
  let lo = 0, hi = t.items.length;
  if (t.anchor === 'end') lo = Math.max(0, hi - WINDOW);
  else { const first = t.items.findIndex((it) => it.kind !== 'node' && it.kind !== 'tool_stub' && it.kind !== 'history'); if (first > 0) hi = first; }
  const pending = [];
  for (let i = hi - 1; i >= lo && pending.length < LAZY_ITEMS; i--) if (t.items[i].kind === 'node' || t.items[i].kind === 'tool_stub') pending.push([i, t.items[i]]);
  if (!pending.length) return false;
  let progressed = false, bytes = 0;
  const ids = [...new Set(pending.filter(([,it]) => it.kind === 'node').map(([,it]) => it.node))];
  let fetched = new Map(), failure = null;
  if (ids.length) {
    try { const page = await Daemon.request('history_items', {bot:name,nodes:ids}); fetched = new Map(page.items.map(row => [row.node,row.item])); }
    catch (e) { failure = String(e?.message ?? e); }
  }
  const knownCalls = new Set(t.items.filter((it) => it.kind === 'tool' || it.kind === 'tool_stub').map((it) => JSON.stringify([it.turn, it.callId])));
  for (const [, it] of pending) {
    const index = t.items.indexOf(it); if (index < 0) continue;
    if (it.kind !== 'tool_stub' && !failure && !fetched.has(it.node)) continue;
    const r = it.kind === 'tool_stub' ? {stub:true} : failure ? {err:failure} : {ok:fetched.get(it.node)};
    if (S.transcripts.get(name) !== t) return false;
    if (r.stub) { count(t, it, -1); it.kind = 'tool'; t.gen += 1; progressed = true; continue; }
    // A lost session is not the item's fault: the node stays and the next attach fetches it. Anything else is final.
    if (r.err && /daemon_disconnected|detached|^io\b/.test(r.err)) break;
    progressed = true;
    const es = r.ok ? entries(r.ok) : [{ kind: 'note', text: `node ${it.node}: ${r.err}` }];
    const rep = [];
    for (const e of es) {
      if (e.kind === 'tool' && it.turn != null && knownCalls.has(JSON.stringify([it.turn, e.callId]))) {
        const live = t.items.find((row) => row.kind === 'tool' && row.turn === it.turn && row.callId === e.callId);
        if (live) { live.summary = e.summary; live.background = e.background; live.spawns = e.spawns; live.from = it.node; }
        continue;
      }
      if (e.kind === 'thought') { const prev = t.items[index - 1]; if (prev && prev.kind === 'thought' && prev.turn === it.turn && rep.length === 0) { prev.text = e.text; continue; } }
      rep.push({ ...e, callId: e.callId ?? it.callId, turn: it.turn });
    }
    const size = r.ok ? JSON.stringify(r.ok).length * 2 : 0;
    if (rep.length) rep[0].bytes = size;
    decodeAt(t, t.items.indexOf(it), rep);
    bytes += size;
    if (bytes >= DECODE_BYTES) break;
  }
  // Call nodes can arrive after their outputs during reverse paging. Reconcile
  // the bounded window once all requested nodes have been decoded.
  const calls = new Map(t.items.filter(it => it.kind === 'tool').map(it => [JSON.stringify([it.turn,it.callId]),it]));
  for (const out of t.items.slice()) {
    const call = calls.get(JSON.stringify([out.turn,out.callId]));
    if (out.kind === 'proc' && call?.background) { out.cmd = call.summary; continue; }
    if (out.kind !== 'out' || !out.raw) continue;
    const specialized = call || {name:'wait',turn:out.turn,callId:out.callId};
    if (applyWaitOrProc(name, {output:out.raw}, specialized, out.from)) {
      const at = t.items.indexOf(out); if (at >= 0) { count(t,out,-1); t.items[at] = {kind:'backing',from:out.from,turn:out.turn}; t.gen += 1; }
    }
  }
  evict(t);
  return progressed;
}
async function loadVisible() {
  await load(S.selected);
  if (S.ui.peek && S.ui.peek !== S.selected) await load(S.ui.peek);
  // Cards on screen show their peer's last line, so those peers load too.
  for (const who of peers().slice(-12)) if (who !== S.selected && who !== S.ui.peek) await load(who);
}

// Events and loads run one at a time, in arrival order, like the terminal
// client's loop. Overlapping handlers would mark tools done before the
// creator lookup and splice the same transcript twice.
let chain = Promise.resolve();
function enqueue(job) { chain = chain.then(job, job); return chain; }

// ---------- lifecycle ----------
// Events are pulled from the core a batch at a time and applied before the next pull, so the pipeline
// from the daemon to the screen is bounded end to end: the transport's queue, then one batch here.
async function pump(session) {
  for (;;) {
    let batch;
    try { batch = await Daemon.pull(session); } catch (e) { if (S.session === session) lost(String(e?.message ?? e)); return; }
    if (S.session !== session) return;
    try { for (const ev of batch.events ?? []) await enqueue(() => handle(ev, session)); }
    catch (e) { if (S.session === session) lost(String(e?.message ?? e)); return; }
    if (batch.closed) { if (S.session === session) lost('the daemon closed the session'); return; }
  }
}
async function handle(ev, session) {
  if (S.session !== session) return;
  const terminal = await onEvent(ev);
  if (FLEET_EVENTS.has(ev.event)) { S.botsGen += 1; if (SHAPE_EVENTS.has(ev.event)) S.shapeGen += 1; else if (ev.bot) patchRailRow(ev.bot); }
  if (ev.bot && S.bots.has(ev.bot)) bot(ev.bot).touched = session;
  // During replay nothing is fetched: a load per node-producing event would serialize a long history
  // into one request each. The first load runs once follow_live arrives.
  if (!terminal) { if (S.live) await loadVisible(); render(); }
}
function lost(reason) { S.session = null; S.attached = false; S.live = false; showDetached(reason); }
// A record from the snapshot. A bot this session's events already touched keeps the state those events
// built and takes only what events do not carry; any other is seated from the record whole.
function seat(record, session) {
  if (S.deleted.has(record.name)) return;
  const b = bot(record.name);
  const conflict = b && b.id != null && record.id != null && b.id !== record.id;
  if (!b || b.touched !== session) { upsert(record); return; }
  if (conflict) return;
  if (record.id != null) b.id = record.id;
  b.model = `${record.provider ?? '?'}/${record.model ?? '?'}`;
  b.workspace = record.workspace ?? null;
  if (record.created_by) { b.parent = record.created_by; b.parentId = record.created_by_id ?? null; }
  seedHistory(record);
}
let attaching = null, retryTimer = null;
function retryAttach(delay = 2000) {
  clearTimeout(retryTimer);
  retryTimer = setTimeout(() => { retryTimer = null; if (!S.attached) attach(); }, delay);
}
function attach() {
  if (!attaching) {
    clearTimeout(retryTimer); retryTimer = null;
    attaching = attachOnce().finally(() => {
      attaching = null;
      if (!S.attached) retryAttach();
    });
  }
  return attaching;
}
async function attachOnce() {
  try {
    if (!S.config) S.config = await Daemon.setup();
    const { session } = await Daemon.attach(S.cursor);
    S.session = session;
    S.deleted = new Set(); S.snapshot = true;
    pump(session);
    // The snapshot, a page at a time, applied as it arrives while the replay flows.
    const listed = new Set(); let after = null;
    for (;;) {
      const page = await Daemon.request('bots', { after, limit: 256 });
      if (S.session !== session) return false;
      for (const record of page.bots ?? []) { listed.add(record.name); seat(record, session); }
      if (!page.next_after) break;
      after = page.next_after;
    }
    // Gone from the store while this page had no session: its live-only `deleted` notice cannot be
    // replayed. A bot this session's events mentioned was born after its page was listed, not deleted.
    for (const [name, b] of [...S.bots]) if (!listed.has(name) && b.touched !== session) { forgetBot(name); }
    S.snapshot = false; S.deleted.clear();
    S.attached = true;
    restore();
    // A selection deleted while detached had no `deleted` event to replay; show a surviving bot.
    if (!S.bots.has(S.selected)) { const first = tree()[0]; S.selected = first ? first.b.name : ''; }
    S.botsGen += 1; S.shapeGen += 1;
    await enqueue(loadVisible);
    if (S.session !== session) return false;
    $('detached').classList.remove('on');
    render();
    return true;
  } catch (e) {
    Daemon.log?.(`attach failed: ${e?.message ?? e}`);
    lost(String(e?.message ?? e));
    return false;
  }
}
function showDetached(reason) {
  S.attached = false;
  $('detached').innerHTML = `<div><b>not attached</b></div><div>${esc(reason)}</div><div style="margin-top:8px">daemon at <span class="k">${esc(S.config?.socket ?? '?')}</span> · retrying</div>`;
  $('detached').classList.add('on');
  retryAttach();
}
function restore() {
  let saved = null; try { saved = JSON.parse(localStorage.getItem(sessionKey()) || 'null'); } catch (_) {}
  if (!saved) return;
  if (saved.selected && S.bots.has(saved.selected)) { S.selected = saved.selected; S.autoSelect = false; }
  if (saved.peek && S.bots.has(saved.peek)) S.ui.peek = saved.peek;
  S.ui.rail = !!saved.rail; S.ui.thoughts = !!saved.thoughts; S.ui.output = !!saved.output;
}
function save() { try { localStorage.setItem(sessionKey(), JSON.stringify({ selected: S.selected, peek: S.ui.peek, rail: S.ui.rail, thoughts: S.ui.thoughts, output: S.ui.output })); } catch (_) {} }
window.addEventListener('beforeunload', save);

// ---------- render ----------
function inline(text) {
  return esc(text).replace(/\*\*(.+?)\*\*/g, '<h>$1</h>').replace(/`([^`]+)`/g, '<code>$1</code>');
}
function markdown(text) {
  const out = []; let fence = null;
  for (const raw of text.split('\n')) {
    const m = raw.trimStart().match(/^```(.*)$/);
    if (m) { if (fence) { out.push(`<pre class="code">${fence.lang ? `<span class="lang">${esc(fence.lang)}</span>` : ''}${esc(fence.body.join('\n'))}</pre>`); fence = null; } else fence = { lang: m[1].trim(), body: [] }; continue; }
    if (fence) { fence.body.push(raw); continue; }
    const h = raw.trimStart().match(/^#+\s*(.*)$/);
    out.push(h ? `<div class="line text"><h>${inline(h[1])}</h></div>` : `<div class="line text">${inline(raw)}</div>`);
  }
  if (fence) out.push(`<pre class="code">${fence.lang ? `<span class="lang">${esc(fence.lang)}</span>` : ''}${esc(fence.body.join('\n'))}</pre>`);
  return out.join('');
}
function cardInner({ status, name, last, elapsed, body }) {
  return `<span class="glyph ${status}">${glyphOf(status)}</span><span class="pn">${esc(name)}</span><span class="el">${elapsed ?? ''}</span><span class="pl">${esc(last)}</span>${body ?? ''}`;
}
function cardHTML(card) {
  return `<div class="peer${card.sel ? ' sel' : ''}" ${card.attr} role="button" tabindex="0">${cardInner(card)}</div>`;
}
const cssEsc = (s) => String(s).replace(/[\x00-\x1f\x7f"\\]/g, (c) => c === '\0' ? '\ufffd' : c === '"' || c === '\\' ? '\\' + c : '\\' + c.charCodeAt(0).toString(16) + ' ');
function peerCard(who) {
  const p = bot(who); if (!p) return null;
  const el = p.turnStarted ? fmt(Date.now() - p.turnStarted) : p.elapsed ? fmt(p.elapsed) : '';
  return { attr: `data-peek="${esc(p.name)}"`, status: p.status, name: p.name, last: lastLine(transcript(who)), elapsed: el, sel: S.ui.peek === who };
}
// Replace one rendered item in place, in whichever pane shows that bot, so a tool finishing or a
// process ending costs the size of its own line, not a rebuild of the window.
function patchItem(name, selector, it) {
  for (const [id, shown] of [['log', S.selected], ['peek', S.ui.peek]]) {
    if (shown !== name) continue;
    const old = $(id).querySelector(selector);
    if (old) old.outerHTML = itemHTML(it);
  }
}
// What changes with time, refreshed in place: peer cards (their peer's status and last line), and
// running tools' elapsed. Cheap: a pane holds a few cards and fewer running tools.
function refreshLive(el) {
  for (const card of el.querySelectorAll('.peer[data-peek]')) {
    const c = peerCard(card.dataset.peek); if (!c) continue;
    card.classList.toggle('sel', c.sel); card.innerHTML = cardInner(c);
  }
  for (const line of el.querySelectorAll('.tool[data-started]')) {
    const span = line.querySelector('.el'); if (span) span.textContent = fmt(Date.now() - Number(line.dataset.started));
  }
}
// A card's one line: the last non-empty line of the newest text, bounded, so a long reply costs the
// parent's render nothing.
function tailOf(s) { const end = s.slice(-400).trimEnd(); const at = end.lastIndexOf('\n'); return end.slice(at + 1).trim().slice(0, 200); }
function lastLine(t) {
  if (t.text) return tailOf(t.text); if (t.thinking) return t.thinking.slice(-400).split('. ').pop().slice(0, 200);
  for (let i = t.items.length - 1; i >= 0; i--) { const it = t.items[i]; if (it.kind === 'text') return tailOf(it.text); if (it.kind === 'tool') return `▸ ${it.name} ${it.summary}`; }
  return '';
}
function itemHTML(it) {
  switch (it.kind) {
    case 'user': return `<div class="line user">› ${esc(it.text)}</div>`;
    case 'text': return markdown(it.text);
    case 'thought': return S.ui.thoughts ? `<div class="line think">${esc(it.text)}</div>` : `<div class="line think folded">thought ${fmt((it.secs || 0) * 1000)}</div>`;
    case 'tool': { const el = it.started ? `<span class="el">${fmt(Date.now() - it.started)}</span>` : it.took >= 1500 ? `<span class="el">${fmt(it.took)}</span>` : ''; return `<div class="line tool" data-call="${esc(it.callId)}"${it.started ? ` data-started="${it.started}"` : ''}>▸ <b>${esc(it.name)}</b> ${esc(it.summary)}${el}</div>`; }
    case 'out': { const rows = it.text.split('\n').filter((l) => l.trim()); const shown = !S.ui.output && rows.length > 2 ? rows.slice(0, 2) : rows; return `<div class="line out">${esc(shown.join('\n'))}${shown.length < rows.length ? ` <span class="more">+${rows.length - shown.length} lines</span>` : ''}</div>`; }
    case 'note': return `<div class="line note">${esc(it.text)}</div>`;
    case 'note_gap': return `<div class="line note">${it.total} ${it.later ? 'later' : 'earlier'} activity notes summarized · durable messages remain available</div>`;
    case 'peer_gap': return `<div class="line note">${it.total} earlier peers · use ^k to find a bot</div>`;
    case 'peer': { const c = peerCard(it.who); return c ? cardHTML(c) : ''; }
    case 'proc': { const status = it.done === null ? 'running' : 'idle'; const last = it.done === null ? it.handle : (it.done || 'done'); return cardHTML({ attr: `data-proc="${esc(it.handle)}"`, status, name: `$ ${it.cmd}`, last, elapsed: '', sel: false }); }
    case 'node': return '';
    default: return '';
  }
}
// The items from `from` on; a blank line separates turns, judged against the nearest earlier item with
// one. A run of bare nodes is one placeholder row, so unloaded history costs one element per gap.
function itemsHTML(t, from = 0) {
  let h = ''; let lastTurn = null; let gap = 0;
  for (let i = from - 1; i >= 0; i--) if (t.items[i].turn != null) { lastTurn = t.items[i].turn; break; }
  const flush = () => { if (gap) { h += `<div class="line pending">… ${gap} earlier</div>`; gap = 0; } };
  for (let i = from; i < t.items.length; i++) {
    const it = t.items[i];
    if (it.kind === 'history') { flush(); h += `<div class="line pending">… ${it.forward ? 'later history · scroll down' : 'earlier history · scroll up'} to load</div>`; continue; }
    if (it.kind === 'node' || it.kind === 'tool_stub') { gap += 1; continue; }
    flush();
    if (it.turn != null && it.turn !== lastTurn) { if (h || from > 0) h += '<div class="line"></div>'; lastTurn = it.turn; }
    h += itemHTML(it);
  }
  flush();
  return h;
}
const tails = new WeakMap();
function renderTail(el, name, t) {
  const kind = t.text ? 'text' : t.thinking ? 'thinking' : '';
  const value = kind ? t[kind] : '';
  let state = tails.get(el);
  const running = bot(name)?.status === 'running';
  if (!state || state.transcript !== t || state.kind !== kind || state.turn !== t.streamingTurn || state.gen !== t.streamGen || state.offset > value.length || state.running !== running) {
    const line = document.createElement('div'); line.className = kind === 'thinking' ? 'line think' : 'line text';
    const text = document.createTextNode('');
    const cursor = document.createElement('span'); cursor.className = 'cursor';
    line.replaceChildren(text, cursor);
    el.replaceChildren(...(kind || running ? [line] : []));
    state = { transcript: t, kind, turn: t.streamingTurn, gen: t.streamGen, offset: 0, text, running };
    tails.set(el, state);
  }
  // Plain text while streaming; the durable message gets Markdown once. No full-prefix parsing
  // or HTML replacement on each delta, and provider text never becomes markup.
  if (value.length > state.offset) { state.text.appendData(value.slice(state.offset)); state.offset = value.length; }
}
// A streamed delta touches only the tail. The items rebuild when their
// count or a fold changes, when a load replaced nodes (gen), or on the
// slow tick that refreshes elapsed counters and cards.
function renderTranscript(el, name) {
  const t = S.transcripts.get(name);
  const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
  const before = el.scrollHeight;
  if (!t) { el.innerHTML = ''; el.dataset.key = ''; return; }
  // A structural change (a load, a fold, another bot) rebuilds the window; items appended since the
  // last render are added on their own; everything else changes in place. A streamed delta touches
  // only the tail.
  const key = `${name}|${t.gen}|${S.ui.thoughts}|${S.ui.output}`;
  const rendered = el.dataset.key === key ? Number(el.dataset.len) : -1;
  let tail = el.lastElementChild;
  if (rendered < 0 || rendered > t.items.length || !tail || !tail.classList.contains('tail')) {
    el.innerHTML = itemsHTML(t) + '<div class="tail"></div>';
    el.dataset.key = key;
    tail = el.lastElementChild;
    // History loaded above the reader keeps their place instead of shoving it down.
    if (!atBottom) el.scrollTop += el.scrollHeight - before;
  } else if (rendered < t.items.length) {
    tail.insertAdjacentHTML('beforebegin', itemsHTML(t, rendered));
  }
  el.dataset.len = String(t.items.length);
  refreshLive(el);
  renderTail(tail, name, t);
  if (atBottom) el.scrollTop = el.scrollHeight;
}
for (const [id, who] of [['log', () => S.selected], ['peek', () => S.ui.peek]]) {
  $(id).addEventListener('scroll', () => {
    const el = $(id); const name = who(); const t = name && S.transcripts.get(name); if (!t) return;
    const nearTop = el.scrollTop < 200, nearEnd = el.scrollHeight - el.scrollTop - el.clientHeight < 200;
    // The window follows the reader: the edge they reach is where bodies stay decoded.
    if (nearTop && !nearEnd) t.anchor = 'top'; else if (nearEnd) t.anchor = 'end';
    if ((nearTop || nearEnd) && (t.nodes > 0 || t.history != null)) enqueue(async () => { await load(name, nearTop); render(); });
  });
}
function titleHTML(b, closable) { return `<span class="glyph ${b.status}">${glyphOf(b.status)}</span><b>${esc(b.name)}</b><span>${labelOf(b.status)}</span>${closable ? '<span class="x">Esc closes</span>' : ''}`; }
// The rail shows a window of rows around the selection; scrolling to an edge extends it. The tree is
// rebuilt when the fleet's shape changes (a bot created, forked or deleted); a status change patches
// the bot's own row. A fleet of thousands costs a screenful of rows, not a row each per event.
const RAIL_ROWS = 300;
const rail = { shapeGen: -1, rows: [], index: new Map(), start: 0, end: 0, key: '' };
function railRows() {
  if (rail.shapeGen !== S.shapeGen) { rail.rows = tree(); rail.index = new Map(rail.rows.map((n, i) => [n.b.name, i])); rail.shapeGen = S.shapeGen; rail.key = ''; }
  return rail.rows;
}
function renderRail() {
  const rows = railRows(); const el = $('bots');
  const sel = rail.index.get(S.selected) ?? 0;
  const key = `${S.shapeGen}|${S.selected}|${rail.start}|${rail.end}`;
  if (rail.key !== key) {
    if (sel < rail.start || sel >= rail.end || rail.key === '') { rail.start = Math.max(0, sel - RAIL_ROWS / 2); rail.end = Math.min(rows.length, rail.start + RAIL_ROWS); }
    rail.key = `${S.shapeGen}|${S.selected}|${rail.start}|${rail.end}`;
    const above = rail.start ? `<div class="botrow more">… ${rail.start} above</div>` : '';
    const below = rail.end < rows.length ? `<div class="botrow more">… ${rows.length - rail.end} below</div>` : '';
    el.innerHTML = above + rows.slice(rail.start, rail.end).map((n) => botRowHTML(n, n.b.name === S.selected)).join('') + below;
    el.dataset.key = rail.key;
  }
}
// A bot's row, replaced in place when its status changes; nothing if it is outside the window.
function patchRailRow(name) {
  if (!S.ui.rail) return;
  const i = rail.index.get(name); if (i === undefined || i < rail.start || i >= rail.end) return;
  const el = $('bots'); const old = el.querySelector(`.botrow[data-bot="${cssEsc(name)}"]`); if (!old) return;
  if (old.nextElementSibling?.classList.contains('w')) old.nextElementSibling.remove();
  old.outerHTML = botRowHTML(rail.rows[i], name === S.selected);
}
$('bots').addEventListener('scroll', () => {
  const el = $('bots'); const rows = rail.rows; let moved = false;
  if (el.scrollTop < 100 && rail.start > 0) { rail.start = Math.max(0, rail.start - RAIL_ROWS / 2); moved = true; }
  if (el.scrollHeight - el.scrollTop - el.clientHeight < 100 && rail.end < rows.length) { rail.end = Math.min(rows.length, rail.end + RAIL_ROWS / 2); moved = true; }
  if (moved) { const before = el.scrollHeight; rail.key = ''; renderRail(); if (el.scrollTop < 100) el.scrollTop += el.scrollHeight - before; }
});
function botRowHTML(n, sel) {
  const b = n.b;
  const w = b.waitingOn.length ? `<div class="w" style="padding-left:${3 + n.depth * 2}ch">⏳ ${b.waitingOn.map((h) => esc(h.replace(/^turn:/, '').split('/')[0])).join(' ')}</div>` : '';
  return `<div class="botrow${sel ? ' sel' : ''}" data-bot="${esc(b.name)}" role="button" tabindex="0"><span class="tree">${n.prefix}</span><span class="glyph ${b.status}">${glyphOf(b.status)}</span><span class="n">${esc(b.name)}</span></div>${w}`;
}
function peers() { return (S.transcripts.get(S.selected)?.peers ?? []).filter((who) => S.bots.has(who)); }
function keybarHTML(b) {
  const busy = b && isActive(b.status);
  const dot = `<span><span class="dot${!S.attached ? ' off' : busy ? ' busy' : ''}"></span>${!S.attached ? 'detached' : busy ? labelOf(b.status) : 'live'}</span>`;
  const keys = [];
  if (S.ui.picker) keys.push('<kbd>↑↓</kbd> choose', '<kbd>Enter</kbd> switch', '<kbd>Esc</kbd> cancel');
  else if (S.ui.peek) keys.push('<kbd>Esc</kbd> close', '<kbd>^p</kbd> next peer', '<kbd>^k</kbd> switch');
  else {
    keys.push('<kbd>^k</kbd> switch'); if (peers().length) keys.push('<kbd>^p</kbd> peek'); keys.push('<kbd>^b</kbd> bots');
    const t = S.transcripts.get(S.selected);
    if (t?.thoughts > 0) keys.push(`<kbd>^t</kbd> ${S.ui.thoughts ? 'fold' : 'thoughts'}`);
    if (t?.longOut > 0) keys.push(`<kbd>^o</kbd> ${S.ui.output ? 'fold' : 'output'}`);
    if (busy) keys.push('<kbd>Esc</kbd> interrupt');
    keys.push('<kbd>^d</kbd> detach');
  }
  return `${dot}<span class="bot">${esc(b?.name ?? '')}</span>${S.ui.toast ? `<span class="toast">${esc(S.ui.toast)}</span>` : ''}<span class="spacer"></span>${keys.join('<span> </span>')}<span><kbd>?</kbd> keys</span>`;
}
function render() {
  const app = $('app'); const b = bot(S.selected);
  app.classList.toggle('rail', S.ui.rail); app.classList.toggle('peek', !!S.ui.peek && S.bots.has(S.ui.peek));
  $('title').innerHTML = b ? titleHTML(b) : '<span>no bots · /new NAME creates one</span>';
  if (b) renderTranscript($('log'), b.name); else $('log').innerHTML = '';
  if (S.ui.rail) renderRail();
  if (S.ui.peek && bot(S.ui.peek)) { $('peektitle').innerHTML = titleHTML(bot(S.ui.peek), true); renderTranscript($('peek'), S.ui.peek); }
  $('who').textContent = b ? `${b.name} ›` : '›';
  $('input').placeholder = b ? (b.status === 'idle' ? '' : `${b.name} is ${labelOf(b.status)}; your message queues`) : '/new NAME [PROVIDER/MODEL]';
  $('keybar').innerHTML = keybarHTML(b);
  if (S.ui.picker) renderPicker();
}
// Once a second, while anything runs: the clocks on cards and tool lines, in place. The activity
// check is cached per fleet change, so a quiet fleet of any size costs nothing here.
let activeAt = -1, active = false;
function anyActive() { if (activeAt !== S.botsGen) { activeAt = S.botsGen; active = [...S.bots.values()].some((b) => isActive(b.status)); } return active; }
setInterval(() => { if (S.attached && anyActive()) { refreshLive($('log')); if (S.ui.peek) refreshLive($('peek')); const b = bot(S.selected); if (b) $('title').innerHTML = titleHTML(b); } }, 1000);

// ---------- picker ----------
function pickerRows() {
  const q = $('pickerq').value.trim().toLowerCase();
  return tree().map((n) => ({ ...n, i: q ? n.b.name.toLowerCase().indexOf(q) : -1 })).filter((r) => !q || r.i >= 0);
}
const PICKER_ROWS = 200;
function renderPicker() {
  const q = $('pickerq').value.trim(); const all = pickerRows(); const rows = all.slice(0, PICKER_ROWS);
  S.ui.pickerSel = Math.min(S.ui.pickerSel, Math.max(0, rows.length - 1));
  const more = all.length > rows.length ? `<div class="empty">${all.length - rows.length} more; type to narrow</div>` : '';
  $('pickerlist').innerHTML = (rows.length ? rows.map((r, idx) => {
    const n = r.b.name; const hit = r.i >= 0 ? `${esc(n.slice(0, r.i))}<span class="hit">${esc(n.slice(r.i, r.i + q.length))}</span>${esc(n.slice(r.i + q.length))}` : esc(n);
    const state = r.b.status === 'idle' ? '' : labelOf(r.b.status);
    const hint = q ? [creatorOf(r.b) ? `↳ ${r.b.parent}` : '', state].filter(Boolean).join(' · ') : state;
    return `<div class="row${idx === S.ui.pickerSel ? ' sel' : ''}" data-pick="${esc(n)}">${q ? '' : `<span class="tree">${r.prefix}</span>`}<span class="glyph ${r.b.status}">${glyphOf(r.b.status)}</span><span class="n">${hit}</span><span class="h">${esc(hint)}</span></div>`;
  }).join('') : '<div class="empty">no bot matches</div>') + more;
}
function openPicker() { S.ui.picker = true; S.ui.pickerSel = 0; $('pickerq').value = ''; $('pickerwrap').classList.add('on'); render(); $('pickerq').focus(); }
function closePicker() { S.ui.picker = false; $('pickerwrap').classList.remove('on'); render(); $('input').focus(); }
async function switchTo(name) { if (!S.bots.has(name)) return; S.selected = name; S.autoSelect = false; if (S.ui.peek === name) S.ui.peek = null; await enqueue(loadVisible); render(); save(); }
function showHelp() { S.ui.help = true; $('helptext').innerHTML = `<b>keys</b>\n ^k   switch bot        ^b   pin the bot rail\n ^p   peek next peer    Esc  close · then interrupt\n ^t   unfold thoughts   ^o   unfold tool output\n ^d   detach (close)    ↑ ↓  previous / next bot\n\n /new NAME [PROVIDER/MODEL]   create a bot\n<i>any key closes this</i>`; $('helpwrap').classList.add('on'); }
function hideHelp() { S.ui.help = false; $('helpwrap').classList.remove('on'); $('input').focus(); }

// ---------- actions ----------
async function submit(text) {
  if (text.startsWith('/new ')) {
    const [name, model] = text.slice(5).trim().split(/\s+/);
    if (!name) throw new Error('name_required');
    const m = model || S.config?.model; if (!m) throw new Error('model_required: NAME PROVIDER/MODEL, or set AGENT_MODEL');
    // Composed now, so an AGENTS.md edited since the window opened reaches this bot.
    const policy = await Daemon.policy();
    await Daemon.request('create', { bot: name, workspace: S.config.workspace, model: m, instructions: policy.instructions, tools: S.config.tools });
    await refreshBot(name); await switchTo(name); toast(`created ${name} · ${policy.note}`); return;
  }
  if (text === '/help' || text === '?') { showHelp(); return; }
  const b = bot(S.selected); if (!b) throw new Error('no bot selected; /new NAME creates one');
  // The identity on screen, so a name that changed hands in between is refused rather than handed the prompt.
  await Daemon.request('submit', { bot: b.name, bot_id: b.id ?? undefined, request_id: `app-${crypto.randomUUID()}`, prompt: text, workspace: b.workspace ?? S.config.workspace, delivery: b.status === 'idle' ? 'reject' : 'queue' });
}
async function interrupt() { const b = bot(S.selected); if (!b || b.runningTurn === null) return; try { await Daemon.request('interrupt', { bot: b.name, turn: b.runningTurn }); } catch (e) { toast(`interrupt: ${e?.message ?? e}`); } }
function detach() { save(); Daemon.close(); }

// ---------- input ----------
$('form').addEventListener('submit', async (e) => {
  e.preventDefault(); const v = $('input').value.trim(); if (!v) return; $('input').value = '';
  try { await submit(v); } catch (err) { toast(String(err?.message ?? err)); $('input').value = v; }
});
$('input').addEventListener('input', () => { if ($('input').value === '?') { $('input').value = ''; showHelp(); } });
$('pickerq').addEventListener('input', renderPicker);
$('pickerq').addEventListener('keydown', async (e) => {
  const rows = pickerRows();
  if (e.key === 'Escape') { closePicker(); e.preventDefault(); }
  else if (e.key === 'ArrowDown' || (e.ctrlKey && e.key === 'n')) { S.ui.pickerSel = Math.min(rows.length - 1, S.ui.pickerSel + 1); renderPicker(); e.preventDefault(); }
  else if (e.key === 'ArrowUp' || (e.ctrlKey && e.key === 'p')) { S.ui.pickerSel = Math.max(0, S.ui.pickerSel - 1); renderPicker(); e.preventDefault(); }
  else if (e.key === 'Enter') { const r = rows[S.ui.pickerSel]; closePicker(); if (r) await switchTo(r.b.name); e.preventDefault(); }
});
$('pickerlist').addEventListener('click', async (e) => { const r = e.target.closest('[data-pick]'); if (r) { closePicker(); await switchTo(r.dataset.pick); } });
document.addEventListener('keydown', async (e) => {
  if (S.ui.help) { hideHelp(); e.preventDefault(); return; }
  if (S.ui.picker) return;
  const k = e.key, ctrl = e.ctrlKey || e.metaKey;
  if (ctrl && k === 'k') { openPicker(); e.preventDefault(); return; }
  if (ctrl && k === 'b') { S.ui.rail = !S.ui.rail; render(); save(); e.preventDefault(); return; }
  if (ctrl && k === 'd') { detach(); e.preventDefault(); return; }
  if (ctrl && k === 't') { S.ui.thoughts = !S.ui.thoughts; render(); save(); e.preventDefault(); return; }
  if (ctrl && k === 'o') { S.ui.output = !S.ui.output; render(); save(); e.preventDefault(); return; }
  if (ctrl && k === 'p') { const ps = peers(); if (ps.length) { const i = ps.indexOf(S.ui.peek); S.ui.peek = ps[(i + 1) % ps.length]; await enqueue(loadVisible); render(); save(); } e.preventDefault(); return; }
  if (k === 'Escape') { if (S.ui.peek) S.ui.peek = null; else if (S.ui.rail) S.ui.rail = false; else await interrupt(); render(); save(); e.preventDefault(); return; }
  const empty = $('input').value === '';
  if (empty && (k === 'ArrowUp' || k === 'ArrowDown')) { const names = tree().map((n) => n.b.name); let i = names.indexOf(S.selected); if (i >= 0) { i = (i + (k === 'ArrowDown' ? 1 : names.length - 1)) % names.length; await switchTo(names[i]); } e.preventDefault(); return; }
  if (e.target.id !== 'input' && k.length === 1 && !ctrl) $('input').focus();
});
document.addEventListener('click', async (e) => {
  if (S.ui.help) { hideHelp(); return; }
  if (e.target.closest('#pickerwrap') && !e.target.closest('.picker')) { closePicker(); return; }
  const row = e.target.closest('[data-bot]'); const peek = e.target.closest('[data-peek]');
  if (row) await switchTo(row.dataset.bot);
  else if (peek) { S.ui.peek = S.ui.peek === peek.dataset.peek ? null : peek.dataset.peek; await enqueue(loadVisible); render(); save(); }
  if (!e.target.closest('input')) $('input').focus();
});

// ---------- boot ----------
render();
attach().then(() => $('input').focus());
})();
